//! A live camera stream running on the tokio runtime.
//!
//! Each stream reconnects forever with back-off and publishes its latest frame
//! into a shared [`Slot`], which the UI thread polls. Dropping the
//! [`StreamHandle`] stops the stream (and kills its ffmpeg process, if any).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{Rgb8Pixel, SharedPixelBuffer};

use crate::cloud::{CloudCamera, CloudClient, CloudError};
use crate::config::Quality;
use crate::ffmpeg;

pub type Frame = SharedPixelBuffer<Rgb8Pixel>;

/// One camera's live video through the Tapo cloud relay.
#[derive(Clone)]
pub struct StreamSpec {
    pub quality: Quality,
    pub ffmpeg: Option<PathBuf>,
    pub client: Arc<CloudClient>,
    pub camera: CloudCamera,
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
    /// For logging how long the first picture took: the stream's name, when
    /// it was asked for and how long the relay request took.
    first_frame: Option<(String, Instant, Duration)>,
}

impl FrameSink {
    pub(crate) fn new(slot: Arc<Mutex<Slot>>) -> Self {
        Self { slot, frames: 0, window_start: Instant::now(), window_frames: 0, first_frame: None }
    }

    /// Logs how long the next frame takes to come, from `started`.
    pub fn time_first_frame(&mut self, name: String, started: Instant, asked: Duration) {
        log::info!("live: {name}: relay ready after {} ms", started.elapsed().as_millis());
        self.first_frame = Some((name, started, asked));
    }

    pub fn push(&mut self, frame: Frame) {
        if let Some((name, started, asked)) = self.first_frame.take() {
            log::info!(
                "live: {name}: first picture after {} ms (the cloud answered in {} ms)",
                started.elapsed().as_millis(),
                asked.as_millis()
            );
        }
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
        let reason = match ffmpeg::run(&spec, &mut sink).await {
            Ok(()) => "Stream ended".to_string(),
            Err(e) => {
                if e.downcast_ref::<CloudError>().is_some_and(CloudError::is_auth) {
                    crate::app::on_ui(|app| app.cloud_auth_failed());
                }
                format!("{e:#}")
            }
        };
        if sink.frames > frames_before {
            backoff = MIN_BACKOFF; // it was working; reconnect quickly
        }
        log::warn!("{} ({:?}): {reason}; retrying in {backoff:?}", spec.camera.name, spec.quality);
        sink.set_status(Status::Retrying(format!("{reason} — retrying in {}s", backoff.as_secs())));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}
