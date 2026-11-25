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
use std::io::{Read, Seek, SeekFrom};
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

/// Flush-tail threshold (`-200 dB`, 56 dB below 24-bit PCM) applied only when an IR exceeds
/// `MAX_PARTITION_BLOCK` taps, preventing multi-second sub-`-250 dB` float noise gaps between
/// tracks while convolving 100% of all non-zero taps during active playback.
const LONG_IR_FLUSH_THRESHOLD: f64 = 1e-10;

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

/// Pluggable stateful stereo convolution stage operating strictly in IEEE 754 `f64`.
pub trait StereoConvolver: Send {
    fn name(&self) -> &'static str;

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
    r2c: Arc<dyn RealToComplex<f64>>,
    c2r: Arc<dyn ComplexToReal<f64>>,
    ir_spectra_left: Vec<Vec<Complex<f64>>>,
    ir_spectra_right: Vec<Vec<Complex<f64>>>,
    nonzero_left: Vec<bool>,
    nonzero_right: Vec<bool>,
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
            .finish()
    }
}

impl PreparedStereoIr {
    /// Prepare UPOLS FFT partitions, measure peak frequency gain with `>= 2x` zero-padding,
    /// auto-normalize any `> 0 dB` IR down to `0 dB` (preserving L/R balance), and compute the
    /// post-normalization L1 norm bound in pure `f64`.
    pub fn from_stereo_ir(
        label: impl Into<String>,
        sample_rate_hz: u32,
        left: &[f64],
        right: &[f64],
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

        // Convolve 100% of all non-zero taps during active playback; for ultra-long (>64k)
        // REW WAVs, flush the full audible & ultrasonic tail down to -200 dB (1e-10).
        let flush_tail_samples = if active_taps <= MAX_PARTITION_BLOCK {
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
            r2c,
            c2r,
            ir_spectra_left,
            ir_spectra_right,
            nonzero_left,
            nonzero_right,
        })
    }

    pub fn from_wav(wav: &StereoImpulseWav) -> Result<Self, String> {
        Self::from_stereo_ir(&wav.label, wav.sample_rate_hz, &wav.left, &wav.right)
    }
}

/// Measure peak frequency-domain magnitude `max_f max(|H_L(f)|, |H_R(f)|)` using a `>= 2x`
/// zero-padded real FFT (reducing inter-bin peak underestimation on narrow resonances).
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

    let mut max_mag = 0.0_f64;
    for ch in [left, right] {
        time_buf.fill(0.0);
        time_buf[..ch.len()].copy_from_slice(ch);
        r2c.process_with_scratch(&mut time_buf, &mut spec, &mut scratch)
            .map_err(|e| format!("IR gain-analysis FFT failed: {e}"))?;
        for c in &spec {
            let mag = c.norm();
            if mag > max_mag {
                max_mag = mag;
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
    fn process_one_block(
        &mut self,
        block: &[f64],
        r2c: &dyn RealToComplex<f64>,
        c2r: &dyn ComplexToReal<f64>,
        ir_spectra: &[Vec<Complex<f64>>],
        nonzero: &[bool],
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

        // 3. Complex MAC across all non-zero IR partitions.
        for c in &mut self.acc_spec {
            *c = Complex::new(0.0, 0.0);
        }

        let p_count = self.num_partitions;
        for p in 0..p_count {
            if !nonzero[p] {
                continue;
            }
            let fdl_idx = if head >= p {
                head - p
            } else {
                head + p_count - p
            };
            let x_spec = &self.fdl[fdl_idx];
            let h_spec = &ir_spectra[p];
            for ((acc, &x), &h) in self.acc_spec.iter_mut().zip(x_spec.iter()).zip(h_spec.iter()) {
                acc.re += x.re * h.re - x.im * h.im;
                acc.im += x.re * h.im + x.im * h.re;
            }
        }

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
    fn push_samples(
        &mut self,
        input: &[f64],
        r2c: &dyn RealToComplex<f64>,
        c2r: &dyn ComplexToReal<f64>,
        ir_spectra: &[Vec<Complex<f64>>],
        nonzero: &[bool],
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
            self.process_one_block(&staged, r2c, c2r, ir_spectra, nonzero, &mut out)?;
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
    fn flush_exact(
        &mut self,
        remaining_output: usize,
        r2c: &dyn RealToComplex<f64>,
        c2r: &dyn ComplexToReal<f64>,
        ir_spectra: &[Vec<Complex<f64>>],
        nonzero: &[bool],
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
            self.process_one_block(&block, r2c, c2r, ir_spectra, nonzero, &mut out)?;
        }

        let zero_block = vec![0.0_f64; b];
        while out.len() < remaining_output {
            self.process_one_block(&zero_block, r2c, c2r, ir_spectra, nonzero, &mut out)?;
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

        let mut out_l = self.left.flush_exact(
            remaining,
            self.ir.r2c.as_ref(),
            self.ir.c2r.as_ref(),
            &self.ir.ir_spectra_left,
            &self.ir.nonzero_left,
        )?;
        let mut out_r = self.right.flush_exact(
            remaining,
            self.ir.r2c.as_ref(),
            self.ir.c2r.as_ref(),
            &self.ir.ir_spectra_right,
            &self.ir.nonzero_right,
        )?;
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

        let mut out_l = self.left.push_samples(
            &input.left,
            self.ir.r2c.as_ref(),
            self.ir.c2r.as_ref(),
            &self.ir.ir_spectra_left,
            &self.ir.nonzero_left,
        )?;
        let mut out_r = self.right.push_samples(
            &input.right,
            self.ir.r2c.as_ref(),
            self.ir.c2r.as_ref(),
            &self.ir.ir_spectra_right,
            &self.ir.nonzero_right,
        )?;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    cache: Mutex<HashMap<u32, CachedIrEntry>>,
    warned_keys: Mutex<HashSet<String>>,
    warnings: Mutex<Vec<String>>,
}

impl RewConvolverFactory {
    pub fn new(config: ConvolutionConfig) -> Self {
        Self {
            config,
            cache: Mutex::new(HashMap::new()),
            warned_keys: Mutex::new(HashSet::new()),
            warnings: Mutex::new(Vec::new()),
        }
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

        let wav = load_stereo_wav(path)?;
        if wav.sample_rate_hz != rate_hz {
            return Err(format!(
                "REW WAV '{}' is {} Hz, expected {} Hz",
                path.display(),
                wav.sample_rate_hz,
                rate_hz
            ));
        }
        let prepared = Arc::new(PreparedStereoIr::from_wav(&wav)?);

        if prepared.max_boost_db > 0.0 || prepared.l1_norm_db > 3.0 {
            let notice = format!(
                "  [rew] WARNING: '{}' loaded unnormalized (max |H(f)| +{:.2} dB, diagnostic L1 +{:.2} dB); per-track convolution output will scale only by any excess over 0 dBFS.",
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
