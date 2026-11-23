//! Live video from the Tapo cloud relay: its MPEG-TS (H.264/H.265) goes into
//! an ffmpeg child process, which writes raw RGB frames to a pipe.

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
use crate::stream::{FrameSink, StreamSpec};

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

/// Set once the cloud refuses the sub-stream, so later requests ask for HD.
static CLOUD_VGA_REFUSED: AtomicBool = AtomicBool::new(false);

/// Requests the camera's relay and decodes it until something stops.
pub async fn run(spec: &StreamSpec, sink: &mut FrameSink) -> Result<()> {
    let ffmpeg = spec
        .ffmpeg
        .as_deref()
        .context("ffmpeg not found: live view needs it. Put ffmpeg.exe next to TapoViewer.exe or set its path in Settings")?;
    let (mut resolution, w, h) = match spec.quality {
        Quality::Hd => (Resolution::Hd, 1920usize, 1080usize),
        _ => (Resolution::Vga, 640, 360),
    };
    if CLOUD_VGA_REFUSED.load(Ordering::Relaxed) {
        resolution = Resolution::Hd;
    }

    let client = &spec.client;
    let info = match client.request_relay(&spec.camera, resolution).await {
        Err(e) if resolution == Resolution::Vga && e.is_refused() => {
            log::warn!("cloud refused the VGA stream ({e}); using HD from now on");
            CLOUD_VGA_REFUSED.store(true, Ordering::Relaxed);
            client.request_relay(&spec.camera, Resolution::Hd).await?
        }
        result => result?,
    };
    let mut relay = relay::open(client.tls(), &info, &spec.camera.model).await?;
    decode_relay(&mut relay, ffmpeg, w, h, sink).await
}

/// Decodes the relay's MPEG-TS (after its HTTP head) until either the relay
/// or ffmpeg stops.
async fn decode_relay<R>(relay: &mut R, ffmpeg: &Path, w: usize, h: usize, sink: &mut FrameSink) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    // A live pipe: bound the probing, or ffmpeg waits for more input before
    // its first frame. No `-fflags nobuffer`: it throws away the probed
    // packets, so the picture would only start at the next keyframe.
    let input = ["-flags", "low_delay", "-analyzeduration", "0", "-probesize", "32768", "-f", "mpegts", "-i", "pipe:0"];
    let mut decoder = Decoder::spawn(ffmpeg, &input, w, h)?;
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

pub(crate) enum Stopped {
    NoVideo,
    /// ffmpeg exited; its last error line, if any.
    Exited(String),
}

/// An ffmpeg process writing fixed-size RGB frames to stdout.
pub(crate) struct Decoder {
    child: tokio::process::Child,
    pub stdin: Option<tokio::process::ChildStdin>,
    pub stdout: tokio::process::ChildStdout,
    pub last_error: Arc<Mutex<String>>,
    pub width: usize,
    pub height: usize,
    /// From [`Decoder::spawn_timed`]: each frame's time in seconds from the
    /// start of the input, in output order.
    pub times: Option<tokio::sync::mpsc::UnboundedReceiver<f64>>,
}

impl Decoder {
    pub fn spawn(ffmpeg: &Path, input: &[&str], width: usize, height: usize) -> Result<Self> {
        Self::start(ffmpeg, input, width, height, false)
    }

    /// Like [`Decoder::spawn`], also reporting each frame's time (from
    /// ffmpeg's `showinfo` filter) on [`Decoder::times`].
    pub fn spawn_timed(ffmpeg: &Path, input: &[&str], width: usize, height: usize) -> Result<Self> {
        Self::start(ffmpeg, input, width, height, true)
    }

    fn start(ffmpeg: &Path, input: &[&str], width: usize, height: usize, timed: bool) -> Result<Self> {
        let mut child = command(ffmpeg, input, width, height, timed)
            .spawn()
            .with_context(|| format!("Could not start {}", ffmpeg.display()))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("ffmpeg stdout")?;
        let stderr = child.stderr.take().context("ffmpeg stderr")?;

        // Keep ffmpeg's last error line for the status text.
        let last_error = Arc::new(Mutex::new(String::new()));
        let sink_line = last_error.clone();
        let (times_tx, times) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !timed {
                    if !line.trim().is_empty() {
                        *sink_line.lock().unwrap() = line;
                    }
                } else if let Some(t) = showinfo_time(&line) {
                    let _ = times_tx.send(t);
                } else if line.contains("rror") {
                    // At this log level, only lines that say so are errors.
                    *sink_line.lock().unwrap() = line;
                }
            }
        });
        let times = timed.then_some(times);
        Ok(Self { child, stdin, stdout, last_error, width, height, times })
    }

    /// Pushes frames into `sink` until ffmpeg stops or goes quiet.
    async fn push_frames(&mut self, sink: &mut FrameSink) -> Stopped {
        self.push_frames_within(sink, FRAME_TIMEOUT).await
    }

    /// Like [`Decoder::push_frames`], waiting up to `quiet` for each frame.
    pub async fn push_frames_within(&mut self, sink: &mut FrameSink, quiet: Duration) -> Stopped {
        let (w, h) = (self.width, self.height);
        let mut buf = vec![0u8; w * h * 3];
        loop {
            match timeout(quiet, self.stdout.read_exact(&mut buf)).await {
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

/// Decodes a single encoded picture (an event's keyframe from a hub,
/// `image/avc` or `image/hevc`) into a `width`×`height` frame.
pub async fn decode_picture(ffmpeg: &Path, data: &[u8], content_type: &str, width: usize, height: usize) -> Result<SharedPixelBuffer<slint::Rgb8Pixel>> {
    use tokio::io::AsyncWriteExt;
    let format = if content_type.contains("hevc") || content_type.contains("265") { "hevc" } else { "h264" };
    let mut decoder = Decoder::spawn(ffmpeg, &["-f", format, "-i", "pipe:0"], width, height)?;
    let mut stdin = decoder.stdin.take().context("ffmpeg stdin")?;
    stdin.write_all(data).await.context("Could not pass the picture to ffmpeg")?;
    drop(stdin);
    let mut buf = vec![0u8; width * height * 3];
    match timeout(Duration::from_secs(10), decoder.stdout.read_exact(&mut buf)).await {
        Ok(Ok(_)) => Ok(SharedPixelBuffer::clone_from_slice(&buf, width as u32, height as u32)),
        _ => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let error = decoder.last_error.lock().unwrap().clone();
            bail!("Could not decode the picture{}", if error.is_empty() { String::new() } else { format!(": {error}") })
        }
    }
}

/// The frame time in a `showinfo` line: `... n:  12 pts: 108000 pts_time:1.2 ...`.
fn showinfo_time(line: &str) -> Option<f64> {
    if !line.contains("Parsed_showinfo") {
        return None;
    }
    let rest = line.split(" pts_time:").nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

fn command(ffmpeg: &Path, input: &[&str], w: usize, h: usize, timed: bool) -> tokio::process::Command {
    // Letterbox into a fixed size so every frame has a known byte length.
    let mut vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"
    );
    if timed {
        vf = format!("showinfo=checksum=0,{vf}");
    }
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostats", "-loglevel", if timed { "info" } else { "error" }])
        .args(input)
        // rawvideo has no timestamps, so ffmpeg would otherwise pad to a
        // guessed constant rate, duplicating frames (e.g. 15 fps → 30).
        .args(["-an", "-sn", "-vf", &vf, "-fps_mode", "passthrough"])
        .args(["-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
        .stdin(Stdio::piped())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Slot, Status};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn reads_frame_times() {
        let line = "[Parsed_showinfo_0 @ 0x7f] n:   1 pts:   6000 pts_time:0.0666667 duration:   6000 iskey:0 type:P";
        assert_eq!(showinfo_time(line), Some(0.0666667));
        assert_eq!(showinfo_time("[Parsed_showinfo_0 @ 0x7f] config in time_base: 1/90000"), None);
        assert_eq!(showinfo_time("Input #0, mpegts, from 'pipe:0': pts_time:1"), None);
    }

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
