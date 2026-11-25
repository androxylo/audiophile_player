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
use ap_core::headroom::{
    MAX_REALISTIC_TRUE_PEAK_DB, MAX_REALISTIC_TRUE_PEAK_LINEAR, MIN_REALISTIC_PEAK_DB,
    PEAK_TAG_ROUNDING_TOLERANCE, TrackPeakMetadata,
};
use ap_core::metadata::{TrackMetadata, parse_track_metadata_fields};
use flac_codec::decode::{FlacSampleReader, Metadata};
use flac_codec::metadata::{Cuesheet, VorbisComment};

use crate::md5::FlacSampleMd5Hasher;

/// Unified bounded-block audio source interface (local FLAC files now; live Tidal streams in Milestone 2).
pub trait AudioBlockSource: Send {
    fn spec(&self) -> StreamSpec;
    fn peak_meta(&self) -> TrackPeakMetadata;
    fn is_streaming(&self) -> bool;
    fn total_frames(&self) -> Option<u64>;

    /// Extended per-track FLAC metadata (`VORBIS_COMMENT` + `CUESHEET` pre-emphasis, Phase 3).
    fn track_meta(&self) -> TrackMetadata {
        TrackMetadata::default()
    }

    /// Optional 16-byte `STREAMINFO` MD5 signature for exact track-identity verification
    /// (`HEADROOM_DESIGN.md` §2 full-scan YAML lookup). Returns `None` for live streams or
    /// FLAC files with an all-zero/unset MD5 field.
    fn streaminfo_md5(&self) -> Option<[u8; 16]> {
        None
    }

    /// RFC 9639 §8.2 sample MD5 computed incrementally over decoded integer samples.
    /// Returns `Some(digest)` ONLY after the stream reached clean EOF with zero decode errors.
    fn computed_md5(&self) -> Option<[u8; 16]> {
        None
    }

    /// Decode up to `max_frames` stereo frames into a pure IEEE 754 `f64` block.
    /// Returns `Ok(None)` cleanly at end-of-stream.
    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String>;
}

/// Extract per-track pre-emphasis facts from a FLAC `CUESHEET` block (`OQ-41`):
/// returns `(track1_pre_emphasis, mixed_emphasis_flags, audio_track_count)`.
pub fn parse_cuesheet_emphasis(cuesheet: &Cuesheet) -> (bool, bool, u16) {
    let mut track1_emph: Option<bool> = None;
    let mut mixed = false;
    let mut audio_tracks: u16 = 0;

    for track in cuesheet.tracks() {
        // Ignore lead-out (`number == None`) and non-audio data tracks.
        if track.number.is_none() || track.non_audio {
            continue;
        }
        audio_tracks = audio_tracks.saturating_add(1);
        match track1_emph {
            None => track1_emph = Some(track.pre_emphasis),
            Some(first) if first != track.pre_emphasis => mixed = true,
            Some(_) => {}
        }
    }

    (track1_emph.unwrap_or(false), mixed, audio_tracks)
}

/// Extract extended [`TrackMetadata`] from FLAC `VorbisComment` and `Cuesheet` metadata blocks.
pub fn extract_flac_track_metadata(
    vc: Option<&VorbisComment>,
    cuesheet: Option<&Cuesheet>,
) -> TrackMetadata {
    let mut meta = vc
        .map(|v| parse_track_metadata_fields(&v.fields))
        .unwrap_or_default();
    if let Some(cs) = cuesheet {
        let (pre_emph, mixed, count) = parse_cuesheet_emphasis(cs);
        meta.cuesheet_pre_emphasis = pre_emph;
        meta.cuesheet_mixed_emphasis = mixed;
        meta.cuesheet_audio_tracks = count;
    }
    meta
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
    track_meta: TrackMetadata,
    total_frames: Option<u64>,
    streaminfo_md5: Option<[u8; 16]>,
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
    let vc = reader.metadata().get::<VorbisComment>();
    let cs = reader.metadata().get::<Cuesheet>();
    let peak_meta = parse_peak_metadata(vc);
    let track_meta = extract_flac_track_metadata(vc, cs);
    let total_frames = reader.total_samples();
    let streaminfo_md5 = reader.md5().copied();
    let shift = 32 - bits_per_sample;

    Ok(ValidatedFlac {
        reader,
        source_channels,
        spec,
        peak_meta,
        track_meta,
        total_frames,
        streaminfo_md5,
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
    track_meta: TrackMetadata,
    total_frames: Option<u64>,
    streaminfo_md5: Option<[u8; 16]>,
    md5_hasher: Option<FlacSampleMd5Hasher>,
    finalized_md5: Option<[u8; 16]>,
    saw_decode_error: bool,
    shift: u32,
    scratch: Vec<i32>,
    eof: bool,
    pending_error: Option<String>,
}

impl FlacBlockReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let validated = open_validated(path)?;
        let md5_hasher = Some(FlacSampleMd5Hasher::new(validated.spec.bits_per_sample));
        Ok(Self {
            reader: validated.reader,
            source_channels: usize::from(validated.source_channels),
            spec: validated.spec,
            peak_meta: validated.peak_meta,
            track_meta: validated.track_meta,
            total_frames: validated.total_frames,
            streaminfo_md5: validated.streaminfo_md5,
            md5_hasher,
            finalized_md5: None,
            saw_decode_error: false,
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

    fn track_meta(&self) -> TrackMetadata {
        self.track_meta.clone()
    }

    fn is_streaming(&self) -> bool {
        false
    }

    fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    fn streaminfo_md5(&self) -> Option<[u8; 16]> {
        self.streaminfo_md5
    }

    fn computed_md5(&self) -> Option<[u8; 16]> {
        self.finalized_md5
    }

    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
        if let Some(err) = self.pending_error.take() {
            self.eof = true;
            self.saw_decode_error = true;
            self.md5_hasher = None;
            self.finalized_md5 = None;
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
                    self.saw_decode_error = true;
                    self.md5_hasher = None;
                    self.finalized_md5 = None;
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
        let sample_count = frames * ch;
        if sample_count > 0 {
            if let Some(ref mut hasher) = self.md5_hasher {
                hasher.update_raw_samples(&self.scratch[..sample_count]);
            }
        }
        if self.eof && !self.saw_decode_error {
            if let Some(hasher) = self.md5_hasher.take() {
                self.finalized_md5 = Some(hasher.finalize());
            }
        }
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
    /// `REPLAYGAIN_*_TRUE_PEAK`: explicit `dB`/`dBTP` suffix, bare negative dBTP (`[-120, 0]`),
    /// or bare positive linear amplitude ONLY when cross-checked against a companion
    /// `REPLAYGAIN_*_PEAK` sample-peak tag (prevents bare `+0.5 dBTP` written as `"0.5"` without
    /// a sample-peak tag from being misinterpreted as `-6.02 dBFS`).
    ReplayGainLinearOrExplicitDb,
    /// `TRUE_PEAK`: explicit `dB` / `dBTP` suffix, or unambiguous bare negative dBTP in
    /// `[-120.0, 0.0]`. Bare positive values (`0.3`, `+0.3`) are ambiguous between hot dBTP and
    /// quiet linear amplitude and are rejected safely (`None` -> fail-safe `-3 dB`).
    TruePeakDbOnly,
    /// `R128_*_TRUE_PEAK`: require explicit `dB` / `dBTP` suffix so R128 Q7.8 integers (e.g.
    /// `-768`) are never misparsed as `-768 dBTP` (`10^-38.4`).
    ExplicitDbOnly,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ParsedTruePeak {
    /// Unambiguous value (from explicit `dB`/`dBTP` suffix or bare negative dBTP) within `(0, 2.0]`.
    Unambiguous(f64),
    /// Bare positive ReplayGain linear candidate in `(0, 2.0]` requiring companion sample-peak cross-check.
    BarePositiveLinearCandidate(f64),
    /// Explicitly implausible true-peak value (`> 2.0` linear / `> +6.0206 dBTP`): marks track peak
    /// as unknown (`P = 0`) without falling back to a quieter sample-peak tag (`HEADROOM_DESIGN.md` §2).
    OutOfRange,
}

/// Parse a True Peak Vorbis comment value with strict unit disambiguation and §2 plausibility
/// bounds `(0, 2.0]` linear / `<= +6.0206 dBTP` so a hot master tagged as unitless positive dBTP
/// (`+0.3` / `0.3` / `0.5`), an implausible unit error (`+15 dBTP` / `5.0`), or an R128 Q7.8
/// integer (`-768`) can NEVER be misinterpreted as a quiet linear peak.
fn parse_true_peak_value(raw: &str, unit: TruePeakTagUnit) -> Option<ParsedTruePeak> {
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
        if val > MAX_REALISTIC_TRUE_PEAK_DB + 1e-9 {
            Some(ParsedTruePeak::OutOfRange)
        } else if val >= MIN_REALISTIC_PEAK_DB - 1e-9 {
            Some(ParsedTruePeak::Unambiguous(10.0_f64.powf(val / 20.0)))
        } else {
            None
        }
    } else {
        match unit {
            TruePeakTagUnit::ReplayGainLinearOrExplicitDb => {
                if num_str.starts_with('-') && (MIN_REALISTIC_PEAK_DB..=0.0).contains(&val) {
                    // Negative values cannot be linear amplitudes -> unambiguous dBTP.
                    Some(ParsedTruePeak::Unambiguous(10.0_f64.powf(val / 20.0)))
                } else if !num_str.starts_with('+') && val > 0.0 {
                    if val > MAX_REALISTIC_TRUE_PEAK_LINEAR + 1e-9 {
                        Some(ParsedTruePeak::OutOfRange)
                    } else {
                        // Bare positive linear candidate in (0, 2.0]: requires companion sample-peak
                        // tag to rule out unitless hot dBTP like `REPLAYGAIN_TRACK_TRUE_PEAK=0.5`.
                        Some(ParsedTruePeak::BarePositiveLinearCandidate(val))
                    }
                } else {
                    None
                }
            }
            TruePeakTagUnit::TruePeakDbOnly => {
                // Only unambiguous bare negative dBTP (e.g. `-4.2`); reject bare positive (`0.3`,
                // `+0.3`, `0.0`) and Q7.8-like `< -120.0`.
                if num_str.starts_with('-') && (MIN_REALISTIC_PEAK_DB..=0.0).contains(&val) {
                    Some(ParsedTruePeak::Unambiguous(10.0_f64.powf(val / 20.0)))
                } else {
                    None
                }
            }
            TruePeakTagUnit::ExplicitDbOnly => None,
        }
    }
}

/// Update a true-peak candidate slot without letting a lower-confidence bare-positive linear
/// candidate overwrite an already-parsed unambiguous true-peak tag (Review #5 Finding #24),
/// while letting an explicit `OutOfRange` (> +6.02 dBTP) stay sticky as invalid.
fn update_tp_candidate(slot: &mut Option<ParsedTruePeak>, parsed: ParsedTruePeak) {
    match (&*slot, parsed) {
        (Some(ParsedTruePeak::OutOfRange), _) | (_, ParsedTruePeak::OutOfRange) => {
            *slot = Some(ParsedTruePeak::OutOfRange);
        }
        (_, ParsedTruePeak::Unambiguous(lin)) => {
            *slot = Some(ParsedTruePeak::Unambiguous(lin));
        }
        (Some(ParsedTruePeak::Unambiguous(_)), ParsedTruePeak::BarePositiveLinearCandidate(_)) => {
            // Preserve unambiguous tag (e.g. TRUE_PEAK / R128_TRACK_TRUE_PEAK).
        }
        (_, cand) => {
            *slot = Some(cand);
        }
    }
}

/// Parse ReplayGain / peak Vorbis comment tags from a `VorbisComment` metadata block.
pub fn parse_peak_metadata_fields(fields: &[String]) -> TrackPeakMetadata {
    let mut meta = TrackPeakMetadata::default();
    let mut track_tp_candidate: Option<ParsedTruePeak> = None;
    let mut album_tp_candidate: Option<ParsedTruePeak> = None;
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
                if let Some(parsed) = parse_true_peak_value(v, TruePeakTagUnit::TruePeakDbOnly) {
                    update_tp_candidate(&mut track_tp_candidate, parsed);
                }
            }
            "REPLAYGAIN_TRACK_TRUE_PEAK" => {
                if let Some(parsed) =
                    parse_true_peak_value(v, TruePeakTagUnit::ReplayGainLinearOrExplicitDb)
                {
                    update_tp_candidate(&mut track_tp_candidate, parsed);
                }
            }
            "R128_TRACK_TRUE_PEAK" => {
                if let Some(parsed) = parse_true_peak_value(v, TruePeakTagUnit::ExplicitDbOnly) {
                    update_tp_candidate(&mut track_tp_candidate, parsed);
                }
            }
            "REPLAYGAIN_ALBUM_GAIN" => meta.replaygain_album_gain_db = val_num,
            "REPLAYGAIN_ALBUM_PEAK" => meta.replaygain_album_peak = val_num,
            "REPLAYGAIN_ALBUM_TRUE_PEAK" => {
                if let Some(parsed) =
                    parse_true_peak_value(v, TruePeakTagUnit::ReplayGainLinearOrExplicitDb)
                {
                    update_tp_candidate(&mut album_tp_candidate, parsed);
                }
            }
            "R128_ALBUM_TRUE_PEAK" => {
                if let Some(parsed) = parse_true_peak_value(v, TruePeakTagUnit::ExplicitDbOnly) {
                    update_tp_candidate(&mut album_tp_candidate, parsed);
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

    if matches!(track_tp_candidate, Some(ParsedTruePeak::OutOfRange))
        || matches!(album_tp_candidate, Some(ParsedTruePeak::OutOfRange))
    {
        meta.true_peak_invalid = true;
        return meta;
    }

    let resolve_candidate = |cand: Option<ParsedTruePeak>, companion_sp: Option<f64>| -> Option<f64> {
        match cand? {
            ParsedTruePeak::OutOfRange => None,
            ParsedTruePeak::Unambiguous(lin) => Some(lin),
            ParsedTruePeak::BarePositiveLinearCandidate(lin) => {
                // Only accept a bare positive ReplayGain true-peak value when a realistic
                // companion sample-peak tag confirms linear scaling (`tp >= sp * 0.995`).
                let sp = companion_sp?;
                if sp.is_finite() && sp > 0.0 && lin >= sp * (1.0 - PEAK_TAG_ROUNDING_TOLERANCE) {
                    Some(lin)
                } else {
                    None
                }
            }
        }
    };

    let resolved_track_tp = resolve_candidate(track_tp_candidate, meta.replaygain_track_peak);
    let resolved_album_tp = resolve_candidate(
        album_tp_candidate,
        meta.replaygain_album_peak.or(meta.replaygain_track_peak),
    );

    // §2 tag sanity: when both track and album true-peak tags exist, track <= album (within 0.5%).
    if let (Some(t_tp), Some(a_tp)) = (resolved_track_tp, resolved_album_tp) {
        if t_tp > a_tp * (1.0 + PEAK_TAG_ROUNDING_TOLERANCE) {
            meta.true_peak_invalid = true;
            return meta;
        }
    }

    // Also verify album true peak against album sample peak when album true peak stands in.
    let valid_album_tp = resolved_album_tp.filter(|&a_tp| {
        !meta.replaygain_album_peak.is_some_and(|a_sp| {
            a_sp.is_finite() && a_sp > 0.0 && a_tp < a_sp * (1.0 - PEAK_TAG_ROUNDING_TOLERANCE)
        })
    });

    meta.true_peak_linear = resolved_track_tp.or(valid_album_tp);

    // Mathematical sanity cross-check: True Peak can never be lower than discrete sample peak.
    // If a mis-tagged or mis-unit-ed True Peak falls below the track's sample peak (beyond 0.5%
    // tag-rounding tolerance), discard `true_peak_linear` so headroom falls back safely.
    if let (Some(tp), Some(sp)) = (meta.true_peak_linear, meta.replaygain_track_peak) {
        if sp.is_finite() && sp > 0.0 && tp < sp * (1.0 - PEAK_TAG_ROUNDING_TOLERANCE) {
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
