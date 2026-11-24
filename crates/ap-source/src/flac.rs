//! Bounded-memory streaming RFC 9639 FLAC decoding (16-bit, 24-bit, and 32-bit FLAC 1.4+)
//! plus ReplayGain / True Peak Vorbis comment parsing.
//!
//! Primary interface: [`AudioBlockSource`] + [`FlacBlockReader`], which reads bounded
//! [`StereoF64Buffer`] blocks incrementally so arbitrarily large FLAC files (multi-hour
//! 32/384 masters) and future Milestone 2 live streams share one bounded-RAM pipeline.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;
use flac_codec::decode::{FlacSampleReader, Metadata};
use flac_codec::metadata::VorbisComment;

/// Unified bounded-block audio source interface (local FLAC files now; live Tidal streams in Milestone 2).
pub trait AudioBlockSource: Send {
    fn spec(&self) -> StreamSpec;
    fn peak_meta(&self) -> TrackPeakMetadata;
    fn is_streaming(&self) -> bool;
    fn total_frames(&self) -> Option<u64>;

    /// Decode up to `max_frames` stereo frames into a pure IEEE 754 `f64` block.
    /// Returns `Ok(None)` cleanly at end-of-stream.
    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String>;
}

/// Upper bound on upfront `Vec::with_capacity` frame pre-allocation from untrusted
/// `STREAMINFO.total_samples` metadata (`1 << 24` = 16,777,216 frames). Legitimate longer
/// tracks grow incrementally while corrupt headers claiming `2^36 - 1` samples cannot trigger
/// an OOM abort.
pub const MAX_PREALLOC_FRAMES: usize = 1 << 24;

/// Distinct error prefix returned for valid FLAC files whose channel layout is unsupported
/// (e.g. 3+ channel surround), allowing callers to distinguish unsupported layouts from
/// corrupted files.
pub const UNSUPPORTED_CHANNEL_LAYOUT_PREFIX: &str = "unsupported channel layout:";

/// Clamp an untrusted `STREAMINFO.total_samples` hint to [`MAX_PREALLOC_FRAMES`].
pub fn capped_prealloc_frames(total_frames: Option<u64>) -> usize {
    total_frames
        .map(|n| n.min(MAX_PREALLOC_FRAMES as u64) as usize)
        .unwrap_or(0)
}

struct ValidatedFlac {
    reader: FlacSampleReader<BufReader<File>>,
    source_channels: u8,
    spec: StreamSpec,
    peak_meta: TrackPeakMetadata,
    total_frames: Option<u64>,
    shift: u32,
}

/// Validate decoded FLAC STREAMINFO parameters (channels, sample rate, bit depth) and build
/// the canonical stereo `StreamSpec` (mono is duplicated into dual-mono stereo downstream).
pub fn validate_flac_stream_params(
    source_channels: u8,
    sample_rate_hz: u32,
    bits_per_sample: u32,
    path: &Path,
) -> Result<StreamSpec, String> {
    match source_channels {
        1 | 2 => {}
        _ => {
            return Err(format!(
                "{UNSUPPORTED_CHANNEL_LAYOUT_PREFIX} {source_channels} channels in '{}' (only mono and stereo FLAC are supported)",
                path.display()
            ));
        }
    }
    if sample_rate_hz == 0 {
        return Err(format!(
            "invalid FLAC sample rate 0 Hz (non-audio stream) in '{}'",
            path.display()
        ));
    }
    if bits_per_sample == 0 || bits_per_sample > 32 {
        return Err(format!(
            "unsupported FLAC bit depth {bits_per_sample} in '{}'",
            path.display()
        ));
    }

    Ok(StreamSpec {
        sample_rate_hz,
        bits_per_sample: bits_per_sample as u16,
        channels: 2,
    })
}

fn open_validated(path: &Path) -> Result<ValidatedFlac, String> {
    let file = File::open(path)
        .map_err(|e| format!("failed to open FLAC '{}': {e}", path.display()))?;
    let reader = FlacSampleReader::new(BufReader::new(file))
        .map_err(|e| format!("failed to parse FLAC '{}': {e}", path.display()))?;

    let source_channels = reader.channel_count();
    let sample_rate_hz = reader.sample_rate();
    let bits_per_sample = reader.bits_per_sample();

    // Mono sources are duplicated into dual-mono stereo (L == R) so downstream stereo DSP
    // and DAC stages always receive 2-channel frames.
    let spec = validate_flac_stream_params(source_channels, sample_rate_hz, bits_per_sample, path)?;
    let peak_meta = parse_peak_metadata(reader.metadata().get::<VorbisComment>());
    let total_frames = reader.total_samples();
    let shift = 32 - bits_per_sample;

    Ok(ValidatedFlac {
        reader,
        source_channels,
        spec,
        peak_meta,
        total_frames,
        shift,
    })
}

/// Streaming RFC 9639 FLAC reader yielding bounded `StereoF64Buffer` chunks without
/// ever loading an entire multi-gigabyte file into RAM.
///
/// Supports both mono (`1` channel, duplicated identically into stereo L/R) and stereo
/// (`2` channels) FLAC files at 1..=32 bits per sample.
pub struct FlacBlockReader {
    reader: FlacSampleReader<BufReader<File>>,
    source_channels: usize,
    spec: StreamSpec,
    peak_meta: TrackPeakMetadata,
    total_frames: Option<u64>,
    shift: u32,
    scratch: Vec<i32>,
    eof: bool,
    pending_error: Option<String>,
}

impl FlacBlockReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let validated = open_validated(path)?;
        Ok(Self {
            reader: validated.reader,
            source_channels: usize::from(validated.source_channels),
            spec: validated.spec,
            peak_meta: validated.peak_meta,
            total_frames: validated.total_frames,
            shift: validated.shift,
            scratch: Vec::new(),
            eof: false,
            pending_error: None,
        })
    }

    /// Original channel count stored in the FLAC `STREAMINFO` header (`1` for mono, `2` for stereo).
    pub fn source_channels(&self) -> u8 {
        self.source_channels as u8
    }
}

impl AudioBlockSource for FlacBlockReader {
    fn spec(&self) -> StreamSpec {
        self.spec
    }

    fn peak_meta(&self) -> TrackPeakMetadata {
        self.peak_meta
    }

    fn is_streaming(&self) -> bool {
        false
    }

    fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
        if let Some(err) = self.pending_error.take() {
            self.eof = true;
            return Err(err);
        }
        if self.eof || max_frames == 0 {
            return Ok(None);
        }

        let ch = self.source_channels;
        let target_samples = max_frames.saturating_mul(ch);
        if self.scratch.len() < target_samples {
            self.scratch.resize(target_samples, 0);
        }

        let mut filled = 0;
        while filled < target_samples {
            match self.reader.read(&mut self.scratch[filled..target_samples]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    filled += n;
                }
                Err(e) => {
                    let msg = format!("FLAC stream decode error: {e}");
                    if filled >= ch {
                        // Return the valid frames already decoded before the error and surface
                        // the error on the next `read_next_block()` call so truncated or
                        // partially corrupt tracks never discard up to 65,536 frames (~1.5 s).
                        self.pending_error = Some(msg);
                        break;
                    }
                    self.eof = true;
                    return Err(msg);
                }
            }
        }

        let frames = filled / ch;
        if frames == 0 {
            return Ok(None);
        }

        // Exact power-of-two scaling (2^31) preserves every bit in f64 (53-bit mantissa > 32-bit).
        let inv_scale = 1.0 / ((1_u64 << 31) as f64);
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        let shift = self.shift;

        if ch == 1 {
            for &sample in &self.scratch[..frames] {
                let v = (sample.wrapping_shl(shift) as f64) * inv_scale;
                left.push(v);
                right.push(v);
            }
        } else {
            for pair in self.scratch[..frames * 2].chunks_exact(2) {
                left.push((pair[0].wrapping_shl(shift) as f64) * inv_scale);
                right.push((pair[1].wrapping_shl(shift) as f64) * inv_scale);
            }
        }

        Ok(Some(StereoF64Buffer {
            sample_rate_hz: self.spec.sample_rate_hz,
            left,
            right,
        }))
    }
}

/// Full-memory decoded local FLAC track in pure IEEE 754 `f64` (convenience helper for tests/fixtures).
#[derive(Debug)]
pub struct DecodedFlacTrack {
    pub spec: StreamSpec,
    pub peak_meta: TrackPeakMetadata,
    pub audio: StereoF64Buffer,
}

/// Full-memory bit-exact integer decoded local FLAC track (convenience helper for raw WDM-KS tests).
#[derive(Debug)]
pub struct DecodedFlacInt32 {
    pub spec: StreamSpec,
    pub peak_meta: TrackPeakMetadata,
    pub pcm: InterleavedInt32Buffer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TruePeakTagUnit {
    /// `REPLAYGAIN_*_TRUE_PEAK`: ReplayGain convention is linear amplitude (`> 0.0`, no `'+'`),
    /// or explicit `dB` / `dBTP` suffix.
    ReplayGainLinearOrExplicitDb,
    /// `TRUE_PEAK`: explicit `dB` / `dBTP` suffix, or unambiguous bare negative dBTP in
    /// `[-120.0, 0.0]`. Bare positive values (`0.3`, `+0.3`) are ambiguous between hot dBTP and
    /// quiet linear amplitude and are rejected safely (`None` -> fail-safe `-3 dB`).
    TruePeakDbOnly,
    /// `R128_*_TRUE_PEAK`: require explicit `dB` / `dBTP` suffix so R128 Q7.8 integers (e.g.
    /// `-768`) are never misparsed as `-768 dBTP` (`10^-38.4`).
    ExplicitDbOnly,
}

/// Parse a True Peak Vorbis comment value with strict unit disambiguation so a hot master
/// tagged as unitless positive dBTP (`+0.3` / `0.3`) or an R128 Q7.8 integer (`-768`) can NEVER
/// be misinterpreted as a quiet linear peak.
fn parse_true_peak_linear(raw: &str, unit: TruePeakTagUnit) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let (num_str, has_db_suffix) = if let Some(s) = lower.strip_suffix("dbtp") {
        (s.trim_end(), true)
    } else if let Some(s) = lower.strip_suffix("db") {
        (s.trim_end(), true)
    } else {
        (trimmed, false)
    };
    if num_str.is_empty() {
        return None;
    }
    let val = num_str.parse::<f64>().ok()?;
    if !val.is_finite() {
        return None;
    }

    if has_db_suffix {
        if (-120.0..=24.0).contains(&val) {
            Some(10.0_f64.powf(val / 20.0))
        } else {
            None
        }
    } else {
        match unit {
            TruePeakTagUnit::ReplayGainLinearOrExplicitDb => {
                // Reject leading '+' (almost certainly dBTP) and implausible amplitudes.
                if !num_str.starts_with('+') && val > 0.0 && val <= 16.0 {
                    Some(val)
                } else {
                    None
                }
            }
            TruePeakTagUnit::TruePeakDbOnly => {
                // Only unambiguous bare negative dBTP (e.g. `-4.2`); reject bare positive (`0.3`,
                // `+0.3`, `0.0`) and Q7.8-like `< -120.0`.
                if num_str.starts_with('-') && (-120.0..=0.0).contains(&val) {
                    Some(10.0_f64.powf(val / 20.0))
                } else {
                    None
                }
            }
            TruePeakTagUnit::ExplicitDbOnly => None,
        }
    }
}

/// Parse ReplayGain / peak Vorbis comment tags from a `VorbisComment` metadata block.
pub fn parse_peak_metadata_fields(fields: &[String]) -> TrackPeakMetadata {
    let mut meta = TrackPeakMetadata::default();
    let mut album_true_peak: Option<f64> = None;
    for field in fields {
        let Some((k, v)) = field.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_uppercase();
        let val_num = v
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok());
        match key.as_str() {
            "REPLAYGAIN_TRACK_GAIN" => meta.replaygain_track_gain_db = val_num,
            "REPLAYGAIN_TRACK_PEAK" => meta.replaygain_track_peak = val_num,
            "TRUE_PEAK" => {
                if let Some(lin) = parse_true_peak_linear(v, TruePeakTagUnit::TruePeakDbOnly) {
                    meta.true_peak_linear = Some(lin);
                }
            }
            "REPLAYGAIN_TRACK_TRUE_PEAK" => {
                if let Some(lin) =
                    parse_true_peak_linear(v, TruePeakTagUnit::ReplayGainLinearOrExplicitDb)
                {
                    meta.true_peak_linear = Some(lin);
                }
            }
            "R128_TRACK_TRUE_PEAK" => {
                if let Some(lin) = parse_true_peak_linear(v, TruePeakTagUnit::ExplicitDbOnly) {
                    meta.true_peak_linear = Some(lin);
                }
            }
            "REPLAYGAIN_ALBUM_GAIN" => meta.replaygain_album_gain_db = val_num,
            "REPLAYGAIN_ALBUM_PEAK" => meta.replaygain_album_peak = val_num,
            "REPLAYGAIN_ALBUM_TRUE_PEAK" => {
                if let Some(lin) =
                    parse_true_peak_linear(v, TruePeakTagUnit::ReplayGainLinearOrExplicitDb)
                {
                    album_true_peak = Some(lin);
                }
            }
            "R128_ALBUM_TRUE_PEAK" => {
                if let Some(lin) = parse_true_peak_linear(v, TruePeakTagUnit::ExplicitDbOnly) {
                    album_true_peak = Some(lin);
                }
            }
            "R128_TRACK_GAIN" => {
                meta.replaygain_track_gain_db = val_num.map(|q| q / 256.0);
            }
            "R128_ALBUM_GAIN" => {
                meta.replaygain_album_gain_db = val_num.map(|q| q / 256.0);
            }
            _ => {}
        }
    }
    if meta.true_peak_linear.is_none() {
        meta.true_peak_linear = album_true_peak;
    }

    // Mathematical sanity cross-check: True Peak can never be lower than discrete sample peak.
    // If a mis-tagged or mis-unit-ed True Peak falls below the track's sample peak (beyond 0.5%
    // tag-rounding tolerance), discard `true_peak_linear` so headroom falls back safely.
    if let (Some(tp), Some(sp)) = (meta.true_peak_linear, meta.replaygain_track_peak) {
        if sp.is_finite() && sp > 0.0 && tp < sp * 0.995 {
            meta.true_peak_linear = None;
        }
    }

    meta
}

fn parse_peak_metadata(vc: Option<&VorbisComment>) -> TrackPeakMetadata {
    match vc {
        Some(v) => parse_peak_metadata_fields(&v.fields),
        None => TrackPeakMetadata::default(),
    }
}

/// Decode a local mono or stereo FLAC file completely into RAM as bit-exact MSB-aligned stereo
/// `i32` frames (mono sources are duplicated identically to L and R).
pub fn decode_flac_full_memory_int32(path: &Path) -> Result<DecodedFlacInt32, String> {
    let mut validated = open_validated(path)?;
    let ch = usize::from(validated.source_channels);
    let prealloc_frames = capped_prealloc_frames(validated.total_frames);
    let mut raw_interleaved = Vec::with_capacity(prealloc_frames.saturating_mul(ch));
    validated
        .reader
        .read_to_end(&mut raw_interleaved)
        .map_err(|e| format!("FLAC decode error in '{}': {e}", path.display()))?;

    let shift = validated.shift;
    let mut frames = Vec::with_capacity(raw_interleaved.len() / ch);
    if ch == 1 {
        for &s in &raw_interleaved {
            let shifted = s.wrapping_shl(shift);
            frames.push([shifted, shifted]);
        }
    } else {
        for pair in raw_interleaved.chunks_exact(2) {
            frames.push([pair[0].wrapping_shl(shift), pair[1].wrapping_shl(shift)]);
        }
    }

    Ok(DecodedFlacInt32 {
        spec: validated.spec,
        peak_meta: validated.peak_meta,
        pcm: InterleavedInt32Buffer {
            sample_rate_hz: validated.spec.sample_rate_hz,
            frames,
        },
    })
}

/// Decode a local mono or stereo FLAC file completely into RAM as pure IEEE 754 `f64` planar
/// stereo buffers (mono sources are duplicated identically to L and R).
pub fn decode_flac_full_memory(path: &Path) -> Result<DecodedFlacTrack, String> {
    let mut src = FlacBlockReader::open(path)?;
    let spec = src.spec();
    let peak_meta = src.peak_meta();
    let cap = capped_prealloc_frames(src.total_frames());
    let mut left = Vec::with_capacity(cap);
    let mut right = Vec::with_capacity(cap);

    while let Some(block) = src.read_next_block(65_536)? {
        left.extend_from_slice(&block.left);
        right.extend_from_slice(&block.right);
    }

    Ok(DecodedFlacTrack {
        spec,
        peak_meta,
        audio: StereoF64Buffer {
            sample_rate_hz: spec.sample_rate_hz,
            left,
            right,
        },
    })
}
