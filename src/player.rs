//! Plays a hub recording inside the app: either one saved earlier, or one
//! coming through the cloud, which is saved to `Videos\TapoViewer` as it
//! arrives. ffmpeg's `-re` paces the picture to real time; the download runs
//! ahead into the file, and the player follows the file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

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
            Source::Cloud { path, .. } => Some(partial_path(path)),
            Source::File(_) => None,
        };
        let sink = FrameSink::new(slot.clone());
        let task = rt.spawn(run(ffmpeg, source, partial.clone(), sink, state.clone(), Arc::new(notify)));
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

async fn run(
    ffmpeg: PathBuf,
    source: Source,
    partial: Option<PathBuf>,
    mut sink: FrameSink,
    state: Arc<Mutex<PlayerState>>,
    notify: Notify,
) {
    let update = |f: &dyn Fn(&mut PlayerState)| {
        f(&mut state.lock().unwrap());
        notify();
    };
    // ffmpeg reads at the clip's pace, so this holds only a few chunks.
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    match source {
        Source::File(path) => {
            update(&|s| s.message = "Playing".into());
            let (_, finished) = watch::channel(Some(true));
            let (read, played) = tokio::join!(feed(&path, &path, finished, tx), play(&ffmpeg, rx, &mut sink));
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
            // The download runs at full speed into the file; the player
            // follows the file. `finished` says how the download ended.
            let partial = partial.unwrap_or_else(|| partial_path(&path));
            let (done, finished) = watch::channel(None::<bool>);
            let download = async {
                let saved = save(&client, &request, &path, &partial, &state, &notify).await;
                let _ = done.send(Some(saved.is_ok()));
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
            let playback = async {
                let (fed, played) = tokio::join!(feed(&partial, &path, finished, tx), play(&ffmpeg, rx, &mut sink));
                fed.and(played)
            };
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

/// Sends the recording being written to `partial` (renamed to `path` when
/// complete) to `tx`, following the file as it grows until `finished` says
/// the download is over: `Some(true)` complete, `Some(false)` failed.
/// The built-in ffmpeg reads no files, so even a saved one goes through here.
async fn feed(
    partial: &Path,
    path: &Path,
    mut finished: watch::Receiver<Option<bool>>,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let mut file = loop {
        if let Ok(f) = tokio::fs::File::open(partial).await {
            break f;
        }
        let outcome = *finished.borrow();
        match outcome {
            Some(true) => break tokio::fs::File::open(path).await?,
            Some(false) => return Ok(()),
            None => {}
        }
        // Before the hub has answered there's no file yet.
        let _ = tokio::time::timeout(Duration::from_millis(200), finished.changed()).await;
    };
    let mut buf = vec![0u8; 256 * 1024];
    let mut last_pass = false;
    loop {
        let n = file.read(&mut buf).await?;
        if n > 0 {
            if tx.send(buf[..n].to_vec()).await.is_err() {
                return Ok(()); // the player stopped
            }
            continue;
        }
        if last_pass {
            return Ok(());
        }
        // Once everything is written: one more pass to the end of the file.
        last_pass = finished.borrow().is_some();
        if !last_pass {
            let _ = tokio::time::timeout(Duration::from_millis(200), finished.changed()).await;
        }
    }
}

/// Downloads to `path` (through `partial`), reporting progress in `state`.
async fn save(
    client: &CloudClient,
    request: &ClipRequest,
    path: &Path,
    partial: &Path,
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
    save_clip(client, request, path, partial, &mut progress).await
}

/// A download's own file until it's complete, so two downloads of a clip
/// (or one stopped and one started) don't share one.
pub fn partial_path(path: &Path) -> PathBuf {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    path.with_extension(format!("ts.{tag}.part"))
}

/// Downloads a recording to `path`, through `partial` (see [`partial_path`]),
/// which is renamed once the hub has sent all of it.
pub async fn save_clip(
    client: &CloudClient,
    request: &ClipRequest,
    path: &Path,
    partial: &Path,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("Could not create {}", dir.display()))?;
    }
    let file = std::fs::File::create(partial).with_context(|| format!("Could not create {}", partial.display()))?;
    let mut file = std::io::BufWriter::new(file);
    let mut media = |data: &[u8]| -> Result<()> {
        file.write_all(data)?;
        // Let the player see it soon.
        file.flush()?;
        Ok(())
    };
    let result = clip::download(client, request, &mut media, progress).await;
    let result = result.and_then(|_| {
        file.flush()?;
        drop(file);
        std::fs::rename(partial, path).with_context(|| format!("Could not save {}", path.display()))?;
        Ok(())
    });
    if result.is_err() {
        let _ = std::fs::remove_file(partial);
    }
    result
}

/// Shows the MPEG-TS that arrives on `feed` until it ends, at its own pace.
async fn play(ffmpeg: &Path, mut feed: mpsc::Receiver<Vec<u8>>, sink: &mut FrameSink) -> Result<()> {
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
