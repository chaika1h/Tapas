//! A live camera stream running on the tokio runtime.
//!
//! Each stream reconnects forever with back-off and publishes its latest frame
//! into a shared [`Slot`], which the UI thread polls. Dropping the
//! [`StreamHandle`] stops the stream (and kills its ffmpeg process, if any).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{Rgb8Pixel, SharedPixelBuffer};

use crate::config::Quality;
use crate::{ffmpeg, mjpeg};

pub type Frame = SharedPixelBuffer<Rgb8Pixel>;

#[derive(Clone)]
pub struct StreamSpec {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub quality: Quality,
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Clone, PartialEq)]
pub enum Status {
    Connecting,
    Live,
    Retrying(String),
}

pub struct Slot {
    pub frame: Option<Frame>,
    /// Incremented on every new frame.
    pub version: u64,
    pub status: Status,
    pub fps: f32,
}

impl Slot {
    pub fn is_live(&self) -> bool {
        self.status == Status::Live
    }

    pub fn status_text(&self) -> String {
        match &self.status {
            Status::Connecting => "Connecting…".into(),
            Status::Retrying(msg) => msg.clone(),
            Status::Live => match &self.frame {
                Some(f) => format!("{}×{} · {:.0} fps", f.width(), f.height(), self.fps),
                None => "Live".into(),
            },
        }
    }
}

pub struct StreamHandle {
    pub slot: Arc<Mutex<Slot>>,
    task: tokio::task::JoinHandle<()>,
}

impl StreamHandle {
    pub fn spawn(rt: &tokio::runtime::Handle, spec: StreamSpec) -> Self {
        let slot = Arc::new(Mutex::new(Slot {
            frame: None,
            version: 0,
            status: Status::Connecting,
            fps: 0.0,
        }));
        let task = rt.spawn(run(spec, FrameSink::new(slot.clone())));
        Self { slot, task }
    }

    pub fn latest_frame(&self) -> Option<Frame> {
        self.slot.lock().unwrap().frame.clone()
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Where stream readers deliver decoded frames.
pub struct FrameSink {
    slot: Arc<Mutex<Slot>>,
    pub frames: u64,
    window_start: Instant,
    window_frames: u32,
}

impl FrameSink {
    fn new(slot: Arc<Mutex<Slot>>) -> Self {
        Self { slot, frames: 0, window_start: Instant::now(), window_frames: 0 }
    }

    pub fn push(&mut self, frame: Frame) {
        self.frames += 1;
        self.window_frames += 1;
        let now = Instant::now();
        let elapsed = now - self.window_start;
        let mut slot = self.slot.lock().unwrap();
        if elapsed >= Duration::from_secs(1) {
            slot.fps = self.window_frames as f32 / elapsed.as_secs_f32();
            self.window_start = now;
            self.window_frames = 0;
        }
        slot.frame = Some(frame);
        slot.version += 1;
        slot.status = Status::Live;
    }

    fn set_status(&self, status: Status) {
        self.slot.lock().unwrap().status = status;
    }
}

async fn run(spec: StreamSpec, mut sink: FrameSink) {
    const MIN_BACKOFF: Duration = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    let mut backoff = MIN_BACKOFF;

    loop {
        sink.set_status(Status::Connecting);
        let frames_before = sink.frames;
        let result = match spec.quality {
            Quality::Mjpeg => mjpeg::run(&spec, &mut sink).await,
            Quality::Sd | Quality::Hd => ffmpeg::run(&spec, &mut sink).await,
        };
        let reason = match result {
            Ok(()) => "Stream ended".to_string(),
            Err(e) => format!("{e:#}"),
        };
        if sink.frames > frames_before {
            backoff = MIN_BACKOFF; // it was working; reconnect quickly
        }
        log::warn!("{} ({:?}): {reason}; retrying in {backoff:?}", spec.host, spec.quality);
        sink.set_status(Status::Retrying(format!("{reason} — retrying in {}s", backoff.as_secs())));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Builds an `rtsp://` URL, optionally with URL-encoded credentials.
pub fn rtsp_url(spec: &StreamSpec, path: &str, with_creds: bool) -> anyhow::Result<url::Url> {
    let mut url = url::Url::parse(&format!("rtsp://{}:{}/{path}", spec.host.trim(), spec.port))
        .map_err(|e| anyhow::anyhow!("invalid camera address {:?}: {e}", spec.host))?;
    if with_creds {
        let _ = url.set_username(&spec.username);
        let _ = url.set_password(Some(&spec.password));
    }
    Ok(url)
}

/// Turns low-level RTSP/ffmpeg errors into something a user can act on.
pub fn friendly_error(msg: &str, port: u16) -> String {
    let lower = msg.to_ascii_lowercase();
    if msg.contains("401") || lower.contains("unauthorized") {
        "Camera account username/password rejected".into()
    } else if lower.contains("connection refused") || msg.contains("10061") {
        format!("Camera refused the connection on port {port}")
    } else if lower.contains("no route to host")
        || lower.contains("timed out")
        || lower.contains("unreachable")
        || msg.contains("10060")
        || msg.contains("10065")
    {
        "Camera did not answer: check the IP address and that the PC can reach it".into()
    } else if msg.contains("404") || lower.contains("not found") {
        "Camera has no such stream".into()
    } else {
        msg.to_string()
    }
}
