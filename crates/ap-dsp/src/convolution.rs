//! First-class core REW stereo convolution engine operating at `352.8 / 384 kHz` in `f64`.

use std::path::Path;

use ap_core::buffer::StereoF64Buffer;

/// Loaded stereo Room EQ Wizard impulse response prepared for `352.8 / 384 kHz` `f64` convolution.
#[derive(Debug)]
pub struct RewStereoIr {
    pub sample_rate_hz: u32,
    pub left_ir: Vec<f64>,
    pub right_ir: Vec<f64>,
    pub max_passband_gain_db: f64,
}

/// Load a stereo REW WAV impulse response (handling 352.8/384 kHz derivation per INV-05).
pub fn load_rew_ir(_path: &Path, _target_rate_hz: u32) -> Result<RewStereoIr, String> {
    todo!("Implement REW stereo WAV loader & gain inspection (INV-05)")
}

/// Apply stereo REW overlap-add convolution strictly in `f64`.
pub fn convolve_stereo_f64(
    _audio: &StereoF64Buffer,
    _ir: &RewStereoIr,
) -> Result<StereoF64Buffer, String> {
    todo!("Implement f64 FFT overlap-add stereo convolution (INV-05)")
}
