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

/// The minimal ffmpeg embedded at build time (see `build.rs`), unpacked on
/// first use to `%LOCALAPPDATA%\Tapas`.
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
///
/// Uses the in-process OpenH264 decoder for H.264 streams (zero `ffmpeg.exe`
/// spawn, zero `-probesize` delay, zero pipe IPC) and transparently falls back
/// to `ffmpeg.exe` (with D3D11VA/DXVA2 hardware decode on Windows HD) if the
/// camera sends HEVC/H.265 or an unsupported H.264 profile.
pub async fn run(spec: &StreamSpec, sink: &mut FrameSink) -> Result<()> {
    let (mut resolution, w, h) = match spec.quality {
        Quality::Hd => (Resolution::Hd, 2560usize, 1440usize),
        _ => (Resolution::Vga, 640, 360),
    };
    if CLOUD_VGA_REFUSED.load(Ordering::Relaxed) {
        resolution = Resolution::Hd;
    }

    let client = &spec.client;
    let started = std::time::Instant::now();
    let info = match client.request_relay(&spec.camera, resolution).await {
        Err(e) if resolution == Resolution::Vga && e.is_refused() => {
            log::warn!("cloud refused the VGA stream ({e}); using HD from now on");
            CLOUD_VGA_REFUSED.store(true, Ordering::Relaxed);
            client.request_relay(&spec.camera, Resolution::Hd).await?
        }
        result => result?,
    };
    let asked = started.elapsed();
    let mut relay = relay::open(client.tls(), &info, &spec.camera.model).await?;
    let name = format!("{} ({:?})", spec.camera.name, spec.quality);
    sink.time_first_frame(name.clone(), started, asked);
    decode_relay_hybrid(&mut relay, spec.ffmpeg.as_deref(), &name, w, h, sink).await
}

/// Decodes a live relay stream using in-process OpenH264 first, seamlessly
/// handing off buffered initial TS bytes to `ffmpeg.exe` if HEVC or an
/// unsupported bitstream is encountered before the first frame.
async fn decode_relay_hybrid<R>(
    relay: &mut R,
    ffmpeg: Option<&Path>,
    name: &str,
    w: usize,
    h: usize,
    sink: &mut FrameSink,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use crate::live_decode::{H264Decoder, TsVideoDemuxer, VideoCodec, WorkerSignal};
    use tokio::io::AsyncWriteExt;

    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let audio_id = sink.audio_id;
    let recorder = sink.recorder.clone();

    // Start the blocking OpenH264 worker thread. On HD streams keep native
    // pixels up to 4K (zero downscaling, hits AVX2 SIMD direct path on 2K/4K).
    let (inproc_w, inproc_h) = if w >= 1280 { (3840, 2160) } else { (w, h) };
    let (worker_in_tx, mut worker_in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let (worker_out_tx, mut worker_out_rx) = tokio::sync::mpsc::unbounded_channel::<WorkerSignal>();
    let _worker_handle = tokio::task::spawn_blocking(move || {
        let mut demux = TsVideoDemuxer::new();
        let mut decoder = match H264Decoder::new(inproc_w, inproc_h) {
            Ok(d) => d,
            Err(e) => {
                let _ = worker_out_tx.send(WorkerSignal::Fallback(e.to_string()));
                return;
            }
        };
        let mut got_frame = false;
        let mut init_bytes = 0usize;
        let mut init_aus = 0usize;
        let mut last_err = String::new();

        while let Some(chunk) = worker_in_rx.blocking_recv() {
            if !got_frame {
                init_bytes += chunk.len();
            }
            let mut abort_to_ffmpeg: Option<String> = None;
            demux.feed(&chunk, |au, codec| {
                if abort_to_ffmpeg.is_some() {
                    return;
                }
                if !got_frame && codec == Some(VideoCodec::Hevc) {
                    abort_to_ffmpeg = Some("HEVC/H.265 stream".into());
                    return;
                }
                if !got_frame {
                    init_aus += 1;
                }
                match decoder.decode_au(au) {
                    Ok(Some(frame)) => {
                        got_frame = true;
                        let _ = worker_out_tx.send(WorkerSignal::Frame(frame));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        if !got_frame {
                            last_err = e;
                        }
                    }
                }
            });
            if !got_frame && demux.codec() == Some(VideoCodec::Hevc) && abort_to_ffmpeg.is_none() {
                abort_to_ffmpeg = Some("HEVC/H.265 stream".into());
            }
            if !got_frame && abort_to_ffmpeg.is_none() && (init_bytes >= 256 * 1024 || init_aus >= 6) {
                abort_to_ffmpeg = Some(if last_err.is_empty() {
                    "in-process H.264 decoder produced no initial frame".into()
                } else {
                    format!("OpenH264 could not decode stream ({last_err})")
                });
            }
            if let Some(reason) = abort_to_ffmpeg {
                let _ = worker_out_tx.send(WorkerSignal::Fallback(reason));
                return;
            }
        }
    });

    enum Mode {
        InProcess {
            tx: tokio::sync::mpsc::Sender<Vec<u8>>,
            init_buf: Vec<u8>,
            got_first: bool,
        },
        Ffmpeg {
            stdin: tokio::process::ChildStdin,
            frames_rx: tokio::sync::mpsc::Receiver<Result<slint::SharedPixelBuffer<slint::Rgba8Pixel>, Stopped>>,
        },
    }

    const MAX_INIT_BUF: usize = 512 * 1024;
    let mut mode = Mode::InProcess {
        tx: worker_in_tx,
        init_buf: Vec::with_capacity(64 * 1024),
        got_first: false,
    };

    let relay_fut = relay::stream_video_chunks(relay, &chunk_tx, audio_id, Some(&recorder));
    tokio::pin!(relay_fut);

    let deadline = tokio::time::sleep(FRAME_TIMEOUT);
    tokio::pin!(deadline);

    let mut relay_done: Option<Result<()>> = None;

    loop {
        match &mut mode {
            Mode::InProcess { tx, init_buf, got_first } => {
                tokio::select! {
                    res = &mut relay_fut, if relay_done.is_none() => {
                        relay_done = Some(res);
                        return relay_done.take().unwrap();
                    }
                    maybe_chunk = chunk_rx.recv() => {
                        let Some(chunk) = maybe_chunk else {
                            return relay_done.take().unwrap_or(Ok(()));
                        };
                        if !*got_first && init_buf.len() + chunk.len() <= MAX_INIT_BUF {
                            init_buf.extend_from_slice(&chunk);
                        }
                        let _ = tx.send(chunk).await;
                    }
                    maybe_sig = worker_out_rx.recv() => {
                        match maybe_sig {
                            Some(WorkerSignal::Frame(frame)) => {
                                if !*got_first {
                                    *got_first = true;
                                    *init_buf = Vec::new();
                                    log::info!(
                                        "live: {name}: using in-process H.264 decoder ({}×{})",
                                        frame.width(),
                                        frame.height()
                                    );
                                }
                                sink.push(frame);
                                deadline.as_mut().reset(tokio::time::Instant::now() + FRAME_TIMEOUT);
                            }
                            Some(WorkerSignal::Fallback(reason)) => {
                                log::info!("live: {name}: switching to ffmpeg ({reason})");
                                let ffmpeg_path = ffmpeg.context(
                                    "ffmpeg not found: needed to decode this stream. Put ffmpeg.exe next to Tapas.exe or set its path in Settings",
                                )?;
                                let mut dec = spawn_live_decoder(ffmpeg_path, w, h)?;
                                let mut stdin = dec.stdin.take().context("ffmpeg stdin")?;
                                if !init_buf.is_empty() {
                                    let replay = std::mem::take(init_buf);
                                    stdin.write_all(&replay).await.context("Video decoder stopped")?;
                                }
                                let (ftx, frx) = tokio::sync::mpsc::channel(4);
                                tokio::spawn(async move {
                                    let (fw, fh) = (dec.width as u32, dec.height as u32);
                                    loop {
                                        let mut frame = SharedPixelBuffer::<slint::Rgba8Pixel>::new(fw, fh);
                                        match timeout(FRAME_TIMEOUT, dec.stdout.read_exact(frame.make_mut_bytes())).await {
                                            Err(_) => {
                                                let _ = ftx.send(Err(Stopped::NoVideo)).await;
                                                return;
                                            }
                                            Ok(Err(_)) => {
                                                let _ = timeout(Duration::from_secs(2), dec.child.wait()).await;
                                                tokio::time::sleep(Duration::from_millis(100)).await;
                                                let line = dec.last_error.lock().unwrap().clone();
                                                let _ = ftx.send(Err(Stopped::Exited(line))).await;
                                                return;
                                            }
                                            Ok(Ok(_)) => {
                                                if ftx.send(Ok(frame)).await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                });
                                mode = Mode::Ffmpeg { stdin, frames_rx: frx };
                                deadline.as_mut().reset(tokio::time::Instant::now() + FRAME_TIMEOUT);
                            }
                            None => bail!("in-process video decoder stopped"),
                        }
                    }
                    () = &mut deadline => {
                        bail!("No video for {}s", FRAME_TIMEOUT.as_secs());
                    }
                }
            }
            Mode::Ffmpeg { stdin, frames_rx } => {
                tokio::select! {
                    res = &mut relay_fut, if relay_done.is_none() => {
                        return res;
                    }
                    maybe_chunk = chunk_rx.recv() => {
                        let Some(chunk) = maybe_chunk else {
                            return Ok(());
                        };
                        stdin.write_all(&chunk).await.context("Video decoder stopped")?;
                    }
                    maybe_frame = frames_rx.recv() => {
                        match maybe_frame {
                            Some(Ok(frame)) => {
                                sink.push(frame);
                                deadline.as_mut().reset(tokio::time::Instant::now() + FRAME_TIMEOUT);
                            }
                            Some(Err(Stopped::NoVideo)) => bail!("No video for {}s", FRAME_TIMEOUT.as_secs()),
                            Some(Err(Stopped::Exited(line))) if line.is_empty() => bail!("ffmpeg stopped"),
                            Some(Err(Stopped::Exited(line))) => bail!("ffmpeg: {line}"),
                            None => bail!("ffmpeg stopped"),
                        }
                    }
                    () = &mut deadline => {
                        bail!("No video for {}s", FRAME_TIMEOUT.as_secs());
                    }
                }
            }
        }
    }
}

fn live_input_args(w: usize) -> [&'static str; 12] {
    // Bound probing so low-bitrate SD streams (~35 KB/s) emit frame 1 quickly
    // instead of buffering ~1 s of probe data. No `-fflags nobuffer`: it drops
    // probed packets and forces waiting for the next keyframe.
    let probesize = if w <= 640 { "12288" } else { "32768" };
    [
        "-flags",
        "low_delay",
        "-fflags",
        "+discardcorrupt",
        "-analyzeduration",
        "0",
        "-fpsprobesize",
        "0",
        "-probesize",
        probesize,
        "-f",
        "mpegts",
    ]
}

fn spawn_live_decoder(ffmpeg: &Path, w: usize, h: usize) -> Result<Decoder> {
    let mut args = Vec::with_capacity(14);
    args.extend_from_slice(&live_input_args(w));
    args.extend_from_slice(&["-i", "pipe:0"]);
    Decoder::spawn(ffmpeg, &args, w, h)
}

/// Decodes the relay's MPEG-TS (after its HTTP head) until either the relay
/// or ffmpeg stops.
#[cfg(test)]
async fn decode_relay<R>(relay: &mut R, ffmpeg: &Path, w: usize, h: usize, sink: &mut FrameSink) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let decoder = spawn_live_decoder(ffmpeg, w, h)?;
    decode_relay_with_decoder(relay, decoder, sink).await
}

#[cfg(test)]
async fn decode_relay_with_decoder<R>(relay: &mut R, mut decoder: Decoder, sink: &mut FrameSink) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut stdin = decoder.stdin.take().context("ffmpeg stdin")?;
    let audio_id = sink.audio_id;
    let recorder = sink.recorder.clone();
    tokio::select! {
        result = relay::copy_video_with_audio_and_recorder(relay, &mut stdin, audio_id, Some(&recorder)) => result,
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
    #[cfg(test)]
    async fn push_frames(&mut self, sink: &mut FrameSink) -> Stopped {
        self.push_frames_within(sink, FRAME_TIMEOUT).await
    }

    /// Like [`Decoder::push_frames`], waiting up to `quiet` for each frame.
    #[cfg(test)]
    pub async fn push_frames_within(&mut self, sink: &mut FrameSink, quiet: Duration) -> Stopped {
        let (w, h) = (self.width as u32, self.height as u32);
        loop {
            // Read directly into the SharedPixelBuffer to avoid a redundant
            // 8.3 MB/frame copy on HD streams.
            let mut frame = SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
            match timeout(quiet, self.stdout.read_exact(frame.make_mut_bytes())).await {
                Err(_) => return Stopped::NoVideo,
                Ok(Err(_)) => {
                    let _ = timeout(Duration::from_secs(2), self.child.wait()).await;
                    tokio::time::sleep(Duration::from_millis(100)).await; // let stderr drain
                    return Stopped::Exited(self.last_error.lock().unwrap().clone());
                }
                Ok(Ok(_)) => sink.push(frame),
            }
        }
    }
}

/// Decodes a single encoded picture (an event's keyframe from a hub,
/// `image/avc` or `image/hevc`) into a `width`×`height` frame.
pub async fn decode_picture(ffmpeg: &Path, data: &[u8], content_type: &str, width: usize, height: usize) -> Result<SharedPixelBuffer<slint::Rgba8Pixel>> {
    use tokio::io::AsyncWriteExt;
    let format = if content_type.contains("hevc") || content_type.contains("265") { "hevc" } else { "h264" };
    let mut decoder = Decoder::spawn(ffmpeg, &["-f", format, "-i", "pipe:0"], width, height)?;
    let mut stdin = decoder.stdin.take().context("ffmpeg stdin")?;
    stdin.write_all(data).await.context("Could not pass the picture to ffmpeg")?;
    drop(stdin);
    let mut frame = SharedPixelBuffer::<slint::Rgba8Pixel>::new(width as u32, height as u32);
    match timeout(Duration::from_secs(10), decoder.stdout.read_exact(frame.make_mut_bytes())).await {
        Ok(Ok(_)) => Ok(frame),
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
    // 2K C520WS streams into 2560x1440 are a 1:1 no-op; use bilinear if scaling HD.
    let scale_flags = if w >= 1280 { "bilinear" } else { "fast_bilinear" };
    let mut vf = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:flags={scale_flags},pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"
    );
    if timed {
        vf = format!("showinfo=checksum=0,{vf}");
    }
    // Keep RAM bounded (avoid per-core thread explosion), while allowing
    // 2 decode threads on HD streams for snappier 1080p/2K decoding.
    let decode_threads = if w >= 1280 { "2" } else { "1" };
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostats", "-loglevel", if timed { "info" } else { "error" }])
        .args(["-filter_threads", "1", "-threads", decode_threads]);
    // Use D3D11VA/DXVA2 GPU decode on Windows HD streams (Playback and HEVC
    // live fallback); `-hwaccel auto` transparently falls back to CPU if unavailable.
    if cfg!(windows) && w >= 1280 {
        cmd.args(["-hwaccel", "auto"]);
    }
    cmd.args(input)
        // rawvideo has no timestamps, so ffmpeg would otherwise pad to a
        // guessed constant rate, duplicating frames (e.g. 15 fps → 30).
        .args(["-an", "-sn", "-vf", &vf, "-fps_mode", "passthrough"])
        // Output RGBA directly so Slint's femtovg-wgpu renderer uploads the
        // buffer as-is instead of allocating and converting RGB->RGBA per frame.
        .args(["-pix_fmt", "rgba", "-f", "rawvideo", "pipe:1"])
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

/// Remuxes a Tapo MPEG-TS recording at `ts_path` into a standard `.mp4` at
/// `mp4_path` with lossless video copy (`-c:v copy`) and `+faststart`.
///
/// - Standard MPEG-TS AAC (`0x0f`) is copied losslessly (`aac_adtstoasc`).
/// - Tapo's proprietary G.711 A-law/u-law tracks (`0x90`/`0x91`, which FFmpeg's
///   MPEG-TS demuxer does not recognize as audio) are decoded to PCM by
///   [`crate::audio::TsAudioDemuxer`] and encoded to AAC (`64 kbps`).
/// - If audio remuxing fails for any reason, falls back to video-only MP4 so
///   the recording is still saved.
pub async fn remux_ts_to_mp4(ffmpeg: &Path, ts_path: &Path, mp4_path: &Path) -> Result<()> {
    let ts_owned = ts_path.to_path_buf();
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let pcm_tmp = mp4_path.with_extension(format!("pcm.{tag}.part"));
    let pcm_for_task = pcm_tmp.clone();

    let (hevc, audio_mode) = tokio::task::spawn_blocking(move || prepare_ts_audio(&ts_owned, &pcm_for_task))
        .await
        .context("Audio inspection task failed")??;

    let mp4_tmp = mp4_path.with_extension(format!("mp4.{tag}.part"));
    let run_result = async {
        match audio_mode {
            RemuxAudio::Aac => {
                if let Err(e) = run_remux(ffmpeg, ts_path, None, &mp4_tmp, hevc, RemuxAudio::Aac).await {
                    log::warn!("mp4: AAC copy failed ({e:#}); retrying video-only");
                    let _ = std::fs::remove_file(&mp4_tmp);
                    run_remux(ffmpeg, ts_path, None, &mp4_tmp, hevc, RemuxAudio::None).await?;
                }
            }
            RemuxAudio::Pcm(rate) => {
                if let Err(e) = run_remux(ffmpeg, ts_path, Some(&pcm_tmp), &mp4_tmp, hevc, RemuxAudio::Pcm(rate)).await {
                    log::warn!("mp4: G.711->AAC remux failed ({e:#}); retrying video-only");
                    let _ = std::fs::remove_file(&mp4_tmp);
                    run_remux(ffmpeg, ts_path, None, &mp4_tmp, hevc, RemuxAudio::None).await?;
                }
            }
            RemuxAudio::None => {
                run_remux(ffmpeg, ts_path, None, &mp4_tmp, hevc, RemuxAudio::None).await?;
            }
        }
        if std::fs::metadata(&mp4_tmp).map(|m| m.len()).unwrap_or(0) == 0 {
            bail!("ffmpeg produced an empty MP4 file");
        }
        std::fs::rename(&mp4_tmp, mp4_path)
            .with_context(|| format!("Could not save {}", mp4_path.display()))?;
        Ok(())
    }
    .await;

    let _ = std::fs::remove_file(&pcm_tmp);
    if run_result.is_err() {
        let _ = std::fs::remove_file(&mp4_tmp);
    }
    run_result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemuxAudio {
    None,
    Aac,
    Pcm(u32),
}

fn prepare_ts_audio(ts_path: &Path, pcm_path: &Path) -> Result<(bool, RemuxAudio)> {
    use std::io::{Read, Write};
    use crate::audio::{AudioCodec, TsAudioDemuxer};

    let mut file = std::fs::File::open(ts_path).with_context(|| format!("Could not open {}", ts_path.display()))?;
    let mut probe = vec![0u8; 128 * 1024];
    let n = file.read(&mut probe)?;
    probe.truncate(n);

    let mut demux = TsAudioDemuxer::new();
    let mut chunks = Vec::new();
    demux.feed(&probe, &mut chunks);
    let hevc = demux.video_is_hevc();

    match demux.codec() {
        Some(AudioCodec::Aac) => Ok((hevc, RemuxAudio::Aac)),
        Some(AudioCodec::Pcma | AudioCodec::Pcmu) => {
            let mut pcm_out = std::io::BufWriter::new(
                std::fs::File::create(pcm_path).with_context(|| format!("Could not create {}", pcm_path.display()))?,
            );
            let mut sample_rate = 8_000u32;
            let mut wrote_samples = 0usize;
            let write_chunks = |chunks: &mut Vec<crate::audio::PcmChunk>,
                                out: &mut std::io::BufWriter<std::fs::File>,
                                rate: &mut u32,
                                wrote: &mut usize|
             -> Result<()> {
                for c in chunks.drain(..) {
                    *rate = c.sample_rate;
                    *wrote += c.samples.len();
                    for s in c.samples {
                        out.write_all(&s.to_le_bytes())?;
                    }
                }
                Ok(())
            };
            write_chunks(&mut chunks, &mut pcm_out, &mut sample_rate, &mut wrote_samples)?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let got = file.read(&mut buf)?;
                if got == 0 {
                    break;
                }
                demux.feed(&buf[..got], &mut chunks);
                write_chunks(&mut chunks, &mut pcm_out, &mut sample_rate, &mut wrote_samples)?;
            }
            demux.finish(&mut chunks);
            write_chunks(&mut chunks, &mut pcm_out, &mut sample_rate, &mut wrote_samples)?;
            pcm_out.flush()?;
            if wrote_samples == 0 {
                let _ = std::fs::remove_file(pcm_path);
                Ok((hevc, RemuxAudio::None))
            } else {
                Ok((hevc, RemuxAudio::Pcm(sample_rate)))
            }
        }
        None => Ok((hevc, RemuxAudio::None)),
    }
}

async fn run_remux(
    ffmpeg: &Path,
    ts_path: &Path,
    pcm_path: Option<&Path>,
    mp4_tmp: &Path,
    hevc: bool,
    audio: RemuxAudio,
) -> Result<()> {
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostats", "-loglevel", "error", "-y", "-fflags", "+genpts", "-i"])
        .arg(ts_path);
    if let (RemuxAudio::Pcm(rate), Some(pcm)) = (audio, pcm_path) {
        cmd.args(["-f", "s16le", "-ar", &rate.to_string(), "-ac", "1", "-i"]).arg(pcm);
    }
    cmd.args(["-map", "0:v:0"]);
    match audio {
        RemuxAudio::Aac => {
            cmd.args(["-map", "0:a:0"]);
        }
        RemuxAudio::Pcm(_) => {
            cmd.args(["-map", "1:a:0"]);
        }
        RemuxAudio::None => {}
    }
    cmd.args(["-c:v", "copy"]);
    if hevc {
        cmd.args(["-tag:v", "hvc1"]);
    }
    match audio {
        RemuxAudio::Aac => {
            cmd.args(["-c:a", "copy", "-bsf:a", "aac_adtstoasc"]);
        }
        RemuxAudio::Pcm(_) => {
            cmd.args(["-c:a", "aac", "-b:a", "64k", "-shortest"]);
        }
        RemuxAudio::None => {
            cmd.arg("-an");
        }
    }
    cmd.args(["-avoid_negative_ts", "make_zero", "-movflags", "+faststart", "-f", "mp4"])
        .arg(mp4_tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().await.context("Could not start ffmpeg for MP4 export")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            bail!("ffmpeg exited with {}", output.status);
        } else {
            bail!("ffmpeg failed: {detail}");
        }
    }
    Ok(())
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
        let distinct: std::collections::HashSet<_> = bytes.chunks(4).step_by(97).collect();
        assert!(distinct.len() > 20, "frame looks blank");
        drop(relay_tx);
    }

    #[tokio::test]
    async fn remuxes_ts_to_mp4() {
        let Some(ffmpeg) = find("") else {
            eprintln!("skipped: no ffmpeg (build one with tools/build-ffmpeg.sh native)");
            return;
        };
        let dir = std::env::temp_dir().join(format!("tapas-mp4-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let ts_path = dir.join("sample.ts");
        let mp4_path = dir.join("sample.mp4");
        std::fs::write(&ts_path, include_bytes!("../tests/fixtures/relay-sample.ts")).unwrap();

        remux_ts_to_mp4(&ffmpeg, &ts_path, &mp4_path).await.unwrap();
        let bytes = std::fs::read(&mp4_path).unwrap();
        assert!(bytes.len() > 10_000, "mp4 unexpectedly small: {} bytes", bytes.len());
        assert_eq!(&bytes[4..8], b"ftyp", "missing ISO BMFF ftyp box");
        let moov = bytes.windows(4).position(|w| w == b"moov").expect("missing moov box");
        let mdat = bytes.windows(4).position(|w| w == b"mdat").expect("missing mdat box");
        assert!(moov < mdat, "expected +faststart (moov before mdat)");
        assert!(bytes.windows(4).any(|w| w == b"mp4a"), "expected preserved AAC audio track");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn remuxes_g711_pcm_to_aac_mp4() {
        let Some(ffmpeg) = find("") else {
            eprintln!("skipped: no ffmpeg (build one with tools/build-ffmpeg.sh native)");
            return;
        };
        let dir = std::env::temp_dir().join(format!("tapas-mp4-pcm-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let ts_path = dir.join("sample.ts");
        let pcm_path = dir.join("audio.pcm");
        let mp4_path = dir.join("sample_g711.mp4");
        std::fs::write(&ts_path, include_bytes!("../tests/fixtures/relay-sample.ts")).unwrap();
        // 2 seconds of 8 kHz mono 16-bit PCM sine-ish tone (tests pcm_s16le -> aac + swresample)
        let mut pcm = Vec::with_capacity(8_000 * 2 * 2);
        for i in 0..(8_000 * 2) {
            let s: i16 = if (i / 10) % 2 == 0 { 4000 } else { -4000 };
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&pcm_path, &pcm).unwrap();

        run_remux(&ffmpeg, &ts_path, Some(&pcm_path), &mp4_path, false, RemuxAudio::Pcm(8_000))
            .await
            .unwrap();
        let bytes = std::fs::read(&mp4_path).unwrap();
        assert_eq!(&bytes[4..8], b"ftyp", "missing ISO BMFF ftyp box");
        let moov = bytes.windows(4).position(|w| w == b"moov").expect("missing moov box");
        let mdat = bytes.windows(4).position(|w| w == b"mdat").expect("missing mdat box");
        assert!(moov < mdat, "expected +faststart (moov before mdat)");
        assert!(bytes.windows(4).any(|w| w == b"mp4a"), "expected encoded AAC audio track");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_input_args_uses_smaller_probe_for_sd() {
        let sd = live_input_args(640);
        let hd = live_input_args(1280);
        assert!(sd.windows(2).any(|w| w == ["-probesize", "12288"]));
        assert!(hd.windows(2).any(|w| w == ["-probesize", "32768"]));
        assert!(sd.windows(2).any(|w| w == ["-fpsprobesize", "0"]));
        assert!(sd.windows(2).any(|w| w == ["-fflags", "+discardcorrupt"]));
    }

    #[tokio::test]
    async fn decodes_relay_video_in_process_without_ffmpeg() {
        let ts = include_bytes!("../tests/fixtures/relay-sample.ts");
        let mut body = Vec::new();
        for chunk in ts.chunks(1316 * 3 + 7) {
            body.extend(
                format!("----device-stream-boundary--\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\n\r\n", chunk.len())
                    .as_bytes(),
            );
            body.extend(chunk);
            body.extend(b"\r\n");
        }
        let (mut relay_tx, relay_rx) = tokio::io::duplex(1 << 20);
        relay_tx.write_all(&body).await.unwrap();
        let mut relay = BufReader::new(relay_rx);

        let slot = Arc::new(Mutex::new(Slot { frame: None, version: 0, status: Status::Connecting, fps: 0.0 }));
        let mut sink = FrameSink::new(slot.clone());
        let enough_frames = async {
            while slot.lock().unwrap().version < 25 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            _ = enough_frames => {}
            result = decode_relay_hybrid(&mut relay, None, "test (Sd)", 640, 360, &mut sink) => {
                panic!("in-process decoding stopped unexpectedly: {result:?}")
            }
            _ = tokio::time::sleep(Duration::from_secs(10)) => {
                panic!("only {} frames decoded in-process", slot.lock().unwrap().version)
            }
        }

        let frame = slot.lock().unwrap().frame.clone().unwrap();
        assert_eq!((frame.width(), frame.height()), (320, 180));
        let distinct: std::collections::HashSet<_> = frame.as_bytes().chunks(4).step_by(37).collect();
        assert!(distinct.len() > 20, "in-process frame looks blank");
        drop(relay_tx);
    }
}
