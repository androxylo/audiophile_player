//! Bit-exact full-memory RFC 9639 FLAC decoding (16-bit, 24-bit, and 32-bit FLAC 1.4+)
//! plus ReplayGain / True Peak Vorbis comment parsing.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;
use flac_codec::decode::{FlacSampleReader, Metadata};
use flac_codec::metadata::VorbisComment;

/// Full-memory decoded local FLAC track in pure IEEE 754 `f64` for DSP processing.
#[derive(Debug)]
pub struct DecodedFlacTrack {
    pub spec: StreamSpec,
    pub peak_meta: TrackPeakMetadata,
    pub audio: StereoF64Buffer,
}

/// Full-memory bit-exact integer decoded local FLAC track (MSB-aligned `i32`, zero float conversion).
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
                // RFC 7845 Q7.8 fixed-point dB
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
///
/// Full RFC 9639 / FLAC 1.4+ support (including 16-bit, 24-bit, and 32-bit HiRes FLAC):
/// - 16-bit samples are shifted left by 16 bits (`s << 16`).
/// - 24-bit samples are shifted left by 8 bits (`s << 8`).
/// - 32-bit samples pass through untouched (`shift = 0`).
/// Zero floating-point math, zero dithering, zero loss of precision.
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
    let decoded = decode_flac_full_memory_int32(path)?;
    // Exact power-of-two scaling (2^31) preserves every bit in f64 (53-bit mantissa > 32-bit int).
    let inv_scale = 1.0 / ((1_u64 << 31) as f64);
    let n = decoded.pcm.frames.len();
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for &[l, r] in &decoded.pcm.frames {
        left.push((l as f64) * inv_scale);
        right.push((r as f64) * inv_scale);
    }
    Ok(DecodedFlacTrack {
        spec: decoded.spec,
        peak_meta: decoded.peak_meta,
        audio: StereoF64Buffer {
            sample_rate_hz: decoded.spec.sample_rate_hz,
            left,
            right,
        },
    })
}
