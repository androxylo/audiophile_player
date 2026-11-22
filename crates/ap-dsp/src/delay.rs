//! Optional per-channel room alignment delay (only minor DSP exception permitted).
//!
//! At `352.8 / 384 kHz`, 1 sample is ~2.6 microseconds (~0.9 mm acoustic distance),
//! enabling pure integer-sample delay with zero fractional-delay filter coloration.

use ap_core::buffer::StereoF64Buffer;
use ap_core::config::ChannelDelayConfig;

/// Apply exact integer-sample channel delay at `352.8 / 384 kHz`.
pub fn apply_channel_delay(_audio: &mut StereoF64Buffer, _delay: ChannelDelayConfig) {
    todo!("Implement integer-sample channel delay at 8fS (INV-05)")
}
