//! Watching a camera's recordings on its hub along the day, like the Tapo
//! app's playback: from any moment, on through the day's recordings, at 1× to
//! 16×, with pause.
//!
//! The video comes through the cloud in windows of two minutes (see
//! [`crate::clip`]), each fetched shortly before the picture needs it. What
//! has been fetched stays in memory for a while (see [`Cache`]), so going
//! back, or forward into what has already arrived, starts at once. Each
//! recording is decoded by its own ffmpeg, whose `showinfo` timestamps say
//! when each frame was recorded; frames are shown at that time divided by
//! the speed. Nothing is written to disk.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::Result;
use slint::SharedPixelBuffer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::clip::{self, ClipRequest, Media};
use crate::cloud::{Clip, CloudCamera, CloudClient, CloudError, HubCamera};
use crate::ffmpeg::Decoder;
use crate::stream::{Frame, FrameSink, Slot, Status};

pub const SPEEDS: [u32; 5] = [1, 2, 4, 8, 16];
/// From this speed only keyframes are decoded: decoding every frame eight
/// times faster than real time is too much for many PCs.
const KEYFRAMES_FROM: u32 = 8;
/// Seconds of recording fetched at a time.
const WINDOW: i64 = 120;
/// The next window is fetched when less than this many seconds (times the
/// speed) are left to show.
const AHEAD: i64 = 45;
/// How much fetched video is kept in memory, across recordings: a few
/// minutes of a camera's HD video. Cleared when leaving Playback.
const CACHE_BYTES: usize = 48 << 20;
/// Paused this long, playback lets go of its ffmpeg (whose queues fill
/// while nothing is read: ~100 MB) and the hub's session; the picture stays,
/// and playing on starts again from it, mostly from the cache.
const PARK_AFTER: Duration = Duration::from_secs(10);
const WIDTH: usize = 1280;
const HEIGHT: usize = 720;

/// What to play: a camera's recordings on a hub, for one day.
#[derive(Clone)]
pub struct Source {
    pub client: Arc<CloudClient>,
    pub hub: CloudCamera,
    pub camera: HubCamera,
    pub player_id: String,
    /// The day's recordings, in any order.
    pub segments: Vec<Clip>,
    pub ffmpeg: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct PlayState {
    /// When the picture shown was recorded (Unix ms).
    pub position_ms: i64,
    pub paused: bool,
    pub speed: u32,
    /// Waiting for video.
    pub buffering: bool,
    /// Why playback stopped, if it did.
    pub message: String,
    /// Past the day's last recording.
    pub ended: bool,
}

enum Cmd {
    Seek(i64),
    Pause(bool),
    Speed(u32),
}

pub struct Playback {
    pub slot: Arc<Mutex<Slot>>,
    pub state: Arc<Mutex<PlayState>>,
    cmds: mpsc::UnboundedSender<Cmd>,
    task: tokio::task::JoinHandle<()>,
}

impl Playback {
    pub fn start(rt: &tokio::runtime::Handle, source: Source, at_ms: i64, speed: u32) -> Self {
        let slot = Arc::new(Mutex::new(Slot { frame: None, version: 0, status: Status::Connecting, fps: 0.0 }));
        let state = Arc::new(Mutex::new(PlayState { position_ms: at_ms, speed, buffering: true, ..Default::default() }));
        let (cmds, rx) = mpsc::unbounded_channel();
        let mut segments = source.segments.clone();
        segments.sort_by_key(|c| c.start);
        let source = Source { segments, ..source };
        let task = rt.spawn(run(source, FrameSink::new(slot.clone()), state.clone(), rx, at_ms));
        Self { slot, state, cmds, task }
    }

    pub fn seek(&self, at_ms: i64) {
        let _ = self.cmds.send(Cmd::Seek(at_ms));
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmds.send(Cmd::Pause(paused));
    }

    pub fn set_speed(&self, speed: u32) {
        let _ = self.cmds.send(Cmd::Speed(speed));
    }

    pub fn latest_frame(&self) -> Option<Frame> {
        self.slot.lock().unwrap().frame.clone()
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        // Dropping the task ends any download through its session guard.
        self.task.abort();
    }
}

/// The recording that `at_ms` falls in, or else the next one.
fn segment_at(segments: &[Clip], at_ms: i64) -> Option<Clip> {
    segments.iter().find(|c| c.end * 1000 > at_ms).copied()
}

/// How playing a recording ended.
enum Outcome {
    /// It got to the end.
    Ended,
    Seek(i64),
    /// Start again from the position (the decoding mode changed).
    Restart,
    /// Paused for a while: decoding and fetching stopped until it plays on.
    Parked,
    Failed(String),
}

async fn run(source: Source, mut sink: FrameSink, state: Arc<Mutex<PlayState>>, mut cmds: mpsc::UnboundedReceiver<Cmd>, mut at: i64) {
    loop {
        let Some(segment) = segment_at(&source.segments, at) else {
            {
                let mut s = state.lock().unwrap();
                s.ended = true;
                s.buffering = false;
                s.message = "No more recordings on this day".into();
            }
            match wait_for_seek(&mut cmds, &state).await {
                Some(t) => {
                    at = t;
                    continue;
                }
                None => return,
            }
        };
        let from = at.max(segment.start * 1000);
        {
            let mut s = state.lock().unwrap();
            s.position_ms = from;
            s.ended = false;
            s.buffering = true;
            s.message.clear();
        }
        match play_segment(&source, segment, from, &mut sink, &state, &mut cmds).await {
            Outcome::Ended => at = segment.end * 1000,
            Outcome::Seek(t) => at = t,
            Outcome::Restart => at = state.lock().unwrap().position_ms,
            Outcome::Parked => {
                log::info!("playback: paused a while; stopped decoding and fetching until it plays on");
                match wait_for_play(&mut cmds, &state).await {
                    Some(t) => at = t,
                    None => return,
                }
            }
            Outcome::Failed(message) => {
                log::warn!("playback: {message}");
                if message.contains("expired") {
                    crate::app::on_ui(|app| app.cloud_auth_failed());
                }
                {
                    let mut s = state.lock().unwrap();
                    s.buffering = false;
                    s.message = message;
                }
                match wait_for_seek(&mut cmds, &state).await {
                    Some(t) => at = t,
                    None => return,
                }
            }
        }
    }
}

/// While parked: handles commands until playing on (from where it stopped)
/// or a seek.
async fn wait_for_play(cmds: &mut mpsc::UnboundedReceiver<Cmd>, state: &Mutex<PlayState>) -> Option<i64> {
    loop {
        match cmds.recv().await? {
            Cmd::Seek(t) => return Some(t),
            Cmd::Pause(true) => state.lock().unwrap().paused = true,
            Cmd::Pause(false) => {
                let mut s = state.lock().unwrap();
                s.paused = false;
                return Some(s.position_ms);
            }
            Cmd::Speed(speed) => state.lock().unwrap().speed = speed,
        }
    }
}

/// Handles commands until a seek (or the player going away).
async fn wait_for_seek(cmds: &mut mpsc::UnboundedReceiver<Cmd>, state: &Mutex<PlayState>) -> Option<i64> {
    loop {
        match cmds.recv().await? {
            Cmd::Seek(t) => return Some(t),
            Cmd::Pause(p) => state.lock().unwrap().paused = p,
            Cmd::Speed(s) => state.lock().unwrap().speed = s,
        }
    }
}

/// Plays one recording from `from_ms` until it ends or a command says
/// otherwise.
async fn play_segment(
    source: &Source,
    segment: Clip,
    from_ms: i64,
    sink: &mut FrameSink,
    state: &Arc<Mutex<PlayState>>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
) -> Outcome {
    let keyframes = state.lock().unwrap().speed >= KEYFRAMES_FROM;
    // Start from the first picture: no waiting to size up the stream (its
    // audio can't be decoded here anyway).
    let mut input = vec!["-analyzeduration", "0", "-probesize", "131072", "-f", "mpegts", "-i", "pipe:0"];
    if keyframes {
        input.splice(0..0, ["-skip_frame", "nokey"]);
    }
    let mut decoder = match Decoder::spawn_timed(&source.ffmpeg, &input, WIDTH, HEIGHT) {
        Ok(d) => d,
        Err(e) => return Outcome::Failed(format!("{e:#}")),
    };
    let stdin = decoder.stdin.take();
    let times = decoder.times.take();

    // Fetching (and feeding ffmpeg) runs beside the frame loop.
    let anchor = Arc::new(Mutex::new(None::<i64>)); // the first keyframe's recorded time
    let fetch_error = Arc::new(Mutex::new(None::<String>));
    let fetching = tokio::spawn(fetch_and_feed(
        source.clone(),
        segment,
        from_ms,
        stdin,
        state.clone(),
        anchor.clone(),
        fetch_error.clone(),
    ));
    let _stop_fetching = AbortOnDrop(fetching);

    // Frames come off ffmpeg in a task of their own, so a command never
    // interrupts a read half-way through a frame.
    let (frames_tx, mut frames) = mpsc::channel::<(Frame, f64)>(2);
    let reading = tokio::spawn(read_frames(decoder, times, frames_tx));
    let _stop_reading = AbortOnDrop(reading);

    let mut first_time: Option<f64> = None;
    // (when, recorded ms) of the frame the pacing is measured from.
    let mut pace: Option<(Instant, i64)> = None;
    // Since when it's been paused, if it is.
    let mut paused_since: Option<Instant> = None;
    let mut last_shown = from_ms;
    let mut last_display = Instant::now();
    let (mut got, mut shown, mut report) = (0u32, 0u32, Instant::now());
    loop {
        if report.elapsed() >= Duration::from_secs(1) {
            log::debug!("playback: {got} frames in, {shown} shown, at {last_shown}, {} queued", frames.len());
            (got, shown, report) = (0, 0, Instant::now());
        }
        // The next frame, handling commands meanwhile.
        let (frame, t) = loop {
            tokio::select! {
                next = frames.recv() => match next {
                    Some(f) => break f,
                    None => {
                        let error = fetch_error.lock().unwrap().take();
                        return match error {
                            Some(e) => Outcome::Failed(e),
                            None => Outcome::Ended,
                        };
                    }
                },
                cmd = cmds.recv() => match cmd {
                    None => return Outcome::Failed(String::new()),
                    Some(Cmd::Seek(t)) => return Outcome::Seek(t),
                    Some(Cmd::Pause(p)) => {
                        state.lock().unwrap().paused = p;
                        pace = None;
                    }
                    Some(Cmd::Speed(s)) => {
                        state.lock().unwrap().speed = s;
                        if (s >= KEYFRAMES_FROM) != keyframes {
                            return Outcome::Restart;
                        }
                        pace = None;
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(1200)) => {
                    state.lock().unwrap().buffering = true;
                }
            }
        };
        got += 1;
        let t0 = *first_time.get_or_insert(t);
        let base = anchor.lock().unwrap().unwrap_or(from_ms);
        let recorded = base + ((t - t0) * 1000.0).round() as i64;
        if recorded < from_ms - 500 {
            continue; // before the moment asked for (the hub starts at a keyframe)
        }
        state.lock().unwrap().buffering = false;

        // Wait for the frame's moment, or while paused.
        loop {
            let (paused, speed) = {
                let s = state.lock().unwrap();
                (s.paused, s.speed.max(1))
            };
            let wait = if paused {
                // Paused long enough: let go of ffmpeg and the hub.
                let since = *paused_since.get_or_insert_with(Instant::now);
                Some(since + PARK_AFTER)
            } else {
                paused_since = None;
                let (since, at) = *pace.get_or_insert((Instant::now(), last_shown));
                let due = since + Duration::from_millis(((recorded - at).max(0) as f64 / speed as f64) as u64);
                if Instant::now() >= due {
                    break;
                }
                Some(due)
            };
            tokio::select! {
                _ = async {
                    match wait {
                        Some(due) => tokio::time::sleep_until(due).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if paused {
                        return Outcome::Parked;
                    }
                    break;
                }
                cmd = cmds.recv() => match cmd {
                    None => return Outcome::Failed(String::new()),
                    Some(Cmd::Seek(t)) => return Outcome::Seek(t),
                    Some(Cmd::Pause(p)) => {
                        state.lock().unwrap().paused = p;
                        pace = None;
                    }
                    Some(Cmd::Speed(s)) => {
                        state.lock().unwrap().speed = s;
                        if (s >= KEYFRAMES_FROM) != keyframes {
                            return Outcome::Restart;
                        }
                        pace = None;
                    }
                },
            }
        }
        // Behind (a slow PC at high speed): skip frames rather than fall
        // further behind, but keep the picture moving.
        let late = pace.is_some_and(|(since, at)| {
            let speed = state.lock().unwrap().speed.max(1) as f64;
            let due = since + Duration::from_millis(((recorded - at).max(0) as f64 / speed) as u64);
            Instant::now() > due + Duration::from_millis(300)
        });
        last_shown = recorded;
        state.lock().unwrap().position_ms = recorded;
        if late && last_display.elapsed() < Duration::from_millis(100) {
            continue;
        }
        last_display = Instant::now();
        shown += 1;
        sink.push(frame);
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn read_frames(
    mut decoder: Decoder,
    times: Option<mpsc::UnboundedReceiver<f64>>,
    frames: mpsc::Sender<(Frame, f64)>,
) {
    let Some(mut times) = times else { return };
    let (w, h) = (decoder.width, decoder.height);
    let mut buf = vec![0u8; w * h * 4];
    let mut last = 0.0;
    loop {
        if decoder.stdout.read_exact(&mut buf).await.is_err() {
            let error = decoder.last_error.lock().unwrap().clone();
            if !error.is_empty() {
                log::warn!("playback: ffmpeg: {error}");
            }
            return;
        }
        // showinfo reports a frame as it passes, before it's written out.
        let t = match tokio::time::timeout(Duration::from_secs(2), times.recv()).await {
            Ok(Some(t)) => t,
            _ => last + 1.0 / 15.0,
        };
        last = t;
        let frame = SharedPixelBuffer::clone_from_slice(&buf, w as u32, h as u32);
        if frames.send((frame, t)).await.is_err() {
            return;
        }
    }
}

/// A piece of a recording as the hub sent it: about a frame.
#[derive(Clone)]
struct Piece {
    pts: i64,
    key: bool,
    data: Arc<[u8]>,
}

/// One stretch of a recording fetched in one go, its pieces in order.
struct Run {
    id: u64,
    /// (camera, recording start)
    recording: (String, i64),
    pieces: VecDeque<Piece>,
    bytes: usize,
    /// Everything up to here (ms) has been fetched.
    until: i64,
    used: std::time::Instant,
}

/// Video fetched lately, so seeking back, or forward into what has already
/// arrived, needs no new download. The least recently used stretches go
/// once it holds [`CACHE_BYTES`], and the active stretch trims from its
/// front at keyframe boundaries so a long continuous recording never grows
/// unboundedly.
#[derive(Default)]
struct Cache {
    runs: Vec<Run>,
    bytes: usize,
    next_id: u64,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// How much recorded video is held in memory now, in bytes.
pub fn cached_bytes() -> usize {
    CACHE.lock().unwrap().bytes
}

/// Releases all cached recorded video (called when closing playback or
/// leaving the Playback page for a while).
pub fn clear_cache() {
    let mut cache = CACHE.lock().unwrap();
    if cache.bytes > 0 {
        log::info!("playback: cleared {} MB of cached recordings", cache.bytes >> 20);
    }
    cache.runs.clear();
    cache.bytes = 0;
}

impl Cache {
    /// The pieces to show from `at_ms` (from the keyframe at or before it),
    /// the stretch they belong to and how far it goes.
    fn find(&mut self, recording: &(String, i64), at_ms: i64) -> Option<(Vec<Piece>, u64, i64)> {
        let run = self.runs.iter_mut().find(|r| {
            &r.recording == recording && r.pieces.front().is_some_and(|p| p.pts <= at_ms) && at_ms < r.until
        })?;
        let from = run.pieces.iter().rposition(|p| p.key && p.pts <= at_ms)?;
        run.used = std::time::Instant::now();
        Some((run.pieces.range(from..).cloned().collect(), run.id, run.until))
    }

    fn start(&mut self, recording: (String, i64)) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let now = std::time::Instant::now();
        self.runs.push(Run { id, recording, pieces: VecDeque::new(), bytes: 0, until: i64::MIN, used: now });
        id
    }

    fn add(&mut self, id: u64, piece: Piece) {
        let Some(run) = self.runs.iter_mut().find(|r| r.id == id) else { return };
        run.bytes += piece.data.len();
        self.bytes += piece.data.len();
        run.until = run.until.max(piece.pts);
        run.used = std::time::Instant::now();
        run.pieces.push_back(piece);
        self.evict(id);
    }

    /// Everything up to `until` (ms) is in the stretch.
    fn complete_to(&mut self, id: u64, until: i64) {
        if let Some(run) = self.runs.iter_mut().find(|r| r.id == id && !r.pieces.is_empty()) {
            run.until = run.until.max(until);
        }
    }

    fn evict(&mut self, keep: u64) {
        while self.bytes > CACHE_BYTES {
            let Some(oldest) = self
                .runs
                .iter()
                .enumerate()
                .filter(|(_, r)| r.id != keep)
                .min_by_key(|(_, r)| r.used)
                .map(|(i, _)| i)
            else {
                // Only the active stretch remains: trim its oldest pieces from
                // the front so a long continuous recording stays within budget.
                let Some(run) = self.runs.iter_mut().find(|r| r.id == keep) else { return };
                while self.bytes > CACHE_BYTES && run.pieces.len() > 1 {
                    let Some(old) = run.pieces.pop_front() else { break };
                    run.bytes -= old.data.len();
                    self.bytes -= old.data.len();
                }
                // Re-align the front to a keyframe so future seeks from this
                // stretch can always start cleanly.
                while run.pieces.len() > 1 && run.pieces.front().is_some_and(|p| !p.key) {
                    let Some(old) = run.pieces.pop_front() else { break };
                    run.bytes -= old.data.len();
                    self.bytes -= old.data.len();
                }
                return;
            };
            let run = self.runs.remove(oldest);
            self.bytes -= run.bytes;
        }
    }
}

/// The video of one recording from `from_ms` to `to_ms`, if it's all in
/// memory: from the keyframe at or before `from_ms`.
pub fn cached_video(camera_id: &str, recording_start: i64, from_ms: i64, to_ms: i64) -> Option<Vec<Arc<[u8]>>> {
    let mut cache = CACHE.lock().unwrap();
    let recording = (camera_id.to_string(), recording_start);
    let run = cache.runs.iter_mut().find(|r| {
        r.recording == recording && r.pieces.front().is_some_and(|p| p.pts <= from_ms) && r.until >= to_ms
    })?;
    let first = run.pieces.iter().rposition(|p| p.key && p.pts <= from_ms)?;
    run.used = std::time::Instant::now();
    Some(run.pieces.range(first..).take_while(|p| p.pts <= to_ms).map(|p| p.data.clone()).collect())
}

/// Feeds ffmpeg the recording from `from_ms` to its end: what's in memory
/// first, then the rest fetched window by window, a little ahead of the
/// picture.
async fn fetch_and_feed(
    source: Source,
    segment: Clip,
    from_ms: i64,
    stdin: Option<tokio::process::ChildStdin>,
    state: Arc<Mutex<PlayState>>,
    anchor: Arc<Mutex<Option<i64>>>,
    error: Arc<Mutex<Option<String>>>,
) {
    let Some(mut stdin) = stdin else { return };
    let (tx, mut rx) = mpsc::unbounded_channel::<Arc<[u8]>>();
    let feeding = async move {
        while let Some(piece) = rx.recv().await {
            if stdin.write_all(&piece).await.is_err() {
                return;
            }
        }
        // Closing stdin lets ffmpeg finish the recording.
    };
    let fetching = async move {
        let recording = (source.camera.device_id.clone(), segment.start);
        let mut start = from_ms / 1000;
        // The newest piece passed on (i64::MIN: none yet). A window starts at
        // a keyframe before the moment asked for, so at its start, pieces
        // already passed on are skipped.
        let newest = AtomicI64::new(i64::MIN);
        let overlap = AtomicBool::new(false);
        let cached = CACHE.lock().unwrap().find(&recording, from_ms);
        let run = match cached {
            Some((pieces, id, until)) => {
                log::debug!("playback: {:.0}s from memory", (until - pieces[0].pts) as f64 / 1000.0);
                *anchor.lock().unwrap() = Some(pieces[0].pts);
                for piece in &pieces {
                    newest.fetch_max(piece.pts, Ordering::Relaxed);
                    let _ = tx.send(piece.data.clone());
                }
                start = start.max(until / 1000);
                id
            }
            None => CACHE.lock().unwrap().start(recording),
        };
        while start < segment.end {
            let end = (start + WINDOW).min(segment.end);
            let request = ClipRequest {
                hub: source.hub.clone(),
                camera: source.camera.clone(),
                clip: Clip { start, end, kind: segment.kind },
                player_id: source.player_id.clone(),
            };
            let mut media = |m: Media<'_>| -> Result<()> {
                let data: Arc<[u8]> = Arc::from(m.data);
                if let Some(pts) = m.pts_ms {
                    let seen = newest.load(Ordering::Relaxed);
                    if overlap.load(Ordering::Relaxed) && pts <= seen {
                        return Ok(());
                    }
                    overlap.store(false, Ordering::Relaxed);
                    newest.store(seen.max(pts), Ordering::Relaxed);
                    if m.key {
                        let mut a = anchor.lock().unwrap();
                        if a.is_none() {
                            *a = Some(pts);
                        }
                    }
                    CACHE.lock().unwrap().add(run, Piece { pts, key: m.key, data: data.clone() });
                }
                let _ = tx.send(data);
                Ok(())
            };
            let mut progress = |_| {};
            overlap.store(newest.load(Ordering::Relaxed) != i64::MIN, Ordering::Relaxed);
            let mut result = clip::download(&source.client, &request, &mut media, &mut progress).await;
            if let Err(e) = &result
                && !e.downcast_ref::<CloudError>().is_some_and(CloudError::is_auth)
            {
                log::warn!("playback: fetching {start}..{end} failed ({e:#}); trying again");
                tokio::time::sleep(Duration::from_secs(1)).await;
                overlap.store(newest.load(Ordering::Relaxed) != i64::MIN, Ordering::Relaxed);
                result = clip::download(&source.client, &request, &mut media, &mut progress).await;
            }
            if let Err(e) = result {
                *error.lock().unwrap() = Some(format!("{e:#}"));
                return;
            }
            CACHE.lock().unwrap().complete_to(run, end * 1000);
            start = end;
            // Wait until the picture gets close to the end of what's fetched.
            loop {
                let (position, speed) = {
                    let s = state.lock().unwrap();
                    (s.position_ms, s.speed.max(1) as i64)
                };
                if start >= segment.end || position + AHEAD * 1000 * speed >= start * 1000 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        drop(tx);
    };
    tokio::join!(feeding, fetching);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_recording_to_play() {
        let segments = [Clip { start: 100, end: 200, kind: 1 }, Clip { start: 300, end: 400, kind: 1 }];
        assert_eq!(segment_at(&segments, 50_000).unwrap().start, 100);
        assert_eq!(segment_at(&segments, 150_000).unwrap().start, 100);
        // In a gap: the next one.
        assert_eq!(segment_at(&segments, 250_000).unwrap().start, 300);
        assert!(segment_at(&segments, 400_000).is_none());
    }

    fn piece(pts: i64, key: bool, size: usize) -> Piece {
        Piece { pts, key, data: Arc::from(vec![0u8; size]) }
    }

    #[test]
    fn cache_starts_at_the_keyframe_and_forgets_the_oldest() {
        let mut cache = Cache::default();
        let recording = ("cam".to_string(), 100);
        let run = cache.start(recording.clone());
        for (i, key) in [true, false, false, true, false].into_iter().enumerate() {
            cache.add(run, piece(1_000 + i as i64 * 500, key, 10));
        }
        cache.complete_to(run, 4_000);
        // From the keyframe at or before the moment, up to what's fetched.
        let (pieces, id, until) = cache.find(&recording, 2_700).unwrap();
        assert_eq!((pieces.len(), pieces[0].pts, id, until), (2, 2_500, run, 4_000));
        assert!(cache.find(&recording, 900).is_none());
        assert!(cache.find(&recording, 4_000).is_none());
        assert!(cache.find(&("other".into(), 100), 2_700).is_none());

        // Over the limit, the least recently used stretch goes, not the one
        // being filled.
        let big = cache.start(("cam".into(), 200));
        cache.add(big, piece(9_000, true, CACHE_BYTES));
        assert!(cache.find(&recording, 2_700).is_none());
        assert_eq!(cache.runs.len(), 1);

        // Even a single active stretch trims from its front at a keyframe
        // boundary instead of growing past CACHE_BYTES.
        let half = CACHE_BYTES / 2;
        let mut active = Cache::default();
        let long = active.start(("cam".into(), 300));
        active.add(long, piece(1_000, true, half));
        active.add(long, piece(1_500, false, 1024));
        active.add(long, piece(2_000, true, half - 1024));
        active.add(long, piece(2_500, false, 4096));
        active.complete_to(long, 3_000);
        assert!(active.bytes <= CACHE_BYTES, "active run grew to {}", active.bytes);
        assert!(active.find(&("cam".into(), 300), 1_200).is_none());
        let (pieces, _, _) = active.find(&("cam".into(), 300), 2_200).unwrap();
        assert!(pieces[0].key && pieces[0].pts == 2_000);
    }
}
