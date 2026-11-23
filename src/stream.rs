//! A live camera stream running on the tokio runtime.
//!
//! Each stream reconnects forever with back-off and publishes its latest frame
//! into a shared [`Slot`], which the UI thread polls. Dropping the
//! [`StreamHandle`] stops the stream (and kills its ffmpeg process, if any).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::cloud::{CloudCamera, CloudClient, CloudError};
use crate::config::Quality;
use crate::ffmpeg;

pub type Frame = SharedPixelBuffer<Rgba8Pixel>;

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
    /// Audio source id for [`crate::audio`]; mute/unmute switches the active id.
    pub audio_id: u64,
    recorder: crate::relay::LiveRecorder,
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
        let audio_id = crate::audio::next_source_id();
        let recorder: crate::relay::LiveRecorder = Arc::new(Mutex::new(None));
        let task = rt.spawn(run(spec, FrameSink::with_audio_and_recorder(slot.clone(), audio_id, recorder.clone())));
        Self { slot, audio_id, recorder, task }
    }

    pub fn latest_frame(&self) -> Option<Frame> {
        self.slot.lock().unwrap().frame.clone()
    }

    /// Starts teeing the live stream's raw MPEG-TS packets into `path`.
    pub fn start_recording(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::File::create(path)?;
        *self.recorder.lock().unwrap() = Some(std::io::BufWriter::new(file));
        Ok(())
    }

    /// Stops teeing MPEG-TS and flushes/closes the file. Returns whether recording was active.
    pub fn stop_recording(&self) -> bool {
        use std::io::Write;
        if let Some(mut writer) = self.recorder.lock().unwrap().take() {
            let _ = writer.flush();
            true
        } else {
            false
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.stop_recording();
        crate::audio::release(self.audio_id);
        self.task.abort();
    }
}

/// Where stream readers deliver decoded frames.
pub struct FrameSink {
    slot: Arc<Mutex<Slot>>,
    pub audio_id: u64,
    pub recorder: crate::relay::LiveRecorder,
    pub frames: u64,
    window_start: Instant,
    window_frames: u32,
    /// For logging how long the first picture took: the stream's name, when
    /// it was asked for and how long the relay request took.
    first_frame: Option<(String, Instant, Duration)>,
}

impl FrameSink {
    #[cfg(test)]
    pub(crate) fn new(slot: Arc<Mutex<Slot>>) -> Self {
        Self::with_audio(slot, 0)
    }

    pub(crate) fn with_audio(slot: Arc<Mutex<Slot>>, audio_id: u64) -> Self {
        Self::with_audio_and_recorder(slot, audio_id, Arc::new(Mutex::new(None)))
    }

    pub(crate) fn with_audio_and_recorder(
        slot: Arc<Mutex<Slot>>,
        audio_id: u64,
        recorder: crate::relay::LiveRecorder,
    ) -> Self {
        Self { slot, audio_id, recorder, frames: 0, window_start: Instant::now(), window_frames: 0, first_frame: None }
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
