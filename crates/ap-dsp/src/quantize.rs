//! Single final floating-point -> integer PCM quantization stage with 8fS noise shaping.
//!
//! Supports configurable effective DAC bit depth (e.g. R2R/multibit "DAC Bits" for
//! Schiit Gungnir 2's TI DAC8812CRUZ chips, finalized in INV-01 / INV-04) packed cleanly
//! into 32-bit or 24-bit integer PCM containers.

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};

use crate::selector::NoiseShaperProfile;

/// Quantize `f64` stereo audio once into final interleaved `i32` PCM for the DAC.
pub fn quantize_to_int32(
    _audio: &StereoF64Buffer,
    _shaper: NoiseShaperProfile,
    _dac_bits: u8,
) -> InterleavedInt32Buffer {
    todo!("Implement 8fS noise shapers (NS5/LNS15/PGGB-class/TPDF) and final quantization (INV-04)")
}
