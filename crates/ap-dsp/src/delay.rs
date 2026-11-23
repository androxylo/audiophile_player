//! Optional per-channel room alignment delay stage (`352.8 / 384 kHz` pure `f64`).
//!
//! At `352.8 / 384 kHz`, 1 sample is ~2.6 microseconds (~0.9 mm acoustic distance),
//! enabling pure integer-sample delay with zero fractional-delay coloration.
//! Currently provides the explicit zero-copy [`NullChannelDelayer`] placeholder.

use ap_core::buffer::StereoF64Buffer;

/// Pluggable per-channel room alignment delay stage operating strictly in IEEE 754 `f64`.
pub trait ChannelDelayer: Send + Sync {
    fn name(&self) -> &'static str;

    fn apply(&self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;
}

/// Explicit zero-copy null channel delayer (`StereoF64Buffer -> StereoF64Buffer`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullChannelDelayer;

impl ChannelDelayer for NullChannelDelayer {
    fn name(&self) -> &'static str {
        "null-delay"
    }

    fn apply(&self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch in channel delayer".to_string());
        }
        Ok(input)
    }
}
