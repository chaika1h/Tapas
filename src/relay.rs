//! Reads a camera's live video, or a hub's recording, from the Tapo cloud
//! relay.
//!
//! The relay is an HTTP/1.1 POST that never finishes: the request announces an
//! enormous body, and the relay answers with a `multipart/mixed` stream whose
//! parts carry MPEG-TS. Port of `openVideoStream` and `parseRelayResponse`
//! from Topo TV's `lib/api/tapo_cloud.dart` (MIT licence).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::cloud::{APP_NAME, APP_VERSION, RelayInfo};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// The relay can take several seconds between its headers and the first
/// video, and a camera that goes quiet for longer has stopped.
const DATA_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_LINE: u64 = 64 * 1024;
/// The official client matches the boundary by containment; the wire form
/// normally has extra leading hyphens.
const BOUNDARY: &str = "--device-stream-boundary--";

pub type RelayReader = BufReader<TlsStream<TcpStream>>;

/// The request the official client sends for live video. `model` is the
/// camera's model (e.g. `C200`), which the relay expects in `X-Client-Model`.
pub fn request_head(info: &RelayInfo, model: &str) -> String {
    let model = if model.is_empty() { "C200" } else { model };
    head(info, model, &[("X-Arrive-Latency", "0")])
}

/// The request for a recording, as Topo's playback client (and the Tapo
/// app's playback path) sends it.
pub fn recording_head(info: &RelayInfo) -> String {
    head(info, "OnTapo", &[("X-Compete", "1"), ("X-Arrive-Latency", "200")])
}

fn head(info: &RelayInfo, model: &str, extra: &[(&str, &str)]) -> String {
    let mut headers = vec![
        ("Host", info.host.clone()),
        ("User-Agent", format!("Client={APP_NAME} Android {APP_VERSION}/1.3")),
        ("Keep-Relay", "3600".into()),
        ("Accept", "*/*".into()),
        ("Content-Type", "multipart/mixed;boundary=--client-stream-boundary--".into()),
        ("Content-Length", i64::MAX.to_string()),
        ("X-token", info.token.clone()),
        ("X-Client-Model", model.to_string()),
        ("X-Client-UUID", info.player_id.clone()),
        ("X-Redirect-Times", "0".into()),
        // PullMode.DIRECT_FORWARDING in the official client.
        ("X-Pull-Mode", "2".into()),
        ("X-Version", "2.0".into()),
    ];
    headers.extend(extra.iter().map(|(n, v)| (*n, v.to_string())));
    headers.push(("Connection", "Keep-alive".into()));
    if let Some(cookie) = &info.elb_cookie {
        headers.push(("Cookie", cookie.clone()));
    }
    let mut head = format!("POST {} HTTP/1.1\r\n", info.path);
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    head
}

/// Connects to the relay, sends the request and checks its answer. Read the
/// video with [`copy_video`].
pub async fn open(tls: Arc<rustls::ClientConfig>, info: &RelayInfo, model: &str) -> Result<RelayReader> {
    let mut stream = connect(tls, info).await?;
    stream.write_all(request_head(info, model).as_bytes()).await.context("Could not talk to the video relay")?;
    stream.flush().await?;
    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    read_response_head(&mut reader).await?;
    Ok(reader)
}

/// A TLS connection to the relay, before the request.
pub async fn connect(tls: Arc<rustls::ClientConfig>, info: &RelayInfo) -> Result<TlsStream<TcpStream>> {
    let tcp = connect_tcp(&info.host, info.port).await?;
    let _ = tcp.set_nodelay(true);
    let name = rustls::pki_types::ServerName::try_from(info.host.clone()).context("Invalid video relay address")?;
    timeout(CONNECT_TIMEOUT, TlsConnector::from(tls).connect(name, tcp))
        .await
        .map_err(|_| anyhow!("The video relay did not answer"))?
        .context("Secure connection to the video relay failed")
}

/// Resolves and connects TCP with a 5-minute DNS cache (IPv4 preferred first)
/// so HD switches and reconnects to the same regional relay skip repeated DNS
/// lookups and slow IPv6 fallbacks.
async fn connect_tcp(host: &str, port: u16) -> Result<TcpStream> {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    const DNS_TTL: Duration = Duration::from_secs(300);
    static DNS_CACHE: OnceLock<Mutex<HashMap<(String, u16), (Instant, Vec<SocketAddr>)>>> = OnceLock::new();
    let cache = DNS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (host.to_string(), port);

    let cached = {
        let guard = cache.lock().unwrap();
        guard.get(&key).filter(|(at, _)| at.elapsed() < DNS_TTL).map(|(_, addrs)| addrs.clone())
    };
    if let Some(addrs) = cached {
        if let Ok(Ok(stream)) = timeout(CONNECT_TIMEOUT, TcpStream::connect(addrs.as_slice())).await {
            return Ok(stream);
        }
        cache.lock().unwrap().remove(&key);
    }

    let mut addrs: Vec<SocketAddr> = timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| anyhow!("The video relay did not answer"))?
        .context("Could not reach the video relay")?
        .collect();
    if addrs.is_empty() {
        bail!("Could not reach the video relay");
    }
    // Prefer IPv4 addresses first so dual-stack relays don't stall on broken IPv6 routes.
    addrs.sort_by_key(|a| a.is_ipv6());
    let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(addrs.as_slice()))
        .await
        .map_err(|_| anyhow!("The video relay did not answer"))?
        .context("Could not reach the video relay")?;
    cache.lock().unwrap().insert(key, (Instant::now(), addrs));
    Ok(stream)
}

/// One part of the relay's answer.
#[derive(Debug, Default)]
pub struct Part {
    /// Lower-case names.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Part {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    pub fn is_json(&self) -> bool {
        self.header("content-type").is_some_and(|t| t.contains("json"))
    }
}

/// Reads the next part, waiting up to `idle` for it to start.
pub async fn read_part<R: AsyncBufRead + Unpin>(reader: &mut R, idle: Duration) -> Result<Part> {
    const MAX_PART: usize = 32 << 20;
    loop {
        if read_line_within(reader, idle).await?.contains(BOUNDARY) {
            break;
        }
    }
    let mut part = Part::default();
    loop {
        let header = read_line(reader).await?;
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            part.headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let length: usize = part.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if length > MAX_PART {
        bail!("Malformed data from the video relay (a part of {length} bytes)");
    }
    part.body = vec![0; length];
    timeout(DATA_TIMEOUT, reader.read_exact(&mut part.body))
        .await
        .map_err(|_| anyhow!("The video relay stopped in the middle of a part"))?
        .context("The video relay closed the connection")?;
    Ok(part)
}

/// [`read_part`] found nothing within its wait.
#[derive(Debug)]
pub struct IdleTimeout;

impl std::fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The relay sent nothing for a while")
    }
}

impl std::error::Error for IdleTimeout {}

/// A part of the client's half of the conversation (the request body).
pub fn client_part(headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut part = Vec::with_capacity(body.len() + 160);
    part.extend_from_slice(b"----client-stream-boundary--\r\n");
    for (name, value) in headers {
        part.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    part.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    part.extend_from_slice(body);
    part.extend_from_slice(b"\r\n");
    part
}

/// Reads the relay's HTTP status and headers; anything but 200 is an error.
pub async fn read_response_head<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<()> {
    let status = read_line(reader).await?;
    while !read_line(reader).await?.is_empty() {}
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if status.starts_with("HTTP/1.") && matches!(code, "500" | "503") {
        return Err(anyhow!(RelayBusy(status.trim().to_string())));
    }
    if !status.starts_with("HTTP/1.") || code != "200" {
        bail!("The video relay refused the connection ({})", status.trim());
    }
    Ok(())
}

/// The relay turned the connection away with a server error, as it does
/// while the device is still busy with another session.
#[derive(Debug)]
pub struct RelayBusy(pub String);

impl std::fmt::Display for RelayBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "The video relay refused the connection ({}); the device may still be busy", self.0)
    }
}

impl std::error::Error for RelayBusy {}

/// Optional live MPEG-TS recorder shared with [`copy_video_with_audio_and_recorder`].
/// When `Some(writer)`, exact MPEG-TS bytes from the relay are teed to disk with
/// zero re-encoding overhead.
pub type LiveRecorder = Arc<std::sync::Mutex<Option<std::io::BufWriter<std::fs::File>>>>;

/// Copies the MPEG-TS carried in the multipart body to `out` until the relay
/// stops, which is always an error for a live stream.
#[cfg(test)]
pub async fn copy_video<R, W>(reader: &mut R, out: &mut W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    copy_video_with_audio_and_recorder(reader, out, 0, None).await
}

/// Like [`copy_video`], also demuxing and playing embedded MPEG-TS audio when
/// `audio_id` is currently the active audio source, and optionally teeing the
/// raw MPEG-TS stream into `recorder` for live MP4 recording (zero overhead
/// while muted and not recording).
#[cfg(test)]
pub async fn copy_video_with_audio_and_recorder<R, W>(
    reader: &mut R,
    out: &mut W,
    audio_id: u64,
    recorder: Option<&LiveRecorder>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use std::io::Write;
    let mut demux = crate::audio::TsAudioDemuxer::new();
    let mut pcm_chunks = Vec::new();
    let mut was_audible = false;
    let mut buf = [0u8; 8192];

    loop {
        if !read_line(reader).await?.contains(BOUNDARY) {
            continue; // the blank line after a part, or anything unexpected
        }
        let mut length = 0u64;
        let mut encrypted = false;
        loop {
            let header = read_line(reader).await?;
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "x-if-encrypt" => encrypted = value.trim() == "1",
                _ => {}
            }
        }
        if encrypted {
            bail!("This camera sends encrypted video through the cloud, which Tapas can't play yet");
        }
        if length == 0 {
            continue;
        }
        let audible = crate::audio::is_active(audio_id);
        let recording = recorder.is_some_and(|r| r.lock().unwrap().is_some());
        if audible && !was_audible {
            demux.reset();
        }
        was_audible = audible;

        let mut part = (&mut *reader).take(length);
        if !audible && !recording {
            let copied = timeout(DATA_TIMEOUT, tokio::io::copy_buf(&mut part, out))
                .await
                .map_err(|_| anyhow!("The camera stopped sending video"))?
                .context("Video decoder stopped")?;
            if copied < length {
                bail!("The video relay closed the connection");
            }
        } else {
            let mut copied = 0u64;
            while copied < length {
                let n = timeout(DATA_TIMEOUT, part.read(&mut buf))
                    .await
                    .map_err(|_| anyhow!("The camera stopped sending video"))?
                    .context("Lost the connection to the video relay")?;
                if n == 0 {
                    bail!("The video relay closed the connection");
                }
                copied += n as u64;
                timeout(DATA_TIMEOUT, out.write_all(&buf[..n]))
                    .await
                    .map_err(|_| anyhow!("The camera stopped sending video"))?
                    .context("Video decoder stopped")?;
                if let Some(rec) = recorder {
                    let mut guard = rec.lock().unwrap();
                    if let Some(writer) = guard.as_mut()
                        && let Err(e) = writer.write_all(&buf[..n])
                    {
                        log::warn!("live recording write failed: {e}");
                        *guard = None;
                    }
                }
                if crate::audio::is_active(audio_id) {
                    pcm_chunks.clear();
                    demux.feed(&buf[..n], &mut pcm_chunks);
                    for chunk in pcm_chunks.drain(..) {
                        crate::audio::push_pcm(audio_id, chunk.sample_rate, &chunk.samples);
                    }
                }
            }
        }
    }
}

/// Streams raw MPEG-TS chunks from the relay multipart body into `tx`, while
/// demuxing live audio (when `audio_id` is active) and teeing bytes into
/// `recorder` (when recording). Used by the in-process H.264 decoder path.
pub async fn stream_video_chunks<R>(
    reader: &mut R,
    tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    audio_id: u64,
    recorder: Option<&LiveRecorder>,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    use std::io::Write;
    let mut demux = crate::audio::TsAudioDemuxer::new();
    let mut pcm_chunks = Vec::new();
    let mut was_audible = false;
    let mut buf = vec![0u8; 16 * 1024];

    loop {
        if !read_line(reader).await?.contains(BOUNDARY) {
            continue;
        }
        let mut length = 0u64;
        let mut encrypted = false;
        loop {
            let header = read_line(reader).await?;
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else { continue };
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "x-if-encrypt" => encrypted = value.trim() == "1",
                _ => {}
            }
        }
        if encrypted {
            bail!("This camera sends encrypted video through the cloud, which Tapas can't play yet");
        }
        if length == 0 {
            continue;
        }
        let audible = crate::audio::is_active(audio_id);
        if audible && !was_audible {
            demux.reset();
        }
        was_audible = audible;

        let mut part = (&mut *reader).take(length);
        let mut copied = 0u64;
        while copied < length {
            let n = timeout(DATA_TIMEOUT, part.read(&mut buf))
                .await
                .map_err(|_| anyhow!("The camera stopped sending video"))?
                .context("Lost the connection to the video relay")?;
            if n == 0 {
                bail!("The video relay closed the connection");
            }
            copied += n as u64;
            let slice = &buf[..n];
            if let Some(rec) = recorder {
                let mut guard = rec.lock().unwrap();
                if let Some(writer) = guard.as_mut()
                    && let Err(e) = writer.write_all(slice)
                {
                    log::warn!("live recording write failed: {e}");
                    *guard = None;
                }
            }
            if crate::audio::is_active(audio_id) {
                pcm_chunks.clear();
                demux.feed(slice, &mut pcm_chunks);
                for chunk in pcm_chunks.drain(..) {
                    crate::audio::push_pcm(audio_id, chunk.sample_rate, &chunk.samples);
                }
            }
            if tx.send(slice.to_vec()).await.is_err() {
                bail!("Video decoder stopped");
            }
        }
    }
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<String> {
    read_line_within(reader, DATA_TIMEOUT)
        .await
        .map_err(|e| if e.is::<IdleTimeout>() { anyhow!("The camera stopped sending video") } else { e })
}

/// A line, or [`IdleTimeout`] if none arrives within `wait`.
async fn read_line_within<R: AsyncBufRead + Unpin>(reader: &mut R, wait: Duration) -> Result<String> {
    let mut line = Vec::new();
    let n = timeout(wait, (&mut *reader).take(MAX_LINE).read_until(b'\n', &mut line))
        .await
        .map_err(|_| anyhow!(IdleTimeout))?
        .context("Lost the connection to the video relay")?;
    if n == 0 {
        bail!("The video relay closed the connection");
    }
    if line.last() != Some(&b'\n') {
        bail!("Malformed data from the video relay");
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    // Header bytes are Latin-1 by HTTP convention.
    Ok(line.into_iter().map(char::from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(body: &[u8], extra: &str) -> Vec<u8> {
        let mut p = format!(
            "----device-stream-boundary--\r\nContent-Type: video/mp2t\r\nContent-Length: {}\r\n{extra}\r\n",
            body.len()
        )
        .into_bytes();
        p.extend_from_slice(body);
        p.extend_from_slice(b"\r\n");
        p
    }

    async fn run(input: Vec<u8>) -> (Result<()>, Vec<u8>) {
        let mut reader = BufReader::new(&input[..]);
        let mut out = Vec::new();
        let head = read_response_head(&mut reader).await;
        if head.is_err() {
            return (head, out);
        }
        let result = copy_video(&mut reader, &mut out).await;
        (result, out)
    }

    const OK_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: multipart/mixed;boundary=--device-stream-boundary--\r\n\r\n";

    #[tokio::test]
    async fn joins_part_bodies_until_the_relay_closes() {
        let mut input = OK_HEAD.to_vec();
        input.extend(part(b"\x47first", ""));
        input.extend(part(b"", "")); // empty parts are skipped
        // Binary bodies may contain CR/LF and even the boundary text.
        input.extend(part(b"\x47a\r\n----device-stream-boundary--\r\nb", ""));
        let (result, out) = run(input).await;
        assert_eq!(out, b"\x47first\x47a\r\n----device-stream-boundary--\r\nb");
        assert!(result.unwrap_err().to_string().contains("closed"));
    }

    #[tokio::test]
    async fn refuses_non_200() {
        let (result, _) = run(b"HTTP/1.1 403 Forbidden\r\n\r\n".to_vec()).await;
        assert!(result.unwrap_err().to_string().contains("403"));
    }

    #[tokio::test]
    async fn reports_encrypted_video() {
        let mut input = OK_HEAD.to_vec();
        input.extend(part(b"\x47x", "X-If-Encrypt: 1\r\n"));
        let (result, out) = run(input).await;
        assert!(out.is_empty());
        assert!(result.unwrap_err().to_string().contains("encrypted"));
    }

    #[tokio::test]
    async fn truncated_part_is_an_error() {
        let mut input = OK_HEAD.to_vec();
        input.extend(b"----device-stream-boundary--\r\nContent-Length: 10\r\n\r\nabc");
        let (result, out) = run(input).await;
        assert_eq!(out, b"abc");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reads_parts_with_their_headers() {
        let mut input = b"----device-stream-boundary--\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}\r\n".to_vec();
        input.extend(part(b"\x47ts", "X-Data-Sequence: 7\r\n"));
        let mut reader = BufReader::new(&input[..]);
        let json = read_part(&mut reader, Duration::from_secs(1)).await.unwrap();
        assert!(json.is_json());
        assert_eq!(json.body, b"{}");
        let media = read_part(&mut reader, Duration::from_secs(1)).await.unwrap();
        assert!(!media.is_json());
        assert_eq!(media.header("x-data-sequence"), Some("7"));
        assert_eq!(media.body, b"\x47ts");
        assert!(read_part(&mut reader, Duration::from_secs(1)).await.is_err());
    }

    #[tokio::test]
    async fn idle_relay_is_reported_as_such() {
        let (_tx, rx) = tokio::io::duplex(64);
        let mut reader = BufReader::new(rx);
        let e = read_part(&mut reader, Duration::from_millis(50)).await.unwrap_err();
        assert!(e.is::<IdleTimeout>());
    }

    #[test]
    fn client_parts_match_the_app() {
        let p = client_part(&[("Content-Type", "application/json"), ("X-Data-Window-Size", "50")], b"{}");
        assert_eq!(
            p,
            b"----client-stream-boundary--\r\nContent-Type: application/json\r\nX-Data-Window-Size: 50\r\nContent-Length: 2\r\n\r\n{}\r\n"
        );
    }

    #[test]
    fn recording_request_presents_as_playback_client() {
        let info = RelayInfo {
            host: "relay.example".into(),
            port: 443,
            path: "/relayservice?x=1".into(),
            token: "tok".into(),
            player_id: "pid".into(),
            elb_cookie: None,
        };
        let head = recording_head(&info);
        for line in ["X-Client-Model: OnTapo", "X-Compete: 1", "X-Arrive-Latency: 200", "X-Client-UUID: pid"] {
            assert!(head.contains(&format!("{line}\r\n")), "missing {line}");
        }
        assert!(!head.contains("Cookie"));
    }

    #[test]
    fn request_matches_official_client() {
        let info = RelayInfo {
            host: "relay.example".into(),
            port: 443,
            path: "/relayservice?x=1&retryTime=0".into(),
            token: "tok".into(),
            player_id: "pid".into(),
            elb_cookie: Some("AWSELB=1".into()),
        };
        let head = request_head(&info, "");
        assert!(head.starts_with("POST /relayservice?x=1&retryTime=0 HTTP/1.1\r\nHost: relay.example\r\n"));
        for line in ["X-token: tok", "X-Client-Model: C200", "X-Client-UUID: pid", "X-Pull-Mode: 2", "Cookie: AWSELB=1"] {
            assert!(head.contains(&format!("{line}\r\n")), "missing {line}");
        }
        assert!(head.contains(&format!("Content-Length: {}\r\n", i64::MAX)));
        assert!(head.ends_with("\r\n\r\n"));
    }

    #[tokio::test]
    async fn live_recorder_tees_mpegts_bytes() {
        use std::io::Write;
        let path = std::env::temp_dir().join(format!("tapas-live-rec-{}.ts", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let recorder: LiveRecorder = Arc::new(std::sync::Mutex::new(Some(std::io::BufWriter::new(file))));

        let mut input = OK_HEAD.to_vec();
        input.extend(part(b"\x47chunk1", ""));
        input.extend(part(b"\x47chunk2", ""));
        let mut reader = BufReader::new(&input[..]);
        read_response_head(&mut reader).await.unwrap();
        let mut out = Vec::new();
        let _ = copy_video_with_audio_and_recorder(&mut reader, &mut out, 0, Some(&recorder)).await;
        if let Some(mut w) = recorder.lock().unwrap().take() {
            w.flush().unwrap();
        }
        let recorded = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, b"\x47chunk1\x47chunk2");
        assert_eq!(recorded, out);
    }
}
