// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Abstract zero-mixer Windows output sink trait and structured error types.

use std::fmt;

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;

/// Number of 1 ms histogram bins tracked for hardware wake intervals (`0..=126 ms`, bin 127 = `>= 127 ms`).
pub const WAKE_HISTOGRAM_BINS: usize = 128;

/// Diagnostics returned after a zero-mixer playback completes (`WDMKS_TIMING_TESTS_REVIEW.md` §4.1).
#[derive(Debug, Clone)]
pub struct PlaybackStats {
    pub device_name: String,
    pub sample_rate_hz: u32,
    pub wire_format: DacWireFormat,
    pub frames_played: usize,
    pub wavert_buffer_bytes: u32,
    pub notifications: usize,
    pub mmcss_pro_audio: bool,
    /// Number of DMA half-buffer fills that received zero frames from a non-EOF producer.
    pub underrun_halves: usize,
    /// Number of DMA half-buffer fills that ran dry mid-half (`0 < written < half_frames` before EOF, S5).
    pub partial_underrun_halves: usize,
    /// Number of coalesced notifications (`>= 2` half boundaries crossed between wakes, S3).
    pub coalesced_wakes: usize,
    /// Number of early/duplicate notifications ignored without writing (S4).
    pub spurious_wakes: usize,
    /// Number of wakes that took the wall-clock lag-rescue branch (S2).
    pub rescue_writes: usize,
    /// Maximum interval observed between consecutive hardware notification wakes (nanoseconds).
    pub max_wake_interval_ns: u64,
    /// 1 ms histogram of hardware notification intervals (`bin[i]` counts intervals in `[i, i+1) ms`,
    /// `bin[127]` counts `>= 127 ms`).
    pub wake_interval_histogram_ms: [u32; WAKE_HISTOGRAM_BINS],
}

impl Default for PlaybackStats {
    fn default() -> Self {
        Self {
            device_name: String::new(),
            sample_rate_hz: 0,
            wire_format: DacWireFormat::Int32,
            frames_played: 0,
            wavert_buffer_bytes: 0,
            notifications: 0,
            mmcss_pro_audio: false,
            underrun_halves: 0,
            partial_underrun_halves: 0,
            coalesced_wakes: 0,
            spurious_wakes: 0,
            rescue_writes: 0,
            max_wake_interval_ns: 0,
            wake_interval_histogram_ms: [0; WAKE_HISTOGRAM_BINS],
        }
    }
}

impl PlaybackStats {
    /// Record one inter-wake elapsed interval (in nanoseconds) into `max_wake_interval_ns`
    /// and the 1 ms `wake_interval_histogram_ms` bins.
    #[inline]
    pub fn record_wake_interval_ns(&mut self, delta_ns: u64) {
        if delta_ns > self.max_wake_interval_ns {
            self.max_wake_interval_ns = delta_ns;
        }
        let bin = ((delta_ns / 1_000_000) as usize).min(WAKE_HISTOGRAM_BINS - 1);
        self.wake_interval_histogram_ms[bin] =
            self.wake_interval_histogram_ms[bin].saturating_add(1);
    }

    /// Returns `true` if any audible/timing anomaly counter fired during this playback segment.
    pub fn has_timing_anomalies(&self) -> bool {
        self.underrun_halves > 0
            || self.partial_underrun_halves > 0
            || self.coalesced_wakes > 0
            || self.spurious_wakes > 0
            || self.rescue_writes > 0
    }

    /// Concise single-line timing diagnostics summary for end-of-track logs & `status --json`.
    pub fn timing_summary(&self) -> String {
        format!(
            "frames={} notifs={} underrun={} partial_underrun={} coalesced={} spurious={} rescue={} max_wake={:.2}ms",
            self.frames_played,
            self.notifications,
            self.underrun_halves,
            self.partial_underrun_halves,
            self.coalesced_wakes,
            self.spurious_wakes,
            self.rescue_writes,
            (self.max_wake_interval_ns as f64) / 1_000_000.0,
        )
    }

    /// Sparse non-zero `(ms_bin, count)` histogram buckets for compact JSON reporting.
    pub fn non_zero_histogram_bins(&self) -> Vec<(usize, u32)> {
        self.wake_interval_histogram_ms
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, c)| c > 0)
            .collect()
    }
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
