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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::cloud::{Clip, CloudCamera, CloudClient, HubCamera};
use crate::relay::{self, IdleTimeout, Part};

/// The Tapo app's window on the download request.
const DATA_WINDOW_SIZE: &str = "50";
/// The Tapo app acknowledges every 25th media part.
const ACK_EVERY: u64 = 25;
const HEARTBEAT: Duration = Duration::from_secs(15);
/// How long the hub may stay quiet before the download is given up.
const IDLE: Duration = Duration::from_secs(30);

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

/// Downloads the clip, handing each piece of MPEG-TS to `media` as it
/// arrives, and `progress` after each piece.
pub async fn download(
    client: &CloudClient,
    request: &ClipRequest,
    media: &mut (dyn FnMut(&[u8]) -> Result<()> + Send),
    progress: &mut (dyn FnMut(Progress) + Send),
) -> Result<Progress> {
    // The app's download is sub-type "cover"; its download of a playback clip
    // is "video". Try the other if the cloud refuses the first.
    let info = match client.request_clip_relay(&request.hub, &request.camera, &request.player_id, "cover").await {
        Err(e) if e.is_refused() => {
            log::warn!("clip relay (cover) refused: {e}; trying the playback download");
            client.request_clip_relay(&request.hub, &request.camera, &request.player_id, "video").await?
        }
        result => result?,
    };
    let mut stream = relay::connect(client.tls(), &info).await?;
    let mut first = relay::recording_head(&info).into_bytes();
    first.extend(relay::client_part(
        &[("Content-Type", "application/json"), ("X-Data-Window-Size", DATA_WINDOW_SIZE)],
        download_request(1, request).to_string().as_bytes(),
    ));
    stream.write_all(&first).await.context("Could not talk to the video relay")?;
    stream.flush().await?;

    let (read_half, write_half) = tokio::io::split(stream);
    let mut reader = BufReader::with_capacity(64 * 1024, read_half);
    let session = Arc::new(Mutex::new(None::<String>));
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(write_parts(write_half, rx));
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
            let part = match relay::read_part(&mut reader, IDLE).await {
                Ok(part) => part,
                Err(e) if e.is::<IdleTimeout>() && state.got.bytes > 0 && state.got.seconds + 2.0 >= expected => {
                    // Some firmware sends no end message.
                    log::info!("clip: the hub went quiet after {:.1}s of video; taking that as the end", state.got.seconds);
                    return Ok(());
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

    // Release the hub's download slot either way.
    let got = state.got;
    drop(state);
    drop(guard);
    let _ = tokio::time::timeout(Duration::from_secs(3), writer).await;
    result?;
    if got.bytes == 0 {
        bail!("The hub finished without sending any video");
    }
    log::info!("clip: {} bytes, {:.1}s of video (clip is {expected}s)", got.bytes, got.seconds);
    Ok(got)
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
    fn handle(&mut self, part: Part, media: &mut (dyn FnMut(&[u8]) -> Result<()> + Send)) -> Result<bool> {
        if part.is_json() {
            return self.control(&part);
        }
        if part.body.is_empty() {
            return Ok(false);
        }
        if part.header("x-if-encrypt") == Some("1") || part.header("x-data-hmac").is_some() {
            log::warn!("clip: encrypted media part, headers {:?}", part.headers);
            bail!("The hub sent the recording encrypted, which Tapo Viewer can't read through the cloud yet");
        }
        if self.got.bytes == 0 {
            log::info!("clip: first media part, headers {:?}", part.headers);
        }
        media(&part.body).context("Could not save the recording")?;
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
        let mut media = move |b: &[u8]| -> Result<()> {
            sink.lock().unwrap().extend_from_slice(b);
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
        let mut media = |_: &[u8]| -> Result<()> { Ok(()) };
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
}
