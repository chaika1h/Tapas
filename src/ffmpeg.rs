//! H.264/H.265 streams decoded by an ffmpeg child process that writes raw RGB
//! frames to a pipe: RTSP from the camera (`stream1` HD, `stream2` SD), or
//! MPEG-TS from the Tapo cloud relay.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use slint::SharedPixelBuffer;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::time::timeout;

use crate::cloud::Resolution;
use crate::config::Quality;
use crate::relay;
use crate::stream::{FrameSink, StreamSpec, rtsp_url};

const FRAME_TIMEOUT: Duration = Duration::from_secs(15);

/// Finds ffmpeg: the configured path, then the copy built into this exe, then
/// next to the exe, then on PATH.
pub fn find(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim().trim_matches('"');
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.is_file().then_some(p);
    }
    if let Some(p) = bundled::extract() {
        return Some(p);
    }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let beside_exe = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.join(name)));
    if let Some(p) = beside_exe.filter(|p| p.is_file()) {
        return Some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// Whether `path` is the copy unpacked from this exe.
pub fn is_bundled(path: &Path) -> bool {
    bundled::extract().is_some_and(|p| p == path)
}

/// The minimal ffmpeg embedded at build time (see `build.rs`), unpacked on
/// first use to `%LOCALAPPDATA%\TapoViewer`.
#[cfg(bundled_ffmpeg)]
mod bundled {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    static BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ffmpeg.bin"));

    pub fn extract() -> Option<PathBuf> {
        static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
        PATH.get_or_init(|| match unpack() {
            Ok(p) => Some(p),
            Err(e) => {
                log::error!("could not unpack the built-in ffmpeg: {e:#}");
                None
            }
        })
        .clone()
    }

    fn unpack() -> anyhow::Result<PathBuf> {
        let dir = crate::config::data_dir();
        let name = format!("ffmpeg-{}{}", env!("BUNDLED_FFMPEG_ID"), std::env::consts::EXE_SUFFIX);
        let path = dir.join(&name);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == BINARY.len() as u64) {
            return Ok(path);
        }
        std::fs::create_dir_all(&dir)?;
        // Remove copies left by older versions of the app.
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let old = entry.file_name().to_string_lossy().into_owned();
            if old.starts_with("ffmpeg-") && old != name {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, BINARY)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&tmp, &path)?;
        log::info!("unpacked built-in ffmpeg to {}", path.display());
        Ok(path)
    }
}

#[cfg(not(bundled_ffmpeg))]
mod bundled {
    pub fn extract() -> Option<std::path::PathBuf> {
        None
    }
}

fn require_ffmpeg(spec: &StreamSpec) -> Result<&Path> {
    spec.ffmpeg
        .as_deref()
        .context("ffmpeg not found: put ffmpeg.exe next to TapoViewer.exe or set its path in Settings")
}

pub async fn run(spec: &StreamSpec, sink: &mut FrameSink) -> Result<()> {
    let ffmpeg = require_ffmpeg(spec)?;
    let (path, w, h) = match spec.quality {
        Quality::Hd => ("stream1", 1920usize, 1080usize),
        _ => ("stream2", 640, 360),
    };
    let url = rtsp_url(spec, path, true)?;
    let input = [
        "-nostdin", "-fflags", "nobuffer", "-flags", "low_delay", "-rtsp_transport", "tcp", "-timeout", "10000000",
        "-i", url.as_str(),
    ];
    let mut decoder = Decoder::spawn(ffmpeg, &input, w, h, false)?;
    match decoder.push_frames(sink).await {
        Stopped::NoVideo => bail!("No video for {}s", FRAME_TIMEOUT.as_secs()),
        Stopped::Exited(line) => bail!(friendly_error(&line, spec, &url)),
    }
}

/// Set once the cloud refuses the sub-stream, so later requests ask for HD.
static CLOUD_VGA_REFUSED: AtomicBool = AtomicBool::new(false);

/// Live view through the Tapo cloud relay: the relay's MPEG-TS goes into
/// ffmpeg's stdin, decoded frames come out as for RTSP.
pub async fn run_cloud(spec: &StreamSpec, sink: &mut FrameSink) -> Result<()> {
    let target = spec.cloud.as_ref().context("This camera isn't linked to your Tapo account")?;
    let ffmpeg = spec
        .ffmpeg
        .as_deref()
        .context("Live view through the Tapo cloud needs ffmpeg: set its path in Settings")?;
    let (mut resolution, w, h) = match spec.quality {
        Quality::Hd => (Resolution::Hd, 1920usize, 1080usize),
        _ => (Resolution::Vga, 640, 360),
    };
    if CLOUD_VGA_REFUSED.load(Ordering::Relaxed) {
        resolution = Resolution::Hd;
    }

    let client = &target.client;
    let info = match client.request_relay(&target.camera, resolution).await {
        Err(e) if resolution == Resolution::Vga && e.is_refused() => {
            log::warn!("cloud refused the VGA stream ({e}); using HD from now on");
            CLOUD_VGA_REFUSED.store(true, Ordering::Relaxed);
            client.request_relay(&target.camera, Resolution::Hd).await?
        }
        result => result?,
    };
    let mut relay = relay::open(client.tls(), &info, &target.camera.model).await?;
    decode_relay(&mut relay, ffmpeg, w, h, sink).await
}

/// Decodes the relay's MPEG-TS (after its HTTP head) until either the relay
/// or ffmpeg stops.
async fn decode_relay<R>(relay: &mut R, ffmpeg: &Path, w: usize, h: usize, sink: &mut FrameSink) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    // A live pipe: bound the probing, or ffmpeg waits for more input before
    // its first frame. Unlike RTSP, no `-fflags nobuffer`: it throws away the
    // probed packets, so the picture only starts at the next keyframe.
    let input = ["-flags", "low_delay", "-analyzeduration", "0", "-probesize", "32768", "-f", "mpegts", "-i", "pipe:0"];
    let mut decoder = Decoder::spawn(ffmpeg, &input, w, h, true)?;
    let mut stdin = decoder.stdin.take().context("ffmpeg stdin")?;
    tokio::select! {
        result = relay::copy_video(relay, &mut stdin) => result,
        stopped = decoder.push_frames(sink) => match stopped {
            Stopped::NoVideo => bail!("No video for {}s", FRAME_TIMEOUT.as_secs()),
            Stopped::Exited(line) if line.is_empty() => bail!("ffmpeg stopped"),
            Stopped::Exited(line) => bail!("ffmpeg: {line}"),
        },
    }
}

enum Stopped {
    NoVideo,
    /// ffmpeg exited; its last error line, if any.
    Exited(String),
}

/// An ffmpeg process writing fixed-size RGB frames to stdout.
struct Decoder {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: tokio::process::ChildStdout,
    last_error: Arc<Mutex<String>>,
    width: usize,
    height: usize,
}

impl Decoder {
    fn spawn(ffmpeg: &Path, input: &[&str], width: usize, height: usize, piped_stdin: bool) -> Result<Self> {
        let mut child = command(ffmpeg, input, width, height, piped_stdin)
            .spawn()
            .with_context(|| format!("Could not start {}", ffmpeg.display()))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
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
        Ok(Self { child, stdin, stdout, last_error, width, height })
    }

    /// Pushes frames into `sink` until ffmpeg stops or goes quiet.
    async fn push_frames(&mut self, sink: &mut FrameSink) -> Stopped {
        let (w, h) = (self.width, self.height);
        let mut buf = vec![0u8; w * h * 3];
        loop {
            match timeout(FRAME_TIMEOUT, self.stdout.read_exact(&mut buf)).await {
                Err(_) => return Stopped::NoVideo,
                Ok(Err(_)) => {
                    let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
                    tokio::time::sleep(Duration::from_millis(100)).await; // let stderr drain
                    return Stopped::Exited(self.last_error.lock().unwrap().clone());
                }
                Ok(Ok(_)) => sink.push(SharedPixelBuffer::clone_from_slice(&buf, w as u32, h as u32)),
            }
        }
    }
}

fn command(ffmpeg: &Path, input: &[&str], w: usize, h: usize, piped_stdin: bool) -> tokio::process::Command {
    // Letterbox into a fixed size so every frame has a known byte length.
    let vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"
    );
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error"])
        .args(input)
        // rawvideo has no timestamps, so ffmpeg would otherwise pad to a
        // guessed constant rate, duplicating frames (e.g. 15 fps → 30).
        .args(["-an", "-sn", "-vf", &vf, "-fps_mode", "passthrough"])
        .args(["-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
        .stdin(if piped_stdin { Stdio::piped() } else { Stdio::null() })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Slot, Status};
    use tokio::io::AsyncWriteExt;

    /// The cloud path end to end from the relay's multipart body: real H.264
    /// MPEG-TS with an audio track the minimal ffmpeg can't decode (as the
    /// cameras' G.711 audio), split into parts of uneven sizes.
    #[tokio::test]
    async fn decodes_relay_video() {
        let Some(ffmpeg) = find("") else {
            eprintln!("skipped: no ffmpeg (build one with tools/build-ffmpeg.sh)");
            return;
        };
        let ts = include_bytes!("../tests/fixtures/relay-sample.ts");
        let mut body = Vec::new();
        for (i, chunk) in ts.chunks(1316 * 3 + 7).enumerate() {
            body.extend(format!("----device-stream-boundary--\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\n", chunk.len()).as_bytes());
            if i % 5 == 0 {
                body.extend(b"X-Session-Id: 7\r\n"); // unknown headers are ignored
            }
            body.extend(b"\r\n");
            body.extend(chunk);
            body.extend(b"\r\n");
        }
        // A relay that stays open after the sample, like a live one.
        let (mut relay_tx, relay_rx) = tokio::io::duplex(1 << 20);
        relay_tx.write_all(&body).await.unwrap();
        let mut relay = BufReader::new(relay_rx);

        let slot = Arc::new(Mutex::new(Slot { frame: None, version: 0, status: Status::Connecting, fps: 0.0 }));
        let mut sink = FrameSink::new(slot.clone());
        let enough_frames = async {
            while slot.lock().unwrap().version < 25 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::select! {
            _ = enough_frames => {}
            result = decode_relay(&mut relay, &ffmpeg, 640, 360, &mut sink) => panic!("decoding stopped: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(20)) => panic!("only {} frames", slot.lock().unwrap().version),
        }

        let frame = slot.lock().unwrap().frame.clone().unwrap();
        assert_eq!((frame.width(), frame.height()), (640, 360));
        // testsrc2 is colourful: the picture must not be flat.
        let bytes = frame.as_bytes();
        let distinct: std::collections::HashSet<_> = bytes.chunks(3).step_by(97).collect();
        assert!(distinct.len() > 20, "frame looks blank");
        drop(relay_tx);
    }
}
