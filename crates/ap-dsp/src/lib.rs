//! Pure IEEE 754 `f64` DSP pipeline locked to `352.8 kHz` and `384 kHz` (`8fS`).
//!
//! Hard invariants:
//! - Strict `4,000,000`-tap ceiling (`MAX_FILTER_TAPS`).
//! - All internal math strictly in floating point; integer quantization happens once in `quantize`.
//! - Core first-class REW stereo convolution + optional integer-sample channel delay only.

pub mod convolution;
pub mod delay;
pub mod pipeline;
pub mod quantize;
pub mod selector;
pub mod shaper_design;
pub mod upsampler;

/// Strict upper bound on filter length (4,000,000 taps).
pub const MAX_FILTER_TAPS: usize = 4_000_000;
