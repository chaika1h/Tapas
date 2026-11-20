//! H.264/H.265 streams (`stream1` HD, `stream2` SD) decoded by an ffmpeg
//! child process that writes raw RGB frames to a pipe.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use slint::SharedPixelBuffer;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::time::timeout;

use crate::config::Quality;
use crate::stream::{FrameSink, StreamSpec, rtsp_url};

const FRAME_TIMEOUT: Duration = Duration::from_secs(15);

/// Finds ffmpeg: the configured path, then next to our exe, then on PATH.
pub fn find(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim().trim_matches('"');
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.is_file().then_some(p);
    }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let beside_exe = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.join(name)));
    if let Some(p) = beside_exe.filter(|p| p.is_file()) {
        return Some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

pub async fn run(spec: &StreamSpec, sink: &mut FrameSink) -> Result<()> {
    let ffmpeg = spec
        .ffmpeg
        .as_deref()
        .context("ffmpeg not found: put ffmpeg.exe next to TapoViewer.exe or set its path in Settings")?;
    let (path, w, h) = match spec.quality {
        Quality::Hd => ("stream1", 1920usize, 1080usize),
        _ => ("stream2", 640, 360),
    };
    let url = rtsp_url(spec, path, true)?;

    let mut child = command(ffmpeg, url.as_str(), w, h)
        .spawn()
        .with_context(|| format!("Could not start {}", ffmpeg.display()))?;
    let mut stdout = child.stdout.take().context("ffmpeg stdout")?;
    let stderr = child.stderr.take().context("ffmpeg stderr")?;

    // Keep ffmpeg's last error line for the status text.
    let last_error = Arc::new(Mutex::new(String::new()));
    let sink_line = last_error.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {
                *sink_line.lock().unwrap() = line;
            }
        }
    });

    let mut buf = vec![0u8; w * h * 3];
    loop {
        match timeout(FRAME_TIMEOUT, stdout.read_exact(&mut buf)).await {
            Err(_) => bail!("No video for {}s", FRAME_TIMEOUT.as_secs()),
            Ok(Err(_)) => {
                let _ = timeout(Duration::from_secs(2), child.wait()).await;
                tokio::time::sleep(Duration::from_millis(100)).await; // let stderr drain
                let line = last_error.lock().unwrap().clone();
                bail!(friendly_error(&line, spec, &url));
            }
            Ok(Ok(_)) => sink.push(SharedPixelBuffer::clone_from_slice(&buf, w as u32, h as u32)),
        }
    }
}

fn command(ffmpeg: &Path, url: &str, w: usize, h: usize) -> tokio::process::Command {
    // Letterbox into a fixed size so every frame has a known byte length.
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"
    );
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-fflags", "nobuffer", "-flags", "low_delay"])
        .args(["-rtsp_transport", "tcp", "-timeout", "10000000"])
        .args(["-i", url])
        .args(["-an", "-sn", "-vf", &vf, "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

fn friendly_error(line: &str, spec: &StreamSpec, url: &url::Url) -> String {
    if line.is_empty() {
        return "ffmpeg stopped".into();
    }
    // ffmpeg echoes the URL, so make sure no password reaches the UI or logs.
    let mut line = line.to_string();
    for secret in [spec.password.as_str(), url.password().unwrap_or("")] {
        if !secret.is_empty() {
            line = line.replace(secret, "***");
        }
    }
    crate::stream::friendly_error(&line, spec.port)
}
