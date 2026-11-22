//! Engine configuration schema (no user volume control, no general DSP suite).

use std::path::PathBuf;

/// Strictly mixer-free Windows output transport selection.
/// Shared Mode is intentionally unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTransport {
    /// Direct Windows Kernel Streaming (`WDM-KS`, Audirvana-style WaveRT pin).
    WdmKs,
    /// Event-driven WASAPI Exclusive (`AUDCLNT_SHAREMODE_EXCLUSIVE`).
    WasapiExclusive,
}

/// Optional per-channel time alignment delay for non-symmetric listening rooms.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChannelDelayConfig {
    /// Delay applied to left channel in microseconds (quantized to integer 8fS samples ~2.6 us).
    pub left_us: f64,
    /// Delay applied to right channel in microseconds.
    pub right_us: f64,
}

/// Top-level headless engine configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub dac_name_filter: String,
    pub transport: OutputTransport,
    pub rew_convolution_wav: Option<PathBuf>,
    pub channel_delay: ChannelDelayConfig,
    pub http_bind_addr: String,
}
