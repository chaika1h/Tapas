//! Pure-Rust MPEG-TS audio demuxer, G.711 / AAC decoder, and Windows audio
//! playback (`winmm` `waveOut`).
//!
//! Tapo cameras and the H500 HomeBase embed audio directly inside the same
//! `video/mp2t` MPEG-TS stream that carries H.264/HEVC video — typically
//! G.711 A-law (`stream_type 0x90`, 8 kHz or 16 kHz), G.711 u-law (`0x91`),
//! or ADTS AAC (`0x0f`). Demuxing and decoding here avoids rebuilding ffmpeg
//! with audio codecs, keeps memory overhead near zero when muted, and keeps
//! the demuxer/decoder reusable across platforms.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use symphonia_codec_aac::AacDecoder;
use symphonia_core::audio::SampleBuffer;
use symphonia_core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder, DecoderOptions};
use symphonia_core::formats::Packet;

/// Only one camera or playback stream plays sound at a time. `0` means muted.
static ACTIVE_SOURCE: AtomicU64 = AtomicU64::new(0);
static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);
static SINK: LazyLock<Mutex<platform::Sink>> = LazyLock::new(|| Mutex::new(platform::Sink::new()));

/// Allocates a unique non-zero audio source id for a stream or playback.
pub fn next_source_id() -> u64 {
    NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Selects which audio source (if any) is audible; immediately flushes any
/// queued sound from the previous source.
pub fn set_active(source_id: Option<u64>) {
    let next = source_id.unwrap_or(0);
    let prev = ACTIVE_SOURCE.swap(next, Ordering::SeqCst);
    if prev != next {
        if let Ok(mut sink) = SINK.lock() {
            if next == 0 {
                sink.close();
            } else {
                sink.reset();
            }
        }
    }
}

/// Fast lock-free check used by stream/playback loops to skip audio decoding
/// completely while muted.
#[inline]
pub fn is_active(source_id: u64) -> bool {
    source_id != 0 && ACTIVE_SOURCE.load(Ordering::Relaxed) == source_id
}

/// Flushes queued audio if `source_id` is currently the active source (called
/// on pause, seek, or speed change).
pub fn flush_if_active(source_id: u64) {
    if is_active(source_id) {
        if let Ok(mut sink) = SINK.lock() {
            sink.reset();
        }
    }
}

/// Releases the active source if it matches `source_id` (called on drop/stop).
pub fn release(source_id: u64) {
    if source_id != 0
        && ACTIVE_SOURCE
            .compare_exchange(source_id, 0, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    {
        if let Ok(mut sink) = SINK.lock() {
            sink.close();
        }
    }
}

/// Closes the audio output device immediately (called on app shutdown).
pub fn shutdown() {
    ACTIVE_SOURCE.store(0, Ordering::SeqCst);
    if let Ok(mut sink) = SINK.lock() {
        sink.close();
    }
}

/// Sends mono `i16` PCM to the speaker if `source_id` is still active.
pub fn push_pcm(source_id: u64, sample_rate: u32, samples: &[i16]) {
    if samples.is_empty() || !is_active(source_id) {
        return;
    }
    if let Ok(mut sink) = SINK.lock() {
        if is_active(source_id) {
            sink.write(sample_rate, samples);
        }
    }
}

// ------------------------------------------------------- MPEG-TS audio demuxer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Pcma,
    Pcmu,
    Aac,
}

/// Decoded mono 16-bit PCM chunk produced by [`TsAudioDemuxer`].
#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub sample_rate: u32,
    pub samples: Vec<i16>,
}

/// Streaming MPEG-TS audio demuxer & decoder (G.711 A-law, G.711 u-law, ADTS AAC).
pub struct TsAudioDemuxer {
    pending: Vec<u8>,
    pmt_pid: Option<u16>,
    audio_pid: Option<u16>,
    codec: Option<AudioCodec>,
    video_hevc: bool,
    pes: Vec<u8>,
    pes_expected: Option<usize>,
    pes_pts: Option<u64>,
    last_g711_pts: Option<u64>,
    g711_rate: u32,
    g711_rate_locked: bool,
    aac_buf: Vec<u8>,
    aac: Option<AacState>,
}

struct AacState {
    asc: [u8; 2],
    decoder: AacDecoder,
    sample_buf: Option<SampleBuffer<i16>>,
}

impl Default for TsAudioDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl TsAudioDemuxer {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            pmt_pid: None,
            audio_pid: None,
            codec: None,
            video_hevc: false,
            pes: Vec::new(),
            pes_expected: None,
            pes_pts: None,
            last_g711_pts: None,
            g711_rate: 8_000,
            g711_rate_locked: false,
            aac_buf: Vec::new(),
            aac: None,
        }
    }

    pub fn codec(&self) -> Option<AudioCodec> {
        self.codec
    }

    pub fn video_is_hevc(&self) -> bool {
        self.video_hevc
    }

    /// Flushes any final buffered PES audio packet at end-of-stream.
    pub fn finish(&mut self, out: &mut Vec<PcmChunk>) {
        self.flush_pes(out);
    }

    /// Resets stream buffers (e.g. after a seek or unmuting mid-stream).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.pes.clear();
        self.pes_expected = None;
        self.pes_pts = None;
        self.last_g711_pts = None;
        self.aac_buf.clear();
    }

    /// Feeds raw MPEG-TS bytes and yields decoded mono `i16` PCM chunks.
    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<PcmChunk>) {
        if !self.pending.is_empty() {
            let need = 188 - self.pending.len();
            let take = need.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() < 188 {
                return;
            }
            let packet = std::mem::take(&mut self.pending);
            self.packet(&packet, out);
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
            self.packet(&data[..188], out);
            data = &data[188..];
        }
    }

    fn packet(&mut self, p: &[u8], out: &mut Vec<PcmChunk>) {
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

        // If PMT hasn't been seen yet (mid-stream unmute), opportunistically
        // sniff PES audio stream IDs (0xC0..=0xDF) on PUSI packets.
        if self.audio_pid.is_none() && pusi && payload.len() >= 9 && payload[..3] == [0, 0, 1] {
            let sid = payload[3];
            if (0xC0..=0xDF).contains(&sid) {
                self.audio_pid = Some(pid);
            }
        }

        if self.audio_pid == Some(pid) {
            if pusi {
                self.flush_pes(out);
                if payload.len() >= 9 && payload[..3] == [0, 0, 1] {
                    let pes_len = ((payload[4] as usize) << 8) | payload[5] as usize;
                    let hdr_len = payload[8] as usize;
                    let pts = parse_pes_pts(payload);
                    if let Some(es) = payload.get(9 + hdr_len..) {
                        self.pes_pts = pts;
                        self.pes_expected = (pes_len > 0).then(|| pes_len.saturating_sub(3 + hdr_len));
                        let take = self.pes_expected.map_or(es.len(), |n| n.min(es.len()));
                        self.pes.extend_from_slice(&es[..take]);
                    }
                }
            } else if !self.pes.is_empty() || self.pes_pts.is_some() {
                let take = self
                    .pes_expected
                    .map_or(payload.len(), |n| n.saturating_sub(self.pes.len()).min(payload.len()));
                self.pes.extend_from_slice(&payload[..take]);
            }
        }
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(&ptr) = payload.first() else { return };
        let Some(sec) = payload.get(1 + ptr as usize..) else { return };
        if sec.len() < 12 || sec[0] != 0x00 {
            return;
        }
        let sec_len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
        let end = (3 + sec_len).saturating_sub(4).min(sec.len());
        let mut i = 8;
        while i + 4 <= end {
            let program = ((sec[i] as u16) << 8) | sec[i + 1] as u16;
            let pid = (((sec[i + 2] & 0x1F) as u16) << 8) | sec[i + 3] as u16;
            if program != 0 {
                self.pmt_pid = Some(pid);
                break;
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
        let sec_len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
        let prog_info_len = (((sec[10] & 0x0F) as usize) << 8) | sec[11] as usize;
        let mut i = 12 + prog_info_len;
        let end = (3 + sec_len).saturating_sub(4).min(sec.len());
        while i + 5 <= end {
            let stype = sec[i];
            let epid = (((sec[i + 1] & 0x1F) as u16) << 8) | sec[i + 2] as u16;
            let es_len = (((sec[i + 3] & 0x0F) as usize) << 8) | sec[i + 4] as usize;
            match stype {
                0x24 => self.video_hevc = true,
                0x90 => {
                    self.audio_pid = Some(epid);
                    self.codec = Some(AudioCodec::Pcma);
                }
                0x91 => {
                    self.audio_pid = Some(epid);
                    self.codec = Some(AudioCodec::Pcmu);
                }
                0x0F => {
                    self.audio_pid = Some(epid);
                    self.codec = Some(AudioCodec::Aac);
                }
                _ => {}
            }
            i += 5 + es_len;
        }
    }

    fn flush_pes(&mut self, out: &mut Vec<PcmChunk>) {
        if self.pes.is_empty() {
            return;
        }
        let data = std::mem::take(&mut self.pes);
        self.pes_expected = None;
        let pts = self.pes_pts.take();

        // Guess codec if PMT hasn't arrived yet: ADTS AAC starts with 0xFF 0xF*.
        let codec = self.codec.unwrap_or_else(|| {
            if data.len() >= 7 && data[0] == 0xFF && (data[1] & 0xF6) == 0xF0 {
                AudioCodec::Aac
            } else {
                AudioCodec::Pcma
            }
        });

        match codec {
            AudioCodec::Pcma | AudioCodec::Pcmu => {
                // Auto-detect 8 kHz vs 16 kHz G.711 from PTS cadence (1 byte = 1 sample).
                if !self.g711_rate_locked
                    && let (Some(prev), Some(curr)) = (self.last_g711_pts, pts)
                    && curr > prev
                {
                    let dt = curr - prev;
                    let est = (data.len() as u64 * 90_000) / dt;
                    if (12_000..=20_000).contains(&est) {
                        self.g711_rate = 16_000;
                        self.g711_rate_locked = true;
                    } else if (6_000..=11_999).contains(&est) {
                        self.g711_rate = 8_000;
                        self.g711_rate_locked = true;
                    }
                }
                if pts.is_some() {
                    self.last_g711_pts = pts;
                }
                let decode = if codec == AudioCodec::Pcma { decode_alaw } else { decode_ulaw };
                let samples: Vec<i16> = data.iter().map(|&b| decode(b)).collect();
                out.push(PcmChunk { sample_rate: self.g711_rate, samples });
            }
            AudioCodec::Aac => {
                self.aac_buf.extend_from_slice(&data);
                self.drain_aac(out);
            }
        }
    }

    fn drain_aac(&mut self, out: &mut Vec<PcmChunk>) {
        while self.aac_buf.len() >= 7 {
            let Some(sync) = find_adts_sync(&self.aac_buf) else {
                self.aac_buf.clear();
                return;
            };
            if sync > 0 {
                self.aac_buf.drain(..sync);
            }
            if self.aac_buf.len() < 7 {
                return;
            }
            let Some(hdr) = parse_adts_header(&self.aac_buf) else {
                self.aac_buf.drain(..2);
                continue;
            };
            if hdr.frame_len < hdr.header_len || hdr.frame_len > 8192 {
                self.aac_buf.drain(..2);
                continue;
            }
            if self.aac_buf.len() < hdr.frame_len {
                return;
            }
            let frame: Vec<u8> = self.aac_buf.drain(..hdr.frame_len).collect();
            let raw = &frame[hdr.header_len..];
            if let Some(samples) = self.decode_aac_frame(hdr.asc, hdr.sample_rate, raw) {
                out.push(PcmChunk { sample_rate: hdr.sample_rate, samples });
            }
        }
    }

    fn decode_aac_frame(&mut self, asc: [u8; 2], sample_rate: u32, payload: &[u8]) -> Option<Vec<i16>> {
        let need_new = self.aac.as_ref().is_none_or(|a| a.asc != asc);
        if need_new {
            let mut params = CodecParameters::new();
            params
                .for_codec(CODEC_TYPE_AAC)
                .with_sample_rate(sample_rate)
                .with_extra_data(asc.into());
            if let Ok(decoder) = AacDecoder::try_new(&params, &DecoderOptions::default()) {
                self.aac = Some(AacState { asc, decoder, sample_buf: None });
            } else {
                return None;
            }
        }
        let state = self.aac.as_mut()?;
        let packet = Packet::new_from_slice(0, 0, 0, payload);
        let decoded = state.decoder.decode(&packet).ok()?;
        let spec = *decoded.spec();
        let ch = spec.channels.count().max(1);
        let capacity = decoded.capacity() as u64;
        let buf = state
            .sample_buf
            .get_or_insert_with(|| SampleBuffer::<i16>::new(capacity, spec));
        if buf.capacity() < decoded.capacity() * ch {
            *buf = SampleBuffer::<i16>::new(capacity, spec);
        }
        buf.copy_interleaved_ref(decoded);
        let interleaved = buf.samples();
        if ch == 1 {
            Some(interleaved.to_vec())
        } else {
            let mono = interleaved
                .chunks_exact(ch)
                .map(|c| {
                    let sum: i32 = c.iter().map(|&s| s as i32).sum();
                    (sum / ch as i32) as i16
                })
                .collect();
            Some(mono)
        }
    }
}

fn parse_pes_pts(pes: &[u8]) -> Option<u64> {
    if pes.len() < 14 || (pes[7] & 0x80) == 0 {
        return None;
    }
    let b = &pes[9..14];
    Some(
        ((b[0] as u64 >> 1) & 0x07) << 30
            | (b[1] as u64) << 22
            | ((b[2] as u64) >> 1) << 15
            | (b[3] as u64) << 7
            | (b[4] as u64) >> 1,
    )
}

struct AdtsHeader {
    asc: [u8; 2],
    sample_rate: u32,
    header_len: usize,
    frame_len: usize,
}

const ADTS_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350,
];

fn find_adts_sync(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w[0] == 0xFF && (w[1] & 0xF6) == 0xF0)
}

fn parse_adts_header(buf: &[u8]) -> Option<AdtsHeader> {
    if buf.len() < 7 || buf[0] != 0xFF || (buf[1] & 0xF6) != 0xF0 {
        return None;
    }
    let no_crc = (buf[1] & 0x01) != 0;
    let obj_type = ((buf[2] >> 6) & 0x03) + 1; // 2 = AAC-LC
    let rate_idx = (buf[2] >> 2) & 0x0F;
    let sample_rate = *ADTS_RATES.get(rate_idx as usize)?;
    let channels = (((buf[2] & 0x01) << 2) | (buf[3] >> 6)).max(1);
    let asc = [(obj_type << 3) | (rate_idx >> 1), ((rate_idx & 1) << 7) | (channels << 3)];
    let frame_len =
        (((buf[3] & 0x03) as usize) << 11) | ((buf[4] as usize) << 3) | ((buf[5] >> 5) as usize);
    let header_len = if no_crc { 7 } else { 9 };
    Some(AdtsHeader { asc, sample_rate, header_len, frame_len })
}

/// ITU-T G.711 A-law byte to signed 16-bit linear PCM.
#[inline]
pub fn decode_alaw(byte: u8) -> i16 {
    let a = byte ^ 0x55;
    let mut t = ((a & 0x0F) as i16) << 4;
    let seg = ((a & 0x70) >> 4) as i16;
    match seg {
        0 => t += 8,
        1 => t += 0x108,
        _ => {
            t += 0x108;
            t <<= seg - 1;
        }
    }
    if (a & 0x80) != 0 { t } else { -t }
}

/// ITU-T G.711 u-law byte to signed 16-bit linear PCM.
#[inline]
pub fn decode_ulaw(byte: u8) -> i16 {
    let u = !byte;
    let t = ((((u & 0x0F) as i16) << 3) + 0x84) << ((u & 0x70) >> 4);
    if (u & 0x80) != 0 { 0x84 - t } else { t - 0x84 }
}

// ------------------------------------------------------------- platform sink

#[cfg(windows)]
mod platform {
    use std::collections::VecDeque;
    use std::ptr;

    use windows_sys::Win32::Media::Audio::{
        CALLBACK_NULL, HWAVEOUT, WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEFORMATEX, WAVEHDR, WHDR_DONE,
        waveOutClose, waveOutOpen, waveOutPrepareHeader, waveOutReset, waveOutUnprepareHeader,
        waveOutWrite,
    };
    use windows_sys::Win32::Media::MMSYSERR_NOERROR;

    /// Maximum buffered audio per stream (~400 ms) so live audio stays in sync.
    const MAX_BUFFERED_MS: usize = 400;

    struct WaveChunk {
        _samples: Vec<i16>,
        hdr: Box<WAVEHDR>,
    }

    pub struct Sink {
        handle: HWAVEOUT,
        sample_rate: u32,
        queued: VecDeque<WaveChunk>,
        queued_samples: usize,
    }

    // SAFETY: `Sink` is only ever accessed through `SINK: Mutex<Sink>`.
    unsafe impl Send for Sink {}

    impl Sink {
        pub fn new() -> Self {
            Self {
                handle: ptr::null_mut(),
                sample_rate: 0,
                queued: VecDeque::new(),
                queued_samples: 0,
            }
        }

        fn open(&mut self, sample_rate: u32) -> bool {
            if !self.handle.is_null() && self.sample_rate == sample_rate {
                return true;
            }
            self.close();
            let fmt = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 1,
                nSamplesPerSec: sample_rate,
                nAvgBytesPerSec: sample_rate * 2,
                nBlockAlign: 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let mut handle: HWAVEOUT = ptr::null_mut();
            let rc = unsafe {
                waveOutOpen(&mut handle, WAVE_MAPPER, &fmt, 0, 0, CALLBACK_NULL)
            };
            if rc != MMSYSERR_NOERROR || handle.is_null() {
                log::warn!("audio: waveOutOpen({sample_rate} Hz) failed (code {rc})");
                return false;
            }
            self.handle = handle;
            self.sample_rate = sample_rate;
            true
        }

        fn reap(&mut self) {
            if self.handle.is_null() {
                return;
            }
            while let Some(front) = self.queued.front() {
                if (front.hdr.dwFlags & WHDR_DONE) == 0 {
                    break;
                }
                if let Some(mut done) = self.queued.pop_front() {
                    self.queued_samples = self.queued_samples.saturating_sub(done._samples.len());
                    unsafe {
                        waveOutUnprepareHeader(
                            self.handle,
                            &mut *done.hdr,
                            std::mem::size_of::<WAVEHDR>() as u32,
                        );
                    }
                }
            }
        }

        pub fn write(&mut self, sample_rate: u32, samples: &[i16]) {
            if !self.open(sample_rate) {
                return;
            }
            self.reap();
            let max_samples = (sample_rate as usize * MAX_BUFFERED_MS) / 1000;
            if self.queued_samples > max_samples {
                // Drop excess to prevent multi-second latency build-up.
                return;
            }
            let owned = samples.to_vec();
            let byte_len = (owned.len() * std::mem::size_of::<i16>()) as u32;
            let mut hdr = Box::new(WAVEHDR {
                lpData: owned.as_ptr() as *mut u8,
                dwBufferLength: byte_len,
                dwBytesRecorded: 0,
                dwUser: 0,
                dwFlags: 0,
                dwLoops: 0,
                lpNext: ptr::null_mut(),
                reserved: 0,
            });
            let hdr_size = std::mem::size_of::<WAVEHDR>() as u32;
            let rc = unsafe { waveOutPrepareHeader(self.handle, &mut *hdr, hdr_size) };
            if rc != MMSYSERR_NOERROR {
                return;
            }
            let rc = unsafe { waveOutWrite(self.handle, &mut *hdr, hdr_size) };
            if rc != MMSYSERR_NOERROR {
                unsafe {
                    waveOutUnprepareHeader(self.handle, &mut *hdr, hdr_size);
                }
                return;
            }
            self.queued_samples += owned.len();
            self.queued.push_back(WaveChunk { _samples: owned, hdr });
        }

        pub fn reset(&mut self) {
            if self.handle.is_null() {
                return;
            }
            unsafe {
                waveOutReset(self.handle);
            }
            let hdr_size = std::mem::size_of::<WAVEHDR>() as u32;
            while let Some(mut chunk) = self.queued.pop_front() {
                unsafe {
                    waveOutUnprepareHeader(self.handle, &mut *chunk.hdr, hdr_size);
                }
            }
            self.queued_samples = 0;
        }

        pub fn close(&mut self) {
            if self.handle.is_null() {
                return;
            }
            self.reset();
            unsafe {
                waveOutClose(self.handle);
            }
            self.handle = ptr::null_mut();
            self.sample_rate = 0;
        }
    }

    impl Drop for Sink {
        fn drop(&mut self) {
            self.close();
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub struct Sink;
    impl Sink {
        pub fn new() -> Self {
            Self
        }
        pub fn write(&mut self, _sample_rate: u32, _samples: &[i16]) {}
        pub fn reset(&mut self) {}
        pub fn close(&mut self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts_packet(pid: u16, pusi: bool, payload: &[u8]) -> [u8; 188] {
        let mut p = [0xFFu8; 188];
        p[0] = 0x47;
        p[1] = if pusi { 0x40 } else { 0 } | ((pid >> 8) as u8 & 0x1F);
        p[2] = pid as u8;
        p[3] = 0x10; // payload only
        let take = payload.len().min(184);
        p[4..4 + take].copy_from_slice(&payload[..take]);
        p
    }

    fn pes_audio(pts: u64, es: &[u8]) -> Vec<u8> {
        let pes_len = (8 + es.len()) as u16;
        let mut out = vec![0, 0, 1, 0xC0, (pes_len >> 8) as u8, pes_len as u8, 0x80, 0x80, 5];
        out.push(0x21 | ((((pts >> 30) & 0x07) as u8) << 1));
        out.push(((pts >> 22) & 0xFF) as u8);
        out.push(0x01 | ((((pts >> 15) & 0x7F) as u8) << 1));
        out.push(((pts >> 7) & 0xFF) as u8);
        out.push(0x01 | (((pts & 0x7F) as u8) << 1));
        out.extend_from_slice(es);
        out
    }

    #[test]
    fn g711_decodes_silence_and_polarity() {
        assert!(decode_alaw(0xD5).abs() <= 16);
        assert!(decode_ulaw(0xFF).abs() <= 16);
        assert!(decode_alaw(0xAA) > 0);
        assert!(decode_alaw(0x2A) < 0);
        assert!(decode_ulaw(0x80) > 0);
        assert!(decode_ulaw(0x00) < 0);
    }

    #[test]
    fn demuxes_pcma_and_detects_wideband_rate() {
        // PAT pointing program 1 -> PMT PID 0x1000
        let pat = [0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0, 0, 0, 0];
        // PMT listing stream_type 0x90 (PCMA) on PID 0x0101
        let pmt = [
            0x00, 0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00, 0x90, 0xE1, 0x01,
            0xF0, 0x00, 0, 0, 0, 0,
        ];
        let mut stream = Vec::new();
        stream.extend_from_slice(&ts_packet(0, true, &pat));
        stream.extend_from_slice(&ts_packet(0x1000, true, &pmt));
        // 160 bytes every 10 ms (900 ticks at 90 kHz) => 16 kHz wideband G.711
        for i in 0..4u64 {
            let pes = pes_audio(i * 900, &[0xAAu8; 160]);
            stream.extend_from_slice(&ts_packet(0x0101, true, &pes));
        }

        let mut demux = TsAudioDemuxer::new();
        let mut chunks = Vec::new();
        demux.feed(&stream, &mut chunks);
        assert_eq!(demux.codec, Some(AudioCodec::Pcma));
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.last().unwrap().sample_rate, 16_000);
        assert_eq!(chunks.last().unwrap().samples.len(), 160);
    }

    #[test]
    fn demuxes_aac_from_relay_fixture() {
        let ts = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/relay-sample.ts")).unwrap();
        let mut demux = TsAudioDemuxer::new();
        let mut chunks = Vec::new();
        demux.feed(&ts, &mut chunks);
        assert_eq!(demux.codec, Some(AudioCodec::Aac));
        assert!(!chunks.is_empty(), "expected decoded AAC PCM chunks from relay-sample.ts");
        assert!(chunks.iter().map(|c| c.samples.len()).sum::<usize>() > 1000);
    }
}
