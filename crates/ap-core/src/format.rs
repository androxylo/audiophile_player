// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! PCM sample rates, locked `352.8 / 384 kHz` DSP families, and integer DAC wire formats.

/// Locked `8fS` floating-point DSP target rates (integer multiples of 44.1 / 48 kHz families).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDspRate {
    /// `352,800 Hz` — exact 8x / 4x / 2x integer target for 44.1 / 88.2 / 176.4 kHz sources.
    Hz352800,
    /// `384,000 Hz` — exact 8x / 4x / 2x integer target for 48 / 96 / 192 kHz sources.
    Hz384000,
}

/// Strictly integer PCM wire formats accepted by `usbaudio2.sys` / Schiit Unison 384.
/// Floating-point wire output is intentionally unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DacWireFormat {
    Int32,
    Int24In32,
    Int24Packed,
    Int16,
}

impl DacWireFormat {
    /// Bytes per individual channel sample on the wire.
    #[inline]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Int32 | Self::Int24In32 => 4,
            Self::Int24Packed => 3,
            Self::Int16 => 2,
        }
    }

    /// Bytes per stereo (2-channel) PCM frame on the wire.
    #[inline]
    pub const fn bytes_per_frame(self) -> usize {
        self.bytes_per_sample() * 2
    }
}

/// Metadata describing an audio stream's sample rate and bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSpec {
    pub sample_rate_hz: u32,
    pub bits_per_sample: u16,
    pub channels: u16,
}
