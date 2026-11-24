//! Downloads a recording from a hub (H500 HomeBase, H200) through the Tapo
//! cloud relay, so recordings can be watched away from the hub's network.
//!
//! The relay connection is the long POST that live video uses (see
//! [`crate::relay`]), opened towards the hub with the camera named in its
//! `customParams`, as the Tapo app does. On it, client and hub have the
//! conversation the hub has on its local media port, ported from the `tapo`
//! crate's `media_stream/download.rs` (MIT licence):
//!
//! 1. The client asks for the clip with a `get` `download` request naming the
//!    camera (`dev_id`) and the clip's start and end. The hub answers with an
//!    `error_code` and a `session_id`, which later client parts carry as
//!    `X-Session-Id`.
//! 2. MPEG-TS arrives in `video/mp2t` parts numbered by `X-Data-Sequence`;
//!    every 25th is acknowledged with a `stream_sequence` notification, or
//!    the hub stops sending.
//! 3. The client sends a heartbeat every 15 seconds. The hub ends the clip with
//!    a `stream_status` notification of `finished`, and the client says `stop`.
//!
//! Which relay sub-type brings video differs (an H500 accepts "cover", the
//! Tapo app's download type, and then sends nothing), so the kinds the app
//! uses are tried in turn, ending with its real-time playback; whichever
//! works is used first after that.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, mpsc};

use crate::cloud::{Clip, ClipStream, CloudCamera, CloudClient, CloudError, HubCamera};
use crate::relay::{self, IdleTimeout, Part};

/// The Tapo app's window on the download request.
const DATA_WINDOW_SIZE: &str = "50";
/// Topo's window on a playback request.
const PLAYBACK_WINDOW_SIZE: &str = "40";
/// The Tapo app acknowledges every 25th media part.
const ACK_EVERY: u64 = 25;
const HEARTBEAT: Duration = Duration::from_secs(15);
/// How long the hub may stay quiet before the download is given up.
const IDLE: Duration = Duration::from_secs(30);
/// Quiet this long once nearly all of the clip has come means it's over.
const END_QUIET: Duration = Duration::from_secs(3);

/// A piece of the recording as the hub sent it.
pub struct Media<'a> {
    pub data: &'a [u8],
    /// `video/mp2t` for recordings; `image/avc` or `image/hevc` for an
    /// event's picture.
    pub content_type: &'a str,
    /// The hub's wall-clock time for this piece (`X-Data-Pts`), in ms.
    pub pts_ms: Option<i64>,
    /// The piece starts a keyframe (`X-If-Iframe`).
    pub key: bool,
}

pub type MediaSink<'a> = dyn FnMut(Media<'_>) -> Result<()> + Send + 'a;

/// Which recording to fetch.
#[derive(Clone)]
pub struct ClipRequest {
    pub hub: CloudCamera,
    pub camera: HubCamera,
    pub clip: Clip,
    /// The id the clip list was searched with.
    pub player_id: String,
}

/// How far a download has got.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Progress {
    pub bytes: u64,
    /// Seconds of video received.
    pub seconds: f64,
}

/// The ways of fetching a recording, in the order tried until one has been
/// seen to work.
const METHODS: [ClipStream; 3] = [ClipStream::Download("video"), ClipStream::Playback, ClipStream::Download("cover")];

/// A hub serves one download at a time: one asked for while another is
/// still closing is refused (the relay answers HTTP 500). Each download
/// holds this until its connection has closed.
static HUB_SLOT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));

/// Video sessions (playback, saving) waiting for the hub. Event pictures
/// give way to them, and take the hub only while it's free, e.g. between
/// the stretches playback fetches.
static VIDEO_WAITING: AtomicUsize = AtomicUsize::new(0);

/// Counts a video session as waiting until it has the hub (or gives up).
struct Waiting;

impl Waiting {
    fn new() -> Self {
        VIDEO_WAITING.fetch_add(1, Ordering::SeqCst);
        Waiting
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        VIDEO_WAITING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The hub, for one session: video waits its turn; a picture waits until no
/// video wants the hub.
async fn take_hub(picture: bool) -> tokio::sync::OwnedSemaphorePermit {
    if !picture {
        let _waiting = Waiting::new();
        return HUB_SLOT.clone().acquire_owned().await.expect("the hub slot is never closed");
    }
    loop {
        if VIDEO_WAITING.load(Ordering::SeqCst) == 0
            && let Ok(slot) = HUB_SLOT.clone().try_acquire_owned()
        {
            return slot;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The index in [`METHODS`] that last brought video, tried first next time.
static WORKING: AtomicUsize = AtomicUsize::new(usize::MAX);

/// The hub (or the cloud) gave no video this way; another may work.
#[derive(Debug)]
struct NoVideo(String);

impl std::fmt::Display for NoVideo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NoVideo {}

/// Fetches the clip, handing each piece of MPEG-TS to `media` as it arrives,
/// and `progress` after each piece.
pub async fn download(
    client: &CloudClient,
    request: &ClipRequest,
    media: &mut MediaSink<'_>,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<Progress> {
    let known = WORKING.load(Ordering::Relaxed);
    let mut order: Vec<usize> = (0..METHODS.len()).collect();
    if known < METHODS.len() {
        order.retain(|&i| i != known);
        order.insert(0, known);
    }
    for i in order {
        let method = METHODS[i];
        log::info!("clip: trying {method:?}");
        let mut result = attempt(client, request, method, media, progress).await;
        // The hub lets go of a session a moment after it ends.
        for _ in 0..4 {
            match &result {
                Err(e) if e.is::<relay::RelayBusy>() => {
                    log::info!("clip: {e:#}; trying again in a second");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    result = attempt(client, request, method, media, progress).await;
                }
                _ => break,
            }
        }
        match result {
            Ok(got) => {
                if WORKING.swap(i, Ordering::Relaxed) != i {
                    log::info!("clip: {method:?} works; using it first from now on");
                }
                return Ok(got);
            }
            Err(e) if e.is::<NoVideo>() || e.downcast_ref::<CloudError>().is_some_and(CloudError::is_refused) => {
                log::warn!("clip: {method:?} brought no video: {e:#}");
            }
            Err(e) => return Err(e),
        }
    }
    bail!("The hub sent no video for this recording. The log has what it said to each kind of request")
}

/// One way of fetching the clip. Fails with [`NoVideo`] if it ends before
/// any video arrives.
async fn attempt(
    client: &CloudClient,
    request: &ClipRequest,
    method: ClipStream,
    media: &mut MediaSink<'_>,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<Progress> {
    let (first_request, window) = match method {
        ClipStream::Download(_) => (download_request(1, request), DATA_WINDOW_SIZE),
        ClipStream::Playback => {
            let client_id = match client.hub_user_id(&request.hub).await {
                Ok(id) => id,
                Err(e) => {
                    log::info!("clip: no user id from the hub ({e}); using 1");
                    1
                }
            };
            (playback_request(1, request, client_id), PLAYBACK_WINDOW_SIZE)
        }
    };
    session(client, request, method, first_request, window, media, progress).await
}

/// One session with the hub through the relay: `first_request`, then what it
/// sends until it says it's done.
async fn session(
    client: &CloudClient,
    request: &ClipRequest,
    method: ClipStream,
    first_request: Value,
    window: &str,
    media: &mut MediaSink<'_>,
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<Progress> {
    let picture = first_request.pointer("/params/download/end_time").is_none();
    let slot = take_hub(picture).await;
    let info = client.request_clip_relay(&request.hub, &request.camera, &request.player_id, method).await?;
    let mut stream = relay::connect(client.tls(), &info).await?;
    let mut first = relay::recording_head(&info).into_bytes();
    first.extend(relay::client_part(
        &[("Content-Type", "application/json"), ("X-Data-Window-Size", window)],
        first_request.to_string().as_bytes(),
    ));
    stream.write_all(&first).await.context("Could not talk to the video relay")?;
    stream.flush().await?;

    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::with_capacity(64 * 1024, read_half);
    let session = Arc::new(Mutex::new(None::<String>));
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        write_parts(write_half, rx).await;
        // Give the hub a moment to let go of the session.
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(slot);
    });
    // From here on the hub has a session for us. The guard ends it however
    // this function ends, including when the download is cancelled (the
    // player stopped, another clip picked): it says stop, stops the
    // heartbeat, and lets the writer close the connection.
    let mut guard = SessionGuard { tx: Some(tx.clone()), session: session.clone(), heartbeat: None };
    relay::read_response_head(&mut reader).await?;
    guard.heartbeat = Some(tokio::spawn({
        let (tx, session) = (tx.clone(), session.clone());
        async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
            loop {
                tick.tick().await;
                let part = control_part(&session, &[], &json!({"type": "notification", "params": {"event_type": "heartbeat"}}));
                if tx.send(part).is_err() {
                    break;
                }
            }
        }
    }));

    let mut state = Download { tx, session: session.clone(), clock: TsClock::default(), got: Progress::default(), last_sequence: None };
    let expected = request.clip.seconds() as f64;
    let deadline = Instant::now() + Duration::from_secs(request.clip.seconds().max(0) as u64 + 300);
    let result = async {
        loop {
            if Instant::now() > deadline {
                bail!("The recording took too long to arrive");
            }
            // Near the end, a short silence is the end: playback sends no
            // end message on some firmware, and may run on into what follows.
            let near_end = state.got.bytes > 0 && state.got.seconds + 1.0 >= expected;
            if state.got.seconds >= expected + 1.0 {
                return Ok(());
            }
            let part = match relay::read_part(&mut reader, if near_end { END_QUIET } else { IDLE }).await {
                Ok(part) => part,
                Err(e) if e.is::<IdleTimeout>() && near_end => {
                    log::info!("clip: the hub went quiet after {:.1}s of video; taking that as the end", state.got.seconds);
                    return Ok(());
                }
                Err(e) if e.is::<IdleTimeout>() && state.got.bytes == 0 => {
                    return Err(NoVideo(format!("nothing for {}s", IDLE.as_secs())).into());
                }
                Err(e) if e.is::<IdleTimeout>() => bail!("The hub stopped sending the recording"),
                Err(e) => return Err(e),
            };
            if state.handle(part, media)? {
                return Ok(());
            }
            progress(state.got);
        }
    }
    .await;

    // Release the hub's session either way.
    let got = state.got;
    drop(state);
    drop(guard);
    let _ = tokio::time::timeout(Duration::from_secs(3), writer).await;
    result?;
    if got.bytes == 0 {
        return Err(NoVideo("the hub finished without sending any video".into()).into());
    }
    log::info!("clip: {} bytes, {:.1}s of video (clip is {expected}s)", got.bytes, got.seconds);
    Ok(got)
}

/// The Tapo app's playback request (`GetVodParams`), as Topo sends it to
/// cameras, with the hub's camera named by its MAC address as the app does.
fn playback_request(seq: u32, request: &ClipRequest, client_id: i64) -> Value {
    json!({
        "type": "request",
        "seq": seq,
        "params": {
            "method": "get",
            "playback": {
                "client_id": client_id,
                "channels": [0],
                "scale": "1/1",
                "start_time": request.clip.start.to_string(),
                "end_time": request.clip.end.to_string(),
                "player_id": request.player_id,
                "camera_mac": request.camera.mac,
                "streams": [],
            }
        }
    })
}

/// Downloads a recording into `partial` (see [`partial_path`]) and remuxes it
/// into an `.mp4` at `path` (or renames directly if `ffmpeg` is `None`).
pub async fn save_clip(
    client: &CloudClient,
    request: &ClipRequest,
    path: &Path,
    partial: &Path,
    ffmpeg: Option<&Path>,
    progress: &mut (dyn FnMut(Progress) + Send),
    on_finalize: impl FnOnce() + Send,
) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("Could not create {}", dir.display()))?;
    }
    let file = std::fs::File::create(partial).with_context(|| format!("Could not create {}", partial.display()))?;
    let mut file = std::io::BufWriter::new(file);
    let mut media = |m: Media<'_>| -> Result<()> {
        file.write_all(m.data)?;
        Ok(())
    };
    let result = download(client, request, &mut media, progress).await;
    let dl = result.and_then(|_| {
        file.flush()?;
        Ok(())
    });
    drop(file);
    let final_result = match dl {
        Ok(()) => {
            if let Some(ff) = ffmpeg {
                on_finalize();
                let res = crate::ffmpeg::remux_ts_to_mp4(ff, partial, path).await;
                let _ = std::fs::remove_file(partial);
                res
            } else {
                std::fs::rename(partial, path).with_context(|| format!("Could not save {}", path.display()))
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(partial);
            Err(e)
        }
    };
    final_result
}

/// A download's own file until it's complete, so two downloads of a clip
/// don't share one.
pub fn partial_path(path: &Path) -> PathBuf {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    path.with_extension(format!("ts.{tag}.part"))
}

/// The picture media types the Tapo app asks for (`DownloadMediaType`):
/// THUMBNAIL, then PUSH_IMAGE.
const PICTURE_TYPES: [i64; 2] = [3, 1];
/// The index in [`PICTURE_TYPES`] that last brought a picture.
static PICTURE_TYPE: AtomicUsize = AtomicUsize::new(0);

/// The picture the hub keeps for an event (the thumbnails of the Tapo app's
/// event list), for the event starting at `request.clip.start`: one encoded
/// keyframe and its content type, `image/avc` (H.264) or `image/hevc`. It
/// comes through a "cover" download, the app's.
pub async fn event_picture(client: &CloudClient, request: &ClipRequest) -> Result<(Vec<u8>, String)> {
    let first = PICTURE_TYPE.load(Ordering::Relaxed);
    let order = [first, 1 - first];
    for i in order {
        let mut outcome: Result<(Vec<u8>, String)> = Err(anyhow::anyhow!("not tried"));
        for _ in 0..4 {
            let (mut data, mut kind) = (Vec::new(), String::new());
            let ask = picture_request(1, request, PICTURE_TYPES[i]);
            let result = {
                let mut sink = |m: Media<'_>| -> Result<()> {
                    if kind.is_empty() {
                        kind = m.content_type.to_string();
                    }
                    data.extend_from_slice(m.data);
                    Ok(())
                };
                session(client, request, ClipStream::Download("cover"), ask, DATA_WINDOW_SIZE, &mut sink, &mut |_| {})
                    .await
            };
            match result {
                Err(e) if e.is::<relay::RelayBusy>() => {
                    outcome = Err(e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) => {
                    outcome = Err(e);
                    break;
                }
                Ok(_) => {
                    outcome = Ok((data, kind));
                    break;
                }
            }
        }
        match outcome {
            Ok((data, kind)) if !data.is_empty() => {
                PICTURE_TYPE.store(i, Ordering::Relaxed);
                return Ok((data, kind));
            }
            Ok(_) => {}
            Err(e) if e.is::<NoVideo>() => log::info!("clip: no event picture as media type {}", PICTURE_TYPES[i]),
            Err(e) => return Err(e),
        }
    }
    bail!("The hub has no picture for this event")
}

/// The Tapo app's request for an event picture (`GetDownloadParams` without
/// an end).
fn picture_request(seq: u32, request: &ClipRequest, media_type: i64) -> Value {
    json!({
        "type": "request",
        "seq": seq,
        "params": {
            "method": "get",
            "download": {
                "channels": [0],
                "client_id": 1,
                "media_type": media_type,
                "start_time": request.clip.start.to_string(),
                "dev_id": request.camera.device_id,
                "streams": [],
            }
        }
    })
}

/// The Tapo app's request for H500 facial profile JPEGs (`GetImageRequest` /
/// `GetDownloadFaceImageParams` extending `ImageParams("get_face")` on `STREAM_TYPE_IMAGE = 6`).
fn face_image_request(seq: u32, face_ids: &[i64]) -> Value {
    json!({
        "type": "request",
        "seq": seq,
        "params": {
            "method": "get",
            "image": {
                "type": "get_face",
                "face_id": face_ids,
            }
        }
    })
}

/// Downloads face profile JPEGs for `face_ids` from an H500 hub over the
/// cloud relay (`STREAM_TYPE_IMAGE = 6`). Yields to active playback/downloads
/// via `take_hub(true)` so it never competes with video streams.
pub async fn face_images(
    client: &CloudClient,
    hub: &CloudCamera,
    player_id: &str,
    face_ids: &[i64],
) -> Result<HashMap<i64, Vec<u8>>> {
    if face_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut last_err = None;
    for attempt_no in 0..3 {
        match face_images_once(client, hub, player_id, face_ids).await {
            Ok(map) => return Ok(map),
            Err(e) if e.is::<relay::RelayBusy>() && attempt_no < 2 => {
                log::info!("face_images: {e:#}; retrying in 1s");
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("The hub was busy while fetching face photos")))
}

async fn face_images_once(
    client: &CloudClient,
    hub: &CloudCamera,
    player_id: &str,
    face_ids: &[i64],
) -> Result<HashMap<i64, Vec<u8>>> {
    let slot = take_hub(true).await;
    let info = client.request_face_image_relay(hub, player_id).await?;
    let mut stream = relay::connect(client.tls(), &info).await?;
    let ask = face_image_request(1, face_ids);
    let mut first = relay::recording_head(&info).into_bytes();
    first.extend(relay::client_part(
        &[("Content-Type", "application/json"), ("X-Data-Window-Size", DATA_WINDOW_SIZE)],
        ask.to_string().as_bytes(),
    ));
    stream.write_all(&first).await.context("Could not talk to the video relay")?;
    stream.flush().await?;

    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::with_capacity(64 * 1024, read_half);
    let session = Arc::new(Mutex::new(None::<String>));
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        write_parts(write_half, rx).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(slot);
    });
    let mut guard = SessionGuard { tx: Some(tx.clone()), session: session.clone(), heartbeat: None };
    relay::read_response_head(&mut reader).await?;
    guard.heartbeat = Some(tokio::spawn({
        let (tx, session) = (tx.clone(), session.clone());
        async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
            loop {
                tick.tick().await;
                let part = control_part(&session, &[], &json!({"type": "notification", "params": {"event_type": "heartbeat"}}));
                if tx.send(part).is_err() {
                    break;
                }
            }
        }
    }));

    let mut state = FaceImageSession::new(tx, session.clone(), face_ids);
    let deadline = Instant::now() + Duration::from_secs(45);
    let result = async {
        loop {
            if Instant::now() > deadline {
                break;
            }
            let wait = if state.received.len() >= face_ids.len() { END_QUIET } else { Duration::from_secs(15) };
            let part = match relay::read_part(&mut reader, wait).await {
                Ok(part) => part,
                Err(e) if e.is::<IdleTimeout>() => {
                    state.flush_pending(None);
                    break;
                }
                Err(e) if !state.received.is_empty() => {
                    log::info!("face_images: relay ended after {} photo(s): {e:#}", state.received.len());
                    state.flush_pending(None);
                    break;
                }
                Err(e) => return Err(e),
            };
            if state.handle(part)? {
                break;
            }
        }
        Ok(())
    }
    .await;

    let received = std::mem::take(&mut state.received);
    drop(state);
    drop(guard);
    let _ = tokio::time::timeout(Duration::from_secs(3), writer).await;
    result?;
    log::info!("face_images: fetched {}/{} photo(s) from hub", received.len(), face_ids.len());
    Ok(received)
}

/// Multipart state machine for `STREAM_TYPE_IMAGE` (H500 face JPEGs).
struct FaceImageSession {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    session: Arc<Mutex<Option<String>>>,
    requested: Vec<i64>,
    current_buf: Vec<u8>,
    current_face_id: Option<i64>,
    received: HashMap<i64, Vec<u8>>,
}

impl FaceImageSession {
    fn new(tx: mpsc::UnboundedSender<Vec<u8>>, session: Arc<Mutex<Option<String>>>, face_ids: &[i64]) -> Self {
        Self {
            tx,
            session,
            requested: face_ids.to_vec(),
            current_buf: Vec::new(),
            current_face_id: None,
            received: HashMap::new(),
        }
    }

    fn next_unreceived(&self) -> Option<i64> {
        self.requested.iter().copied().find(|id| !self.received.contains_key(id))
    }

    fn flush_pending(&mut self, hint_face_id: Option<i64>) {
        if self.current_buf.is_empty() {
            return;
        }
        let id = hint_face_id.or(self.current_face_id).or_else(|| self.next_unreceived());
        let data = std::mem::take(&mut self.current_buf);
        self.current_face_id = None;
        if let Some(id) = id {
            self.received.insert(id, data);
        }
    }

    fn handle(&mut self, part: Part) -> Result<bool> {
        if part.is_json() {
            let text = String::from_utf8_lossy(&part.body);
            log::info!("face_images: hub says {text}");
            let Ok(message) = serde_json::from_slice::<Value>(&part.body) else { return Ok(false) };
            let params = message.get("params").cloned().unwrap_or(Value::Null);
            match message.get("type").and_then(Value::as_str) {
                Some("response") if message.get("seq").and_then(Value::as_i64) == Some(1) => {
                    let code = params.get("error_code").and_then(Value::as_i64).unwrap_or(0);
                    if code != 0 {
                        bail!("{}", refusal(code));
                    }
                    let id = match params.get("session_id") {
                        Some(Value::String(s)) => Some(s.clone()),
                        Some(Value::Number(n)) => Some(n.to_string()),
                        _ => None,
                    };
                    if id.is_some() {
                        *self.session.lock().unwrap() = id;
                    }
                    Ok(false)
                }
                Some("notification") => {
                    let event = params.get("event_type").and_then(Value::as_str).unwrap_or("");
                    if event != "stream_status" {
                        return Ok(false);
                    }
                    let res = params.get("result").unwrap_or(&params);
                    let status = res
                        .get("status")
                        .and_then(Value::as_str)
                        .or_else(|| params.get("status").and_then(Value::as_str))
                        .unwrap_or("");
                    let face_id = res
                        .get("face_id")
                        .or_else(|| params.get("face_id"))
                        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
                    match status {
                        "image_ended" => {
                            self.flush_pending(face_id);
                            let ack = json!({
                                "type": "notification",
                                "params": {
                                    "event_type": "ack_receive",
                                    "status": "success"
                                }
                            });
                            let _ = self.tx.send(control_part(&self.session, &[], &ack));
                            Ok(false)
                        }
                        "finished" => {
                            self.flush_pending(face_id);
                            Ok(true)
                        }
                        _ => Ok(false),
                    }
                }
                _ => Ok(false),
            }
        } else {
            if part.body.is_empty() {
                return Ok(false);
            }
            if part.header("x-if-encrypt") == Some("1") || part.header("x-data-hmac").is_some() {
                log::warn!("face_images: encrypted part, headers {:?}", part.headers);
                bail!("The hub sent encrypted face photos");
            }
            if self.current_face_id.is_none()
                && let Some(id) = part.header("x-face-id").and_then(|s| s.trim().parse::<i64>().ok())
            {
                self.current_face_id = Some(id);
            }
            self.current_buf.extend_from_slice(&part.body);
            Ok(false)
        }
    }
}

/// The first client part: which clip, from which camera.
fn download_request(seq: u32, request: &ClipRequest) -> Value {
    json!({
        "type": "request",
        "seq": seq,
        "params": {
            "method": "get",
            "download": {
                "dev_id": request.camera.device_id,
                "client_id": 1,
                "end_time": request.clip.end.to_string(),
                "media_type": 0,
                "player_id": request.player_id,
                "start_time": request.clip.start.to_string(),
                "channels": [0],
                "streams": [],
            }
        }
    })
}

/// Ends the hub's download session when dropped: sends `stop`, stops the
/// heartbeat and drops its sender, so the writer task sends what's queued,
/// closes the connection and ends. Without this, a cancelled download would
/// leave the heartbeat keeping the session alive on the hub, which has only
/// a few, until the app exits.
struct SessionGuard {
    tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    session: Arc<Mutex<Option<String>>>,
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        if let Some(tx) = self.tx.take() {
            let stop = json!({"type": "request", "seq": 2, "params": {"method": "do", "stop": "null"}});
            if tx.send(control_part(&self.session, &[], &stop)).is_ok() {
                log::info!("clip: told the hub to stop");
            }
        }
    }
}

fn control_part(session: &Mutex<Option<String>>, extra: &[(&str, &str)], message: &Value) -> Vec<u8> {
    let session = session.lock().unwrap().clone();
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some(id) = &session {
        headers.push(("X-Session-Id", id));
    }
    headers.extend_from_slice(extra);
    headers.push(("Content-Type", "application/json"));
    relay::client_part(&headers, message.to_string().as_bytes())
}

/// Sends the client's parts until every sender is gone, then closes the
/// connection. Never waits long on a stuck connection, so it always ends.
async fn write_parts<W: tokio::io::AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    const WAIT: Duration = Duration::from_secs(10);
    while let Some(part) = rx.recv().await {
        let sent = tokio::time::timeout(WAIT, async {
            writer.write_all(&part).await?;
            writer.flush().await
        })
        .await;
        if !matches!(sent, Ok(Ok(()))) {
            return;
        }
    }
    let _ = tokio::time::timeout(WAIT, writer.shutdown()).await;
}

struct Download {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    session: Arc<Mutex<Option<String>>>,
    clock: TsClock,
    got: Progress,
    last_sequence: Option<u64>,
}

impl Download {
    /// Returns `true` once the hub has sent the whole clip.
    fn handle(&mut self, part: Part, media: &mut MediaSink<'_>) -> Result<bool> {
        if part.is_json() {
            return self.control(&part);
        }
        if part.body.is_empty() {
            return Ok(false);
        }
        if part.header("x-if-encrypt") == Some("1") || part.header("x-data-hmac").is_some() {
            log::warn!("clip: encrypted media part, headers {:?}", part.headers);
            bail!("The hub sent the recording encrypted, which Tapas can't read through the cloud yet");
        }
        if self.got.bytes == 0 {
            log::info!("clip: first media part, headers {:?}", part.headers);
        }
        let pts_ms = part.header("x-data-pts").and_then(|v| v.trim().parse::<i64>().ok());
        let key = part.header("x-if-iframe") == Some("1");
        let content_type = part.header("content-type").unwrap_or("");
        media(Media { data: &part.body, content_type, pts_ms, key }).context("Could not use the recording")?;
        self.got.bytes += part.body.len() as u64;
        self.clock.feed(&part.body);
        self.got.seconds = self.clock.seconds();

        let sequence = part.header("x-data-sequence").and_then(|v| v.parse::<u64>().ok());
        if let Some(seq) = sequence {
            if let Some(last) = self.last_sequence
                && seq > last + 1
            {
                log::warn!("clip: parts {}..{} are missing", last + 1, seq - 1);
            }
            self.last_sequence = Some(seq);
            if seq % ACK_EVERY == 0 {
                let received = seq.to_string();
                let ack = json!({"type": "notification", "params": {"event_type": "stream_sequence"}});
                let _ = self.tx.send(control_part(&self.session, &[("X-Data-Received", &received)], &ack));
            }
        }
        Ok(false)
    }

    fn control(&mut self, part: &Part) -> Result<bool> {
        let text = String::from_utf8_lossy(&part.body);
        log::info!("clip: hub says {text}");
        let Ok(message) = serde_json::from_slice::<Value>(&part.body) else { return Ok(false) };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match message.get("type").and_then(Value::as_str) {
            Some("response") if message.get("seq").and_then(Value::as_i64) == Some(1) => {
                let code = params.get("error_code").and_then(Value::as_i64).unwrap_or(0);
                if code != 0 {
                    bail!("{}", refusal(code));
                }
                let id = match params.get("session_id") {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(Value::Number(n)) => Some(n.to_string()),
                    _ => None,
                };
                if id.is_some() {
                    *self.session.lock().unwrap() = id;
                }
                Ok(false)
            }
            Some("notification") => {
                let event = params.get("event_type").and_then(Value::as_str).unwrap_or("");
                let status = params.get("status").and_then(Value::as_str).unwrap_or("");
                Ok(event == "stream_status" && status == "finished")
            }
            // Topo's camera playback ends on these.
            _ => Ok(text.contains("playback_done") || text.contains("playfinished")),
        }
    }
}

fn refusal(code: i64) -> String {
    match code {
        -40401 | -40406 => format!("The camera's recordings aren't reachable right now (code {code})"),
        -64323 | -71203 => format!("The hub is busy with other downloads; try again shortly (code {code})"),
        _ => format!("The hub refused the recording (code {code})"),
    }
}

/// How much video an MPEG-TS stream holds, from the first and latest
/// presentation timestamps of its video.
#[derive(Debug, Default)]
pub struct TsClock {
    /// Bytes of an incomplete 188-byte packet.
    pending: Vec<u8>,
    first: Option<u64>,
    last: u64,
}

impl TsClock {
    pub fn feed(&mut self, mut data: &[u8]) {
        if !self.pending.is_empty() {
            let need = 188 - self.pending.len();
            let take = need.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() < 188 {
                return;
            }
            let packet = std::mem::take(&mut self.pending);
            self.packet(&packet);
        }
        // Find the sync byte if a part doesn't start on a packet.
        while !data.is_empty() && data[0] != 0x47 {
            data = &data[1..];
        }
        let mut chunks = data.chunks_exact(188);
        for packet in &mut chunks {
            self.packet(packet);
        }
        self.pending.extend_from_slice(chunks.remainder());
    }

    pub fn seconds(&self) -> f64 {
        match self.first {
            Some(first) if self.last >= first => (self.last - first) as f64 / 90_000.0,
            _ => 0.0,
        }
    }

    fn packet(&mut self, p: &[u8]) {
        if p[0] != 0x47 || p[1] & 0x40 == 0 {
            return; // out of sync, or not the start of a PES packet
        }
        let mut at = 4;
        match (p[3] >> 4) & 0x3 {
            1 => {}
            3 => at += 1 + p[4] as usize,
            _ => return,
        }
        let Some(pes) = p.get(at..) else { return };
        // Video streams only (0xE0..0xEF), with a PTS.
        if pes.len() < 14 || pes[..3] != [0, 0, 1] || pes[3] & 0xF0 != 0xE0 || pes[7] & 0x80 == 0 {
            return;
        }
        let b = &pes[9..14];
        let pts = ((b[0] as u64 >> 1) & 0x07) << 30
            | (b[1] as u64) << 22
            | ((b[2] as u64) >> 1) << 15
            | (b[3] as u64) << 7
            | (b[4] as u64) >> 1;
        let first = *self.first.get_or_insert(pts);
        if pts >= first {
            self.last = self.last.max(pts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ClipRequest {
        ClipRequest {
            hub: CloudCamera { device_id: "HUB".into(), ..Default::default() },
            camera: HubCamera { device_id: "CAM".into(), mac: "A8-42".into(), ..Default::default() },
            clip: Clip { start: 1_700_000_000, end: 1_700_000_060, kind: 2 },
            player_id: "PLAYER".into(),
        }
    }

    #[test]
    fn download_request_matches_the_app() {
        assert_eq!(
            download_request(1, &request()),
            json!({"type": "request", "seq": 1, "params": {"method": "get", "download": {
                "dev_id": "CAM", "client_id": 1, "end_time": "1700000060", "media_type": 0,
                "player_id": "PLAYER", "start_time": "1700000000", "channels": [0], "streams": []
            }}})
        );
    }

    #[test]
    fn playback_request_names_the_camera_by_mac() {
        assert_eq!(
            playback_request(1, &request(), 7),
            json!({"type": "request", "seq": 1, "params": {"method": "get", "playback": {
                "client_id": 7, "channels": [0], "scale": "1/1", "start_time": "1700000000",
                "end_time": "1700000060", "player_id": "PLAYER", "camera_mac": "A8-42", "streams": []
            }}})
        );
    }

    #[test]
    fn picture_request_matches_the_app() {
        assert_eq!(
            picture_request(1, &request(), 3),
            json!({"type": "request", "seq": 1, "params": {"method": "get", "download": {
                "channels": [0], "client_id": 1, "media_type": 3, "start_time": "1700000000",
                "dev_id": "CAM", "streams": []
            }}})
        );
    }

    #[test]
    fn control_parts_carry_the_session() {
        let session = Mutex::new(None);
        let p = String::from_utf8(control_part(&session, &[], &json!({"a": 1}))).unwrap();
        assert!(!p.contains("X-Session-Id"));
        *session.lock().unwrap() = Some("42".into());
        let p = String::from_utf8(control_part(&session, &[("X-Data-Received", "25")], &json!({"a": 1}))).unwrap();
        assert!(p.starts_with("----client-stream-boundary--\r\nX-Session-Id: 42\r\nX-Data-Received: 25\r\nContent-Type: application/json\r\n"));
    }

    fn json_part(text: &str) -> Part {
        Part { headers: vec![("content-type".into(), "application/json".into())], body: text.as_bytes().to_vec() }
    }

    fn media_part(seq: u64, body: &[u8]) -> Part {
        Part {
            headers: vec![("content-type".into(), "video/mp2t".into()), ("x-data-sequence".into(), seq.to_string())],
            body: body.to_vec(),
        }
    }

    #[test]
    fn conversation_with_the_hub() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut d = Download {
            tx,
            session: Arc::default(),
            clock: TsClock::default(),
            got: Progress::default(),
            last_sequence: None,
        };
        let saved = Arc::new(Mutex::new(Vec::new()));
        let sink = saved.clone();
        let mut media = move |m: Media<'_>| -> Result<()> {
            assert_eq!(m.content_type, "video/mp2t");
            sink.lock().unwrap().extend_from_slice(m.data);
            Ok(())
        };
        assert!(!d.handle(json_part(r#"{"type":"response","seq":1,"params":{"error_code":0,"session_id":"42"}}"#), &mut media).unwrap());
        assert_eq!(d.session.lock().unwrap().as_deref(), Some("42"));
        for seq in 1..=25 {
            assert!(!d.handle(media_part(seq, b"x"), &mut media).unwrap());
        }
        assert_eq!(saved.lock().unwrap().len(), 25);
        let ack = String::from_utf8(rx.try_recv().unwrap()).unwrap();
        assert!(ack.contains("X-Session-Id: 42\r\nX-Data-Received: 25\r\n") && ack.contains("stream_sequence"), "{ack}");
        assert!(rx.try_recv().is_err(), "only every 25th part is acknowledged");
        assert!(!d.handle(json_part(r#"{"type":"notification","params":{"event_type":"heartbeat"}}"#), &mut media).unwrap());
        assert!(d.handle(json_part(r#"{"type":"notification","params":{"event_type":"stream_status","status":"finished"}}"#), &mut media).unwrap());
    }

    #[test]
    fn refusals_and_encryption_are_errors() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut d = Download { tx, session: Arc::default(), clock: TsClock::default(), got: Progress::default(), last_sequence: None };
        let mut media = |_: Media<'_>| -> Result<()> { Ok(()) };
        let e = d.handle(json_part(r#"{"type":"response","seq":1,"params":{"error_code":-40210}}"#), &mut media).unwrap_err();
        assert!(e.to_string().contains("-40210"));
        let mut encrypted = media_part(1, b"x");
        encrypted.headers.push(("x-if-encrypt".into(), "1".into()));
        assert!(d.handle(encrypted, &mut media).unwrap_err().to_string().contains("encrypted"));
    }

    #[test]
    fn clock_measures_the_fixture() {
        // tests/fixtures/relay-sample.ts is 6 s of 15 fps video.
        let ts = include_bytes!("../tests/fixtures/relay-sample.ts");
        let mut clock = TsClock::default();
        for chunk in ts.chunks(1000) {
            clock.feed(chunk);
        }
        let s = clock.seconds();
        assert!((5.0..6.5).contains(&s), "{s}");
    }

    #[test]
    fn face_image_conversation_handles_jpegs_and_acks() {
        assert_eq!(
            face_image_request(1, &[101, 202]),
            json!({"type": "request", "seq": 1, "params": {"method": "get", "image": {"type": "get_face", "face_id": [101, 202]}}})
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut s = FaceImageSession::new(tx, Arc::default(), &[101, 202]);
        assert!(!s.handle(json_part(r#"{"type":"response","seq":1,"params":{"error_code":0,"session_id":"88"}}"#)).unwrap());
        assert_eq!(s.session.lock().unwrap().as_deref(), Some("88"));

        let jpeg_part = |bytes: &[u8]| Part {
            headers: vec![("content-type".into(), "image/jpeg".into())],
            body: bytes.to_vec(),
        };
        assert!(!s.handle(jpeg_part(b"\xff\xd8part1")).unwrap());
        assert!(!s.handle(jpeg_part(b"part2\xff\xd9")).unwrap());
        assert!(!s
            .handle(json_part(
                r#"{"type":"notification","params":{"event_type":"stream_status","result":{"status":"image_ended","face_id":101}}}"#,
            ))
            .unwrap());
        let ack1 = String::from_utf8(rx.try_recv().unwrap()).unwrap();
        assert!(ack1.contains("X-Session-Id: 88") && ack1.contains("ack_receive"), "{ack1}");

        // Second face without explicit face_id falls back to requested order.
        assert!(!s.handle(jpeg_part(b"\xff\xd8second\xff\xd9")).unwrap());
        assert!(!s
            .handle(json_part(
                r#"{"type":"notification","params":{"event_type":"stream_status","status":"image_ended"}}"#,
            ))
            .unwrap());
        assert!(rx.try_recv().is_ok());

        assert!(s
            .handle(json_part(
                r#"{"type":"notification","params":{"event_type":"stream_status","status":"finished"}}"#,
            ))
            .unwrap());
        assert_eq!(s.received.get(&101).map(Vec::as_slice), Some(&b"\xff\xd8part1part2\xff\xd9"[..]));
        assert_eq!(s.received.get(&202).map(Vec::as_slice), Some(&b"\xff\xd8second\xff\xd9"[..]));
    }
}
