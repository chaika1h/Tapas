//! In-process MPEG-TS video demuxer and OpenH264 live decoder.
//!
//! Decodes H.264 (`stream_type == 0x1B`) cloud relay streams directly in
//! memory with zero `ffmpeg.exe` subprocess spawn, zero Windows Defender scan
//! latency, zero `-probesize` buffering delay, and zero raw-frame pipe IPC.
//! Streams that use H.265/HEVC (`0x24`) or an unsupported H.264 profile signal
//! fallback so [`crate::ffmpeg`] seamlessly hands them off to `ffmpeg.exe`
//! (with D3D11VA/DXVA2 hardware decode on Windows).

use openh264::OpenH264API;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use slint::{Rgba8Pixel, SharedPixelBuffer};

use crate::stream::Frame;

/// Video codec discovered in the MPEG-TS Program Map Table (PMT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// Incremental MPEG-TS parser that extracts complete video PES access units
/// (Annex-B NAL sequences) and detects H.264 vs HEVC from the PMT.
pub struct TsVideoDemuxer {
    pending: Vec<u8>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    codec: Option<VideoCodec>,
    pes: Vec<u8>,
    in_pes: bool,
}

impl Default for TsVideoDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl TsVideoDemuxer {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            pmt_pid: None,
            video_pid: None,
            codec: None,
            pes: Vec::new(),
            in_pes: false,
        }
    }

    pub fn codec(&self) -> Option<VideoCodec> {
        self.codec
    }

    /// Feeds raw MPEG-TS bytes and invokes `on_au` for every completed video
    /// PES access unit (Annex-B bitstream) along with the detected codec.
    pub fn feed(&mut self, mut data: &[u8], mut on_au: impl FnMut(&[u8], Option<VideoCodec>)) {
        if !self.pending.is_empty() {
            let need = 188 - self.pending.len();
            let take = need.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() < 188 {
                return;
            }
            let pkt = std::mem::take(&mut self.pending);
            self.packet(&pkt, &mut on_au);
        }
        while !data.is_empty() {
            if data[0] != 0x47 {
                let Some(pos) = data.iter().position(|&b| b == 0x47) else { return };
                data = &data[pos..];
            }
            if data.len() < 188 {
                self.pending.extend_from_slice(data);
                return;
            }
            self.packet(&data[..188], &mut on_au);
            data = &data[188..];
        }
    }

    /// Flushes any trailing buffered video PES access unit (e.g. at EOF).
    #[cfg(test)]
    pub fn flush(&mut self, mut on_au: impl FnMut(&[u8], Option<VideoCodec>)) {
        if !self.pes.is_empty() {
            on_au(&self.pes, self.codec);
            self.pes.clear();
        }
        self.in_pes = false;
    }

    fn packet(&mut self, p: &[u8], on_au: &mut impl FnMut(&[u8], Option<VideoCodec>)) {
        if p[0] != 0x47 {
            return;
        }
        let pusi = (p[1] & 0x40) != 0;
        let pid = (((p[1] & 0x1F) as u16) << 8) | (p[2] as u16);
        let afc = (p[3] >> 4) & 0x3;
        let at = match afc {
            1 => 4,
            3 => 5 + p[4] as usize,
            _ => return,
        };
        let Some(payload) = p.get(at..) else { return };

        if pid == 0 && pusi {
            self.parse_pat(payload);
            return;
        }
        if self.pmt_pid == Some(pid) && pusi {
            self.parse_pmt(payload);
            return;
        }

        // Opportunistic fallback if video PES arrives before PMT.
        if self.video_pid.is_none() && pusi && payload.len() >= 9 && payload[..3] == [0, 0, 1] {
            let sid = payload[3];
            if (0xE0..=0xEF).contains(&sid) {
                self.video_pid = Some(pid);
            }
        }

        if self.video_pid == Some(pid) {
            if pusi {
                if !self.pes.is_empty() {
                    on_au(&self.pes, self.codec);
                    self.pes.clear();
                }
                self.in_pes = false;
                if payload.len() >= 9 && payload[..3] == [0, 0, 1] {
                    let hdr_len = payload[8] as usize;
                    if let Some(es) = payload.get(9 + hdr_len..) {
                        self.pes.extend_from_slice(es);
                        self.in_pes = true;
                    }
                }
            } else if self.in_pes {
                self.pes.extend_from_slice(payload);
            }
        }
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(&ptr) = payload.first() else { return };
        let Some(sec) = payload.get(1 + ptr as usize..) else { return };
        if sec.len() < 12 || sec[0] != 0x00 {
            return;
        }
        let sec_len = ((((sec[1] & 0x0F) as usize) << 8) | (sec[2] as usize)).min(sec.len().saturating_sub(3));
        let mut i = 8;
        let end = (3 + sec_len).saturating_sub(4);
        while i + 4 <= end && i + 4 <= sec.len() {
            let prog = ((sec[i] as u16) << 8) | (sec[i + 1] as u16);
            let pid = (((sec[i + 2] & 0x1F) as u16) << 8) | (sec[i + 3] as u16);
            if prog != 0 {
                self.pmt_pid = Some(pid);
                return;
            }
            i += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
        let Some(&ptr) = payload.first() else { return };
        let Some(sec) = payload.get(1 + ptr as usize..) else { return };
        if sec.len() < 16 || sec[0] != 0x02 {
            return;
        }
        let sec_len = ((((sec[1] & 0x0F) as usize) << 8) | (sec[2] as usize)).min(sec.len().saturating_sub(3));
        let prog_info_len = (((sec[10] & 0x0F) as usize) << 8) | (sec[11] as usize);
        let mut i = 12 + prog_info_len;
        let end = (3 + sec_len).saturating_sub(4);
        while i + 5 <= end && i + 5 <= sec.len() {
            let stream_type = sec[i];
            let es_pid = (((sec[i + 1] & 0x1F) as u16) << 8) | (sec[i + 2] as u16);
            let es_info_len = (((sec[i + 3] & 0x0F) as usize) << 8) | (sec[i + 4] as usize);
            match stream_type {
                0x1B => {
                    self.video_pid = Some(es_pid);
                    self.codec = Some(VideoCodec::H264);
                }
                0x24 => {
                    self.video_pid = Some(es_pid);
                    self.codec = Some(VideoCodec::Hevc);
                }
                _ => {}
            }
            i += 5 + es_info_len;
        }
    }
}

/// In-process H.264 decoder backed by Cisco OpenH264 with SIMD YUV420→RGBA8
/// conversion directly into Slint [`SharedPixelBuffer`]s.
pub struct H264Decoder {
    decoder: Decoder,
    max_w: usize,
    max_h: usize,
}

impl H264Decoder {
    pub fn new(max_w: usize, max_h: usize) -> anyhow::Result<Self> {
        let config = DecoderConfig::new()
            .debug(false)
            .flush_after_decode(Flush::NoFlush);
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| anyhow::anyhow!("OpenH264 init failed: {e}"))?;
        Ok(Self { decoder, max_w, max_h })
    }

    /// Decodes one Annex-B access unit (SPS/PPS/slice) into an RGBA [`Frame`].
    pub fn decode_au(&mut self, au: &[u8]) -> Result<Option<Frame>, String> {
        match self.decoder.decode(au) {
            Ok(Some(yuv)) => Ok(yuv_to_frame(&yuv, self.max_w, self.max_h)),
            Ok(None) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Computes the target `(w, h)` that fits `(src_w, src_h)` within `(max_w, max_h)`
/// while preserving aspect ratio (rounded to even pixels).
fn fit_dimensions(src_w: usize, src_h: usize, max_w: usize, max_h: usize) -> (usize, usize) {
    if src_w == 0 || src_h == 0 {
        return (0, 0);
    }
    if src_w <= max_w && src_h <= max_h {
        return (src_w, src_h);
    }
    let scale_w = max_w as f64 / src_w as f64;
    let scale_h = max_h as f64 / src_h as f64;
    let scale = scale_w.min(scale_h);
    let w = ((src_w as f64 * scale).round() as usize).max(2) & !1;
    let h = ((src_h as f64 * scale).round() as usize).max(2) & !1;
    (w.min(max_w), h.min(max_h))
}

/// Converts a decoded YUV420 frame directly into a Slint RGBA [`Frame`].
/// Uses OpenH264's AVX2/SIMD converter when no downscaling is needed, or a
/// single-pass fused YUV420→scaled RGBA converter when downscaling (e.g. 2K→1080p).
fn yuv_to_frame(yuv: &DecodedYUV<'_>, max_w: usize, max_h: usize) -> Option<Frame> {
    let (src_w, src_h) = yuv.dimensions();
    if src_w < 2 || src_h < 2 {
        return None;
    }
    let (dst_w, dst_h) = fit_dimensions(src_w, src_h, max_w, max_h);
    let mut frame = SharedPixelBuffer::<Rgba8Pixel>::new(dst_w as u32, dst_h as u32);

    if dst_w == src_w && dst_h == src_h {
        yuv.write_rgba8(frame.make_mut_bytes());
        return Some(frame);
    }

    // Single-pass fused YUV420 (BT.601 limited range) -> scaled RGBA8 directly
    // into the destination buffer, avoiding a 15 MB intermediate 2K RGBA frame.
    let (y_stride, u_stride, v_stride) = yuv.strides();
    let y_plane = yuv.y();
    let u_plane = yuv.u();
    let v_plane = yuv.v();
    let out = frame.make_mut_bytes();

    // Precompute source X coordinates (16.16 fixed point) for the row.
    let x_map: Vec<usize> = (0..dst_w)
        .map(|dx| ((dx * src_w) / dst_w).min(src_w - 1))
        .collect();

    for dy in 0..dst_h {
        let sy = ((dy * src_h) / dst_h).min(src_h - 1);
        let y_row = &y_plane[sy * y_stride..];
        let u_row = &u_plane[(sy >> 1) * u_stride..];
        let v_row = &v_plane[(sy >> 1) * v_stride..];
        let dst_row = &mut out[dy * dst_w * 4..(dy + 1) * dst_w * 4];

        for (dx, &sx) in x_map.iter().enumerate() {
            let y = y_row[sx] as i32 - 16;
            let u = u_row[sx >> 1] as i32 - 128;
            let v = v_row[sx >> 1] as i32 - 128;

            let c = 298 * y;
            let r = ((c + 409 * v + 128) >> 8).clamp(0, 255) as u8;
            let g = ((c - 100 * u - 208 * v + 128) >> 8).clamp(0, 255) as u8;
            let b = ((c + 516 * u + 128) >> 8).clamp(0, 255) as u8;

            let p = dx * 4;
            dst_row[p] = r;
            dst_row[p + 1] = g;
            dst_row[p + 2] = b;
            dst_row[p + 3] = 255;
        }
    }

    Some(frame)
}

/// Outcome of running the in-process H.264 worker on raw MPEG-TS chunks.
pub enum WorkerSignal {
    Frame(Frame),
    /// Stream is HEVC or failed to decode with OpenH264 before producing a
    /// frame; caller should replay buffered initial TS bytes into `ffmpeg.exe`.
    Fallback(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_relay_fixture_in_process() {
        let ts = include_bytes!("../tests/fixtures/relay-sample.ts");
        let mut demux = TsVideoDemuxer::new();
        let mut dec = H264Decoder::new(640, 360).unwrap();
        let mut frames = Vec::new();

        // Feed in realistic 4 KB chunks to exercise TS packet reassembly across boundaries.
        for chunk in ts.chunks(4096) {
            demux.feed(chunk, |au, _| {
                if let Ok(Some(f)) = dec.decode_au(au) {
                    frames.push(f);
                }
            });
        }
        demux.flush(|au, _| {
            if let Ok(Some(f)) = dec.decode_au(au) {
                frames.push(f);
            }
        });

        assert_eq!(demux.codec(), Some(VideoCodec::H264));
        assert!(frames.len() >= 10, "expected >=10 decoded frames, got {}", frames.len());
        let first = &frames[0];
        assert_eq!((first.width(), first.height()), (320, 180));
        let distinct: std::collections::HashSet<_> = first.as_bytes().chunks(4).step_by(37).collect();
        assert!(distinct.len() > 20, "first decoded frame looks blank");
    }

    #[test]
    fn downscales_in_single_pass() {
        let ts = include_bytes!("../tests/fixtures/relay-sample.ts");
        let mut demux = TsVideoDemuxer::new();
        // Force downscale of 320x180 fixture to 160x90.
        let mut dec = H264Decoder::new(160, 90).unwrap();
        let mut got = None;
        demux.feed(ts, |au, _| {
            if got.is_none()
                && let Ok(Some(f)) = dec.decode_au(au)
            {
                got = Some(f);
            }
        });
        let frame = got.expect("expected scaled frame");
        assert_eq!((frame.width(), frame.height()), (160, 90));
        let distinct: std::collections::HashSet<_> = frame.as_bytes().chunks(4).step_by(19).collect();
        assert!(distinct.len() > 15, "scaled frame looks blank");
    }

    #[test]
    fn detects_hevc_from_pmt() {
        // Minimal PAT (PID 0 -> PMT PID 0x1000) + PMT (PID 0x1000 -> video PID 0x0100, stream_type 0x24 HEVC).
        let mut pat = [0xFFu8; 188];
        pat[0] = 0x47;
        pat[1] = 0x40; // PUSI, PID 0
        pat[2] = 0x00;
        pat[3] = 0x10; // payload only
        pat[4] = 0x00; // pointer field
        pat[5] = 0x00; // table_id = PAT
        pat[6] = 0xB0;
        pat[7] = 13; // section_length = 13 (5 hdr + 4 prog + 4 CRC)
        pat[13] = 0x00;
        pat[14] = 0x01; // program 1
        pat[15] = 0xF0;
        pat[16] = 0x00; // PMT PID = 0x1000

        let mut pmt = [0xFFu8; 188];
        pmt[0] = 0x47;
        pmt[1] = 0x50; // PUSI, PID 0x1000
        pmt[2] = 0x00;
        pmt[3] = 0x10;
        pmt[4] = 0x00; // pointer
        pmt[5] = 0x02; // table_id = PMT
        pmt[6] = 0xB0;
        pmt[7] = 18; // section_length = 18 (9 hdr + 5 stream + 4 CRC)
        pmt[15] = 0xF0;
        pmt[16] = 0x00; // program_info_length = 0
        pmt[17] = 0x24; // HEVC
        pmt[18] = 0xE1;
        pmt[19] = 0x00; // video PID 0x0100
        pmt[20] = 0xF0;
        pmt[21] = 0x00; // es_info_length = 0

        let mut demux = TsVideoDemuxer::new();
        demux.feed(&pat, |_, _| {});
        demux.feed(&pmt, |_, _| {});
        assert_eq!(demux.codec(), Some(VideoCodec::Hevc));
    }
}
