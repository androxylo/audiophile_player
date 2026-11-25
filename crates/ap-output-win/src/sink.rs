//! Abstract zero-mixer Windows output sink trait and structured error types.

use std::fmt;

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;

/// Diagnostics returned after a zero-mixer playback completes.
#[derive(Debug, Clone)]
pub struct PlaybackStats {
    pub device_name: String,
    pub sample_rate_hz: u32,
    pub wire_format: DacWireFormat,
    pub frames_played: usize,
    pub wavert_buffer_bytes: u32,
    pub notifications: usize,
    pub mmcss_pro_audio: bool,
    /// Number of DMA half-buffer fills that encountered a producer underrun (silence padded).
    pub underrun_halves: usize,
}

/// Errors returned by zero-mixer Windows audio output backends.
///
/// Crucially distinguishes [`OutputError::DeviceBusy`] (e.g. another exclusive/KS player
/// like Audirvana holding the DAC pin) from format or driver errors — and never falls
/// back to Windows Shared Mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputError {
    DeviceNotFound(String),
    DeviceBusy {
        device: String,
        os_error: u32,
        detail: String,
    },
    UnsupportedFormat {
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        os_error: u32,
        detail: String,
    },
    InvalidBuffer(String),
    Win32 {
        operation: &'static str,
        code: u32,
        detail: String,
    },
}

impl OutputError {
    pub fn is_device_busy(&self) -> bool {
        matches!(self, Self::DeviceBusy { .. })
    }
}

impl fmt::Display for OutputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceNotFound(msg) => write!(f, "audio device not found: {msg}"),
            Self::DeviceBusy {
                device,
                os_error,
                detail,
            } => write!(
                f,
                "audio device '{device}' is busy/locked (Win32 error {os_error:#x}: {detail}). \
                 Another exclusive/kernel-mode player (such as Audirvana) may be holding the DAC; \
                 please stop it and retry. Shared Mode fallback is strictly disabled."
            ),
            Self::UnsupportedFormat {
                sample_rate_hz,
                wire_format,
                os_error,
                detail,
            } => write!(
                f,
                "DAC rejected format {sample_rate_hz} Hz / {wire_format:?} (Win32 error {os_error:#x}: {detail})"
            ),
            Self::InvalidBuffer(msg) => write!(f, "invalid PCM buffer: {msg}"),
            Self::Win32 {
                operation,
                code,
                detail,
            } => write!(f, "Win32 {operation} failed (error {code:#x}): {detail}"),
        }
    }
}

impl std::error::Error for OutputError {}

/// Output sink guaranteed to bypass `audiodg.exe` and the Windows audio mixer.
pub trait ZeroMixerAudioSink {
    /// Stream a pre-computed full-memory integer PCM buffer to the locked hardware DAC.
    fn play_full_memory(
        &mut self,
        pcm: &InterleavedInt32Buffer,
        wire_format: DacWireFormat,
    ) -> Result<PlaybackStats, OutputError>;

    /// Stop playback immediately and release the exclusive hardware pin/endpoint cleanly.
    ///
    /// Note: `stop()` latches the sink's stop state so any concurrent or pending play call
    /// observes the interruption. Call [`ZeroMixerAudioSink::arm`] before starting a new track
    /// after stopping.
    fn stop(&mut self) -> Result<(), OutputError>;

    /// Clear any latched stop state so subsequent playback calls proceed normally.
    fn arm(&mut self);
}
