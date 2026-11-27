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

/// Incremental MPEG-TS parser that extracts complete H.264 Access Units
/// (Annex-B NAL sequences) and detects H.264 vs HEVC from the PMT.
///
/// Unlike naive PUSI-only demuxers, this splits the video elementary stream on
/// Annex-B start codes (`00 00 01` / `00 00 00 01`) and H.264 Access Unit
/// boundaries (AUD, SPS/PPS/SEI after VCL, or a new VCL slice with
/// `first_mb_in_slice == 0`). That guarantees full framerate even when a camera
/// packs an entire 1-second GOP into one PES packet, keeps multi-slice frames
/// intact, and strips trailing non-VCL NALs that would trip OpenH264's
/// `DecodeFrameNoDelay`.
pub struct TsVideoDemuxer {
    pending: Vec<u8>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    codec: Option<VideoCodec>,
    in_pes: bool,
    es_buf: Vec<u8>,
    first_nal_hdr: Option<usize>,
    scan_pos: usize,
    au_buf: Vec<u8>,
    au_has_vcl: bool,
}

impl Default for TsVideoDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// Finds the next Annex-B start code (`00 00 01` or `00 00 00 01`) at or after
/// `from`, returning `(start_code_offset, nal_header_offset)`.
fn find_start_code(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let len = buf.len();
    if from + 3 > len {
        return None;
    }
    let mut i = from;
    while i + 3 <= len {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let sc_start = if i > from && buf[i - 1] == 0 { i - 1 } else { i };
            return Some((sc_start, i + 3));
        }
        i += 1;
    }
    None
}

/// Returns `true` if a NAL unit starting at `nal[hdr..]` begins a new H.264
/// Access Unit when a VCL slice has already been accumulated in the current AU.
#[inline]
fn starts_new_au(nal: &[u8], hdr: usize) -> bool {
    let Some(&first) = nal.get(hdr) else { return false };
    match first & 0x1F {
        // AUD, SPS, PPS, SEI
        6 | 7 | 8 | 9 => true,
        // VCL coded slice: Exp-Golomb `ue(0)` starts with a single `1` bit,
        // so `first_mb_in_slice == 0` iff bit 7 of the slice payload is set.
        1..=5 => nal.get(hdr + 1).is_some_and(|&b| (b & 0x80) != 0),
        _ => false,
    }
}

const MAX_AU_BYTES: usize = 4 * 1024 * 1024;

impl TsVideoDemuxer {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            pmt_pid: None,
            video_pid: None,
            codec: None,
            in_pes: false,
            es_buf: Vec::new(),
            first_nal_hdr: None,
            scan_pos: 0,
            au_buf: Vec::new(),
            au_has_vcl: false,
        }
    }

    pub fn codec(&self) -> Option<VideoCodec> {
        self.codec
    }

    /// Feeds raw MPEG-TS bytes and invokes `on_au` for every completed video
    /// Access Unit (Annex-B bitstream) along with the detected codec.
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

    /// Flushes any trailing buffered NAL / Access Unit (e.g. at EOF).
    #[cfg(test)]
    pub fn flush(&mut self, mut on_au: impl FnMut(&[u8], Option<VideoCodec>)) {
        self.flush_es_nal(&mut on_au);
        if self.au_has_vcl && !self.au_buf.is_empty() {
            on_au(&self.au_buf, self.codec);
            self.au_buf.clear();
            self.au_has_vcl = false;
        }
        self.in_pes = false;
    }

    fn flush_es_nal(&mut self, on_au: &mut impl FnMut(&[u8], Option<VideoCodec>)) {
        if let Some(hdr) = self.first_nal_hdr.take()
            && !self.es_buf.is_empty()
        {
            Self::push_nal_to_au(
                &mut self.au_buf,
                &mut self.au_has_vcl,
                &self.es_buf,
                hdr,
                self.codec,
                on_au,
            );
        }
        self.es_buf.clear();
        self.scan_pos = 0;
    }

    fn push_nal_to_au(
        au_buf: &mut Vec<u8>,
        au_has_vcl: &mut bool,
        nal: &[u8],
        hdr: usize,
        codec: Option<VideoCodec>,
        on_au: &mut impl FnMut(&[u8], Option<VideoCodec>),
    ) {
        let Some(&nal_hdr) = nal.get(hdr) else { return };
        let nal_type = nal_hdr & 0x1F;
        match nal_type {
            // AUD, SPS, PPS, SEI
            6 | 7 | 8 | 9 => {
                if *au_has_vcl {
                    on_au(au_buf, codec);
                    au_buf.clear();
                    *au_has_vcl = false;
                }
                au_buf.extend_from_slice(nal);
            }
            // VCL coded slice (non-IDR, partitions, IDR)
            1..=5 => {
                let first_mb_zero = nal.get(hdr + 1).is_some_and(|&b| (b & 0x80) != 0);
                if *au_has_vcl && first_mb_zero {
                    on_au(au_buf, codec);
                    au_buf.clear();
                    *au_has_vcl = false;
                }
                au_buf.extend_from_slice(nal);
                *au_has_vcl = true;
            }
            // End-of-sequence / end-of-stream: flush completed picture, do not
            // append trailing non-VCL after VCL slices.
            10 | 11 => {
                if *au_has_vcl {
                    on_au(au_buf, codec);
                    au_buf.clear();
                    *au_has_vcl = false;
                }
            }
            // Filler data / unknown non-VCL: ignore after VCL so OpenH264's
            // DecodeFrameNoDelay does not discard a completed frame.
            _ => {}
        }
        if au_buf.len() > MAX_AU_BYTES {
            au_buf.clear();
            *au_has_vcl = false;
        }
    }

    fn drain_nals(&mut self, on_au: &mut impl FnMut(&[u8], Option<VideoCodec>)) {
        if self.first_nal_hdr.is_none() {
            match find_start_code(&self.es_buf, 0) {
                Some((sc_start, hdr_idx)) => {
                    if sc_start > 0 {
                        self.es_buf.drain(..sc_start);
                    }
                    let hdr = hdr_idx - sc_start;
                    self.first_nal_hdr = Some(hdr);
                    self.scan_pos = hdr;
                }
                None => {
                    // Retain up to 3 trailing bytes in case a start code spans packets.
                    if self.es_buf.len() > 3 {
                        let drop_n = self.es_buf.len() - 3;
                        self.es_buf.drain(..drop_n);
                    }
                    return;
                }
            }
        }

        let mut pos = 0usize;
        let mut cur_hdr = self.first_nal_hdr.unwrap();
        while let Some((next_sc, next_hdr)) = find_start_code(&self.es_buf, self.scan_pos) {
            Self::push_nal_to_au(
                &mut self.au_buf,
                &mut self.au_has_vcl,
                &self.es_buf[pos..next_sc],
                cur_hdr - pos,
                self.codec,
                on_au,
            );
            pos = next_sc;
            cur_hdr = next_hdr;
            self.scan_pos = next_hdr;
        }

        if pos > 0 {
            self.es_buf.drain(..pos);
            let rel_hdr = cur_hdr - pos;
            self.first_nal_hdr = Some(rel_hdr);
            self.scan_pos = self.es_buf.len().saturating_sub(3).max(rel_hdr);
        } else {
            self.scan_pos = self.es_buf.len().saturating_sub(3).max(cur_hdr);
        }

        if self.es_buf.len() > MAX_AU_BYTES {
            self.es_buf.clear();
            self.first_nal_hdr = None;
            self.scan_pos = 0;
        }
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
                self.in_pes = false;
                if payload.len() >= 9 && payload[..3] == [0, 0, 1] {
                    let hdr_len = payload[8] as usize;
                    if let Some(es) = payload.get(9 + hdr_len..) {
                        // When a new PES starts cleanly with an Annex-B start code,
                        // finalize the previous PES's trailing NAL immediately and
                        // flush the completed picture with zero extra frame latency.
                        if let Some((0, hdr)) = find_start_code(es, 0) {
                            self.flush_es_nal(on_au);
                            if self.au_has_vcl && starts_new_au(es, hdr) {
                                on_au(&self.au_buf, self.codec);
                                self.au_buf.clear();
                                self.au_has_vcl = false;
                            }
                        }
                        self.es_buf.extend_from_slice(es);
                        self.in_pes = true;
                        self.drain_nals(on_au);
                    }
                }
            } else if self.in_pes {
                self.es_buf.extend_from_slice(payload);
                self.drain_nals(on_au);
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

    #[test]
    fn splits_multi_frame_single_pes_at_full_framerate() {
        // Extract all raw Annex-B ES bytes from relay-sample.ts and repackage
        // the entire 90-frame clip inside ONE single PES packet (PUSI=1 only on
        // the very first packet). A PUSI-only demuxer produces 0 frames before
        // EOF and 1 frame at EOF; our Annex-B demuxer must emit every frame
        // incrementally before flush().
        let ts = include_bytes!("../tests/fixtures/relay-sample.ts");
        let mut es_all = Vec::new();
        {
            let mut d = TsVideoDemuxer::new();
            d.feed(ts, |au, _| es_all.extend_from_slice(au));
            d.flush(|au, _| es_all.extend_from_slice(au));
        }
        assert!(!es_all.is_empty());

        // Build MPEG-TS with one single PUSI packet (video PID 0x0100, stream_id 0xE0)
        // followed by continuation packets carrying all 90 frames.
        let mut single_pes_ts = Vec::new();
        let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00];
        pes.extend_from_slice(&es_all);
        for (i, chunk) in pes.chunks(184).enumerate() {
            let mut pkt = [0xFFu8; 188];
            pkt[0] = 0x47;
            pkt[1] = if i == 0 { 0x41 } else { 0x01 }; // PUSI only on packet 0, PID 0x100
            pkt[2] = 0x00;
            if chunk.len() == 184 {
                pkt[3] = 0x10 | ((i & 0x0F) as u8);
                pkt[4..].copy_from_slice(chunk);
            } else {
                // Adaptation field padding for short final packet
                let pad = 184 - chunk.len();
                pkt[3] = 0x30 | ((i & 0x0F) as u8);
                pkt[4] = (pad - 1) as u8;
                if pad >= 2 {
                    pkt[5] = 0x00;
                }
                pkt[4 + pad..].copy_from_slice(chunk);
            }
            single_pes_ts.extend_from_slice(&pkt);
        }

        let mut demux = TsVideoDemuxer::new();
        let mut dec = H264Decoder::new(640, 360).unwrap();
        let mut decoded_before_flush = 0usize;
        for chunk in single_pes_ts.chunks(188 * 7) {
            demux.feed(chunk, |au, _| {
                if let Ok(Some(_)) = dec.decode_au(au) {
                    decoded_before_flush += 1;
                }
            });
        }
        assert!(
            decoded_before_flush >= 80,
            "expected >=80 frames decoded incrementally inside one PES, got {decoded_before_flush}"
        );
    }

    #[test]
    fn keeps_multi_slice_frame_together_and_splits_next_picture() {
        // Synthetic Annex-B stream:
        // Picture 1: SPS + PPS + slice 1 (first_mb=0 -> 0x80) + slice 2 (first_mb>0 -> 0x40)
        // Picture 2: slice 1 (first_mb=0 -> 0x80)
        let mut es = Vec::new();
        es.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0x00, 0x1E]); // SPS
        es.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x38, 0x80]); // PPS
        es.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x11, 0x22]); // IDR slice 1 (bit 7 = 1)
        es.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x44, 0x33, 0x44]); // IDR slice 2 (bit 7 = 0)
        es.extend_from_slice(&[0, 0, 0, 1, 0x41, 0x9A, 0x55, 0x66]); // P slice 1 (bit 7 = 1)

        let mut pes = vec![0x00, 0x00, 0x01, 0xE0, 0x00, 0x00, 0x80, 0x00, 0x00];
        pes.extend_from_slice(&es);
        let mut pkt = [0xFFu8; 188];
        let pad = 184 - pes.len();
        pkt[0] = 0x47;
        pkt[1] = 0x41;
        pkt[2] = 0x00;
        pkt[3] = 0x30;
        pkt[4] = (pad - 1) as u8;
        pkt[5] = 0x00;
        pkt[4 + pad..].copy_from_slice(&pes);

        let mut demux = TsVideoDemuxer::new();
        let mut aus = Vec::new();
        demux.feed(&pkt, |au, _| aus.push(au.to_vec()));
        demux.flush(|au, _| aus.push(au.to_vec()));

        assert_eq!(aus.len(), 2, "expected 2 access units (2-slice IDR + 1-slice P)");
        assert!(aus[0].windows(4).any(|w| w == [0x65, 0x88, 0x11, 0x22]));
        assert!(aus[0].windows(4).any(|w| w == [0x65, 0x44, 0x33, 0x44]));
        assert!(aus[1].windows(4).any(|w| w == [0x41, 0x9A, 0x55, 0x66]));
    }
}
