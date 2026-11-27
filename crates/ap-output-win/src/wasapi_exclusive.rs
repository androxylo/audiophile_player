// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Event-driven WASAPI Exclusive (`AUDCLNT_SHAREMODE_EXCLUSIVE`) backend.
//!
//! Uses in-process `AudioSes.dll` (`IMMDeviceEnumerator` -> `IAudioClient` ->
//! `IAudioRenderClient`) strictly in **Exclusive Mode** (`AUDCLNT_SHAREMODE_EXCLUSIVE`)
//! with hardware event notification (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`):
//! - Evicts Windows shared-mode clients (`audiodg.exe`) and maps the hardware endpoint
//!   buffer directly — Windows Shared Mode is intentionally unimplemented and impossible
//!   to fall back to.
//! - Never calls `GetMixFormat`; always constructs exact integer PCM `WAVEFORMATEXTENSIBLE`
//!   containers (`Int32`, `Int24In32`, `Int24Packed`, `Int16`).
//! - Performs the mandatory MSDN `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` alignment dance
//!   (re-activating a fresh `IAudioClient` with the driver's frame-aligned period).
//! - Reuses the exact `WaveRtPumpSource` wire encoder, click-free stop fade-out, off-RT
//!   block recycler, and MMCSS `"Pro Audio"` hygiene from `wdm_ks` / `hygiene`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::{Duration, Instant};

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::{DacOutputCapabilities, DacWireFormat};

use crate::hygiene::{MemoryLockGuard, MmcssGuard, OptimizationLevel};
use crate::sink::{OutputError, PlaybackStats, ZeroMixerAudioSink};
use crate::wdm_ks::{StreamBlock, StreamingWaveRtPump, WaveRtFramePump, WaveRtPumpSource};

/// WASAPI Exclusive delivery pacing mode (`WDMKS_TIMING_TESTS_REVIEW.md` §5.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WasapiDeliveryMode {
    /// Event-driven ping-pong (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, recommended default).
    #[default]
    EventDriven,
    /// Push (polling via `GetCurrentPadding`) fallback for USB/PCI drivers whose event callback
    /// mode is unreliable. Still strictly `AUDCLNT_SHAREMODE_EXCLUSIVE` (zero mixer).
    Push,
}

#[cfg(windows)]
use core::ffi::c_void;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, E_INVALIDARG, HANDLE, RPC_E_CHANGED_MODE, S_FALSE, S_OK, WAIT_OBJECT_0,
};
#[cfg(windows)]
use windows_sys::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_IN_USE,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED,
    AUDCLNT_E_UNSUPPORTED_FORMAT, AUDCLNT_SHAREMODE, AUDCLNT_SHAREMODE_EXCLUSIVE,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE, EDataFlow, ERole, MMDeviceEnumerator,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eRender,
};
#[cfg(windows)]
use windows_sys::Win32::Media::KernelStreaming::{
    KSDATAFORMAT_SUBTYPE_PCM, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
#[cfg(windows)]
use windows_sys::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear};
#[cfg(windows)]
use windows_sys::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, STGM_READ,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
#[cfg(windows)]
use windows_sys::Win32::System::Variant::VT_LPWSTR;
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
#[cfg(windows)]
use windows_sys::core::{GUID, HRESULT, PCWSTR, PWSTR};

/// Discovered Windows WASAPI active audio render endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasapiRenderDevice {
    pub friendly_name: String,
    pub endpoint_id: String,
}

/// Pure, unit-testable computation of the aligned 100-nanosecond period required by
/// MSDN after `IAudioClient::Initialize` returns `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED`.
///
/// Formula from Microsoft's Exclusive-Mode Streams reference:
/// `hnsPeriod = (REFERENCE_TIME)((10_000_000.0 * buffer_frames / sample_rate_hz) + 0.5)`
pub fn aligned_buffer_duration_hns(aligned_frames: u32, sample_rate_hz: u32) -> i64 {
    let sr = f64::from(sample_rate_hz.max(1));
    let frames = f64::from(aligned_frames.max(1));
    ((10_000_000.0 * frames) / sr + 0.5) as i64
}

/// Pure, unit-testable HRESULT classifier for WASAPI Exclusive operations.
pub fn classify_wasapi_hr(
    hr: i32,
    operation: &'static str,
    device_name: &str,
    sample_rate_hz: Option<u32>,
    wire_format: Option<DacWireFormat>,
) -> OutputError {
    const HR_DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;
    const HR_EXCLUSIVE_NOT_ALLOWED: i32 = 0x8889_000E_u32 as i32;
    const HR_UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;
    const HR_DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
    const HR_RESOURCES_INVALIDATED: i32 = 0x8889_0026_u32 as i32;
    const HR_INVALIDARG: i32 = 0x8007_0057_u32 as i32;

    let code = hr as u32;
    match hr {
        HR_DEVICE_IN_USE => OutputError::DeviceBusy {
            device: device_name.to_string(),
            os_error: code,
            detail: "WASAPI Exclusive endpoint is in use by another exclusive/KS client (AUDCLNT_E_DEVICE_IN_USE)".to_string(),
        },
        HR_EXCLUSIVE_NOT_ALLOWED => OutputError::DeviceBusy {
            device: device_name.to_string(),
            os_error: code,
            detail: "Exclusive Mode is disabled for this device; enable 'Allow applications to take exclusive control of this device' in Windows Sound settings (AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED)".to_string(),
        },
        HR_UNSUPPORTED_FORMAT | HR_INVALIDARG if sample_rate_hz.is_some() && wire_format.is_some() => {
            OutputError::UnsupportedFormat {
                sample_rate_hz: sample_rate_hz.unwrap_or(0),
                wire_format: wire_format.unwrap_or(DacWireFormat::Int32),
                os_error: code,
                detail: if hr == HR_UNSUPPORTED_FORMAT {
                    "WASAPI Exclusive endpoint rejected integer PCM format (AUDCLNT_E_UNSUPPORTED_FORMAT)".to_string()
                } else {
                    "WASAPI Exclusive endpoint rejected format parameters (E_INVALIDARG)".to_string()
                },
            }
        }
        HR_DEVICE_INVALIDATED | HR_RESOURCES_INVALIDATED => OutputError::DeviceNotFound(format!(
            "WASAPI endpoint '{device_name}' was disconnected or invalidated (HRESULT {code:#010x})"
        )),
        _ => OutputError::Win32 {
            operation,
            code,
            detail: format!("WASAPI call {operation} failed on '{device_name}' (HRESULT {code:#010x})"),
        },
    }
}

/// Exclude Audiophile Player's own virtual exclusive capture endpoint so auto-selection
/// never loops output back into itself.
fn is_own_virtual_endpoint(dev: &WasapiRenderDevice) -> bool {
    let name = dev.friendly_name.to_ascii_lowercase();
    let id = dev.endpoint_id.to_ascii_lowercase();
    id.contains("apexclusiveaudio")
        || id.contains("aptidaltest")
        || name.contains("audiophile player")
}

/// Pure, unit-testable WASAPI render endpoint selection policy (mirrors `select_wdm_ks_device`).
pub fn select_wasapi_device(
    devices: &[WasapiRenderDevice],
    device_filter: Option<&str>,
) -> Result<WasapiRenderDevice, OutputError> {
    if devices.is_empty() {
        return Err(OutputError::DeviceNotFound(
            "no active Windows WASAPI audio render endpoints found".to_string(),
        ));
    }

    let eligible: Vec<&WasapiRenderDevice> = devices
        .iter()
        .filter(|d| !is_own_virtual_endpoint(d))
        .collect();
    if eligible.is_empty() {
        return Err(OutputError::DeviceNotFound(
            "no eligible Windows WASAPI audio render endpoints found".to_string(),
        ));
    }

    if let Some(filter) = device_filter {
        let needle = filter.to_ascii_lowercase();
        return eligible
            .into_iter()
            .find(|d| d.friendly_name.to_ascii_lowercase().contains(&needle))
            .cloned()
            .ok_or_else(|| {
                OutputError::DeviceNotFound(format!(
                    "no active WASAPI render endpoint matching '{filter}'"
                ))
            });
    }

    let score = |d: &WasapiRenderDevice| -> i32 {
        let name = d.friendly_name.to_ascii_lowercase();
        let mut s = 0;
        if name.contains("gungnir") || name.contains("unison") || name.contains("schiit") {
            s += 100;
        }
        if name.contains("usb") || name.contains("dac") || name.contains("uac") {
            s += 40;
        }
        if name.contains("hdmi")
            || name.contains("displayport")
            || name.contains("nvidia")
            || name.contains("amd high definition")
            || name.contains("bluetooth")
            || name.contains("hands-free")
        {
            s -= 50;
        }
        s
    };

    eligible
        .into_iter()
        .max_by_key(|d| score(d))
        .cloned()
        .ok_or_else(|| OutputError::DeviceNotFound("no WASAPI render endpoint available".to_string()))
}

/// Outcome of a single event-driven WASAPI Exclusive buffer fill step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WasapiStepOutcome {
    /// More periods remain; wait for the next hardware buffer-ready event.
    Continue,
    /// All audio (or fade-out) has clocked out cleanly; call `IAudioClient::Stop()`.
    Finished,
}

/// Pure, hardware-independent event-driven WASAPI Exclusive double-buffer state machine.
///
/// Mirrors `WaveRtLoopController` so stop fade-out, EOF drain, and underrun accounting are
/// unit-testable without hardware and behave identically to `WdmKsSink`.
#[derive(Debug)]
pub struct WasapiLoopController {
    period_frames: usize,
    bpf: usize,
    frames_played: usize,
    underrun_periods: usize,
    partial_underrun_periods: usize,
    fading_stop: bool,
    eof_drain_periods: usize,
}

impl WasapiLoopController {
    pub fn new(period_frames: usize, bpf: usize) -> Result<Self, OutputError> {
        if period_frames == 0 || bpf == 0 {
            return Err(OutputError::Win32 {
                operation: "IAudioClient::GetBufferSize",
                code: 0,
                detail: format!(
                    "WASAPI returned degenerate buffer geometry (period_frames={period_frames}, bpf={bpf})"
                ),
            });
        }
        Ok(Self {
            period_frames,
            bpf,
            frames_played: 0,
            underrun_periods: 0,
            partial_underrun_periods: 0,
            fading_stop: false,
            eof_drain_periods: 0,
        })
    }

    pub fn period_bytes(&self) -> usize {
        self.period_frames * self.bpf
    }

    pub fn frames_played(&self) -> usize {
        self.frames_played
    }

    pub fn underrun_periods(&self) -> usize {
        self.underrun_periods
    }

    pub fn partial_underrun_periods(&self) -> usize {
        self.partial_underrun_periods
    }

    /// Fill the initial hardware buffer before `IAudioClient::Start()`.
    /// Returns `(is_silent_buffer)` indicating whether the buffer is pure silence.
    pub fn prefill(
        &mut self,
        dst: &mut [u8],
        pump: &mut dyn WaveRtPumpSource,
    ) -> Result<bool, OutputError> {
        let written = pump.fill_slice(dst);
        if let Some(err) = pump.take_error() {
            dst.fill(0);
            return Err(err);
        }
        self.frames_played += written;
        if !pump.is_eof() {
            if written == 0 {
                self.underrun_periods += 1;
            } else if written < self.period_frames {
                self.partial_underrun_periods += 1;
            }
        }
        if pump.is_eof() && written == 0 {
            // Empty stream: one silence period already queued.
            self.eof_drain_periods = 1;
        }
        Ok(written == 0)
    }

    /// Handle one event-driven WASAPI buffer-ready wakeup.
    /// Returns `(is_silent, outcome)` where `is_silent == true` allows passing
    /// `AUDCLNT_BUFFERFLAGS_SILENT` to `IAudioRenderClient::ReleaseBuffer`.
    pub fn on_event(
        &mut self,
        dst: &mut [u8],
        pump: &mut dyn WaveRtPumpSource,
        stop_requested: bool,
    ) -> Result<(bool, WasapiStepOutcome), OutputError> {
        if self.fading_stop {
            // The fade-out period was written on the previous event; queue one silent buffer so
            // the fade-out clocks completely out of the DAC before `Stop()`.
            dst.fill(0);
            return Ok((true, WasapiStepOutcome::Finished));
        }

        if stop_requested {
            self.fading_stop = true;
            if pump.is_eof() {
                dst.fill(0);
                return Ok((true, WasapiStepOutcome::Finished));
            }
            let written = pump.fill_fade_out_slice(dst);
            if let Some(err) = pump.take_error() {
                dst.fill(0);
                return Err(err);
            }
            self.frames_played += written;
            return Ok((written == 0, WasapiStepOutcome::Continue));
        }

        if self.eof_drain_periods > 0 {
            dst.fill(0);
            self.eof_drain_periods += 1;
            // In event-driven exclusive double-buffering, 2 silent periods after the last audio
            // period guarantee the final real audio frame has clocked completely through the DAC.
            if self.eof_drain_periods >= 2 {
                return Ok((true, WasapiStepOutcome::Finished));
            }
            return Ok((true, WasapiStepOutcome::Continue));
        }

        let written = pump.fill_slice(dst);
        if let Some(err) = pump.take_error() {
            dst.fill(0);
            return Err(err);
        }
        self.frames_played += written;

        if pump.is_eof() {
            if written == 0 {
                // First completely silent period after EOF.
                self.eof_drain_periods = 1;
            } else if written < self.period_frames {
                // Partial final audio period (zero-padded to end of period); queue 1 more full
                // silent period on the next event before stopping so hardware finishes playing it.
                self.eof_drain_periods = 1;
            }
            return Ok((written == 0, WasapiStepOutcome::Continue));
        }

        if written == 0 {
            self.underrun_periods += 1;
        } else if written < self.period_frames {
            self.partial_underrun_periods += 1;
        }
        Ok((written == 0, WasapiStepOutcome::Continue))
    }
}

/// Pure, unit-testable format-negotiation quirk selector (`WDMKS_TIMING_TESTS_REVIEW.md` §5.3 & §5.5).
///
/// Tries standard `FL|FR` channel mask first, then falls back to `dwChannelMask = 0` when a
/// driver rejects the speaker mask with `UnsupportedFormat`. Never silently accepts a closest match.
pub fn negotiate_channel_mask_with_probe<F>(mut probe_mask: F) -> Result<u32, OutputError>
where
    F: FnMut(u32) -> Result<(), OutputError>,
{
    const FL_FR_MASK: u32 = 0x3;
    match probe_mask(FL_FR_MASK) {
        Ok(()) => Ok(FL_FR_MASK),
        Err(OutputError::UnsupportedFormat { .. }) => {
            probe_mask(0)?;
            Ok(0)
        }
        Err(other) => Err(other),
    }
}

/// Event-driven (or push-fallback) WASAPI Exclusive (`AUDCLNT_SHAREMODE_EXCLUSIVE`) output sink.
#[derive(Debug)]
pub struct WasapiExclusiveSink {
    device: WasapiRenderDevice,
    optimization: OptimizationLevel,
    delivery_mode: WasapiDeliveryMode,
    stop_flag: Arc<AtomicBool>,
}

impl WasapiExclusiveSink {
    /// Locate a WASAPI render endpoint whose friendly name contains `device_filter` (case-insensitive).
    pub fn open(device_filter: &str) -> Result<Self, OutputError> {
        Self::open_with_optimization(device_filter, OptimizationLevel::High)
    }

    pub fn open_with_optimization(
        device_filter: &str,
        optimization: OptimizationLevel,
    ) -> Result<Self, OutputError> {
        Self::open_best(Some(device_filter), optimization)
    }

    /// Open the requested WASAPI Exclusive endpoint, or auto-select the highest-priority USB DAC.
    pub fn open_best(
        device_filter: Option<&str>,
        optimization: OptimizationLevel,
    ) -> Result<Self, OutputError> {
        let devices = enumerate_wasapi_render_devices()?;
        let found = select_wasapi_device(&devices, device_filter)?;
        Ok(Self {
            device: found,
            optimization,
            delivery_mode: WasapiDeliveryMode::EventDriven,
            stop_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Select Event-Driven (default) or Push (polling) WASAPI Exclusive mode (§5.2).
    pub fn with_delivery_mode(mut self, mode: WasapiDeliveryMode) -> Self {
        self.delivery_mode = mode;
        self
    }

    pub fn delivery_mode(&self) -> WasapiDeliveryMode {
        self.delivery_mode
    }

    /// Return a thread-safe stop handle that gracefully interrupts active playback with a
    /// click-free linear fade-out.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }

    /// Re-arm the sink after a previous `stop()` so subsequent playback calls proceed.
    pub fn arm(&mut self) {
        self.stop_flag.store(false, Ordering::SeqCst);
    }

    pub fn device(&self) -> &WasapiRenderDevice {
        &self.device
    }

    /// Probe whether the WASAPI Exclusive endpoint accepts `(sample_rate_hz, wire_format)`.
    #[cfg(windows)]
    pub fn probe_format(
        &self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
    ) -> Result<(), OutputError> {
        probe_wasapi_device_format(&self.device, sample_rate_hz, wire_format)
    }

    #[cfg(not(windows))]
    pub fn probe_format(
        &self,
        _sample_rate_hz: u32,
        _wire_format: DacWireFormat,
    ) -> Result<(), OutputError> {
        Err(OutputError::DeviceNotFound(
            "WASAPI Exclusive is only supported on Windows".to_string(),
        ))
    }

    /// Stream bounded `InterleavedInt32Buffer` blocks from `next_block` directly to the DAC
    /// via event-driven WASAPI Exclusive Mode (`AUDCLNT_SHAREMODE_EXCLUSIVE`).
    #[cfg(windows)]
    pub fn play_block_stream<F, B>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> B,
        B: Into<StreamBlock>,
    {
        let mut pump = StreamingWaveRtPump::new(sample_rate_hz, wire_format, next_block);
        self.run_wasapi_pump(sample_rate_hz, wire_format, &mut pump)
    }

    /// Stream bounded `InterleavedInt32Buffer` blocks with an off-RT-thread block recycler.
    #[cfg(windows)]
    pub fn play_block_stream_with_recycler<F, B, R>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
        recycle_block: R,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> B,
        B: Into<StreamBlock>,
        R: FnMut(InterleavedInt32Buffer) + 'static,
    {
        let mut pump = StreamingWaveRtPump::new(sample_rate_hz, wire_format, next_block)
            .with_recycler(recycle_block);
        self.run_wasapi_pump(sample_rate_hz, wire_format, &mut pump)
    }

    #[cfg(not(windows))]
    pub fn play_block_stream<F, B>(
        &mut self,
        _sample_rate_hz: u32,
        _wire_format: DacWireFormat,
        _next_block: F,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> B,
        B: Into<StreamBlock>,
    {
        Err(OutputError::DeviceNotFound(
            "WASAPI Exclusive is only supported on Windows".to_string(),
        ))
    }

    #[cfg(not(windows))]
    pub fn play_block_stream_with_recycler<F, B, R>(
        &mut self,
        _sample_rate_hz: u32,
        _wire_format: DacWireFormat,
        _next_block: F,
        _recycle_block: R,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> B,
        B: Into<StreamBlock>,
        R: FnMut(InterleavedInt32Buffer) + 'static,
    {
        Err(OutputError::DeviceNotFound(
            "WASAPI Exclusive is only supported on Windows".to_string(),
        ))
    }

    #[cfg(windows)]
    fn run_wasapi_pump(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        pump: &mut dyn WaveRtPumpSource,
    ) -> Result<PlaybackStats, OutputError> {
        if self.stop_flag.load(Ordering::SeqCst) {
            return Ok(PlaybackStats {
                device_name: self.device.friendly_name.clone(),
                sample_rate_hz,
                wire_format,
                ..PlaybackStats::default()
            });
        }

        if self.delivery_mode == WasapiDeliveryMode::Push {
            return self.run_wasapi_push_pump(sample_rate_hz, wire_format, pump);
        }

        let mmcss = MmcssGuard::enter_pro_audio(self.optimization);
        let _com = win::ComGuard::init_mta()?;

        let mm_device = win::open_endpoint_by_id(&self.device.endpoint_id, &self.device.friendly_name)?;
        let (audio_client, buffer_frames) = win::initialize_exclusive_event_client(
            &mm_device,
            &self.device.friendly_name,
            sample_rate_hz,
            wire_format,
        )?;

        let event = win::OwnedEvent::create_auto_reset()?;
        win::audio_client_set_event_handle(&audio_client, event.raw(), &self.device.friendly_name)?;
        let render_client =
            win::audio_client_get_render_client(&audio_client, &self.device.friendly_name)?;

        let bpf = wire_format.bytes_per_frame();
        let period_frames = buffer_frames as usize;
        let period_bytes = period_frames * bpf;
        let mut controller = WasapiLoopController::new(period_frames, bpf)?;

        // Pre-fill initial exclusive buffer BEFORE `Start()` (required by MSDN).
        {
            let ptr = win::render_client_get_buffer(
                &render_client,
                buffer_frames,
                &self.device.friendly_name,
            )?;
            let slice = unsafe { core::slice::from_raw_parts_mut(ptr, period_bytes) };
            let prefill_res = controller.prefill(slice, pump);
            let is_silent = matches!(prefill_res, Ok(true)) || prefill_res.is_err();
            let flags = if is_silent {
                AUDCLNT_BUFFERFLAGS_SILENT as u32
            } else {
                0
            };
            win::render_client_release_buffer(
                &render_client,
                buffer_frames,
                flags,
                &self.device.friendly_name,
            )?;
            prefill_res?;
        }

        let start_guard = win::WasapiStartGuard::start(&audio_client, &self.device.friendly_name)?;
        let mut last_wake = Instant::now();
        let mut notifications: usize = 0;
        let mut pump_error: Option<OutputError> = None;
        let mut stats = PlaybackStats {
            device_name: self.device.friendly_name.clone(),
            sample_rate_hz,
            wire_format,
            wavert_buffer_bytes: (period_bytes.saturating_mul(2)) as u32,
            mmcss_pro_audio: mmcss.is_active(),
            ..PlaybackStats::default()
        };

        loop {
            let wait_res = unsafe { WaitForSingleObject(event.raw(), 1500) };
            if wait_res != WAIT_OBJECT_0 {
                drop(start_guard);
                return Err(OutputError::Win32 {
                    operation: "WaitForSingleObject(WASAPI)",
                    code: wait_res,
                    detail: "timed out waiting for WASAPI Exclusive hardware notification event"
                        .to_string(),
                });
            }
            let now = Instant::now();
            let delta_ns = now
                .saturating_duration_since(last_wake)
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            last_wake = now;
            stats.record_wake_interval_ns(delta_ns);
            notifications += 1;

            let stop_requested = self.stop_flag.load(Ordering::SeqCst);
            let ptr = match win::render_client_get_buffer(
                &render_client,
                buffer_frames,
                &self.device.friendly_name,
            ) {
                Ok(p) => p,
                Err(err) => {
                    pump_error = Some(err);
                    break;
                }
            };
            let slice = unsafe { core::slice::from_raw_parts_mut(ptr, period_bytes) };

            let step_res = controller.on_event(slice, pump, stop_requested);
            let (is_silent, finished) = match step_res {
                Ok((silent, WasapiStepOutcome::Continue)) => (silent, false),
                Ok((silent, WasapiStepOutcome::Finished)) => (silent, true),
                Err(err) => {
                    pump_error = Some(err);
                    (true, true)
                }
            };
            let flags = if is_silent {
                AUDCLNT_BUFFERFLAGS_SILENT as u32
            } else {
                0
            };
            if let Err(err) = win::render_client_release_buffer(
                &render_client,
                buffer_frames,
                flags,
                &self.device.friendly_name,
            ) {
                if pump_error.is_none() {
                    pump_error = Some(err);
                }
                break;
            }
            if finished {
                break;
            }
        }

        drop(start_guard);
        if let Some(err) = pump_error {
            return Err(err);
        }

        stats.frames_played = controller.frames_played();
        stats.notifications = notifications;
        stats.underrun_halves = controller.underrun_periods().max(pump.underrun_events());
        stats.partial_underrun_halves = controller.partial_underrun_periods();
        Ok(stats)
    }

    /// Push (polling) WASAPI Exclusive fallback (`AUDCLNT_SHAREMODE_EXCLUSIVE`, flags = 0,
    /// `WDMKS_TIMING_TESTS_REVIEW.md` §5.2 Fallback 1).
    #[cfg(windows)]
    fn run_wasapi_push_pump(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        pump: &mut dyn WaveRtPumpSource,
    ) -> Result<PlaybackStats, OutputError> {
        let mmcss = MmcssGuard::enter_pro_audio(self.optimization);
        let _com = win::ComGuard::init_mta()?;

        let mm_device =
            win::open_endpoint_by_id(&self.device.endpoint_id, &self.device.friendly_name)?;
        let (audio_client, buffer_frames) = win::initialize_exclusive_push_client(
            &mm_device,
            &self.device.friendly_name,
            sample_rate_hz,
            wire_format,
        )?;
        let render_client =
            win::audio_client_get_render_client(&audio_client, &self.device.friendly_name)?;

        let bpf = wire_format.bytes_per_frame();
        let buf_frames_usize = buffer_frames as usize;
        let sleep_dur = Duration::from_secs_f64(
            (buf_frames_usize as f64 / f64::from(sample_rate_hz.max(1))) * 0.25,
        )
        .clamp(Duration::from_millis(2), Duration::from_millis(25));

        let mut frames_played: usize = 0;
        let mut underrun_periods: usize = 0;
        let mut partial_underrun_periods: usize = 0;
        let mut notifications: usize = 0;
        let mut fading_stop = false;
        let mut eof_draining = false;

        // Pre-fill entire exclusive buffer before `Start()`.
        {
            let ptr = win::render_client_get_buffer(
                &render_client,
                buffer_frames,
                &self.device.friendly_name,
            )?;
            let slice = unsafe { core::slice::from_raw_parts_mut(ptr, buf_frames_usize * bpf) };
            let written = pump.fill_slice(slice);
            let err = pump.take_error();
            let flags = if written == 0 || err.is_some() {
                AUDCLNT_BUFFERFLAGS_SILENT as u32
            } else {
                0
            };
            win::render_client_release_buffer(
                &render_client,
                buffer_frames,
                flags,
                &self.device.friendly_name,
            )?;
            if let Some(e) = err {
                return Err(e);
            }
            frames_played += written;
            if pump.is_eof() {
                eof_draining = true;
            }
        }

        let start_guard = win::WasapiStartGuard::start(&audio_client, &self.device.friendly_name)?;
        let mut last_wake = Instant::now();
        let mut pump_error: Option<OutputError> = None;
        let mut stats = PlaybackStats {
            device_name: self.device.friendly_name.clone(),
            sample_rate_hz,
            wire_format,
            wavert_buffer_bytes: (buf_frames_usize * bpf) as u32,
            mmcss_pro_audio: mmcss.is_active(),
            ..PlaybackStats::default()
        };

        loop {
            std::thread::sleep(sleep_dur);
            let now = Instant::now();
            let delta_ns = now
                .saturating_duration_since(last_wake)
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            last_wake = now;
            stats.record_wake_interval_ns(delta_ns);
            notifications += 1;

            let padding = match win::audio_client_get_current_padding(
                &audio_client,
                &self.device.friendly_name,
            ) {
                Ok(p) => p,
                Err(err) => {
                    pump_error = Some(err);
                    break;
                }
            };
            if (eof_draining || fading_stop) && padding == 0 {
                break;
            }
            let avail = buffer_frames.saturating_sub(padding);
            if avail == 0 {
                continue;
            }
            if eof_draining || fading_stop {
                // Wait for remaining padded frames to finish clocking out.
                continue;
            }

            let stop_requested = self.stop_flag.load(Ordering::SeqCst);
            let ptr = match win::render_client_get_buffer(
                &render_client,
                avail,
                &self.device.friendly_name,
            ) {
                Ok(p) => p,
                Err(err) => {
                    pump_error = Some(err);
                    break;
                }
            };
            let slice = unsafe { core::slice::from_raw_parts_mut(ptr, (avail as usize) * bpf) };

            let (written, flags) = if stop_requested {
                fading_stop = true;
                let w = pump.fill_fade_out_slice(slice);
                (
                    w,
                    if w == 0 {
                        AUDCLNT_BUFFERFLAGS_SILENT as u32
                    } else {
                        0
                    },
                )
            } else {
                let w = pump.fill_slice(slice);
                if let Some(e) = pump.take_error() {
                    pump_error = Some(e);
                    (0, AUDCLNT_BUFFERFLAGS_SILENT as u32)
                } else {
                    if pump.is_eof() {
                        eof_draining = true;
                    } else if w == 0 {
                        underrun_periods += 1;
                    } else if w < avail as usize {
                        partial_underrun_periods += 1;
                    }
                    (
                        w,
                        if w == 0 {
                            AUDCLNT_BUFFERFLAGS_SILENT as u32
                        } else {
                            0
                        },
                    )
                }
            };
            frames_played += written;

            if let Err(err) = win::render_client_release_buffer(
                &render_client,
                avail,
                flags,
                &self.device.friendly_name,
            ) {
                if pump_error.is_none() {
                    pump_error = Some(err);
                }
                break;
            }
            if pump_error.is_some() {
                break;
            }
        }

        drop(start_guard);
        if let Some(err) = pump_error {
            return Err(err);
        }

        stats.frames_played = frames_played;
        stats.notifications = notifications;
        stats.underrun_halves = underrun_periods.max(pump.underrun_events());
        stats.partial_underrun_halves = partial_underrun_periods;
        Ok(stats)
    }
}

impl ZeroMixerAudioSink for WasapiExclusiveSink {
    #[cfg(windows)]
    fn play_full_memory(
        &mut self,
        pcm: &InterleavedInt32Buffer,
        wire_format: DacWireFormat,
    ) -> Result<PlaybackStats, OutputError> {
        if pcm.frames.is_empty() {
            return Err(OutputError::InvalidBuffer("PCM buffer is empty".to_string()));
        }
        let _mem_lock = MemoryLockGuard::lock_slice(&pcm.frames);
        let mut pump = WaveRtFramePump::new(&pcm.frames, wire_format);
        self.run_wasapi_pump(pcm.sample_rate_hz, wire_format, &mut pump)
    }

    #[cfg(not(windows))]
    fn play_full_memory(
        &mut self,
        _pcm: &InterleavedInt32Buffer,
        _wire_format: DacWireFormat,
    ) -> Result<PlaybackStats, OutputError> {
        Err(OutputError::DeviceNotFound(
            "WASAPI Exclusive is only supported on Windows".to_string(),
        ))
    }

    fn stop(&mut self) -> Result<(), OutputError> {
        self.stop_flag.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn arm(&mut self) {
        WasapiExclusiveSink::arm(self);
    }
}

/// Enumerate all active Windows WASAPI audio render endpoints (`eRender`, `DEVICE_STATE_ACTIVE`).
#[cfg(windows)]
pub fn enumerate_wasapi_render_devices() -> Result<Vec<WasapiRenderDevice>, OutputError> {
    win::enumerate_render_endpoints()
}

#[cfg(not(windows))]
pub fn enumerate_wasapi_render_devices() -> Result<Vec<WasapiRenderDevice>, OutputError> {
    Err(OutputError::DeviceNotFound(
        "WASAPI Exclusive is only supported on Windows".to_string(),
    ))
}

/// Probe whether a specific WASAPI render endpoint accepts `(sample_rate_hz, wire_format)` in
/// strictly Exclusive Mode (`AUDCLNT_SHAREMODE_EXCLUSIVE`).
#[cfg(windows)]
pub fn probe_wasapi_device_format(
    device: &WasapiRenderDevice,
    sample_rate_hz: u32,
    wire_format: DacWireFormat,
) -> Result<(), OutputError> {
    win::probe_endpoint_exclusive_format(device, sample_rate_hz, wire_format)
}

#[cfg(not(windows))]
pub fn probe_wasapi_device_format(
    _device: &WasapiRenderDevice,
    _sample_rate_hz: u32,
    _wire_format: DacWireFormat,
) -> Result<(), OutputError> {
    Err(OutputError::DeviceNotFound(
        "WASAPI Exclusive is only supported on Windows".to_string(),
    ))
}

/// Probe a WASAPI Exclusive render endpoint to find its highest supported output
/// `(sample_rate_hz, wire_format)` for both the 44.1 kHz (`352,800 Hz`) and 48 kHz (`384,000 Hz`)
/// DSP clock families, honoring an optional user `max_rate_hz` ceiling.
pub fn probe_wasapi_device_capabilities(
    device: &WasapiRenderDevice,
    max_rate_hz: Option<u32>,
) -> Result<DacOutputCapabilities, OutputError> {
    DacOutputCapabilities::from_probe(max_rate_hz, |rate, fmt| {
        match probe_wasapi_device_format(device, rate, fmt) {
            Ok(()) => Ok(true),
            Err(e @ OutputError::DeviceBusy { .. }) => Err(e),
            Err(_) => Ok(false),
        }
    })
}

/// Build an exact integer PCM `WAVEFORMATEXTENSIBLE` for WASAPI Exclusive Mode (never float/mix),
/// with an explicit `dwChannelMask` override (supports the `dwChannelMask = 0` driver quirk, §5.3).
#[cfg(windows)]
pub fn build_wasapi_wave_format_with_mask(
    sample_rate_hz: u32,
    channels: u16,
    wire_format: DacWireFormat,
    channel_mask: u32,
) -> WAVEFORMATEXTENSIBLE {
    let (bits_per_sample, valid_bits) = match wire_format {
        DacWireFormat::Int32 => (32_u16, 32_u16),
        DacWireFormat::Int24In32 => (32_u16, 24_u16),
        DacWireFormat::Int24Packed => (24_u16, 24_u16),
        DacWireFormat::Int16 => (16_u16, 16_u16),
    };
    let block_align = channels * (bits_per_sample / 8);
    let avg_bytes_per_sec = sample_rate_hz * u32::from(block_align);

    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: sample_rate_hz,
            nAvgBytesPerSec: avg_bytes_per_sec,
            nBlockAlign: block_align,
            wBitsPerSample: bits_per_sample,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: valid_bits,
        },
        dwChannelMask: channel_mask,
        SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
    }
}

/// Build an exact integer PCM `WAVEFORMATEXTENSIBLE` for WASAPI Exclusive Mode (never float/mix).
#[cfg(windows)]
pub fn build_wasapi_wave_format(
    sample_rate_hz: u32,
    channels: u16,
    wire_format: DacWireFormat,
) -> WAVEFORMATEXTENSIBLE {
    let channel_mask = if channels == 2 {
        SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
    } else if channels == 0 {
        0
    } else {
        1_u32
            .checked_shl(u32::from(channels))
            .map(|v| v - 1)
            .unwrap_or(u32::MAX)
    };
    build_wasapi_wave_format_with_mask(sample_rate_hz, channels, wire_format, channel_mask)
}

#[cfg(windows)]
mod win {
    use super::*;

    // Immutable Microsoft COM IIDs (Windows SDK mmdeviceapi.h / propsys.h / audioclient.h).
    const IID_IMMDEVICE_ENUMERATOR: GUID =
        GUID::from_u128(0xa95664d2_9614_4f35_a746_de8db63617e6);
    const IID_IAUDIO_CLIENT: GUID = GUID::from_u128(0x1cb9ad4c_dbfa_4c32_b178_c2f568a703b2);
    const IID_IAUDIO_RENDER_CLIENT: GUID = GUID::from_u128(0xf294acfc_3146_4483_a7bf_addca7c260e2);

    // PKEY_Device_FriendlyName = {A45C254E-DF1C-4EFD-8020-67D146A850E0}, pid 14
    const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    struct IMMDeviceEnumeratorVtbl {
        base: IUnknownVtbl,
        enum_audio_endpoints: unsafe extern "system" fn(
            *mut c_void,
            EDataFlow,
            u32,
            *mut *mut IMMDeviceCollectionRaw,
        ) -> HRESULT,
        get_default_audio_endpoint: unsafe extern "system" fn(
            *mut c_void,
            EDataFlow,
            ERole,
            *mut *mut IMMDeviceRaw,
        ) -> HRESULT,
        get_device: unsafe extern "system" fn(
            *mut c_void,
            PCWSTR,
            *mut *mut IMMDeviceRaw,
        ) -> HRESULT,
    }

    #[repr(C)]
    struct IMMDeviceEnumeratorRaw {
        vtbl: *const IMMDeviceEnumeratorVtbl,
    }

    #[repr(C)]
    struct IMMDeviceCollectionVtbl {
        base: IUnknownVtbl,
        get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut IMMDeviceRaw) -> HRESULT,
    }

    #[repr(C)]
    struct IMMDeviceCollectionRaw {
        vtbl: *const IMMDeviceCollectionVtbl,
    }

    #[repr(C)]
    struct IMMDeviceVtbl {
        base: IUnknownVtbl,
        activate: unsafe extern "system" fn(
            *mut c_void,
            *const GUID,
            u32,
            *mut PROPVARIANT,
            *mut *mut c_void,
        ) -> HRESULT,
        open_property_store: unsafe extern "system" fn(
            *mut c_void,
            u32,
            *mut *mut IPropertyStoreRaw,
        ) -> HRESULT,
        get_id: unsafe extern "system" fn(*mut c_void, *mut PWSTR) -> HRESULT,
        get_state: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    }

    #[repr(C)]
    pub(super) struct IMMDeviceRaw {
        vtbl: *const IMMDeviceVtbl,
    }

    #[repr(C)]
    struct IPropertyStoreVtbl {
        base: IUnknownVtbl,
        get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        get_at: unsafe extern "system" fn(*mut c_void, u32, *mut PROPERTYKEY) -> HRESULT,
        get_value: unsafe extern "system" fn(
            *mut c_void,
            *const PROPERTYKEY,
            *mut PROPVARIANT,
        ) -> HRESULT,
    }

    #[repr(C)]
    struct IPropertyStoreRaw {
        vtbl: *const IPropertyStoreVtbl,
    }

    #[repr(C)]
    struct IAudioClientVtbl {
        base: IUnknownVtbl,
        initialize: unsafe extern "system" fn(
            *mut c_void,
            AUDCLNT_SHAREMODE,
            u32,
            i64,
            i64,
            *const WAVEFORMATEX,
            *const GUID,
        ) -> HRESULT,
        get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        get_stream_latency: unsafe extern "system" fn(*mut c_void, *mut i64) -> HRESULT,
        get_current_padding: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
        is_format_supported: unsafe extern "system" fn(
            *mut c_void,
            AUDCLNT_SHAREMODE,
            *const WAVEFORMATEX,
            *mut *mut WAVEFORMATEX,
        ) -> HRESULT,
        get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WAVEFORMATEX) -> HRESULT,
        get_device_period: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> HRESULT,
        start: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        stop: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        reset: unsafe extern "system" fn(*mut c_void) -> HRESULT,
        set_event_handle: unsafe extern "system" fn(*mut c_void, HANDLE) -> HRESULT,
        get_service: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    pub(super) struct IAudioClientRaw {
        vtbl: *const IAudioClientVtbl,
    }

    #[repr(C)]
    struct IAudioRenderClientVtbl {
        base: IUnknownVtbl,
        get_buffer: unsafe extern "system" fn(*mut c_void, u32, *mut *mut u8) -> HRESULT,
        release_buffer: unsafe extern "system" fn(*mut c_void, u32, u32) -> HRESULT,
    }

    #[repr(C)]
    pub(super) struct IAudioRenderClientRaw {
        vtbl: *const IAudioRenderClientVtbl,
    }

    /// RAII COM interface pointer guaranteeing `IUnknown::Release()` on every exit path.
    pub(super) struct ComPtr<T> {
        ptr: *mut T,
    }

    impl<T> ComPtr<T> {
        unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
            if ptr.is_null() {
                None
            } else {
                Some(Self { ptr })
            }
        }

        #[inline]
        fn as_ptr(&self) -> *mut T {
            self.ptr
        }
    }

    impl<T> Drop for ComPtr<T> {
        fn drop(&mut self) {
            if !self.ptr.is_null() {
                unsafe {
                    let vtbl = *(self.ptr.cast::<*const IUnknownVtbl>());
                    ((*vtbl).release)(self.ptr.cast::<c_void>());
                }
            }
        }
    }

    /// RAII COM apartment guard (only calls `CoUninitialize` if `CoInitializeEx` succeeded).
    pub(super) struct ComGuard {
        should_uninit: bool,
    }

    impl ComGuard {
        pub(super) fn init_mta() -> Result<Self, OutputError> {
            let hr = unsafe { CoInitializeEx(core::ptr::null(), COINIT_MULTITHREADED as u32) };
            if hr == S_OK || hr == S_FALSE {
                Ok(Self { should_uninit: true })
            } else if hr == RPC_E_CHANGED_MODE {
                // Thread already initialized as STA (e.g. test harness) — safe to proceed without uninit.
                Ok(Self {
                    should_uninit: false,
                })
            } else {
                Err(OutputError::Win32 {
                    operation: "CoInitializeEx",
                    code: hr as u32,
                    detail: format!("COM initialization failed (HRESULT {:#010x})", hr as u32),
                })
            }
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.should_uninit {
                unsafe { CoUninitialize() };
            }
        }
    }

    struct PropVariantGuard(PROPVARIANT);

    impl PropVariantGuard {
        fn zeroed() -> Self {
            Self(unsafe { core::mem::zeroed() })
        }
    }

    impl Drop for PropVariantGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = PropVariantClear(&mut self.0);
            }
        }
    }

    struct CoTaskMemStr(PWSTR);

    impl CoTaskMemStr {
        fn to_string_lossy(&self) -> String {
            if self.0.is_null() {
                return String::new();
            }
            unsafe {
                let mut len = 0usize;
                while *self.0.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16_lossy(core::slice::from_raw_parts(self.0, len))
            }
        }
    }

    impl Drop for CoTaskMemStr {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CoTaskMemFree(self.0.cast::<c_void>()) };
            }
        }
    }

    pub(super) struct OwnedEvent(HANDLE);

    impl OwnedEvent {
        pub(super) fn create_auto_reset() -> Result<Self, OutputError> {
            let h = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
            if h.is_null() {
                return Err(OutputError::Win32 {
                    operation: "CreateEventW(WASAPI)",
                    code: 0,
                    detail: "failed to create WASAPI notification event".to_string(),
                });
            }
            Ok(Self(h))
        }

        pub(super) fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedEvent {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    pub(super) struct WasapiStartGuard<'a> {
        client: &'a ComPtr<IAudioClientRaw>,
    }

    impl<'a> WasapiStartGuard<'a> {
        pub(super) fn start(
            client: &'a ComPtr<IAudioClientRaw>,
            device_name: &str,
        ) -> Result<Self, OutputError> {
            let hr = unsafe {
                let vtbl = (*client.as_ptr()).vtbl;
                ((*vtbl).start)(client.as_ptr().cast::<c_void>())
            };
            if hr < 0 {
                return Err(classify_wasapi_hr(
                    hr,
                    "IAudioClient::Start",
                    device_name,
                    None,
                    None,
                ));
            }
            Ok(Self { client })
        }
    }

    impl Drop for WasapiStartGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                let vtbl = (*self.client.as_ptr()).vtbl;
                let _ = ((*vtbl).stop)(self.client.as_ptr().cast::<c_void>());
                let _ = ((*vtbl).reset)(self.client.as_ptr().cast::<c_void>());
            }
        }
    }

    fn create_device_enumerator() -> Result<ComPtr<IMMDeviceEnumeratorRaw>, OutputError> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &MMDeviceEnumerator,
                core::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMMDEVICE_ENUMERATOR,
                &mut raw,
            )
        };
        if hr < 0 {
            return Err(OutputError::Win32 {
                operation: "CoCreateInstance(MMDeviceEnumerator)",
                code: hr as u32,
                detail: format!("failed to create MMDeviceEnumerator (HRESULT {:#010x})", hr as u32),
            });
        }
        unsafe { ComPtr::from_raw(raw.cast::<IMMDeviceEnumeratorRaw>()) }.ok_or_else(|| {
            OutputError::Win32 {
                operation: "CoCreateInstance(MMDeviceEnumerator)",
                code: 0,
                detail: "MMDeviceEnumerator returned null pointer".to_string(),
            }
        })
    }

    fn read_endpoint_friendly_name(dev: &ComPtr<IMMDeviceRaw>) -> String {
        let mut store_raw: *mut IPropertyStoreRaw = core::ptr::null_mut();
        let hr = unsafe {
            let vtbl = (*dev.as_ptr()).vtbl;
            ((*vtbl).open_property_store)(dev.as_ptr().cast::<c_void>(), STGM_READ, &mut store_raw)
        };
        if hr < 0 {
            return String::new();
        }
        let Some(store) = (unsafe { ComPtr::from_raw(store_raw) }) else {
            return String::new();
        };
        let mut pv = PropVariantGuard::zeroed();
        let hr = unsafe {
            let vtbl = (*store.as_ptr()).vtbl;
            ((*vtbl).get_value)(
                store.as_ptr().cast::<c_void>(),
                &PKEY_DEVICE_FRIENDLY_NAME,
                &mut pv.0,
            )
        };
        if hr < 0 {
            return String::new();
        }
        unsafe {
            if pv.0.Anonymous.Anonymous.vt == VT_LPWSTR {
                let p = pv.0.Anonymous.Anonymous.Anonymous.pwszVal;
                if !p.is_null() {
                    let mut len = 0usize;
                    while *p.add(len) != 0 {
                        len += 1;
                    }
                    return String::from_utf16_lossy(core::slice::from_raw_parts(p, len));
                }
            }
        }
        String::new()
    }

    fn read_endpoint_id(dev: &ComPtr<IMMDeviceRaw>) -> Result<String, OutputError> {
        let mut id_ptr: PWSTR = core::ptr::null_mut();
        let hr = unsafe {
            let vtbl = (*dev.as_ptr()).vtbl;
            ((*vtbl).get_id)(dev.as_ptr().cast::<c_void>(), &mut id_ptr)
        };
        if hr < 0 {
            return Err(OutputError::Win32 {
                operation: "IMMDevice::GetId",
                code: hr as u32,
                detail: format!("IMMDevice::GetId failed (HRESULT {:#010x})", hr as u32),
            });
        }
        let guard = CoTaskMemStr(id_ptr);
        Ok(guard.to_string_lossy())
    }

    pub(super) fn enumerate_render_endpoints() -> Result<Vec<WasapiRenderDevice>, OutputError> {
        let _com = ComGuard::init_mta()?;
        let enumerator = create_device_enumerator()?;

        let mut coll_raw: *mut IMMDeviceCollectionRaw = core::ptr::null_mut();
        let hr = unsafe {
            let vtbl = (*enumerator.as_ptr()).vtbl;
            ((*vtbl).enum_audio_endpoints)(
                enumerator.as_ptr().cast::<c_void>(),
                eRender,
                DEVICE_STATE_ACTIVE,
                &mut coll_raw,
            )
        };
        if hr < 0 {
            return Err(OutputError::Win32 {
                operation: "IMMDeviceEnumerator::EnumAudioEndpoints",
                code: hr as u32,
                detail: format!("EnumAudioEndpoints failed (HRESULT {:#010x})", hr as u32),
            });
        }
        let Some(coll) = (unsafe { ComPtr::from_raw(coll_raw) }) else {
            return Ok(Vec::new());
        };

        let mut count: u32 = 0;
        let hr = unsafe {
            let vtbl = (*coll.as_ptr()).vtbl;
            ((*vtbl).get_count)(coll.as_ptr().cast::<c_void>(), &mut count)
        };
        if hr < 0 {
            return Err(OutputError::Win32 {
                operation: "IMMDeviceCollection::GetCount",
                code: hr as u32,
                detail: format!("GetCount failed (HRESULT {:#010x})", hr as u32),
            });
        }

        let mut out = Vec::with_capacity(count as usize);
        for idx in 0..count {
            let mut dev_raw: *mut IMMDeviceRaw = core::ptr::null_mut();
            let hr = unsafe {
                let vtbl = (*coll.as_ptr()).vtbl;
                ((*vtbl).item)(coll.as_ptr().cast::<c_void>(), idx, &mut dev_raw)
            };
            if hr < 0 {
                continue;
            }
            let Some(dev) = (unsafe { ComPtr::from_raw(dev_raw) }) else {
                continue;
            };
            let Ok(endpoint_id) = read_endpoint_id(&dev) else {
                continue;
            };
            let friendly_name = read_endpoint_friendly_name(&dev);
            let display_name = if friendly_name.is_empty() {
                endpoint_id.clone()
            } else {
                friendly_name
            };
            out.push(WasapiRenderDevice {
                friendly_name: display_name,
                endpoint_id,
            });
        }
        Ok(out)
    }

    pub(super) fn open_endpoint_by_id(
        endpoint_id: &str,
        device_name: &str,
    ) -> Result<ComPtr<IMMDeviceRaw>, OutputError> {
        let enumerator = create_device_enumerator()?;
        let wide_id: Vec<u16> = endpoint_id.encode_utf16().chain(Some(0)).collect();
        let mut dev_raw: *mut IMMDeviceRaw = core::ptr::null_mut();
        let hr = unsafe {
            let vtbl = (*enumerator.as_ptr()).vtbl;
            ((*vtbl).get_device)(
                enumerator.as_ptr().cast::<c_void>(),
                wide_id.as_ptr(),
                &mut dev_raw,
            )
        };
        if hr < 0 {
            return Err(OutputError::DeviceNotFound(format!(
                "WASAPI endpoint '{device_name}' ({endpoint_id}) could not be opened (HRESULT {:#010x})",
                hr as u32
            )));
        }
        unsafe { ComPtr::from_raw(dev_raw) }.ok_or_else(|| {
            OutputError::DeviceNotFound(format!(
                "WASAPI endpoint '{device_name}' returned null pointer"
            ))
        })
    }

    fn activate_audio_client(
        dev: &ComPtr<IMMDeviceRaw>,
        device_name: &str,
    ) -> Result<ComPtr<IAudioClientRaw>, OutputError> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        let hr = unsafe {
            let vtbl = (*dev.as_ptr()).vtbl;
            ((*vtbl).activate)(
                dev.as_ptr().cast::<c_void>(),
                &IID_IAUDIO_CLIENT,
                CLSCTX_ALL,
                core::ptr::null_mut(),
                &mut raw,
            )
        };
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IMMDevice::Activate(IAudioClient)",
                device_name,
                None,
                None,
            ));
        }
        unsafe { ComPtr::from_raw(raw.cast::<IAudioClientRaw>()) }.ok_or_else(|| {
            OutputError::Win32 {
                operation: "IMMDevice::Activate(IAudioClient)",
                code: 0,
                detail: format!("Activate returned null IAudioClient for '{device_name}'"),
            }
        })
    }

    fn audio_client_get_buffer_size(
        client: &ComPtr<IAudioClientRaw>,
        device_name: &str,
    ) -> Result<u32, OutputError> {
        let mut frames: u32 = 0;
        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).get_buffer_size)(client.as_ptr().cast::<c_void>(), &mut frames)
        };
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioClient::GetBufferSize",
                device_name,
                None,
                None,
            ));
        }
        Ok(frames)
    }

    fn audio_client_get_device_period(
        client: &ComPtr<IAudioClientRaw>,
        device_name: &str,
    ) -> Result<(i64, i64), OutputError> {
        let mut default_period: i64 = 0;
        let mut min_period: i64 = 0;
        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).get_device_period)(
                client.as_ptr().cast::<c_void>(),
                &mut default_period,
                &mut min_period,
            )
        };
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioClient::GetDevicePeriod",
                device_name,
                None,
                None,
            ));
        }
        Ok((default_period, min_period))
    }

    fn probe_client_mask(
        client: &ComPtr<IAudioClientRaw>,
        device_name: &str,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        channel_mask: u32,
    ) -> Result<(), OutputError> {
        let wfx = build_wasapi_wave_format_with_mask(sample_rate_hz, 2, wire_format, channel_mask);
        let fmt_ptr = core::ptr::addr_of!(wfx).cast::<WAVEFORMATEX>();
        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).is_format_supported)(
                client.as_ptr().cast::<c_void>(),
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                fmt_ptr,
                core::ptr::null_mut(),
            )
        };
        if hr == S_OK {
            Ok(())
        } else if hr == S_FALSE {
            Err(OutputError::UnsupportedFormat {
                sample_rate_hz,
                wire_format,
                os_error: hr as u32,
                detail: "WASAPI Exclusive endpoint does not support exact integer PCM format (S_FALSE)"
                    .to_string(),
            })
        } else {
            Err(classify_wasapi_hr(
                hr,
                "IAudioClient::IsFormatSupported",
                device_name,
                Some(sample_rate_hz),
                Some(wire_format),
            ))
        }
    }

    pub(super) fn audio_client_get_current_padding(
        client: &ComPtr<IAudioClientRaw>,
        device_name: &str,
    ) -> Result<u32, OutputError> {
        let mut padding: u32 = 0;
        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).get_current_padding)(client.as_ptr().cast::<c_void>(), &mut padding)
        };
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioClient::GetCurrentPadding",
                device_name,
                None,
                None,
            ));
        }
        Ok(padding)
    }

    /// Initialize a Push (polling) WASAPI Exclusive `IAudioClient` (flags = 0, §5.2 Fallback 1).
    pub(super) fn initialize_exclusive_push_client(
        dev: &ComPtr<IMMDeviceRaw>,
        device_name: &str,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
    ) -> Result<(ComPtr<IAudioClientRaw>, u32), OutputError> {
        let client = activate_audio_client(dev, device_name)?;
        let mask = negotiate_channel_mask_with_probe(|m| {
            probe_client_mask(&client, device_name, sample_rate_hz, wire_format, m)
        })
        .unwrap_or(SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT);
        let wfx = build_wasapi_wave_format_with_mask(sample_rate_hz, 2, wire_format, mask);
        let fmt_ptr = core::ptr::addr_of!(wfx).cast::<WAVEFORMATEX>();
        let (default_period, min_period) = audio_client_get_device_period(&client, device_name)?;
        let requested_hns = default_period.max(min_period).saturating_mul(4).max(400_000);

        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).initialize)(
                client.as_ptr().cast::<c_void>(),
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                0,
                requested_hns,
                0,
                fmt_ptr,
                core::ptr::null(),
            )
        };
        if hr == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
            let aligned_frames = audio_client_get_buffer_size(&client, device_name)?;
            let aligned_hns = aligned_buffer_duration_hns(aligned_frames, sample_rate_hz);
            drop(client);
            let fresh_client = activate_audio_client(dev, device_name)?;
            let hr2 = unsafe {
                let vtbl = (*fresh_client.as_ptr()).vtbl;
                ((*vtbl).initialize)(
                    fresh_client.as_ptr().cast::<c_void>(),
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    0,
                    aligned_hns,
                    0,
                    fmt_ptr,
                    core::ptr::null(),
                )
            };
            if hr2 < 0 {
                return Err(classify_wasapi_hr(
                    hr2,
                    "IAudioClient::Initialize(push-aligned)",
                    device_name,
                    Some(sample_rate_hz),
                    Some(wire_format),
                ));
            }
            let buf_frames = audio_client_get_buffer_size(&fresh_client, device_name)?;
            return Ok((fresh_client, buf_frames));
        }
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioClient::Initialize(push)",
                device_name,
                Some(sample_rate_hz),
                Some(wire_format),
            ));
        }
        let buf_frames = audio_client_get_buffer_size(&client, device_name)?;
        Ok((client, buf_frames))
    }

    /// Initialize an event-driven WASAPI Exclusive `IAudioClient` performing the channel-mask
    /// quirk check and mandatory MSDN `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` alignment retry sequence.
    pub(super) fn initialize_exclusive_event_client(
        dev: &ComPtr<IMMDeviceRaw>,
        device_name: &str,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
    ) -> Result<(ComPtr<IAudioClientRaw>, u32), OutputError> {
        let client = activate_audio_client(dev, device_name)?;
        let mask = negotiate_channel_mask_with_probe(|m| {
            probe_client_mask(&client, device_name, sample_rate_hz, wire_format, m)
        })
        .unwrap_or(SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT);
        let wfx = build_wasapi_wave_format_with_mask(sample_rate_hz, 2, wire_format, mask);
        let fmt_ptr = core::ptr::addr_of!(wfx).cast::<WAVEFORMATEX>();

        let (default_period, min_period) = audio_client_get_device_period(&client, device_name)?;
        // In event-driven Exclusive Mode, buffer duration == periodicity; default_period is
        // driver-chosen (typically 10 ms = 100,000 hns) for glitch-free 352.8/384 kHz streaming.
        let requested_hns = default_period.max(min_period).max(100_000);

        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).initialize)(
                client.as_ptr().cast::<c_void>(),
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                requested_hns,
                requested_hns,
                fmt_ptr,
                core::ptr::null(),
            )
        };

        if hr == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
            // Mandatory MSDN alignment dance: query aligned frame count, compute exact 100-ns
            // duration, drop old IAudioClient, re-activate a fresh IAudioClient, and re-initialize.
            let aligned_frames = audio_client_get_buffer_size(&client, device_name)?;
            let aligned_hns = aligned_buffer_duration_hns(aligned_frames, sample_rate_hz);
            drop(client);

            let fresh_client = activate_audio_client(dev, device_name)?;
            let hr2 = unsafe {
                let vtbl = (*fresh_client.as_ptr()).vtbl;
                ((*vtbl).initialize)(
                    fresh_client.as_ptr().cast::<c_void>(),
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    aligned_hns,
                    aligned_hns,
                    fmt_ptr,
                    core::ptr::null(),
                )
            };
            if hr2 < 0 {
                return Err(classify_wasapi_hr(
                    hr2,
                    "IAudioClient::Initialize(aligned)",
                    device_name,
                    Some(sample_rate_hz),
                    Some(wire_format),
                ));
            }
            let buf_frames = audio_client_get_buffer_size(&fresh_client, device_name)?;
            return Ok((fresh_client, buf_frames));
        }

        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioClient::Initialize",
                device_name,
                Some(sample_rate_hz),
                Some(wire_format),
            ));
        }

        let buf_frames = audio_client_get_buffer_size(&client, device_name)?;
        Ok((client, buf_frames))
    }

    pub(super) fn probe_endpoint_exclusive_format(
        device: &WasapiRenderDevice,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
    ) -> Result<(), OutputError> {
        let _com = ComGuard::init_mta()?;
        let dev = open_endpoint_by_id(&device.endpoint_id, &device.friendly_name)?;
        let client = activate_audio_client(&dev, &device.friendly_name)?;
        negotiate_channel_mask_with_probe(|mask| {
            probe_client_mask(&client, &device.friendly_name, sample_rate_hz, wire_format, mask)
        })
        .map(|_| ())
    }

    pub(super) fn audio_client_set_event_handle(
        client: &ComPtr<IAudioClientRaw>,
        event: HANDLE,
        device_name: &str,
    ) -> Result<(), OutputError> {
        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).set_event_handle)(client.as_ptr().cast::<c_void>(), event)
        };
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioClient::SetEventHandle",
                device_name,
                None,
                None,
            ));
        }
        Ok(())
    }

    pub(super) fn audio_client_get_render_client(
        client: &ComPtr<IAudioClientRaw>,
        device_name: &str,
    ) -> Result<ComPtr<IAudioRenderClientRaw>, OutputError> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        let hr = unsafe {
            let vtbl = (*client.as_ptr()).vtbl;
            ((*vtbl).get_service)(
                client.as_ptr().cast::<c_void>(),
                &IID_IAUDIO_RENDER_CLIENT,
                &mut raw,
            )
        };
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioClient::GetService(IAudioRenderClient)",
                device_name,
                None,
                None,
            ));
        }
        unsafe { ComPtr::from_raw(raw.cast::<IAudioRenderClientRaw>()) }.ok_or_else(|| {
            OutputError::Win32 {
                operation: "IAudioClient::GetService(IAudioRenderClient)",
                code: 0,
                detail: format!("GetService returned null IAudioRenderClient for '{device_name}'"),
            }
        })
    }

    pub(super) fn render_client_get_buffer(
        render: &ComPtr<IAudioRenderClientRaw>,
        num_frames: u32,
        device_name: &str,
    ) -> Result<*mut u8, OutputError> {
        let mut ptr: *mut u8 = core::ptr::null_mut();
        let hr = unsafe {
            let vtbl = (*render.as_ptr()).vtbl;
            ((*vtbl).get_buffer)(render.as_ptr().cast::<c_void>(), num_frames, &mut ptr)
        };
        if hr < 0 || ptr.is_null() {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioRenderClient::GetBuffer",
                device_name,
                None,
                None,
            ));
        }
        Ok(ptr)
    }

    pub(super) fn render_client_release_buffer(
        render: &ComPtr<IAudioRenderClientRaw>,
        num_frames: u32,
        flags: u32,
        device_name: &str,
    ) -> Result<(), OutputError> {
        let hr = unsafe {
            let vtbl = (*render.as_ptr()).vtbl;
            ((*vtbl).release_buffer)(render.as_ptr().cast::<c_void>(), num_frames, flags)
        };
        if hr < 0 {
            return Err(classify_wasapi_hr(
                hr,
                "IAudioRenderClient::ReleaseBuffer",
                device_name,
                None,
                None,
            ));
        }
        Ok(())
    }

    // Sanity-check unused import constants match our classifier values at compile time.
    const _: () = {
        assert!(AUDCLNT_E_DEVICE_IN_USE == 0x8889_000A_u32 as i32);
        assert!(AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED == 0x8889_000E_u32 as i32);
        assert!(AUDCLNT_E_UNSUPPORTED_FORMAT == 0x8889_0008_u32 as i32);
        assert!(AUDCLNT_E_DEVICE_INVALIDATED == 0x8889_0004_u32 as i32);
        assert!(E_INVALIDARG == 0x8007_0057_u32 as i32);
    };
}
