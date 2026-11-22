//! Plays a hub recording inside the app: either one saved earlier, or one
//! coming through the cloud, which is saved to `Videos\TapoViewer` as it
//! arrives. ffmpeg's `-re` paces the picture to real time, so a clip that
//! downloads faster than it plays is buffered in memory meanwhile.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::clip::{self, ClipRequest, Progress};
use crate::cloud::{CloudClient, CloudError};
use crate::ffmpeg::{Decoder, Stopped};
use crate::stream::{FrameSink, Slot, Status};

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
/// A download can pause for a while (the hub's own time-outs are longer).
const QUIET: Duration = Duration::from_secs(45);

pub enum Source {
    /// A recording saved earlier.
    File(PathBuf),
    /// Fetched through the cloud and saved to `path` while it plays.
    Cloud { client: Arc<CloudClient>, request: Box<ClipRequest>, path: PathBuf },
}

#[derive(Debug, Clone, Default)]
pub struct PlayerState {
    pub message: String,
    pub downloading: bool,
    /// Seconds of video downloaded so far.
    pub downloaded: f64,
    /// The saved file, once it's complete.
    pub saved: Option<PathBuf>,
    /// Why the download failed, if it did.
    pub download_error: Option<String>,
    /// The picture has stopped: the end of the clip, or an error.
    pub ended: bool,
}

pub struct Player {
    pub slot: Arc<Mutex<Slot>>,
    pub state: Arc<Mutex<PlayerState>>,
    task: tokio::task::JoinHandle<()>,
    rt: tokio::runtime::Handle,
    /// The download's `.part` file, removed if the player is stopped early.
    partial: Option<PathBuf>,
}

impl Player {
    /// `notify` runs (on any thread) whenever `state` changes.
    pub fn start(
        rt: &tokio::runtime::Handle,
        ffmpeg: PathBuf,
        source: Source,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        let slot = Arc::new(Mutex::new(Slot { frame: None, version: 0, status: Status::Connecting, fps: 0.0 }));
        let state = Arc::new(Mutex::new(PlayerState::default()));
        let partial = match &source {
            Source::Cloud { path, .. } => Some(path.with_extension("ts.part")),
            Source::File(_) => None,
        };
        let task = rt.spawn(run(ffmpeg, source, FrameSink::new(slot.clone()), state.clone(), Arc::new(notify)));
        Self { slot, state, task, rt: rt.clone(), partial }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(partial) = self.partial.take() {
            let task = std::mem::replace(&mut self.task, self.rt.spawn(async {}));
            self.rt.spawn(async move {
                let _ = task.await; // the file is closed once the task has stopped
                let _ = tokio::fs::remove_file(partial).await;
            });
        }
    }
}

type Notify = Arc<dyn Fn() + Send + Sync>;

async fn run(ffmpeg: PathBuf, source: Source, mut sink: FrameSink, state: Arc<Mutex<PlayerState>>, notify: Notify) {
    let update = |f: &dyn Fn(&mut PlayerState)| {
        f(&mut state.lock().unwrap());
        notify();
    };
    match source {
        Source::File(path) => {
            update(&|s| s.message = "Playing".into());
            // Through a pipe like a download: the built-in ffmpeg reads no files.
            let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let reading = async {
                let mut file = tokio::fs::File::open(&path).await?;
                let mut buf = vec![0u8; 256 * 1024];
                loop {
                    let n = file.read(&mut buf).await?;
                    if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                drop(tx);
                anyhow::Ok(())
            };
            let (read, played) = tokio::join!(reading, play(&ffmpeg, rx, &mut sink));
            let result = read.and(played);
            update(&|s| {
                s.ended = true;
                s.saved = Some(path.clone());
                s.message = match &result {
                    Ok(()) => "Finished".into(),
                    Err(e) => format!("Can't play this recording: {e:#}"),
                };
            });
        }
        Source::Cloud { client, request, path } => {
            update(&|s| {
                s.downloading = true;
                s.message = "Asking the hub for the recording…".into();
            });
            let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let download = async {
                let saved = save(&client, &request, &path, tx, &state, &notify).await;
                if let Err(e) = &saved {
                    if e.downcast_ref::<CloudError>().is_some_and(CloudError::is_auth) {
                        crate::app::on_ui(|app| app.cloud_auth_failed());
                    }
                    log::warn!("recording download failed: {e:#}");
                }
                // Usually long before the picture gets to the end.
                update(&|s| {
                    s.downloading = false;
                    match &saved {
                        Ok(()) => {
                            s.saved = Some(path.clone());
                            s.message = "Playing · saved to Videos".into();
                        }
                        Err(e) => {
                            s.download_error = Some(format!("{e:#}"));
                            s.message = format!("{e:#}");
                        }
                    }
                });
                saved
            };
            let playback = play(&ffmpeg, rx, &mut sink);
            let (saved, played) = tokio::join!(download, playback);
            update(&|s| {
                s.ended = true;
                s.message = match (&saved, &played) {
                    (Ok(()), Ok(())) => "Finished · saved to Videos".into(),
                    (Ok(()), Err(e)) => format!("Saved to Videos, but it can't be played here: {e:#}"),
                    (Err(e), _) => format!("{e:#}"),
                };
            });
        }
    }
}

/// Downloads to `path` (through a `.part` file), copying the video to `tx`.
async fn save(
    client: &CloudClient,
    request: &ClipRequest,
    path: &Path,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    state: &Mutex<PlayerState>,
    notify: &Notify,
) -> Result<()> {
    let mut progress = |p: Progress| {
        let mut s = state.lock().unwrap();
        if s.downloaded == 0.0 && p.bytes > 0 {
            s.message = "Playing · downloading…".into();
        }
        s.downloaded = p.seconds;
        drop(s);
        notify();
    };
    save_clip(client, request, path, Some(tx), &mut progress).await
}

/// Downloads a recording to `path`, through a `.part` file that is renamed
/// once the hub has sent all of it. With `tx`, the video is also copied
/// there as it arrives.
pub async fn save_clip(
    client: &CloudClient,
    request: &ClipRequest,
    path: &Path,
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<()> {
    let partial = path.with_extension("ts.part");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("Could not create {}", dir.display()))?;
    }
    let file = std::fs::File::create(&partial).with_context(|| format!("Could not create {}", partial.display()))?;
    let mut file = std::io::BufWriter::new(file);
    let mut media = |data: &[u8]| -> Result<()> {
        file.write_all(data)?;
        if let Some(tx) = &tx {
            let _ = tx.send(data.to_vec()); // the player may have stopped
        }
        Ok(())
    };
    let result = clip::download(client, request, &mut media, progress).await;
    drop(tx);
    let result = result.and_then(|_| {
        file.flush()?;
        drop(file);
        std::fs::rename(&partial, path).with_context(|| format!("Could not save {}", path.display()))?;
        Ok(())
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

/// Shows the MPEG-TS that arrives on `feed` until it ends, at its own pace.
async fn play(ffmpeg: &Path, mut feed: mpsc::UnboundedReceiver<Vec<u8>>, sink: &mut FrameSink) -> Result<()> {
    // Don't start ffmpeg (and its frame wait) before there's video.
    let Some(first) = feed.recv().await else {
        return Ok(()); // the download failed; it reports why
    };
    let input = ["-re", "-f", "mpegts", "-i", "pipe:0"];
    let mut decoder = Decoder::spawn(ffmpeg, &input, WIDTH, HEIGHT)?;
    let stdin = decoder.stdin.take();
    let feeding = async move {
        let Some(mut stdin) = stdin else { return };
        if stdin.write_all(&first).await.is_err() {
            return;
        }
        while let Some(chunk) = feed.recv().await {
            if stdin.write_all(&chunk).await.is_err() {
                return;
            }
        }
        // Dropping stdin tells ffmpeg the clip is complete.
    };
    let (_, stopped) = tokio::join!(feeding, decoder.push_frames_within(sink, QUIET));
    match stopped {
        Stopped::Exited(line) if line.is_empty() => Ok(()),
        Stopped::Exited(line) => anyhow::bail!("ffmpeg: {line}"),
        Stopped::NoVideo => anyhow::bail!("No picture for {}s", QUIET.as_secs()),
    }
}
