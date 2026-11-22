//! Curated `f64` upsampling filters to `352.8 / 384 kHz` under the strict 4M-tap cap.

use ap_core::buffer::StereoF64Buffer;

use crate::selector::FilterArchetype;

/// Upsample a stereo `f64` buffer by an exact integer factor (8x / 4x / 2x) to 352.8 or 384 kHz.
pub fn upsample_to_8fs(
    _input: &StereoF64Buffer,
    _filter: FilterArchetype,
) -> Result<StereoF64Buffer, String> {
    todo!("Implement 4M-tap-capped FFT overlap-add / polyphase f64 upsampling (INV-03)")
}
