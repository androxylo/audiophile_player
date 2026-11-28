// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Core first-class REW stereo convolution stage (`352.8 / 384 kHz` pure `f64`, block-streaming).
//!
//! ## Pipeline Position (INV-05)
//! REW stereo convolution executes in pure IEEE 754 `f64` **strictly after** dynamic upsampling
//! (`352.8 / 384 kHz`) and **strictly before** per-channel delay and the single final noise-shaped
//! `f64 -> i32` quantizer. Placing convolution before noise shaping preserves the exact ultrasonic
//! Noise Transfer Function (NTF) and avoids double quantization.
//!
//! ## Algorithm & Headroom Safety
//! Uniform Partitioned Overlap-Save (**UPOLS**) using `realfft` (`f64`, AVX2/FMA SIMD,
//! MIT/Apache-2.0 — GPL-3.0 compatible). Supports arbitrary-length stereo WAV impulse responses
//! including full `2^22 = 4,194,304`-tap REW filters at `352.8 / 384 kHz` with bit-exact
//! streaming equivalence across arbitrary block sizes.
//!
//! Per the decided headroom design (`HEADROOM_DESIGN.md`), REW impulse responses are **never
//! normalized at load time** so positive EQ boosts fill hardware/room dips on purpose. Peak
//! frequency magnitude `G_c = max |H(f)|` is measured with a `>= 2x` zero-padded real FFT across
//! both channels (uncapped), and per-track convolution output is scaled in `f64` **only** by the
//! excess of `P + A_isp + G_up + max(G_c, 0)` over `0 dBFS` (or `M + G_up` when a full-scan peak
//! `M` exists). Time-domain `L1` (`sum |h[n]|`) is computed for diagnostic logging only.
//!
//! ## Configuration & Folder Discovery
//! Convolution is **disabled by default** ([`ConvolutionConfig::disabled()`]) until explicitly
//! configured via `config.toml` or CLI flags (`--conv-dir` / `--conv-file`). When `dir` is set,
//! [`discover_latest_rew_wav`] performs a fast header-only probe and selects the **most recent
//! 2-channel `.wav` by filesystem modification time (`mtime`)** matching the active sample rate
//! (ignoring mono `.L`/`.R` helper files). Note: EqualizerAPO's `config.txt` (`Include:` /
//! `Convolution:` / `Preamp:` / `Delay:`) is **not** parsed; use `file_352800` / `file_384000`
//! to pin a specific WAV when multiple files share a directory.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ap_core::buffer::StereoF64Buffer;
use ap_core::config::ConvolutionConfig;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Minimum and maximum UPOLS partition block sizes (`B`, FFT size `N = 2 * B`).
const MIN_PARTITION_BLOCK: usize = 1_024;
const MAX_PARTITION_BLOCK: usize = 65_536;

/// Maximum supported impulse-response length (`2^22 = 4,194,304` taps per channel, matching
/// `REQUIREMENTS.md` §5.3's 4M-tap REW export cap). Prevents oversized or stray WAV files in a
/// watched convolution directory from exhausting RAM or aborting on FFT/spectrum allocation.
pub const MAX_IR_TAPS: usize = 1 << 22;

/// Hard cap on WAV `data` payload bytes (`MAX_IR_TAPS` stereo `f64` frames = 64 MiB).
pub const MAX_IR_DATA_BYTES: u64 = (MAX_IR_TAPS as u64) * 16;

/// Dispersive-filter warning threshold (`HEADROOM_DESIGN.md` §6): warn when `L1 - G_c > 6.0 dB`
/// so the user knows an excess-phase filter may exceed the `max |H(f)|` peak prediction.
pub const DISPERSIVE_FILTER_WARNING_THRESHOLD_DB: f64 = 6.0;

/// Compute a deterministic 128-bit FNV-1a content fingerprint (`fnv1a128:<32-hex>`) over the
/// exact loaded stereo `f64` IR taps and sample rate for `HEADROOM_DESIGN.md` §4 `post_chain_peaks`
/// invalidation whenever an IR changes.
fn compute_ir_content_hash(sample_rate_hz: u32, left: &[f64], right: &[f64]) -> String {
    const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut h = FNV_OFFSET;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u128::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    };
    feed(&sample_rate_hz.to_le_bytes());
    feed(&(left.len() as u64).to_le_bytes());
    for (&l, &r) in left.iter().zip(right.iter()) {
        // Canonicalize -0.0 -> +0.0 so semantically identical zero taps hash identically.
        let lb = if l == 0.0 { 0.0_f64.to_bits() } else { l.to_bits() };
        let rb = if r == 0.0 { 0.0_f64.to_bits() } else { r.to_bits() };
        feed(&lb.to_le_bytes());
        feed(&rb.to_le_bytes());
    }
    format!("fnv1a128:{h:032x}")
}

/// Flush-tail threshold (`-200 dB`, 56 dB below 24-bit PCM) applied only when an IR exceeds
/// `MAX_PARTITION_BLOCK` taps in normal mode (`full_tail = false`), preventing multi-second
/// sub-`-250 dB` float noise gaps between tracks while convolving 100% of all non-zero taps
/// during active playback. Ultimate mode sets `full_tail = true` (U7: zero tail truncation).
const LONG_IR_FLUSH_THRESHOLD: f64 = 1e-10;

/// Minimum non-zero UPOLS partitions before spawning bit-identical bin-slice MAC threads per channel.
const REW_PARALLEL_MAC_MIN_PARTITIONS: usize = 16;

/// Number of disjoint frequency-bin worker slices per channel for large REW IRs.
const REW_PARALLEL_MAC_SLICES: usize = 4;

/// Microsoft `WAVE_FORMAT_EXTENSIBLE` SubFormat GUIDs (first 16 bytes).
const KSDATAFORMAT_SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Residual stereo convolution impulse tail carried over across consecutive equal-rate tracks
/// for gapless album continuity (overlap-added in `f64` onto the next track's opening samples).
#[derive(Debug, Clone)]
pub struct ConvolutionTail {
    pub sample_rate_hz: u32,
    pub left: Vec<f64>,
    pub right: Vec<f64>,
}

impl ConvolutionTail {
    /// Maximum absolute sample amplitude across both channels (`0.0` if empty).
    pub fn peak_linear(&self) -> f64 {
        let max_l = self
            .left
            .iter()
            .copied()
            .map(f64::abs)
            .fold(0.0_f64, f64::max);
        let max_r = self
            .right
            .iter()
            .copied()
            .map(f64::abs)
            .fold(0.0_f64, f64::max);
        max_l.max(max_r)
    }
}

/// Read-only verification snapshot of an active REW stereo impulse response
/// (`PIPELINE_TESTS_REVIEW.md` §2.3, exposed only under `test` or `feature = "verify"`).
#[cfg(any(test, feature = "verify"))]
#[derive(Debug, Clone, Copy)]
pub struct ConvolverIrExport<'a> {
    pub sample_rate_hz: u32,
    pub left: &'a [f64],
    pub right: &'a [f64],
    pub flush_tail_samples: usize,
}

/// Pluggable stateful stereo convolution stage operating strictly in IEEE 754 `f64`.
pub trait StereoConvolver: Send {
    fn name(&self) -> &'static str;

    /// Read-only access to the active stereo impulse response for independent null verification.
    #[cfg(any(test, feature = "verify"))]
    fn ir(&self) -> Option<ConvolverIrExport<'_>> {
        None
    }

    /// Human-readable description (e.g., active REW WAV filename & tap count) for logs/UI.
    fn description(&self) -> String {
        self.name().to_string()
    }

    /// Maximum positive frequency-domain gain boost `max(G_c, 0)` in dB (uncapped).
    ///
    /// REW IRs are never normalized at load so filter boosts fill hardware dips on purpose;
    /// per-track convolution scaling is computed solely from the excess over `0 dBFS`.
    fn max_boost_db(&self) -> f64;

    /// Frequency-domain peak gain bound `max(G_c, 0)` in dB (L1 is diagnostic only).
    fn peak_gain_bound_db(&self) -> f64 {
        self.max_boost_db()
    }

    /// True if this convolver actively consumes and overlap-adds an incoming [`ConvolutionTail`].
    /// (`false` for [`NullConvolver`], `true` for [`PartitionedStereoConvolver`].)
    fn accepts_tail(&self) -> bool {
        false
    }

    /// Deterministic content hash of the active REW IR (`None` for [`NullConvolver`]), used to
    /// key and invalidate measured `post_chain_peaks` entries (`HEADROOM_DESIGN.md` §4).
    fn content_hash(&self) -> Option<&str> {
        None
    }

    /// Apply per-track excess-only linear attenuation (`<= 1.0`, never boosts) to this track's
    /// newly convolved `f64` output without altering any already-scaled incoming tail.
    fn set_output_scale(&mut self, _scale_linear: f64) {}

    /// Attach a residual impulse tail from the previous equal-rate track to overlap-add onto this
    /// track's opening output in `f64` (strictly before delay & final quantization).
    fn set_incoming_tail(&mut self, _tail: ConvolutionTail) {}

    /// Process one bounded `f64` stereo block (UPOLS overlap state kept inside `self`).
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;

    /// Flush remaining convolution impulse tail at end-of-track (emits full decaying tail).
    fn flush(&mut self) -> Result<StereoF64Buffer, String>;

    /// Flush only the samples needed to complete the exact input track length, returning the
    /// remaining decaying impulse tail separately so the next equal-rate track can overlap-add it
    /// gaplessly without inserting a multi-second silence gap between tracks.
    fn flush_split_tail(&mut self) -> Result<(StereoF64Buffer, Option<ConvolutionTail>), String> {
        Ok((self.flush()?, None))
    }

    /// Flush end-of-track convolver output for gapless handoff while also feeding an upsampler
    /// post-ringing tail (`Review #10 CQ1`): splits at the exact track boundary BEFORE `post_ring`
    /// so `track_end` completes Track A's exact length and `next_tail` carries the complete
    /// zero-driven continuation `(x_A ‖ 0) * h_up * h_conv` starting at Track B sample 0.
    fn flush_split_tail_with_post_ring(
        &mut self,
        post_ring: StereoF64Buffer,
    ) -> Result<(StereoF64Buffer, Option<ConvolutionTail>), String> {
        let track_end = self.flush()?;
        let next_tail = if post_ring.is_empty() {
            None
        } else {
            Some(ConvolutionTail {
                sample_rate_hz: post_ring.sample_rate_hz,
                left: post_ring.left,
                right: post_ring.right,
            })
        };
        Ok((track_end, next_tail))
    }
}

/// Factory creating a fresh stateful [`StereoConvolver`] instance per track/rate.
pub trait StereoConvolverFactory: Send + Sync {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String>;
}

/// Explicit zero-copy null convolver (`StereoF64Buffer -> StereoF64Buffer`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullConvolver {
    sample_rate_hz: u32,
}

impl StereoConvolver for NullConvolver {
    fn name(&self) -> &'static str {
        "null-convolver"
    }

    fn max_boost_db(&self) -> f64 {
        0.0
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch in convolver".to_string());
        }
        self.sample_rate_hz = input.sample_rate_hz;
        Ok(input)
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        Ok(StereoF64Buffer::empty(self.sample_rate_hz))
    }
}

/// Factory returning [`NullConvolver`] for every track/rate.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullConvolverFactory;

impl StereoConvolverFactory for NullConvolverFactory {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String> {
        Ok(Box::new(NullConvolver {
            sample_rate_hz: target_rate_hz,
        }))
    }
}

/// Sample encoding inside a RIFF WAV impulse response file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavSampleEncoding {
    PcmInt,
    IeeeFloat,
}

/// Lightweight RIFF WAV metadata obtained by reading only headers (never audio payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavHeaderInfo {
    pub encoding: WavSampleEncoding,
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub bits_per_sample: u16,
    pub num_frames: usize,
    pub data_offset: u64,
    pub data_bytes: u64,
}

/// Decoded stereo impulse response loaded into pure `f64`.
#[derive(Debug, Clone)]
pub struct StereoImpulseWav {
    pub label: String,
    pub sample_rate_hz: u32,
    pub left: Vec<f64>,
    pub right: Vec<f64>,
}

/// Decoded mono impulse response loaded into pure `f64` (e.g. single-channel REW IR WAV export).
#[derive(Debug, Clone)]
pub struct MonoImpulseWav {
    pub label: String,
    pub sample_rate_hz: u32,
    pub samples: Vec<f64>,
}

/// Output floating-point sample encoding for [`write_stereo_wav`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavOutputFormat {
    /// 32-bit IEEE 754 float (`WAVE_FORMAT_IEEE_FLOAT`, 8 bytes/stereo frame).
    Float32,
    /// 64-bit IEEE 754 float (`WAVE_FORMAT_IEEE_FLOAT`, 16 bytes/stereo frame).
    Float64,
}

/// Exact frequency-domain peak gain (`G_c`) and time-domain `L1` diagnostics for a stereo IR,
/// computed identically to [`PreparedStereoIr::from_stereo_ir`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StereoIrGainMetrics {
    pub active_taps: usize,
    pub max_gain_linear: f64,
    pub max_boost_db: f64,
    pub l1_norm: f64,
    pub l1_norm_db: f64,
}

/// Fast header-only RIFF WAV probe (reads only chunk headers, skipping audio payload).
pub fn probe_wav_header(path: &Path) -> Result<WavHeaderInfo, String> {
    let mut file = File::open(path)
        .map_err(|e| format!("failed to open WAV '{}': {e}", path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|e| format!("failed to stat WAV '{}': {e}", path.display()))?
        .len();

    let mut riff_hdr = [0u8; 12];
    file.read_exact(&mut riff_hdr)
        .map_err(|e| format!("truncated RIFF header in '{}': {e}", path.display()))?;
    if &riff_hdr[0..4] != b"RIFF" || &riff_hdr[8..12] != b"WAVE" {
        return Err(format!("not a standard RIFF/WAVE file: '{}'", path.display()));
    }
    let riff_size = u32::from_le_bytes([riff_hdr[4], riff_hdr[5], riff_hdr[6], riff_hdr[7]]);

    let mut fmt_parsed: Option<(WavSampleEncoding, u16, u32, u16)> = None;
    let mut data_info: Option<(u64, u64)> = None;

    let mut pos: u64 = 12;
    while pos + 8 <= file_len {
        file.seek(SeekFrom::Start(pos))
            .map_err(|e| format!("seek failed in '{}': {e}", path.display()))?;
        let mut chunk_hdr = [0u8; 8];
        if file.read_exact(&mut chunk_hdr).is_err() {
            break;
        }
        let chunk_id = &chunk_hdr[0..4];
        let chunk_size = u32::from_le_bytes([
            chunk_hdr[4],
            chunk_hdr[5],
            chunk_hdr[6],
            chunk_hdr[7],
        ]) as u64;
        let payload_start = pos + 8;

        if chunk_id == b"fmt " {
            if chunk_size < 16 || payload_start + chunk_size > file_len {
                return Err(format!("invalid fmt chunk in '{}'", path.display()));
            }
            let read_len = chunk_size.min(64) as usize;
            let mut fmt_buf = vec![0u8; read_len];
            file.read_exact(&mut fmt_buf)
                .map_err(|e| format!("failed to read fmt chunk in '{}': {e}", path.display()))?;

            let fmt_tag = u16::from_le_bytes([fmt_buf[0], fmt_buf[1]]);
            let channels = u16::from_le_bytes([fmt_buf[2], fmt_buf[3]]);
            let sample_rate_hz =
                u32::from_le_bytes([fmt_buf[4], fmt_buf[5], fmt_buf[6], fmt_buf[7]]);
            let bits_per_sample = u16::from_le_bytes([fmt_buf[14], fmt_buf[15]]);

            let encoding = match fmt_tag {
                1 => WavSampleEncoding::PcmInt,
                3 => WavSampleEncoding::IeeeFloat,
                0xFFFE => {
                    if fmt_buf.len() < 40 {
                        return Err(format!(
                            "truncated WAVE_FORMAT_EXTENSIBLE header in '{}'",
                            path.display()
                        ));
                    }
                    let subformat = &fmt_buf[24..40];
                    if subformat == KSDATAFORMAT_SUBTYPE_PCM {
                        WavSampleEncoding::PcmInt
                    } else if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                        WavSampleEncoding::IeeeFloat
                    } else {
                        return Err(format!(
                            "unsupported WAVE_FORMAT_EXTENSIBLE subformat in '{}'",
                            path.display()
                        ));
                    }
                }
                other => {
                    return Err(format!(
                        "unsupported WAV format tag 0x{other:04x} in '{}'",
                        path.display()
                    ));
                }
            };

            fmt_parsed = Some((encoding, channels, sample_rate_hz, bits_per_sample));
        } else if chunk_id == b"data" {
            let remaining = file_len.saturating_sub(payload_start);
            let data_bytes = if chunk_size == u32::MAX as u64 {
                // Accept 0xFFFFFFFF streaming/RF64 placeholder ONLY when the RIFF header size is
                // also 0xFFFFFFFF and `fmt ` has already been parsed (making `data` the last chunk).
                if riff_size == u32::MAX && fmt_parsed.is_some() {
                    remaining
                } else {
                    return Err(format!(
                        "invalid streaming placeholder data chunk in '{}'",
                        path.display()
                    ));
                }
            } else if chunk_size == 0 {
                // A 0-byte data chunk is treated as genuinely empty (never "rest of file") so
                // trailing LIST/id3 chunks or in-progress unfinalized exports are never parsed
                // as IR audio taps, and scanning advances cleanly if `fmt ` appears later.
                0
            } else if chunk_size > remaining {
                return Err(format!(
                    "truncated WAV '{}': data chunk declares {} bytes, but only {} bytes remain",
                    path.display(),
                    chunk_size,
                    remaining
                ));
            } else {
                chunk_size
            };
            data_info = Some((payload_start, data_bytes));
            if fmt_parsed.is_some() {
                break;
            }
        }

        // RIFF chunks are padded to an even byte boundary.
        pos = payload_start
            .saturating_add(chunk_size)
            .saturating_add(chunk_size & 1);
    }

    let (encoding, channels, sample_rate_hz, bits_per_sample) =
        fmt_parsed.ok_or_else(|| format!("missing fmt chunk in '{}'", path.display()))?;
    let (data_offset, data_bytes) =
        data_info.ok_or_else(|| format!("missing data chunk in '{}'", path.display()))?;

    if channels == 0 || sample_rate_hz == 0 || bits_per_sample == 0 || bits_per_sample % 8 != 0 {
        return Err(format!(
            "invalid WAV parameters ({channels}ch, {sample_rate_hz} Hz, {bits_per_sample}-bit) in '{}'",
            path.display()
        ));
    }

    if data_bytes > MAX_IR_DATA_BYTES {
        return Err(format!(
            "REW WAV '{}' data payload ({data_bytes} bytes) exceeds maximum supported limit ({MAX_IR_DATA_BYTES} bytes / {MAX_IR_TAPS} taps)",
            path.display()
        ));
    }

    let bytes_per_sample = (bits_per_sample / 8) as u64;
    let frame_bytes = bytes_per_sample * (channels as u64);
    let num_frames_u64 = data_bytes / frame_bytes;
    if num_frames_u64 > MAX_IR_TAPS as u64 {
        return Err(format!(
            "REW WAV '{}' length ({num_frames_u64} taps) exceeds maximum supported IR length ({MAX_IR_TAPS} taps)",
            path.display()
        ));
    }
    let num_frames = num_frames_u64 as usize;

    Ok(WavHeaderInfo {
        encoding,
        channels,
        sample_rate_hz,
        bits_per_sample,
        num_frames,
        data_offset,
        data_bytes,
    })
}

/// Load a 2-channel RIFF WAV impulse response directly into IEEE 754 `f64`.
pub fn load_stereo_wav(path: &Path) -> Result<StereoImpulseWav, String> {
    let info = probe_wav_header(path)?;
    if info.channels != 2 {
        return Err(format!(
            "REW convolution requires a 2-channel stereo WAV, got {} channels in '{}'",
            info.channels,
            path.display()
        ));
    }
    if info.num_frames == 0 {
        return Err(format!("REW WAV '{}' contains 0 frames", path.display()));
    }
    if info.num_frames > MAX_IR_TAPS || info.data_bytes > MAX_IR_DATA_BYTES {
        return Err(format!(
            "REW WAV '{}' exceeds maximum supported IR length ({MAX_IR_TAPS} taps)",
            path.display()
        ));
    }

    let mut file = File::open(path)
        .map_err(|e| format!("failed to open WAV '{}': {e}", path.display()))?;
    file.seek(SeekFrom::Start(info.data_offset))
        .map_err(|e| format!("failed to seek to data chunk in '{}': {e}", path.display()))?;

    let bytes_per_sample = (info.bits_per_sample / 8) as usize;
    let frame_bytes = bytes_per_sample * 2;
    let payload_len = info.num_frames * frame_bytes;
    let mut raw = vec![0u8; payload_len];
    file.read_exact(&mut raw)
        .map_err(|e| format!("failed to read WAV payload in '{}': {e}", path.display()))?;

    let mut left = Vec::with_capacity(info.num_frames);
    let mut right = Vec::with_capacity(info.num_frames);

    match (info.encoding, info.bits_per_sample) {
        (WavSampleEncoding::IeeeFloat, 32) => {
            for frame in raw.chunks_exact(8) {
                let l = f32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as f64;
                let r = f32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) as f64;
                if !l.is_finite() || !r.is_finite() {
                    return Err(format!(
                        "non-finite float sample detected in WAV '{}'",
                        path.display()
                    ));
                }
                left.push(l);
                right.push(r);
            }
        }
        (WavSampleEncoding::IeeeFloat, 64) => {
            for frame in raw.chunks_exact(16) {
                let l = f64::from_le_bytes(frame[0..8].try_into().unwrap());
                let r = f64::from_le_bytes(frame[8..16].try_into().unwrap());
                if !l.is_finite() || !r.is_finite() {
                    return Err(format!(
                        "non-finite f64 sample detected in WAV '{}'",
                        path.display()
                    ));
                }
                left.push(l);
                right.push(r);
            }
        }
        (WavSampleEncoding::PcmInt, 16) => {
            const SCALE: f64 = 1.0 / 32_768.0;
            for frame in raw.chunks_exact(4) {
                left.push(i16::from_le_bytes([frame[0], frame[1]]) as f64 * SCALE);
                right.push(i16::from_le_bytes([frame[2], frame[3]]) as f64 * SCALE);
            }
        }
        (WavSampleEncoding::PcmInt, 24) => {
            const SCALE: f64 = 1.0 / 8_388_608.0;
            for frame in raw.chunks_exact(6) {
                let l = ((i32::from_le_bytes([0, frame[0], frame[1], frame[2]])) >> 8) as f64;
                let r = ((i32::from_le_bytes([0, frame[3], frame[4], frame[5]])) >> 8) as f64;
                left.push(l * SCALE);
                right.push(r * SCALE);
            }
        }
        (WavSampleEncoding::PcmInt, 32) => {
            const SCALE: f64 = 1.0 / 2_147_483_648.0;
            for frame in raw.chunks_exact(8) {
                let l = i32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as f64;
                let r = i32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]) as f64;
                left.push(l * SCALE);
                right.push(r * SCALE);
            }
        }
        (enc, bits) => {
            return Err(format!(
                "unsupported WAV format ({enc:?}, {bits}-bit) in '{}'",
                path.display()
            ));
        }
    }

    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    Ok(StereoImpulseWav {
        label,
        sample_rate_hz: info.sample_rate_hz,
        left,
        right,
    })
}

/// Load a 1-channel mono RIFF WAV impulse response directly into IEEE 754 `f64`.
pub fn load_mono_wav(path: &Path) -> Result<MonoImpulseWav, String> {
    let info = probe_wav_header(path)?;
    if info.channels != 1 {
        return Err(format!(
            "mono WAV loader requires a 1-channel WAV, got {} channels in '{}'",
            info.channels,
            path.display()
        ));
    }
    if info.num_frames == 0 {
        return Err(format!("mono WAV '{}' contains 0 frames", path.display()));
    }
    if info.num_frames > MAX_IR_TAPS || info.data_bytes > MAX_IR_DATA_BYTES {
        return Err(format!(
            "mono WAV '{}' exceeds maximum supported IR length ({MAX_IR_TAPS} taps)",
            path.display()
        ));
    }

    let mut file = File::open(path)
        .map_err(|e| format!("failed to open WAV '{}': {e}", path.display()))?;
    file.seek(SeekFrom::Start(info.data_offset))
        .map_err(|e| format!("failed to seek to data chunk in '{}': {e}", path.display()))?;

    let bytes_per_sample = (info.bits_per_sample / 8) as usize;
    let payload_len = info.num_frames * bytes_per_sample;
    let mut raw = vec![0u8; payload_len];
    file.read_exact(&mut raw)
        .map_err(|e| format!("failed to read WAV payload in '{}': {e}", path.display()))?;

    let mut samples = Vec::with_capacity(info.num_frames);
    match (info.encoding, info.bits_per_sample) {
        (WavSampleEncoding::IeeeFloat, 32) => {
            for s in raw.chunks_exact(4) {
                let v = f32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f64;
                if !v.is_finite() {
                    return Err(format!(
                        "non-finite float sample detected in WAV '{}'",
                        path.display()
                    ));
                }
                samples.push(v);
            }
        }
        (WavSampleEncoding::IeeeFloat, 64) => {
            for s in raw.chunks_exact(8) {
                let v = f64::from_le_bytes(s.try_into().unwrap());
                if !v.is_finite() {
                    return Err(format!(
                        "non-finite f64 sample detected in WAV '{}'",
                        path.display()
                    ));
                }
                samples.push(v);
            }
        }
        (WavSampleEncoding::PcmInt, 16) => {
            const SCALE: f64 = 1.0 / 32_768.0;
            for s in raw.chunks_exact(2) {
                samples.push(i16::from_le_bytes([s[0], s[1]]) as f64 * SCALE);
            }
        }
        (WavSampleEncoding::PcmInt, 24) => {
            const SCALE: f64 = 1.0 / 8_388_608.0;
            for s in raw.chunks_exact(3) {
                let v = ((i32::from_le_bytes([0, s[0], s[1], s[2]])) >> 8) as f64;
                samples.push(v * SCALE);
            }
        }
        (WavSampleEncoding::PcmInt, 32) => {
            const SCALE: f64 = 1.0 / 2_147_483_648.0;
            for s in raw.chunks_exact(4) {
                samples.push(i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f64 * SCALE);
            }
        }
        (enc, bits) => {
            return Err(format!(
                "unsupported WAV format ({enc:?}, {bits}-bit) in '{}'",
                path.display()
            ));
        }
    }

    let label = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    Ok(MonoImpulseWav {
        label,
        sample_rate_hz: info.sample_rate_hz,
        samples,
    })
}

/// Write a 2-channel stereo IEEE float RIFF/WAVE impulse response (`32-bit` or `64-bit` float).
///
/// The emitted file satisfies every [`probe_wav_header`] and [`load_stereo_wav`] invariant,
/// including the [`MAX_IR_TAPS`] length ceiling and finiteness validation.
pub fn write_stereo_wav(
    path: &Path,
    sample_rate_hz: u32,
    left: &[f64],
    right: &[f64],
    format: WavOutputFormat,
) -> Result<(), String> {
    if left.len() != right.len() {
        return Err(format!(
            "stereo WAV write L/R length mismatch ({} vs {})",
            left.len(),
            right.len()
        ));
    }
    if left.is_empty() {
        return Err("stereo WAV write cannot write 0 frames".to_string());
    }
    if sample_rate_hz == 0 {
        return Err("stereo WAV write sample_rate_hz must be > 0".to_string());
    }
    if left.len() > MAX_IR_TAPS {
        return Err(format!(
            "stereo WAV write length ({} taps) exceeds maximum supported IR length ({MAX_IR_TAPS} taps)",
            left.len()
        ));
    }
    if left.iter().any(|v| !v.is_finite()) || right.iter().any(|v| !v.is_finite()) {
        return Err(format!(
            "stereo WAV write '{}' contains non-finite (NaN/Inf) samples",
            path.display()
        ));
    }

    let (bits_per_sample, bytes_per_sample): (u16, u64) = match format {
        WavOutputFormat::Float32 => (32, 4),
        WavOutputFormat::Float64 => (64, 8),
    };
    let channels: u16 = 2;
    let block_align = channels * (bytes_per_sample as u16);
    let byte_rate = sample_rate_hz
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| "stereo WAV byte_rate overflow".to_string())?;
    let data_bytes = (left.len() as u64)
        .checked_mul(u64::from(block_align))
        .ok_or_else(|| "stereo WAV data_bytes overflow".to_string())?;
    let riff_size_u64 = 4u64 + (8 + 16) + (8 + data_bytes);
    let data_bytes_u32 = u32::try_from(data_bytes)
        .map_err(|_| "stereo WAV data chunk exceeds 4 GiB RIFF limit".to_string())?;
    let riff_size_u32 = u32::try_from(riff_size_u64)
        .map_err(|_| "stereo WAV RIFF size exceeds 4 GiB limit".to_string())?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create WAV output dir '{}': {e}", parent.display()))?;
    }

    let file = File::create(path)
        .map_err(|e| format!("failed to create WAV '{}': {e}", path.display()))?;
    let mut w = BufWriter::new(file);

    w.write_all(b"RIFF")
        .and_then(|_| w.write_all(&riff_size_u32.to_le_bytes()))
        .and_then(|_| w.write_all(b"WAVE"))
        .and_then(|_| w.write_all(b"fmt "))
        .and_then(|_| w.write_all(&16u32.to_le_bytes()))
        .and_then(|_| w.write_all(&3u16.to_le_bytes())) // WAVE_FORMAT_IEEE_FLOAT
        .and_then(|_| w.write_all(&channels.to_le_bytes()))
        .and_then(|_| w.write_all(&sample_rate_hz.to_le_bytes()))
        .and_then(|_| w.write_all(&byte_rate.to_le_bytes()))
        .and_then(|_| w.write_all(&block_align.to_le_bytes()))
        .and_then(|_| w.write_all(&bits_per_sample.to_le_bytes()))
        .and_then(|_| w.write_all(b"data"))
        .and_then(|_| w.write_all(&data_bytes_u32.to_le_bytes()))
        .map_err(|e| format!("failed to write WAV header '{}': {e}", path.display()))?;

    match format {
        WavOutputFormat::Float32 => {
            for (&l, &r) in left.iter().zip(right.iter()) {
                let lf = l as f32;
                let rf = r as f32;
                if !lf.is_finite() || !rf.is_finite() {
                    return Err(format!(
                        "stereo WAV write '{}' f32 conversion overflowed to non-finite",
                        path.display()
                    ));
                }
                w.write_all(&lf.to_le_bytes())
                    .and_then(|_| w.write_all(&rf.to_le_bytes()))
                    .map_err(|e| format!("failed to write f32 WAV frame '{}': {e}", path.display()))?;
            }
        }
        WavOutputFormat::Float64 => {
            for (&l, &r) in left.iter().zip(right.iter()) {
                w.write_all(&l.to_le_bytes())
                    .and_then(|_| w.write_all(&r.to_le_bytes()))
                    .map_err(|e| format!("failed to write f64 WAV frame '{}': {e}", path.display()))?;
            }
        }
    }

    w.flush()
        .map_err(|e| format!("failed to flush WAV '{}': {e}", path.display()))
}

/// Measure exact peak frequency-domain magnitude (`G_c = max |H(f)|` via `>= 2x` zero-padded RFFT)
/// and time-domain `L1` norm for a stereo IR, matching [`PreparedStereoIr::from_stereo_ir`] identically.
pub fn measure_stereo_ir_gains(
    left: &[f64],
    right: &[f64],
) -> Result<StereoIrGainMetrics, String> {
    if left.len() != right.len() {
        return Err("stereo IR L/R length mismatch".to_string());
    }
    if left.is_empty() {
        return Err("stereo IR cannot be empty".to_string());
    }
    if left.iter().any(|v| !v.is_finite()) || right.iter().any(|v| !v.is_finite()) {
        return Err("stereo IR contains non-finite (NaN/Inf) samples".to_string());
    }
    let Some(last_nz) = left
        .iter()
        .zip(right.iter())
        .rposition(|(&l, &r)| l != 0.0 || r != 0.0)
    else {
        return Err("stereo IR is all zeros (silent impulse response rejected)".to_string());
    };
    let active_taps = last_nz + 1;
    let mut planner = RealFftPlanner::<f64>::new();
    let max_gain_linear =
        compute_max_freq_gain_linear(&mut planner, &left[..active_taps], &right[..active_taps])?;
    let l1_left: f64 = left[..active_taps].iter().map(|v| v.abs()).sum();
    let l1_right: f64 = right[..active_taps].iter().map(|v| v.abs()).sum();
    let l1_norm = l1_left.max(l1_right);
    let max_boost_db = if max_gain_linear > 1.0 + 1e-9 {
        20.0 * max_gain_linear.log10()
    } else {
        0.0
    };
    let l1_norm_db = if l1_norm > 1.0 {
        20.0 * l1_norm.log10()
    } else {
        0.0
    };
    Ok(StereoIrGainMetrics {
        active_taps,
        max_gain_linear,
        max_boost_db,
        l1_norm,
        l1_norm_db,
    })
}

/// Candidate REW stereo WAV found during folder auto-discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredRewFilter {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    pub file_len: u64,
    pub header: WavHeaderInfo,
}

/// Diagnostic record for a `.wav` file skipped during directory discovery due to a probe error
/// (e.g. truncated/incomplete REW export or tap count exceeding [`MAX_IR_TAPS`]).
#[derive(Debug, Clone)]
pub struct SkippedRewWav {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    pub reason: String,
}

fn is_newer_candidate(
    cand_mod: Option<SystemTime>,
    cand_path: &Path,
    prev_mod: Option<SystemTime>,
    prev_path: &Path,
) -> bool {
    match (cand_mod, prev_mod) {
        (Some(a), Some(b)) if a != b => a > b,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => cand_path.file_name() > prev_path.file_name(),
    }
}

/// Discover all rate-matched 2-channel `.wav` candidates in `dir` sorted newest-to-oldest by
/// filesystem modification time (`mtime`, tie-breaking by filename), along with any unreadable or
/// 0-frame rate-matched `.wav` files skipped during probing.
fn discover_rew_wav_candidates_sorted(
    dir: &Path,
    target_rate_hz: u32,
) -> Result<(Vec<DiscoveredRewFilter>, Vec<SkippedRewWav>), String> {
    if !dir.exists() {
        return Ok((Vec::new(), Vec::new()));
    }

    let entries = fs::read_dir(dir)
        .map_err(|e| format!("failed to scan REW filter directory '{}': {e}", dir.display()))?;

    let mut candidates: Vec<DiscoveredRewFilter> = Vec::new();
    let mut skipped: Vec<SkippedRewWav> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let is_wav = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"));
        if !is_wav {
            continue;
        }

        // Follow Windows junctions/symlinks via fs::metadata.
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }

        let modified = meta.modified().ok();
        let header = match probe_wav_header(&path) {
            Ok(h) => h,
            Err(reason) => {
                skipped.push(SkippedRewWav {
                    path,
                    modified,
                    reason,
                });
                continue;
            }
        };
        if header.channels != 2 || header.sample_rate_hz != target_rate_hz {
            continue;
        }
        if header.num_frames == 0 {
            skipped.push(SkippedRewWav {
                path,
                modified,
                reason: "0 frames / unfinalized export".to_string(),
            });
            continue;
        }

        candidates.push(DiscoveredRewFilter {
            path,
            modified,
            file_len: meta.len(),
            header,
        });
    }

    candidates.sort_by(|a, b| {
        if is_newer_candidate(a.modified, &a.path, b.modified, &b.path) {
            std::cmp::Ordering::Less
        } else if is_newer_candidate(b.modified, &b.path, a.modified, &a.path) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });

    Ok((candidates, skipped))
}

/// Auto-discover the **most recent 2-channel `.wav` by filesystem modification time (`mtime`)**
/// in `dir` matching `target_rate_hz`, along with any `.wav` files whose header probe failed or
/// contained 0 frames.
pub fn discover_latest_rew_wav_with_diagnostics(
    dir: &Path,
    target_rate_hz: u32,
) -> Result<(Option<DiscoveredRewFilter>, Vec<SkippedRewWav>), String> {
    let (candidates, skipped) = discover_rew_wav_candidates_sorted(dir, target_rate_hz)?;
    Ok((candidates.into_iter().next(), skipped))
}

/// Auto-discover the **most recent 2-channel `.wav` by filesystem modification time (`mtime`)**
/// in `dir` matching `target_rate_hz`.
///
/// Mono `.L`/`.R` helper files, truncated/unreadable WAVs, and mismatched sample rates are
/// ignored. EqualizerAPO `config.txt` directives are **not** parsed; if multiple rate-matched
/// stereo WAVs exist in `dir`, ties break by `mtime` (then filename), or you can pin an exact
/// filter via `file_352800` / `file_384000` in `config.toml`. Returns `Ok(None)` if `dir` does
/// not exist or contains no matching stereo WAV for `target_rate_hz`.
pub fn discover_latest_rew_wav(
    dir: &Path,
    target_rate_hz: u32,
) -> Result<Option<DiscoveredRewFilter>, String> {
    let (best, _skipped) = discover_latest_rew_wav_with_diagnostics(dir, target_rate_hz)?;
    Ok(best)
}

/// Immutable, pre-transformed stereo IR ready for instant sharing across tracks (`Arc`).
pub struct PreparedStereoIr {
    pub label: String,
    pub sample_rate_hz: u32,
    pub total_taps: usize,
    pub active_taps: usize,
    pub flush_tail_samples: usize,
    pub block_size: usize,
    pub num_partitions: usize,
    /// Measured positive peak frequency-domain boost `max(G_c, 0)` in dB (uncapped; IR is never
    /// normalized at load so boosts fill hardware dips on purpose).
    pub max_boost_db: f64,
    /// Diagnostic time-domain L1 norm `max(sum|h_L|, sum|h_R|)` (never used as a gain criterion).
    pub l1_norm: f64,
    /// Diagnostic L1 norm in dB (`max(0.0, 20 * log10(l1_norm))`; never used as a gain criterion).
    pub l1_norm_db: f64,
    /// Deterministic content hash (`fnv1a128:<32-hex>`) over exact loaded stereo taps and rate.
    pub content_hash: String,
    /// True when constructed in lossless full-tail mode (`U7`: zero `-200 dB` flush truncation).
    pub full_tail: bool,
    r2c: Arc<dyn RealToComplex<f64>>,
    c2r: Arc<dyn ComplexToReal<f64>>,
    ir_spectra_left: Vec<Vec<Complex<f64>>>,
    ir_spectra_right: Vec<Vec<Complex<f64>>>,
    nonzero_left: Vec<bool>,
    nonzero_right: Vec<bool>,
    nonzero_count_left: usize,
    nonzero_count_right: usize,
    #[cfg(any(test, feature = "verify"))]
    raw_ir_left: Vec<f64>,
    #[cfg(any(test, feature = "verify"))]
    raw_ir_right: Vec<f64>,
}

impl std::fmt::Debug for PreparedStereoIr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedStereoIr")
            .field("label", &self.label)
            .field("sample_rate_hz", &self.sample_rate_hz)
            .field("total_taps", &self.total_taps)
            .field("active_taps", &self.active_taps)
            .field("flush_tail_samples", &self.flush_tail_samples)
            .field("block_size", &self.block_size)
            .field("num_partitions", &self.num_partitions)
            .field("max_boost_db", &self.max_boost_db)
            .field("l1_norm", &self.l1_norm)
            .field("l1_norm_db", &self.l1_norm_db)
            .field("content_hash", &self.content_hash)
            .finish()
    }
}

impl PreparedStereoIr {
    /// Prepare UPOLS FFT partitions in pure `f64` keeping IR coefficients verbatim (never
    /// normalized at load per `HEADROOM_DESIGN.md`), measuring peak frequency boost `max(G_c, 0)`
    /// via `>= 2x` zero-padded FFT, diagnostic `L1`, and deterministic content hash.
    pub fn from_stereo_ir(
        label: impl Into<String>,
        sample_rate_hz: u32,
        left: &[f64],
        right: &[f64],
    ) -> Result<Self, String> {
        Self::from_stereo_ir_with_options(label, sample_rate_hz, left, right, false)
    }

    /// Prepare UPOLS FFT partitions with explicit `full_tail` control (`U7` in Ultimate Mode:
    /// when `full_tail == true`, never truncate `flush_tail_samples` at `-200 dB`; skip only
    /// partitions whose taps are literally `0.0`).
    pub fn from_stereo_ir_with_options(
        label: impl Into<String>,
        sample_rate_hz: u32,
        left: &[f64],
        right: &[f64],
        full_tail: bool,
    ) -> Result<Self, String> {
        let label = label.into();
        if left.len() != right.len() {
            return Err("stereo IR L/R length mismatch".to_string());
        }
        if left.is_empty() {
            return Err("stereo IR cannot be empty".to_string());
        }
        if sample_rate_hz == 0 {
            return Err("stereo IR sample_rate_hz must be > 0".to_string());
        }
        if left.iter().any(|v| !v.is_finite()) || right.iter().any(|v| !v.is_finite()) {
            return Err(format!(
                "stereo IR '{label}' contains non-finite (NaN/Inf) samples"
            ));
        }

        let total_taps = left.len();
        let Some(last_nz) = left
            .iter()
            .zip(right.iter())
            .rposition(|(&l, &r)| l != 0.0 || r != 0.0)
        else {
            return Err(format!(
                "stereo IR '{label}' is all zeros (silent impulse response rejected)"
            ));
        };
        let active_taps = last_nz + 1;

        let block_size = active_taps
            .next_power_of_two()
            .clamp(MIN_PARTITION_BLOCK, MAX_PARTITION_BLOCK);
        let fft_len = block_size * 2;
        let spectrum_len = block_size + 1;
        let num_partitions = active_taps.div_ceil(block_size);

        let mut planner = RealFftPlanner::<f64>::new();
        let r2c = planner.plan_fft_forward(fft_len);
        let c2r = planner.plan_fft_inverse(fft_len);

        // Measure exact peak frequency-domain magnitude `G_c = max |H(f)|` via >= 2x zero-padded
        // full-band f64 RFFT. Per HEADROOM_DESIGN.md, the IR is NEVER normalized at load so its
        // boosts fill hardware/room dips on purpose; per-track convolution output is scaled only
        // by the excess over 0 dBFS. Time-domain L1 norm is computed for diagnostics only.
        let raw_max_freq_gain_lin =
            compute_max_freq_gain_linear(&mut planner, &left[..active_taps], &right[..active_taps])?;
        let l1_left: f64 = left[..active_taps].iter().map(|v| v.abs()).sum();
        let l1_right: f64 = right[..active_taps].iter().map(|v| v.abs()).sum();
        let l1_norm = l1_left.max(l1_right);

        let max_boost_db = if raw_max_freq_gain_lin > 1.0 + 1e-9 {
            20.0 * raw_max_freq_gain_lin.log10()
        } else {
            0.0
        };
        let l1_norm_db = if l1_norm > 1.0 {
            20.0 * l1_norm.log10()
        } else {
            0.0
        };

        // Convolve 100% of all non-zero taps during active playback. In ultimate mode (`full_tail`),
        // preserve the complete active impulse tail with zero `-200 dB` truncation (`U7`); in normal
        // mode, trim sub-`-200 dB` flush tails on ultra-long (`> 64k`) IRs.
        let flush_tail_samples = if full_tail || active_taps <= MAX_PARTITION_BLOCK {
            active_taps.saturating_sub(1)
        } else {
            left[..active_taps]
                .iter()
                .zip(right[..active_taps].iter())
                .rposition(|(&l, &r)| {
                    l.abs() > LONG_IR_FLUSH_THRESHOLD || r.abs() > LONG_IR_FLUSH_THRESHOLD
                })
                .unwrap_or(0)
        };

        // Pre-scale IR spectra solely by 1 / N for inverse-FFT normalization (zero IR gain cut).
        let spec_scale = 1.0 / (fft_len as f64);
        let mut scratch = r2c.make_scratch_vec();
        let mut time_buf = vec![0.0_f64; fft_len];

        let mut ir_spectra_left = Vec::with_capacity(num_partitions);
        let mut ir_spectra_right = Vec::with_capacity(num_partitions);
        let mut nonzero_left = Vec::with_capacity(num_partitions);
        let mut nonzero_right = Vec::with_capacity(num_partitions);

        for p in 0..num_partitions {
            let start = p * block_size;
            let end = (start + block_size).min(active_taps);

            // Left partition
            let l_slice = &left[start..end];
            let l_nz = l_slice.iter().any(|&v| v != 0.0);
            nonzero_left.push(l_nz);
            let mut spec_l = vec![Complex::new(0.0, 0.0); spectrum_len];
            if l_nz {
                time_buf.fill(0.0);
                time_buf[..l_slice.len()].copy_from_slice(l_slice);
                r2c.process_with_scratch(&mut time_buf, &mut spec_l, &mut scratch)
                    .map_err(|e| format!("IR left partition FFT failed: {e}"))?;
                for bin in &mut spec_l {
                    bin.re *= spec_scale;
                    bin.im *= spec_scale;
                }
            }
            ir_spectra_left.push(spec_l);

            // Right partition
            let r_slice = &right[start..end];
            let r_nz = r_slice.iter().any(|&v| v != 0.0);
            nonzero_right.push(r_nz);
            let mut spec_r = vec![Complex::new(0.0, 0.0); spectrum_len];
            if r_nz {
                time_buf.fill(0.0);
                time_buf[..r_slice.len()].copy_from_slice(r_slice);
                r2c.process_with_scratch(&mut time_buf, &mut spec_r, &mut scratch)
                    .map_err(|e| format!("IR right partition FFT failed: {e}"))?;
                for bin in &mut spec_r {
                    bin.re *= spec_scale;
                    bin.im *= spec_scale;
                }
            }
            ir_spectra_right.push(spec_r);
        }

        let content_hash = compute_ir_content_hash(sample_rate_hz, left, right);
        let nonzero_count_left = nonzero_left.iter().filter(|&&b| b).count();
        let nonzero_count_right = nonzero_right.iter().filter(|&&b| b).count();

        Ok(Self {
            label,
            sample_rate_hz,
            total_taps,
            active_taps,
            flush_tail_samples,
            block_size,
            num_partitions,
            max_boost_db,
            l1_norm,
            l1_norm_db,
            content_hash,
            full_tail,
            r2c,
            c2r,
            ir_spectra_left,
            ir_spectra_right,
            nonzero_left,
            nonzero_right,
            nonzero_count_left,
            nonzero_count_right,
            #[cfg(any(test, feature = "verify"))]
            raw_ir_left: left.to_vec(),
            #[cfg(any(test, feature = "verify"))]
            raw_ir_right: right.to_vec(),
        })
    }

    pub fn from_wav(wav: &StereoImpulseWav) -> Result<Self, String> {
        Self::from_stereo_ir(&wav.label, wav.sample_rate_hz, &wav.left, &wav.right)
    }

    pub fn from_wav_with_options(wav: &StereoImpulseWav, full_tail: bool) -> Result<Self, String> {
        Self::from_stereo_ir_with_options(
            &wav.label,
            wav.sample_rate_hz,
            &wav.left,
            &wav.right,
            full_tail,
        )
    }
}

/// Evaluate exact DTFT magnitude `|H(omega)| = |sum_{n=0}^{L-1} ir[n] e^{-j omega n}|`
/// in pure `f64` using coupled-form phasor rotation re-anchored every 1024 taps (accurate to
/// machine precision from DC to Nyquist, zero near-DC Goertzel instability).
fn dtft_magnitude(ir: &[f64], omega: f64) -> f64 {
    let step_re = omega.cos();
    let step_im = -omega.sin();
    let mut acc_re = 0.0_f64;
    let mut acc_im = 0.0_f64;
    let mut w_re = 1.0_f64;
    let mut w_im = 0.0_f64;
    for (idx, &h) in ir.iter().enumerate() {
        if idx > 0 && (idx & 1023) == 0 {
            let ang = omega * (idx as f64);
            w_re = ang.cos();
            w_im = -ang.sin();
        }
        acc_re += h * w_re;
        acc_im += h * w_im;
        let next_re = w_re * step_re - w_im * step_im;
        let next_im = w_re * step_im + w_im * step_re;
        w_re = next_re;
        w_im = next_im;
    }
    acc_re.hypot(acc_im)
}

/// Measure peak frequency-domain magnitude `max_f max(|H_L(f)|, |H_R(f)|)` using a `>= 2x`
/// zero-padded real FFT followed by fine DTFT refinement (`+-1` bin to `1/32`-bin resolution)
/// around the strongest spectral peaks (Review #10 CQ3: eliminates inter-bin under-read on
/// narrow high-Q room-mode resonances).
fn compute_max_freq_gain_linear(
    planner: &mut RealFftPlanner<f64>,
    left: &[f64],
    right: &[f64],
) -> Result<f64, String> {
    let n = left
        .len()
        .saturating_mul(2)
        .next_power_of_two()
        .max(65_536);
    let r2c = planner.plan_fft_forward(n);
    let mut time_buf = vec![0.0_f64; n];
    let mut spec = r2c.make_output_vec();
    let mut scratch = r2c.make_scratch_vec();
    let two_pi_over_n = (2.0 * std::f64::consts::PI) / (n as f64);
    let max_bin_f = (spec.len().saturating_sub(1)) as f64;

    let mut max_mag = 0.0_f64;
    for ch in [left, right] {
        time_buf.fill(0.0);
        time_buf[..ch.len()].copy_from_slice(ch);
        r2c.process_with_scratch(&mut time_buf, &mut spec, &mut scratch)
            .map_err(|e| format!("IR gain-analysis FFT failed: {e}"))?;

        let mut ch_coarse_max = 0.0_f64;
        let mut mags: Vec<f64> = Vec::with_capacity(spec.len());
        for c in &spec {
            let mag = c.norm();
            if mag > ch_coarse_max {
                ch_coarse_max = mag;
            }
            mags.push(mag);
        }
        if ch_coarse_max > max_mag {
            max_mag = ch_coarse_max;
        }

        // Collect local spectral maxima within 30% of the channel peak (up to 4 strongest peaks)
        // and refine each with two-stage DTFT evaluation to `1/32`-bin frequency resolution.
        if ch_coarse_max > 0.0 && ch.len() > 1 {
            let thresh = ch_coarse_max * 0.70;
            let mut peaks: Vec<(usize, f64)> = Vec::new();
            let last = mags.len().saturating_sub(1);
            for k in 0..=last {
                let m = mags[k];
                if m < thresh {
                    continue;
                }
                let ge_left = k == 0 || m >= mags[k - 1];
                let ge_right = k == last || m >= mags[k + 1];
                if ge_left && ge_right {
                    peaks.push((k, m));
                }
            }
            peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
            peaks.truncate(4);

            for (peak_k, _) in peaks {
                let center = peak_k as f64;
                // Stage 1: 17 points across [k - 1.0, k + 1.0] (step 0.125 bin).
                let mut best_bin = center;
                let mut best_mag = mags[peak_k];
                for s in -8..=8_i32 {
                    if s == 0 {
                        continue;
                    }
                    let b = (center + (s as f64) * 0.125).clamp(0.0, max_bin_f);
                    let m = dtft_magnitude(ch, b * two_pi_over_n);
                    if m > best_mag {
                        best_mag = m;
                        best_bin = b;
                    }
                }
                // Stage 2: 9 points across [best_bin - 0.125, best_bin + 0.125] (step 1/32 bin).
                let refine_center = best_bin;
                for s in -4..=4_i32 {
                    if s == 0 {
                        continue;
                    }
                    let b = (refine_center + (s as f64) * 0.03125).clamp(0.0, max_bin_f);
                    let m = dtft_magnitude(ch, b * two_pi_over_n);
                    if m > best_mag {
                        best_mag = m;
                    }
                }
                if best_mag > max_mag {
                    max_mag = best_mag;
                }
            }
        }
    }

    Ok(max_mag)
}

/// Per-channel Uniform Partitioned Overlap-Save (UPOLS) streaming state (zero-allocation inner loop).
struct ChannelUpolsState {
    block_size: usize,
    num_partitions: usize,
    fdl: Vec<Vec<Complex<f64>>>,
    fdl_head: usize,
    prev_block: Vec<f64>,
    pending_in: Vec<f64>,
    time_buf: Vec<f64>,
    acc_spec: Vec<Complex<f64>>,
    r2c_scratch: Vec<Complex<f64>>,
    c2r_scratch: Vec<Complex<f64>>,
}

impl ChannelUpolsState {
    fn new(ir: &PreparedStereoIr) -> Self {
        let b = ir.block_size;
        let spectrum_len = b + 1;
        let num_partitions = ir.num_partitions;
        let fdl = (0..num_partitions)
            .map(|_| vec![Complex::new(0.0, 0.0); spectrum_len])
            .collect();

        Self {
            block_size: b,
            num_partitions,
            fdl,
            fdl_head: 0,
            prev_block: vec![0.0; b],
            pending_in: Vec::with_capacity(b),
            time_buf: vec![0.0; b * 2],
            acc_spec: vec![Complex::new(0.0, 0.0); spectrum_len],
            r2c_scratch: ir.r2c.make_scratch_vec(),
            c2r_scratch: ir.c2r.make_scratch_vec(),
        }
    }

    /// Execute one exact `B`-sample UPOLS block (`[prev_block, block] -> y[B..2B]`).
    #[allow(clippy::too_many_arguments)]
    fn process_one_block(
        &mut self,
        block: &[f64],
        r2c: &dyn RealToComplex<f64>,
        c2r: &dyn ComplexToReal<f64>,
        ir_spectra: &[Vec<Complex<f64>>],
        nonzero: &[bool],
        nonzero_count: usize,
        out: &mut Vec<f64>,
    ) -> Result<(), String> {
        let b = self.block_size;
        debug_assert_eq!(block.len(), b);

        // 1. Assemble 2B overlap-save input frame: [x_{m-1}, x_m].
        self.time_buf[..b].copy_from_slice(&self.prev_block);
        self.time_buf[b..2 * b].copy_from_slice(block);
        self.prev_block.copy_from_slice(block);

        // 2. Forward real-to-complex FFT directly into FDL ring slot.
        let head = self.fdl_head;
        r2c.process_with_scratch(
            &mut self.time_buf,
            &mut self.fdl[head],
            &mut self.r2c_scratch,
        )
        .map_err(|e| format!("UPOLS forward FFT failed: {e}"))?;

        // 3. Complex MAC across all non-zero IR partitions via persistent worker pool (F12).
        // Review #9 D9: `for_each_mac_slice` runs inline on the caller thread when `slices == 1`,
        // so one unified loop handles both parallel and serial execution bit-identically.
        let p_count = self.num_partitions;
        let spec_len = self.acc_spec.len();
        let fdl = &self.fdl;
        let mac_slices = crate::worker_pool::mac_slice_count(true);
        let slices = if nonzero_count >= REW_PARALLEL_MAC_MIN_PARTITIONS
            && nonzero_count.saturating_mul(spec_len) >= crate::worker_pool::MAC_PARALLEL_MIN_WORK
            && mac_slices > 1
            && spec_len >= mac_slices * 256
        {
            mac_slices.min(REW_PARALLEL_MAC_SLICES)
        } else {
            1
        };
        crate::worker_pool::for_each_mac_slice(
            &mut self.acc_spec,
            slices,
            |_slice_idx, bin_start, acc_chunk| {
                let bin_end = bin_start + acc_chunk.len();
                for c in acc_chunk.iter_mut() {
                    *c = Complex::new(0.0, 0.0);
                }
                for p in 0..p_count {
                    if !nonzero[p] {
                        continue;
                    }
                    let fdl_idx = if head >= p {
                        head - p
                    } else {
                        head + p_count - p
                    };
                    let x_slice = &fdl[fdl_idx][bin_start..bin_end];
                    let h_slice = &ir_spectra[p][bin_start..bin_end];
                    for ((acc, &x), &h) in
                        acc_chunk.iter_mut().zip(x_slice.iter()).zip(h_slice.iter())
                    {
                        acc.re += x.re * h.re - x.im * h.im;
                        acc.im += x.re * h.im + x.im * h.re;
                    }
                }
            },
        );

        // Guard realfft DC & Nyquist imaginary-zero invariant against -0.0 / tiny rounding.
        self.acc_spec[0].im = 0.0;
        self.acc_spec[b].im = 0.0;

        // 4. Inverse complex-to-real FFT (already 1/N scaled by IR spectra).
        c2r.process_with_scratch(&mut self.acc_spec, &mut self.time_buf, &mut self.c2r_scratch)
            .map_err(|e| format!("UPOLS inverse FFT failed: {e}"))?;

        // 5. Valid linear-convolution output is the second half [B..2B].
        out.extend_from_slice(&self.time_buf[b..2 * b]);

        self.fdl_head = (head + 1) % p_count;
        Ok(())
    }

    /// Push arbitrary-length input slice and emit all completed `B`-sample output blocks.
    #[allow(clippy::too_many_arguments)]
    fn push_samples(
        &mut self,
        input: &[f64],
        r2c: &dyn RealToComplex<f64>,
        c2r: &dyn ComplexToReal<f64>,
        ir_spectra: &[Vec<Complex<f64>>],
        nonzero: &[bool],
        nonzero_count: usize,
    ) -> Result<Vec<f64>, String> {
        let b = self.block_size;
        let total = self.pending_in.len() + input.len();
        let full_blocks = total / b;
        let mut out = Vec::with_capacity(full_blocks * b);

        let mut offset = 0usize;

        // Finish any partially staged pending block first.
        if !self.pending_in.is_empty() {
            let need = b - self.pending_in.len();
            if input.len() < need {
                self.pending_in.extend_from_slice(input);
                return Ok(out);
            }
            self.pending_in.extend_from_slice(&input[..need]);
            offset = need;
            let staged = std::mem::take(&mut self.pending_in);
            self.process_one_block(
                &staged,
                r2c,
                c2r,
                ir_spectra,
                nonzero,
                nonzero_count,
                &mut out,
            )?;
            self.pending_in = staged;
            self.pending_in.clear();
        }

        // Fast-path full blocks straight from input slice (zero intermediate copy).
        while offset + b <= input.len() {
            self.process_one_block(
                &input[offset..offset + b],
                r2c,
                c2r,
                ir_spectra,
                nonzero,
                nonzero_count,
                &mut out,
            )?;
            offset += b;
        }

        // Stage remaining tail (< B samples).
        if offset < input.len() {
            self.pending_in.extend_from_slice(&input[offset..]);
        }

        Ok(out)
    }

    /// Flush exactly `remaining_output` samples by feeding zeros through the UPOLS engine.
    #[allow(clippy::too_many_arguments)]
    fn flush_exact(
        &mut self,
        remaining_output: usize,
        r2c: &dyn RealToComplex<f64>,
        c2r: &dyn ComplexToReal<f64>,
        ir_spectra: &[Vec<Complex<f64>>],
        nonzero: &[bool],
        nonzero_count: usize,
    ) -> Result<Vec<f64>, String> {
        if remaining_output == 0 {
            return Ok(Vec::new());
        }

        let b = self.block_size;
        let mut out = Vec::with_capacity(remaining_output.next_multiple_of(b));

        // First flush any pending partial input block padded with zeros.
        if !self.pending_in.is_empty() {
            let mut block = std::mem::take(&mut self.pending_in);
            block.resize(b, 0.0);
            self.process_one_block(
                &block,
                r2c,
                c2r,
                ir_spectra,
                nonzero,
                nonzero_count,
                &mut out,
            )?;
        }

        let zero_block = vec![0.0_f64; b];
        while out.len() < remaining_output {
            self.process_one_block(
                &zero_block,
                r2c,
                c2r,
                ir_spectra,
                nonzero,
                nonzero_count,
                &mut out,
            )?;
        }

        out.truncate(remaining_output);
        Ok(out)
    }
}

/// Stateful pure-`f64` Uniform Partitioned Overlap-Save (UPOLS) stereo convolver.
pub struct PartitionedStereoConvolver {
    ir: Arc<PreparedStereoIr>,
    left: ChannelUpolsState,
    right: ChannelUpolsState,
    /// Per-track excess-only linear attenuation (`<= 1.0`) applied to this track's own UPOLS
    /// output before overlap-adding any already-scaled `incoming_tail`.
    output_scale: f64,
    incoming_tail: Option<ConvolutionTail>,
    tail_offset: usize,
    total_input_frames: usize,
    total_emitted_frames: usize,
    flushed: bool,
}

impl PartitionedStereoConvolver {
    pub fn new(ir: Arc<PreparedStereoIr>) -> Self {
        let left = ChannelUpolsState::new(&ir);
        let right = ChannelUpolsState::new(&ir);
        Self {
            ir,
            left,
            right,
            output_scale: 1.0,
            incoming_tail: None,
            tail_offset: 0,
            total_input_frames: 0,
            total_emitted_frames: 0,
            flushed: false,
        }
    }

    pub fn prepared_ir(&self) -> &PreparedStereoIr {
        &self.ir
    }

    fn apply_output_scale(&self, out_l: &mut [f64], out_r: &mut [f64]) {
        if self.output_scale < 1.0 {
            let g = self.output_scale;
            for s in out_l.iter_mut() {
                *s *= g;
            }
            for s in out_r.iter_mut() {
                *s *= g;
            }
        }
    }

    /// Overlap-add the next slice of `incoming_tail` onto `out_l`/`out_r`, advancing `tail_offset`.
    fn overlay_incoming_tail(&mut self, out_l: &mut [f64], out_r: &mut [f64]) {
        let Some(ref tail) = self.incoming_tail else {
            return;
        };
        let avail = tail.left.len().saturating_sub(self.tail_offset);
        let n = avail.min(out_l.len());
        if n == 0 {
            self.incoming_tail = None;
            return;
        }
        let start = self.tail_offset;
        for i in 0..n {
            out_l[i] += tail.left[start + i];
            out_r[i] += tail.right[start + i];
        }
        self.tail_offset += n;
        if self.tail_offset >= tail.left.len() {
            self.incoming_tail = None;
        }
    }

    /// Flush UPOLS remaining frames and overlap-add any unconsumed `incoming_tail`, extending
    /// `out_l`/`out_r` if `incoming_tail` outlasts this track's own tail.
    fn flush_combined(&mut self) -> Result<(Vec<f64>, Vec<f64>), String> {
        let target_total = self.total_input_frames + self.ir.flush_tail_samples;
        let remaining = target_total.saturating_sub(self.total_emitted_frames);

        let (mut out_l, mut out_r) = if self.ir.num_partitions >= 2 && remaining >= self.ir.block_size
        {
            let ir = &self.ir;
            let left = &mut self.left;
            let right = &mut self.right;
            let (l_res, r_res) = crate::worker_pool::join_stereo(
                || {
                    left.flush_exact(
                        remaining,
                        ir.r2c.as_ref(),
                        ir.c2r.as_ref(),
                        &ir.ir_spectra_left,
                        &ir.nonzero_left,
                        ir.nonzero_count_left,
                    )
                },
                || {
                    right.flush_exact(
                        remaining,
                        ir.r2c.as_ref(),
                        ir.c2r.as_ref(),
                        &ir.ir_spectra_right,
                        &ir.nonzero_right,
                        ir.nonzero_count_right,
                    )
                },
            );
            (l_res?, r_res?)
        } else {
            let out_l = self.left.flush_exact(
                remaining,
                self.ir.r2c.as_ref(),
                self.ir.c2r.as_ref(),
                &self.ir.ir_spectra_left,
                &self.ir.nonzero_left,
                self.ir.nonzero_count_left,
            )?;
            let out_r = self.right.flush_exact(
                remaining,
                self.ir.r2c.as_ref(),
                self.ir.c2r.as_ref(),
                &self.ir.ir_spectra_right,
                &self.ir.nonzero_right,
                self.ir.nonzero_count_right,
            )?;
            (out_l, out_r)
        };
        debug_assert_eq!(out_l.len(), out_r.len());
        self.apply_output_scale(&mut out_l, &mut out_r);

        if let Some(tail) = self.incoming_tail.take() {
            let rem_tail = tail.left.len().saturating_sub(self.tail_offset);
            if rem_tail > out_l.len() {
                out_l.resize(rem_tail, 0.0);
                out_r.resize(rem_tail, 0.0);
            }
            for i in 0..rem_tail {
                out_l[i] += tail.left[self.tail_offset + i];
                out_r[i] += tail.right[self.tail_offset + i];
            }
            self.tail_offset = tail.left.len();
        }

        Ok((out_l, out_r))
    }
}

impl StereoConvolver for PartitionedStereoConvolver {
    fn name(&self) -> &'static str {
        "rew-fft-f64"
    }

    fn description(&self) -> String {
        if self.output_scale < 1.0 - 1e-9 && self.output_scale > 0.0 {
            let scale_db = 20.0 * self.output_scale.log10();
            format!(
                "rew-fft-f64({}, {}t, +{:.2} dB, scale {:.2} dB)",
                self.ir.label, self.ir.total_taps, self.ir.max_boost_db, scale_db
            )
        } else {
            format!(
                "rew-fft-f64({}, {}t, +{:.2} dB)",
                self.ir.label, self.ir.total_taps, self.ir.max_boost_db
            )
        }
    }

    fn max_boost_db(&self) -> f64 {
        self.ir.max_boost_db
    }

    fn peak_gain_bound_db(&self) -> f64 {
        self.ir.max_boost_db
    }

    fn accepts_tail(&self) -> bool {
        true
    }

    fn content_hash(&self) -> Option<&str> {
        Some(&self.ir.content_hash)
    }

    #[cfg(any(test, feature = "verify"))]
    fn ir(&self) -> Option<ConvolverIrExport<'_>> {
        Some(ConvolverIrExport {
            sample_rate_hz: self.ir.sample_rate_hz,
            left: &self.ir.raw_ir_left,
            right: &self.ir.raw_ir_right,
            flush_tail_samples: self.ir.flush_tail_samples,
        })
    }

    fn set_output_scale(&mut self, scale_linear: f64) {
        if scale_linear.is_finite() {
            self.output_scale = scale_linear.clamp(0.0, 1.0);
        }
    }

    fn set_incoming_tail(&mut self, tail: ConvolutionTail) {
        if tail.sample_rate_hz == self.ir.sample_rate_hz
            && tail.left.len() == tail.right.len()
            && !tail.left.is_empty()
        {
            self.incoming_tail = Some(tail);
            self.tail_offset = 0;
        }
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch in UPOLS convolver".to_string());
        }
        if input.sample_rate_hz != self.ir.sample_rate_hz {
            return Err(format!(
                "REW convolver rate mismatch: IR '{}' is {} Hz but stream is {} Hz",
                self.ir.label, self.ir.sample_rate_hz, input.sample_rate_hz
            ));
        }
        if input.is_empty() {
            return Ok(StereoF64Buffer::empty(self.ir.sample_rate_hz));
        }

        self.total_input_frames += input.len();

        let (mut out_l, mut out_r) =
            if self.ir.num_partitions >= 2 && input.left.len() + self.left.pending_in.len() >= self.ir.block_size {
                let ir = &self.ir;
                let left = &mut self.left;
                let right = &mut self.right;
                let in_l = &input.left;
                let in_r = &input.right;
                let (l_res, r_res) = crate::worker_pool::join_stereo(
                    || {
                        left.push_samples(
                            in_l,
                            ir.r2c.as_ref(),
                            ir.c2r.as_ref(),
                            &ir.ir_spectra_left,
                            &ir.nonzero_left,
                            ir.nonzero_count_left,
                        )
                    },
                    || {
                        right.push_samples(
                            in_r,
                            ir.r2c.as_ref(),
                            ir.c2r.as_ref(),
                            &ir.ir_spectra_right,
                            &ir.nonzero_right,
                            ir.nonzero_count_right,
                        )
                    },
                );
                (l_res?, r_res?)
            } else {
                let out_l = self.left.push_samples(
                    &input.left,
                    self.ir.r2c.as_ref(),
                    self.ir.c2r.as_ref(),
                    &self.ir.ir_spectra_left,
                    &self.ir.nonzero_left,
                    self.ir.nonzero_count_left,
                )?;
                let out_r = self.right.push_samples(
                    &input.right,
                    self.ir.r2c.as_ref(),
                    self.ir.c2r.as_ref(),
                    &self.ir.ir_spectra_right,
                    &self.ir.nonzero_right,
                    self.ir.nonzero_count_right,
                )?;
                (out_l, out_r)
            };
        debug_assert_eq!(out_l.len(), out_r.len());

        if !out_l.is_empty() {
            self.apply_output_scale(&mut out_l, &mut out_r);
            self.overlay_incoming_tail(&mut out_l, &mut out_r);
            self.total_emitted_frames += out_l.len();
        }

        Ok(StereoF64Buffer {
            sample_rate_hz: self.ir.sample_rate_hz,
            left: out_l,
            right: out_r,
        })
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        if self.flushed {
            return Ok(StereoF64Buffer::empty(self.ir.sample_rate_hz));
        }
        self.flushed = true;
        if self.total_input_frames == 0 && self.incoming_tail.is_none() {
            return Ok(StereoF64Buffer::empty(self.ir.sample_rate_hz));
        }

        let (out_l, out_r) = self.flush_combined()?;
        self.total_emitted_frames += out_l.len();

        Ok(StereoF64Buffer {
            sample_rate_hz: self.ir.sample_rate_hz,
            left: out_l,
            right: out_r,
        })
    }

    fn flush_split_tail(&mut self) -> Result<(StereoF64Buffer, Option<ConvolutionTail>), String> {
        if self.flushed {
            return Ok((StereoF64Buffer::empty(self.ir.sample_rate_hz), None));
        }
        self.flushed = true;
        if self.total_input_frames == 0 && self.incoming_tail.is_none() {
            return Ok((StereoF64Buffer::empty(self.ir.sample_rate_hz), None));
        }

        let exact_track_remaining = self
            .total_input_frames
            .saturating_sub(self.total_emitted_frames);
        let (mut out_l, mut out_r) = self.flush_combined()?;
        let split_at = exact_track_remaining.min(out_l.len());
        let tail_l = out_l.split_off(split_at);
        let tail_r = out_r.split_off(split_at);
        self.total_emitted_frames += out_l.len();

        let next_tail = if tail_l.is_empty() {
            None
        } else {
            Some(ConvolutionTail {
                sample_rate_hz: self.ir.sample_rate_hz,
                left: tail_l,
                right: tail_r,
            })
        };

        Ok((
            StereoF64Buffer {
                sample_rate_hz: self.ir.sample_rate_hz,
                left: out_l,
                right: out_r,
            },
            next_tail,
        ))
    }

    fn flush_split_tail_with_post_ring(
        &mut self,
        post_ring: StereoF64Buffer,
    ) -> Result<(StereoF64Buffer, Option<ConvolutionTail>), String> {
        if post_ring.is_empty() {
            return self.flush_split_tail();
        }
        if self.flushed {
            return Ok((StereoF64Buffer::empty(self.ir.sample_rate_hz), None));
        }
        // Record exact remaining track-A frames BEFORE feeding post-ringing samples past Track A!
        let exact_track_remaining = self
            .total_input_frames
            .saturating_sub(self.total_emitted_frames);
        let p_out = self.process_block(post_ring)?;
        self.flushed = true;
        let (f_l, f_r) = self.flush_combined()?;

        let mut out_l = p_out.left;
        let mut out_r = p_out.right;
        out_l.extend_from_slice(&f_l);
        out_r.extend_from_slice(&f_r);

        let split_at = exact_track_remaining.min(out_l.len());
        let tail_l = out_l.split_off(split_at);
        let tail_r = out_r.split_off(split_at);

        let next_tail = if tail_l.is_empty() {
            None
        } else {
            Some(ConvolutionTail {
                sample_rate_hz: self.ir.sample_rate_hz,
                left: tail_l,
                right: tail_r,
            })
        };

        Ok((
            StereoF64Buffer {
                sample_rate_hz: self.ir.sample_rate_hz,
                left: out_l,
                right: out_r,
            },
            next_tail,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CachedIrKey {
    path: PathBuf,
    modified: Option<SystemTime>,
    file_len: u64,
}

struct CachedIrEntry {
    key: CachedIrKey,
    ir: Arc<PreparedStereoIr>,
}

/// Caching, hot-reloading REW stereo convolver factory keyed by active sample rate.
///
/// Configured via [`ConvolutionConfig`] (loaded from `config.toml` or CLI flags).
/// By default (`ConvolutionConfig::disabled()`), convolution is disabled (`NullConvolver`)
/// because room-correction filters are user-specific and cannot have a universal default.
pub struct RewConvolverFactory {
    config: ConvolutionConfig,
    /// When `true` (`U7` Ultimate Mode), never truncate long IR flush tails at `-200 dB`.
    full_tail: bool,
    cache: Mutex<HashMap<u32, CachedIrEntry>>,
    /// Negative cache of invalid `(path, modified, len)` WAVs so broken newest files in `dir`
    /// are not re-read from disk on every track start until modified (Review #5 Finding #17).
    failed_cache: Mutex<HashMap<(u32, CachedIrKey), String>>,
    warned_keys: Mutex<HashSet<String>>,
    warnings: Mutex<Vec<String>>,
}

impl RewConvolverFactory {
    pub fn new(config: ConvolutionConfig) -> Self {
        Self {
            config,
            full_tail: false,
            cache: Mutex::new(HashMap::new()),
            failed_cache: Mutex::new(HashMap::new()),
            warned_keys: Mutex::new(HashSet::new()),
            warnings: Mutex::new(Vec::new()),
        }
    }

    /// Enable or disable lossless full-tail REW convolution (`U7`: zero `-200 dB` flush truncation).
    pub fn with_full_tail(mut self, full_tail: bool) -> Self {
        self.full_tail = full_tail;
        self
    }

    pub fn is_full_tail(&self) -> bool {
        self.full_tail
    }

    pub fn config(&self) -> &ConvolutionConfig {
        &self.config
    }

    /// Return diagnostic notices/warnings emitted so far (e.g., IR auto-normalization, rate
    /// fallback, or missing rate-matched WAV in configured `dir`).
    pub fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn emit_notice_once(&self, key: String, msg: String) {
        let mut seen = self.warned_keys.lock().unwrap_or_else(|e| e.into_inner());
        if !seen.insert(key) {
            return;
        }
        drop(seen);
        eprintln!("{msg}");
        self.warnings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(msg);
    }

    fn load_explicit_file(
        &self,
        rate_hz: u32,
        path: &Path,
    ) -> Result<Box<dyn StereoConvolver>, String> {
        let meta = fs::metadata(path).map_err(|e| {
            format!("configured REW WAV '{}' not accessible: {e}", path.display())
        })?;
        let ir = self.load_or_get_cached(rate_hz, path, meta.modified().ok(), meta.len())?;
        Ok(Box::new(PartitionedStereoConvolver::new(ir)))
    }

    fn load_or_get_cached(
        &self,
        rate_hz: u32,
        path: &Path,
        modified: Option<SystemTime>,
        file_len: u64,
    ) -> Result<Arc<PreparedStereoIr>, String> {
        let key = CachedIrKey {
            path: path.to_path_buf(),
            modified,
            file_len,
        };

        {
            let guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = guard.get(&rate_hz) {
                if entry.key == key {
                    return Ok(Arc::clone(&entry.ir));
                }
            }
        }
        {
            let failed = self.failed_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(reason) = failed.get(&(rate_hz, key.clone())) {
                return Err(reason.clone());
            }
        }

        let load_res = (|| -> Result<Arc<PreparedStereoIr>, String> {
            let wav = load_stereo_wav(path)?;
            if wav.sample_rate_hz != rate_hz {
                return Err(format!(
                    "REW WAV '{}' is {} Hz, expected {} Hz",
                    path.display(),
                    wav.sample_rate_hz,
                    rate_hz
                ));
            }
            Ok(Arc::new(PreparedStereoIr::from_wav_with_options(
                &wav,
                self.full_tail,
            )?))
        })();

        let prepared = match load_res {
            Ok(ir) => ir,
            Err(err) => {
                let mut failed = self.failed_cache.lock().unwrap_or_else(|e| e.into_inner());
                if failed.len() < 128 {
                    failed.insert((rate_hz, key), err.clone());
                }
                return Err(err);
            }
        };

        let dispersion_db = prepared.l1_norm_db - prepared.max_boost_db;
        if dispersion_db > DISPERSIVE_FILTER_WARNING_THRESHOLD_DB {
            let notice = format!(
                "  [rew] WARNING: '{}' is strongly dispersive (diagnostic L1 +{:.2} dB vs max |H(f)| +{:.2} dB; L1 - G_c = +{:.2} dB > {:.1} dB): max |H(f)| prediction may undershoot; over-counter backstop is active.",
                prepared.label,
                prepared.l1_norm_db,
                prepared.max_boost_db,
                dispersion_db,
                DISPERSIVE_FILTER_WARNING_THRESHOLD_DB,
            );
            self.emit_notice_once(
                format!("ir-dispersive:{}:{:?}:{}", path.display(), modified, file_len),
                notice,
            );
        } else if prepared.max_boost_db > 0.0 {
            let notice = format!(
                "  [rew] Loaded '{}' unnormalized (max |H(f)| +{:.2} dB, diagnostic L1 +{:.2} dB); per-track convolution output will scale only by any excess over 0 dBFS.",
                prepared.label,
                prepared.max_boost_db,
                prepared.l1_norm_db,
            );
            self.emit_notice_once(
                format!("ir-diag:{}:{:?}:{}", path.display(), modified, file_len),
                notice,
            );
        }

        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            rate_hz,
            CachedIrEntry {
                key,
                ir: Arc::clone(&prepared),
            },
        );
        Ok(prepared)
    }
}

impl StereoConvolverFactory for RewConvolverFactory {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String> {
        if !self.config.enabled {
            return Ok(Box::new(NullConvolver {
                sample_rate_hz: target_rate_hz,
            }));
        }

        // 1. Per-rate explicit file override (`file_352800` / `file_384000`).
        let rate_file = match target_rate_hz {
            352_800 => self.config.file_352800.as_deref(),
            384_000 => self.config.file_384000.as_deref(),
            _ => None,
        };
        if let Some(path) = rate_file {
            return self.load_explicit_file(target_rate_hz, path);
        }

        // 2. Single explicit WAV file (`file`): probe header first; use only when sample rate
        // matches, otherwise log and fall through to `dir` -> `NullConvolver`.
        if let Some(path) = self.config.file.as_deref() {
            let header = probe_wav_header(path)?;
            if header.sample_rate_hz == target_rate_hz {
                return self.load_explicit_file(target_rate_hz, path);
            }
            self.emit_notice_once(
                format!("file-rate-mismatch:{}:{target_rate_hz}", path.display()),
                format!(
                    "  [rew] Configured file '{}' is {} Hz (track target is {} Hz) — falling through to directory discovery / bypass.",
                    path.display(),
                    header.sample_rate_hz,
                    target_rate_hz,
                ),
            );
        }

        // 3. Auto-discovery directory (`dir`: newest 2-ch WAV by mtime matching target_rate_hz).
        if let Some(dir) = self.config.dir.as_deref() {
            if !dir.is_dir() {
                self.emit_notice_once(
                    format!("missing-dir:{}", dir.display()),
                    format!(
                        "  [rew] WARNING: configured convolution dir '{}' does not exist — using null-convolver.",
                        dir.display()
                    ),
                );
                return Ok(Box::new(NullConvolver {
                    sample_rate_hz: target_rate_hz,
                }));
            }
            let (candidates, mut skipped) =
                discover_rew_wav_candidates_sorted(dir, target_rate_hz)?;

            let mut loaded: Option<(DiscoveredRewFilter, Arc<PreparedStereoIr>)> = None;
            for cand in candidates {
                match self.load_or_get_cached(
                    target_rate_hz,
                    &cand.path,
                    cand.modified,
                    cand.file_len,
                ) {
                    Ok(ir) => {
                        loaded = Some((cand, ir));
                        break;
                    }
                    Err(reason) => {
                        skipped.push(SkippedRewWav {
                            path: cand.path,
                            modified: cand.modified,
                            reason,
                        });
                    }
                }
            }

            for sk in &skipped {
                let is_relevant = match &loaded {
                    None => true,
                    Some((chosen, _)) => is_newer_candidate(
                        sk.modified,
                        &sk.path,
                        chosen.modified,
                        &chosen.path,
                    ),
                };
                if is_relevant {
                    let fname = sk
                        .path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("<wav>");
                    self.emit_notice_once(
                        format!("skipped-wav:{}:{:?}", sk.path.display(), sk.modified),
                        format!(
                            "  [rew] WARNING: skipped newer/invalid WAV '{}' during {} Hz discovery ({})",
                            fname, target_rate_hz, sk.reason
                        ),
                    );
                }
            }

            return match loaded {
                Some((_, ir)) => Ok(Box::new(PartitionedStereoConvolver::new(ir))),
                None => {
                    self.emit_notice_once(
                        format!("no-match:{}:{target_rate_hz}", dir.display()),
                        format!(
                            "  [rew] WARNING: no 2-channel {} Hz WAV found in '{}' — using null-convolver.",
                            target_rate_hz,
                            dir.display()
                        ),
                    );
                    Ok(Box::new(NullConvolver {
                        sample_rate_hz: target_rate_hz,
                    }))
                }
            };
        }

        Ok(Box::new(NullConvolver {
            sample_rate_hz: target_rate_hz,
        }))
    }
}
