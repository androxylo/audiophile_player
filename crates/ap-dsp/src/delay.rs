//! Optional per-channel room alignment delay stage (`352.8 / 384 kHz` pure `f64`, block-streaming).
//!
//! At `352.8 / 384 kHz`, 1 sample is ~2.6 microseconds (~0.9 mm acoustic distance),
//! enabling pure integer-sample delay with zero fractional-delay coloration.
//! Currently provides the explicit zero-copy [`NullChannelDelayer`] placeholder.

use ap_core::buffer::StereoF64Buffer;

/// Pluggable stateful per-channel room alignment delay stage operating strictly in IEEE 754 `f64`.
pub trait ChannelDelayer: Send {
    fn name(&self) -> &'static str;

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;

    fn flush(&mut self) -> Result<StereoF64Buffer, String>;
}

/// Factory creating a fresh stateful [`ChannelDelayer`] instance per track/rate.
pub trait ChannelDelayerFactory: Send + Sync {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn ChannelDelayer>, String>;
}

/// Explicit zero-copy null channel delayer (`StereoF64Buffer -> StereoF64Buffer`).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullChannelDelayer {
    sample_rate_hz: u32,
}

impl ChannelDelayer for NullChannelDelayer {
    fn name(&self) -> &'static str {
        "null-delay"
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch in channel delayer".to_string());
        }
        self.sample_rate_hz = input.sample_rate_hz;
        Ok(input)
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        Ok(StereoF64Buffer::empty(self.sample_rate_hz))
    }
}

/// Factory returning [`NullChannelDelayer`] for every track/rate.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullChannelDelayerFactory;

impl ChannelDelayerFactory for NullChannelDelayerFactory {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn ChannelDelayer>, String> {
        Ok(Box::new(NullChannelDelayer {
            sample_rate_hz: target_rate_hz,
        }))
    }
}
