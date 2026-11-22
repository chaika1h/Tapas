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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
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
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((info.host.as_str(), info.port)))
        .await
        .map_err(|_| anyhow!("The video relay did not answer"))?
        .context("Could not reach the video relay")?;
    let _ = tcp.set_nodelay(true);
    let name = rustls::pki_types::ServerName::try_from(info.host.clone()).context("Invalid video relay address")?;
    timeout(CONNECT_TIMEOUT, TlsConnector::from(tls).connect(name, tcp))
        .await
        .map_err(|_| anyhow!("The video relay did not answer"))?
        .context("Secure connection to the video relay failed")
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
    if !status.starts_with("HTTP/1.") || code != "200" {
        bail!("The video relay refused the connection ({})", status.trim());
    }
    Ok(())
}

/// Copies the MPEG-TS carried in the multipart body to `out` until the relay
/// stops, which is always an error for a live stream.
pub async fn copy_video<R, W>(reader: &mut R, out: &mut W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
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
            bail!("This camera sends encrypted video through the cloud, which Tapo Viewer can't play yet");
        }
        if length == 0 {
            continue;
        }
        let mut part = (&mut *reader).take(length);
        let copied = timeout(DATA_TIMEOUT, tokio::io::copy_buf(&mut part, out))
            .await
            .map_err(|_| anyhow!("The camera stopped sending video"))?
            .context("Video decoder stopped")?;
        if copied < length {
            bail!("The video relay closed the connection");
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
}
