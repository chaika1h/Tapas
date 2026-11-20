//! Continuous reader for a Tapo camera's MJPEG RTSP stream (`stream8`).
//!
//! Tapo cameras don't follow RFC 2435 here: each RTP/JPEG packet carries a
//! slice of an already-complete JFIF file rather than the bare scan data, so a
//! standard depacketizer produces corrupt images. We therefore read raw RTP
//! packets and concatenate their bodies. This approach comes from the `tapo`
//! crate's snapshot helper (MIT licensed), extended to stream continuously.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use retina::codec::CodecItem;
use retina::client::{
    Credentials, PacketItem, PlayOptions, Session, SessionOptions, SetupOptions,
    TcpTransportOptions, Transport,
};
use slint::SharedPixelBuffer;
use tokio::time::timeout;

use crate::stream::{FrameSink, StreamSpec, rtsp_url};

const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// Give up on this mode after this many unusable frames in a row.
const MAX_BAD_FRAMES: u32 = 30;

pub async fn run(spec: &StreamSpec, sink: &mut FrameSink) -> Result<()> {
    read_tapo(connect(spec).await?, sink).await?;
    // Only reached if the camera sends standard RFC 2435 RTP/JPEG: let retina
    // rebuild the JPEG headers. Switching to demuxing needs a fresh session.
    log::info!("{}: standard RTP/JPEG stream, switching decoder", spec.host);
    read_standard(connect(spec).await?, sink).await
}

async fn connect(spec: &StreamSpec) -> Result<Session<retina::client::Playing>> {
    let url = rtsp_url(spec, "stream8", false)?;
    let opts = SessionOptions::default()
        .creds(Some(Credentials { username: spec.username.clone(), password: spec.password.clone() }))
        .user_agent("tapo-viewer".to_owned());

    let mut session = timeout(IO_TIMEOUT, Session::describe(url, opts))
        .await
        .map_err(|_| anyhow!("Camera did not answer"))?
        .map_err(|e| rtsp_error(e, spec))?;

    let video = session
        .streams()
        .iter()
        .position(|s| matches!(s.media(), "video" | "image") && s.encoding_name() == "jpeg")
        .context("Camera has no built-in MJPEG stream; switch it to SD or HD (ffmpeg)")?;

    timeout(
        IO_TIMEOUT,
        session.setup(video, SetupOptions::default().transport(Transport::Tcp(TcpTransportOptions::default()))),
    )
    .await
    .map_err(|_| anyhow!("Camera did not answer (setup)"))?
    .map_err(|e| rtsp_error(e, spec))?;

    timeout(IO_TIMEOUT, session.play(PlayOptions::default()))
        .await
        .map_err(|_| anyhow!("Camera did not answer (play)"))?
        .map_err(|e| rtsp_error(e, spec))
}

/// Reads Tapo-style packets (complete JFIF files split across RTP packets).
/// Runs until an error, or returns `Ok(())` if the first frame turns out to
/// be standard RFC 2435 data instead.
async fn read_tapo(mut session: Session<retina::client::Playing>, sink: &mut FrameSink) -> Result<()> {
    let mut frame: Vec<u8> = Vec::with_capacity(128 * 1024);
    let mut have_start = false;
    let mut checked_format = false;
    let mut bad_frames = 0u32;

    loop {
        let item = match timeout(IO_TIMEOUT, session.next()).await {
            Err(_) => bail!("No video for {}s", IO_TIMEOUT.as_secs()),
            Ok(None) => bail!("Camera closed the stream"),
            Ok(Some(item)) => item.map_err(|e| anyhow!(e.to_string()))?,
        };
        let PacketItem::Rtp(pkt) = item else { continue };
        let mark = pkt.mark();
        let payload = pkt.payload();

        let Some((frag_offset, body)) = jpeg_body(payload) else { continue };
        if frag_offset == 0 {
            if !checked_format {
                if !body.starts_with(&[0xff, 0xd8]) {
                    return Ok(());
                }
                checked_format = true;
            }
            frame.clear();
            have_start = true;
        } else if !have_start {
            continue; // joined mid-frame
        }
        frame.extend_from_slice(body);

        if mark && have_start {
            have_start = false;
            let n = frame.len();
            let complete = n >= 4 && frame[..2] == [0xff, 0xd8] && frame[n - 2..] == [0xff, 0xd9];
            match complete.then(|| decode(&frame)).transpose() {
                Ok(Some(img)) => {
                    bad_frames = 0;
                    sink.push(img);
                }
                Ok(None) | Err(_) => {
                    bad_frames += 1;
                    if bad_frames >= MAX_BAD_FRAMES {
                        bail!("Camera's MJPEG stream isn't in the expected format; switch it to SD or HD (ffmpeg)");
                    }
                }
            }
            frame.clear();
        }
    }
}

/// Reads standard RFC 2435 RTP/JPEG through retina's depacketizer.
async fn read_standard(session: Session<retina::client::Playing>, sink: &mut FrameSink) -> Result<()> {
    let mut frames = session.demuxed()?;
    loop {
        let item = match timeout(IO_TIMEOUT, frames.next()).await {
            Err(_) => bail!("No video for {}s", IO_TIMEOUT.as_secs()),
            Ok(None) => bail!("Camera closed the stream"),
            Ok(Some(item)) => item.map_err(|e| anyhow!(e.to_string()))?,
        };
        if let CodecItem::VideoFrame(f) = item {
            match decode(f.data()) {
                Ok(img) => sink.push(img),
                Err(e) => log::debug!("bad JPEG frame: {e}"),
            }
        }
    }
}

/// Strips the RFC 2435 headers; returns the fragment offset and the body.
fn jpeg_body(payload: &[u8]) -> Option<(u32, &[u8])> {
    if payload.len() < 8 {
        return None;
    }
    let frag_offset = u32::from_be_bytes([0, payload[1], payload[2], payload[3]]);
    let type_field = payload[4];
    let q = payload[5];
    let mut body = 8usize;
    if type_field >= 64 {
        body += 4; // restart marker header
    }
    if frag_offset == 0 && q >= 128 {
        // quantization table header: MBZ, precision, 16-bit length, tables
        let hdr = payload.get(body..body + 4)?;
        body += 4 + ((hdr[2] as usize) << 8 | hdr[3] as usize);
    }
    payload.get(body..).map(|b| (frag_offset, b))
}

fn decode(jpeg: &[u8]) -> Result<crate::stream::Frame> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)?.into_rgb8();
    let (w, h) = img.dimensions();
    Ok(SharedPixelBuffer::clone_from_slice(img.as_raw(), w, h))
}

fn rtsp_error(e: retina::Error, spec: &StreamSpec) -> anyhow::Error {
    anyhow!(crate::stream::friendly_error(&e.to_string(), spec.port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_main_header() {
        let mut p = vec![0, 0, 0, 0, 1, 50, 80, 45];
        p.extend_from_slice(&[0xff, 0xd8, 1, 2]);
        assert_eq!(jpeg_body(&p), Some((0, &[0xff, 0xd8, 1, 2][..])));
    }

    #[test]
    fn strips_quant_table_on_first_fragment() {
        let mut p = vec![0, 0, 0, 0, 1, 255, 80, 45, 0, 0, 0, 2, 9, 9];
        p.extend_from_slice(&[0xff, 0xd8]);
        assert_eq!(jpeg_body(&p), Some((0, &[0xff, 0xd8][..])));
    }

    #[test]
    fn later_fragment_keeps_offset() {
        let p = vec![0, 0, 1, 0, 1, 255, 80, 45, 7, 7];
        assert_eq!(jpeg_body(&p), Some((256, &[7, 7][..])));
    }
}
