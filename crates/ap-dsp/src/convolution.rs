//! Core first-class REW stereo convolution stage (`352.8 / 384 kHz` pure `f64`).
//!
//! Currently provides the explicit zero-copy [`NullConvolver`] placeholder while keeping
//! the exact [`StereoConvolver`] trait boundary ready for FFT overlap-add convolution in
//! `INV-05`. Default REW filter location: `C:\Program Files\EqualizerAPO\config` (most
//! recent rate-matched 2-channel WAV by change date).

use ap_core::buffer::StereoF64Buffer;

/// Default directory where the user's REW stereo WAV convolution filters live.
pub const DEFAULT_REW_CONVOLUTION_DIR: &str = r"C:\Program Files\EqualizerAPO\config";

/// Pluggable stereo convolution stage operating strictly in IEEE 754 `f64`.
pub trait StereoConvolver: Send + Sync {
    fn name(&self) -> &'static str;

    /// Maximum positive frequency-domain gain boost (`>= 0.0 dB`) introduced by this filter,
    /// fed into `ap_core::headroom::select_headroom` so clipping is mathematically impossible.
    fn max_boost_db(&self) -> f64;

    fn convolve(&self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;
}

/// Explicit zero-copy null convolver (`StereoF64Buffer -> StereoF64Buffer`).
///
/// Keeps the full processing pipeline tidy and uniform until real FFT overlap-add REW
/// stereo convolution is implemented.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullConvolver;

impl StereoConvolver for NullConvolver {
    fn name(&self) -> &'static str {
        "null-convolver"
    }

    fn max_boost_db(&self) -> f64 {
        0.0
    }

    fn convolve(&self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch in convolver".to_string());
        }
        Ok(input)
    }
}
