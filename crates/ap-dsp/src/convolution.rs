//! Core first-class REW stereo convolution stage (`352.8 / 384 kHz` pure `f64`, block-streaming).
//!
//! Currently provides the explicit zero-copy [`NullConvolver`] placeholder while keeping
//! the exact stateful [`StereoConvolver`] (`process_block` + `flush`) boundary ready for
//! partitioned 4M-tap FFT overlap-add convolution in `INV-05`.
//! Default REW filter location: `C:\Program Files\EqualizerAPO\config`.

use ap_core::buffer::StereoF64Buffer;

/// Default directory where the user's REW stereo WAV convolution filters live.
pub const DEFAULT_REW_CONVOLUTION_DIR: &str = r"C:\Program Files\EqualizerAPO\config";

/// Pluggable stateful stereo convolution stage operating strictly in IEEE 754 `f64`.
pub trait StereoConvolver: Send {
    fn name(&self) -> &'static str;

    /// Maximum positive frequency-domain gain boost (`>= 0.0 dB`) introduced by this filter,
    /// fed into `ap_core::headroom::select_headroom` so clipping is mathematically impossible.
    fn max_boost_db(&self) -> f64;

    /// Process one bounded `f64` stereo block (overlap-add / overlap-save state kept inside `self`).
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;

    /// Flush remaining convolution impulse tail at end-of-track.
    fn flush(&mut self) -> Result<StereoF64Buffer, String>;
}

/// Factory creating a fresh stateful [`StereoConvolver`] instance per track/rate.
pub trait StereoConvolverFactory: Send + Sync {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String>;
}

/// Explicit zero-copy null convolver (`StereoF64Buffer -> StereoF64Buffer`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullConvolver {
    sample_rate_hz: u32,
}

impl StereoConvolver for NullConvolver {
    fn name(&self) -> &'static str {
        "null-convolver"
    }

    fn max_boost_db(&self) -> f64 {
        0.0
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch in convolver".to_string());
        }
        self.sample_rate_hz = input.sample_rate_hz;
        Ok(input)
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        Ok(StereoF64Buffer::empty(self.sample_rate_hz))
    }
}

/// Factory returning [`NullConvolver`] for every track/rate.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullConvolverFactory;

impl StereoConvolverFactory for NullConvolverFactory {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String> {
        Ok(Box::new(NullConvolver {
            sample_rate_hz: target_rate_hz,
        }))
    }
}
