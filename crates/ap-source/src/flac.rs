//! Bit-exact full-memory FLAC decoding into `StereoF64Buffer` + ReplayGain/True Peak metadata.

use std::path::Path;

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;

/// Full-memory decoded local FLAC track ready for pure `f64` DSP processing.
#[derive(Debug)]
pub struct DecodedFlacTrack {
    pub spec: StreamSpec,
    pub peak_meta: TrackPeakMetadata,
    pub audio: StereoF64Buffer,
}

/// Decode a local FLAC file completely into RAM (implemented in Milestone 1).
pub fn decode_flac_full_memory(_path: &Path) -> Result<DecodedFlacTrack, String> {
    todo!("Implement bit-exact FLAC RAM decoding + Vorbis ReplayGain/True Peak parsing")
}
