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

/// Streaming RFC 9639 FLAC reader yielding bounded `StereoF64Buffer` chunks without
/// ever loading an entire multi-gigabyte file into RAM.
pub struct FlacBlockReader {
    reader: FlacSampleReader<BufReader<File>>,
    spec: StreamSpec,
    peak_meta: TrackPeakMetadata,
    total_frames: Option<u64>,
    shift: u32,
    scratch: Vec<i32>,
    eof: bool,
}

impl FlacBlockReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let file = File::open(path)
            .map_err(|e| format!("failed to open FLAC '{}': {e}", path.display()))?;
        let reader = FlacSampleReader::new(BufReader::new(file))
            .map_err(|e| format!("failed to parse FLAC '{}': {e}", path.display()))?;

        let channels = reader.channel_count();
        let sample_rate_hz = reader.sample_rate();
        let bits_per_sample = reader.bits_per_sample();

        if channels != 2 {
            return Err(format!(
                "only stereo (2-channel) FLAC is supported, got {channels} channels in '{}'",
                path.display()
            ));
        }
        if bits_per_sample == 0 || bits_per_sample > 32 {
            return Err(format!(
                "unsupported FLAC bit depth {bits_per_sample} in '{}'",
                path.display()
            ));
        }

        let spec = StreamSpec {
            sample_rate_hz,
            bits_per_sample: bits_per_sample as u16,
            channels: u16::from(channels),
        };
        let peak_meta = parse_peak_metadata(reader.metadata().get::<VorbisComment>());
        let total_frames = reader.total_samples();
        let shift = 32 - bits_per_sample;

        Ok(Self {
            reader,
            spec,
            peak_meta,
            total_frames,
            shift,
            scratch: Vec::new(),
            eof: false,
        })
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
        if self.eof || max_frames == 0 {
            return Ok(None);
        }

        let target_samples = max_frames.saturating_mul(2);
        if self.scratch.len() < target_samples {
            self.scratch.resize(target_samples, 0);
        }

        let mut filled = 0;
        while filled < target_samples {
            let n = self
                .reader
                .read(&mut self.scratch[filled..target_samples])
                .map_err(|e| format!("FLAC stream decode error: {e}"))?;
            if n == 0 {
                self.eof = true;
                break;
            }
            filled += n;
        }

        let frames = filled / 2;
        if frames == 0 {
            return Ok(None);
        }

        // Exact power-of-two scaling (2^31) preserves every bit in f64 (53-bit mantissa > 32-bit).
        let inv_scale = 1.0 / ((1_u64 << 31) as f64);
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        let shift = self.shift;

        for pair in self.scratch[..frames * 2].chunks_exact(2) {
            left.push((pair[0].wrapping_shl(shift) as f64) * inv_scale);
            right.push((pair[1].wrapping_shl(shift) as f64) * inv_scale);
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

/// Parse ReplayGain / peak Vorbis comment tags from a `VorbisComment` metadata block.
pub fn parse_peak_metadata_fields(fields: &[String]) -> TrackPeakMetadata {
    let mut meta = TrackPeakMetadata::default();
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
            "REPLAYGAIN_TRACK_PEAK" | "TRUE_PEAK" | "REPLAYGAIN_TRACK_TRUE_PEAK" => {
                meta.replaygain_track_peak = val_num;
            }
            "REPLAYGAIN_ALBUM_GAIN" => meta.replaygain_album_gain_db = val_num,
            "REPLAYGAIN_ALBUM_PEAK" | "REPLAYGAIN_ALBUM_TRUE_PEAK" => {
                meta.replaygain_album_peak = val_num;
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
    meta
}

fn parse_peak_metadata(vc: Option<&VorbisComment>) -> TrackPeakMetadata {
    match vc {
        Some(v) => parse_peak_metadata_fields(&v.fields),
        None => TrackPeakMetadata::default(),
    }
}

/// Decode a local stereo FLAC file completely into RAM as bit-exact MSB-aligned `i32` frames.
pub fn decode_flac_full_memory_int32(path: &Path) -> Result<DecodedFlacInt32, String> {
    let file = File::open(path)
        .map_err(|e| format!("failed to open FLAC '{}': {e}", path.display()))?;
    let mut reader = FlacSampleReader::new(BufReader::new(file))
        .map_err(|e| format!("failed to parse FLAC '{}': {e}", path.display()))?;

    let channels = reader.channel_count();
    let sample_rate_hz = reader.sample_rate();
    let bits_per_sample = reader.bits_per_sample();

    if channels != 2 {
        return Err(format!(
            "only stereo (2-channel) FLAC is supported, got {channels} channels in '{}'",
            path.display()
        ));
    }
    if bits_per_sample == 0 || bits_per_sample > 32 {
        return Err(format!(
            "unsupported FLAC bit depth {bits_per_sample} in '{}'",
            path.display()
        ));
    }

    let spec = StreamSpec {
        sample_rate_hz,
        bits_per_sample: bits_per_sample as u16,
        channels: u16::from(channels),
    };
    let peak_meta = parse_peak_metadata(reader.metadata().get::<VorbisComment>());
    let shift = 32 - bits_per_sample;

    let total_frames = reader.total_samples().unwrap_or(0) as usize;
    let mut raw_interleaved = Vec::with_capacity(total_frames.saturating_mul(2));
    reader
        .read_to_end(&mut raw_interleaved)
        .map_err(|e| format!("FLAC decode error in '{}': {e}", path.display()))?;

    let mut frames = Vec::with_capacity(raw_interleaved.len() / 2);
    for pair in raw_interleaved.chunks_exact(2) {
        frames.push([pair[0].wrapping_shl(shift), pair[1].wrapping_shl(shift)]);
    }

    Ok(DecodedFlacInt32 {
        spec,
        peak_meta,
        pcm: InterleavedInt32Buffer {
            sample_rate_hz,
            frames,
        },
    })
}

/// Decode a local stereo FLAC file completely into RAM as pure IEEE 754 `f64` planar buffers.
pub fn decode_flac_full_memory(path: &Path) -> Result<DecodedFlacTrack, String> {
    let mut src = FlacBlockReader::open(path)?;
    let spec = src.spec();
    let peak_meta = src.peak_meta();
    let cap = src.total_frames().unwrap_or(0) as usize;
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
