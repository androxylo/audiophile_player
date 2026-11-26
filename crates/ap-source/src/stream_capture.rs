// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Live exclusive-mode kernel stream capture source (`\\.\ApExclusiveAudio`).
//!
//! Connects to Audiophile Player's opt-in pure-Rust KMDF + ACX 1.1 virtual render driver
//! (`drivers/ap-exclusive-acx`), enforces the zero-mixer checks (`audiodg.exe` rejected in
//! kernel at `Run`/`SetRenderPacket` and double-checked in user mode), drains kernel PCM on
//! a decoupled background reader thread (preventing kernel ring overruns during heavy DSP/REW
//! setup), throttles kernel reads against engine cushion fill so DAC clock slaving locks in
//! both directions, preserves new-generation boundary bytes via a one-chunk stash, and feeds
//! exact pure-`f64` stereo blocks into [`AudioBlockSource`] with `is_streaming() == true`
//! (`-3 dB` unknown-peak headroom, dynamic rate following across `44.1..=384 kHz`).

use std::collections::VecDeque;
use std::fmt;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;
use ap_stream_protocol::{
    AP_STREAM_ABI_VERSION, ApDriverConfig, ApDriverStatus, ApPcmChunkHeader, ApStreamFormat,
    DEVICE_WIN32_PATH, IOCTL_AP_GET_STATUS, IOCTL_AP_READ_PCM, IOCTL_AP_RESET_RING,
    IOCTL_AP_SET_CONFIG, is_audiodg_name, is_system_or_idle_name, proc_name_str, stage_name,
};

use crate::flac::AudioBlockSource;

/// Maximum raw PCM payload requested per `IOCTL_AP_READ_PCM` call (`128 KiB`).
const IOCTL_PCM_PAYLOAD_BYTES: usize = 131_072;

/// Idle time after `stream_running == 0` and empty ring before returning clean EOF (`Ok(None)`)
/// so the engine drains the REW tail and releases the WDM-KS DAC pin promptly on Tidal Pause/Stop.
const PAUSE_EOF_GRACE: Duration = Duration::from_millis(250);

/// Target engine output buffer fill before throttling kernel ring reads (Review #8 B4).
/// Keeping engine fill near 1.0 s lets kernel ring fill reflect real DAC consumption so the
/// kernel's signed ±500 ppm clock slaving locks smoothly for both fast and slow DAC crystals.
pub const CAPTURE_TARGET_ENGINE_FILL: Duration = Duration::from_millis(1_000);

/// Maximum buffered raw PCM bytes held inside the decoupled user-mode drainer queue (~4 s @
/// 384 kHz 32-bit stereo = 12 MiB; absorbs heavy REW/filter setup without kernel ring overruns,
/// Review #8 B6).
const MAX_USER_QUEUE_BYTES: usize = 12 * 1024 * 1024;

/// Consecutive transient `get_status` errors tolerated before dropping the driver handle
/// (Review #8 B7).
const MAX_TRANSIENT_STATUS_ERRORS: u32 = 5;

/// Typed capture transport error distinguishing permanent device loss from transient IOCTL
/// errors and ABI mismatches (Review #8 B7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// Device disconnected, handle closed, or driver uninstalled/restarted.
    Gone(String),
    /// Temporary / retryable IOCTL error (do not drop the exclusive handle immediately).
    Transient(String),
    /// Protocol ABI mismatch or truncated reply.
    Abi(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Gone(s) | Self::Transient(s) | Self::Abi(s) => f.write_str(s),
        }
    }
}

impl From<CaptureError> for String {
    fn from(e: CaptureError) -> Self {
        e.to_string()
    }
}

/// Reason why a [`StreamCaptureSource`] reached EOF (Review #8 B1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEndReason {
    Running,
    GenerationChanged(u32),
    Paused,
    Cancelled,
    Error,
}

/// Abstract transport over `\\.\ApExclusiveAudio` (implemented by Win32 IOCTL in production and
/// deterministic mocks in unit tests).
pub trait CaptureTransport: Send {
    fn get_status(&mut self) -> Result<ApDriverStatus, CaptureError>;
    fn set_config(&mut self, cfg: ApDriverConfig) -> Result<(), CaptureError>;
    fn reset_ring(&mut self) -> Result<(), CaptureError>;
    /// Read available PCM bytes into `io_buf` (`size_of::<ApPcmChunkHeader>() + payload`),
    /// returning `(header, payload_bytes)`.
    fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), CaptureError>;
}

/// Shared handle wrapper + one-chunk generation-boundary stash (Review #8 B2) + consumption/pause
/// coordination atomics (Review #8 B1/B5).
#[derive(Clone)]
pub struct SharedCaptureTransport {
    inner: Arc<Mutex<Option<Box<dyn CaptureTransport>>>>,
    stash: Arc<Mutex<Option<(ApPcmChunkHeader, Vec<u8>)>>>,
    /// Generation currently being actively consumed by the engine producer (`0` = not consuming).
    active_generation: Arc<AtomicU32>,
    /// Generation explicitly paused or preempted by the user (`0` = none), so monitor retries
    /// never fight the user (Review #8 B5).
    user_paused_generation: Arc<AtomicU32>,
    /// Last generation whose dispatch/open failed and should be retried after backoff (B5).
    failed_generation: Arc<AtomicU32>,
}

impl SharedCaptureTransport {
    pub fn new(transport: Option<Box<dyn CaptureTransport>>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(transport)),
            stash: Arc::new(Mutex::new(None)),
            active_generation: Arc::new(AtomicU32::new(0)),
            user_paused_generation: Arc::new(AtomicU32::new(0)),
            failed_generation: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn replace(&self, transport: Option<Box<dyn CaptureTransport>>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *g = transport;
        let mut st = self.stash.lock().unwrap_or_else(|e| e.into_inner());
        *st = None;
    }

    /// Save the first chunk of a newer generation read by the outgoing source so zero samples
    /// are lost at generation boundaries (Review #8 B2).
    pub fn stash_chunk(&self, hdr: ApPcmChunkHeader, payload: Vec<u8>) {
        if payload.is_empty() {
            return;
        }
        let mut st = self.stash.lock().unwrap_or_else(|e| e.into_inner());
        *st = Some((hdr, payload));
    }

    /// Take a stashed chunk matching `generation` (discarding any stale older-generation stash).
    pub fn take_stash_for(&self, generation: u32) -> Option<(ApPcmChunkHeader, Vec<u8>)> {
        let mut st = self.stash.lock().unwrap_or_else(|e| e.into_inner());
        match st.as_ref() {
            Some((hdr, _)) if hdr.stream_generation == generation => st.take(),
            Some((hdr, _)) if hdr.stream_generation < generation => {
                *st = None;
                None
            }
            _ => None,
        }
    }

    pub fn set_active_generation(&self, gen_id: u32) {
        self.active_generation.store(gen_id, Ordering::Release);
        if gen_id != 0 {
            self.failed_generation.store(0, Ordering::Release);
        }
    }

    pub fn active_generation(&self) -> u32 {
        self.active_generation.load(Ordering::Acquire)
    }

    pub fn is_actively_consuming(&self) -> bool {
        self.active_generation() != 0
    }

    pub fn mark_user_paused(&self, gen_id: u32) {
        self.user_paused_generation.store(gen_id, Ordering::Release);
    }

    pub fn clear_user_paused(&self) {
        self.user_paused_generation.store(0, Ordering::Release);
    }

    pub fn user_paused_generation(&self) -> u32 {
        self.user_paused_generation.load(Ordering::Acquire)
    }

    pub fn mark_dispatch_failed(&self, gen_id: u32) {
        if gen_id != 0 {
            self.failed_generation.store(gen_id, Ordering::Release);
        }
    }

    pub fn take_failed_generation(&self) -> u32 {
        self.failed_generation.swap(0, Ordering::AcqRel)
    }

    pub fn get_status(&self) -> Result<ApDriverStatus, CaptureError> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err(CaptureError::Gone("capture driver not connected".to_string()));
        };
        t.get_status()
    }

    pub fn set_config(&self, cfg: ApDriverConfig) -> Result<(), CaptureError> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err(CaptureError::Gone("capture driver not connected".to_string()));
        };
        t.set_config(cfg)
    }

    pub fn reset_ring(&self) -> Result<(), CaptureError> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err(CaptureError::Gone("capture driver not connected".to_string()));
        };
        t.reset_ring()
    }

    pub fn read_pcm(&self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), CaptureError> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err(CaptureError::Gone("capture driver not connected".to_string()));
        };
        t.read_pcm(io_buf)
    }
}

#[cfg(windows)]
fn classify_win32_error(op: &str, err: u32) -> CaptureError {
    // Win32 device-gone / invalid-handle error codes (Review #8 B7):
    // 2 = ERROR_FILE_NOT_FOUND, 5 = ERROR_ACCESS_DENIED, 6 = ERROR_INVALID_HANDLE,
    // 31 = ERROR_GEN_FAILURE, 1167 = ERROR_DEVICE_NOT_CONNECTED, 1168 = ERROR_NOT_FOUND
    let msg = format!("{op} failed (err={err})");
    match err {
        2 | 5 | 6 | 31 | 1167 | 1168 => CaptureError::Gone(msg),
        _ => CaptureError::Transient(msg),
    }
}

#[cfg(windows)]
pub struct Win32CaptureTransport {
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for Win32CaptureTransport {}

#[cfg(windows)]
impl Win32CaptureTransport {
    pub fn try_open() -> Result<Self, CaptureError> {
        use std::ptr;
        use windows_sys::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };

        let wide: Vec<u16> = DEVICE_WIN32_PATH
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            let err = unsafe { GetLastError() };
            return Err(classify_win32_error(
                &format!("CreateFileW({DEVICE_WIN32_PATH})"),
                err,
            ));
        }
        let mut transport = Self { handle };
        let status = transport.get_status()?;
        if status.abi_version != AP_STREAM_ABI_VERSION {
            return Err(CaptureError::Abi(format!(
                "capture driver ABI mismatch (driver={}, engine={AP_STREAM_ABI_VERSION})",
                status.abi_version
            )));
        }
        transport.set_config(ApDriverConfig {
            strict_reject_audiodg: 1,
            reset_counters: 0,
        })?;
        Ok(transport)
    }
}

#[cfg(windows)]
impl Drop for Win32CaptureTransport {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
impl CaptureTransport for Win32CaptureTransport {
    fn get_status(&mut self) -> Result<ApDriverStatus, CaptureError> {
        use std::ffi::c_void;
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::IO::DeviceIoControl;

        let mut status = ApDriverStatus::default();
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_AP_GET_STATUS,
                ptr::null(),
                0,
                (&mut status as *mut ApDriverStatus).cast::<c_void>(),
                size_of::<ApDriverStatus>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            return Err(classify_win32_error("IOCTL_AP_GET_STATUS", err));
        }
        // Require exact reply size (Review #8 B8).
        if returned as usize != size_of::<ApDriverStatus>() {
            return Err(CaptureError::Abi(format!(
                "IOCTL_AP_GET_STATUS reply size mismatch (got {returned}, expected {})",
                size_of::<ApDriverStatus>()
            )));
        }
        if status.abi_version != AP_STREAM_ABI_VERSION {
            return Err(CaptureError::Abi(format!(
                "ABI mismatch: driver={} engine={AP_STREAM_ABI_VERSION}",
                status.abi_version
            )));
        }
        Ok(status)
    }

    fn set_config(&mut self, cfg: ApDriverConfig) -> Result<(), CaptureError> {
        use std::ffi::c_void;
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::IO::DeviceIoControl;

        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_AP_SET_CONFIG,
                (&cfg as *const ApDriverConfig).cast::<c_void>(),
                size_of::<ApDriverConfig>() as u32,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            return Err(classify_win32_error("IOCTL_AP_SET_CONFIG", err));
        }
        Ok(())
    }

    fn reset_ring(&mut self) -> Result<(), CaptureError> {
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::IO::DeviceIoControl;

        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_AP_RESET_RING,
                ptr::null(),
                0,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            return Err(classify_win32_error("IOCTL_AP_RESET_RING", err));
        }
        Ok(())
    }

    fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), CaptureError> {
        use std::ffi::c_void;
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::IO::DeviceIoControl;

        let hdr_size = size_of::<ApPcmChunkHeader>();
        if io_buf.len() < hdr_size {
            return Err(CaptureError::Abi("read_pcm buffer smaller than header".to_string()));
        }
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_AP_READ_PCM,
                ptr::null(),
                0,
                io_buf.as_mut_ptr().cast::<c_void>(),
                io_buf.len() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            return Err(classify_win32_error("IOCTL_AP_READ_PCM", err));
        }
        if (returned as usize) < hdr_size {
            return Err(CaptureError::Abi(format!(
                "short IOCTL_AP_READ_PCM reply ({returned} bytes)"
            )));
        }
        let hdr = unsafe { core::ptr::read_unaligned(io_buf.as_ptr().cast::<ApPcmChunkHeader>()) };
        let payload = (hdr.pcm_bytes as usize).min((returned as usize) - hdr_size);
        Ok((hdr, payload))
    }
}

/// Validate that `ApStreamFormat` has coherent stereo integer-PCM parameters (Review #8 B9).
pub fn validate_stream_format(fmt: &ApStreamFormat) -> Result<(), String> {
    if fmt.is_float != 0 {
        return Err("rejected floating-point stream (integer PCM only)".to_string());
    }
    if fmt.channels != 2 || fmt.sample_rate < 44_100 || fmt.sample_rate > 384_000 {
        return Err(format!(
            "unsupported exclusive stream format: {} Hz, {} ch",
            fmt.sample_rate, fmt.channels
        ));
    }
    if !matches!(fmt.bits_per_sample, 16 | 24 | 32) {
        return Err(format!(
            "unsupported container bit depth: {}-bit",
            fmt.bits_per_sample
        ));
    }
    if fmt.valid_bits_per_sample > fmt.bits_per_sample {
        return Err(format!(
            "invalid valid_bits_per_sample {} > bits_per_sample {}",
            fmt.valid_bits_per_sample, fmt.bits_per_sample
        ));
    }
    let expected_align = (fmt.channels as u32) * (fmt.bits_per_sample as u32) / 8;
    if (fmt.block_align as u32) != expected_align {
        return Err(format!(
            "inconsistent block_align {} (expected {} for {} ch x {}-bit)",
            fmt.block_align, expected_align, fmt.channels, fmt.bits_per_sample
        ));
    }
    Ok(())
}

/// Verify that a driver status snapshot represents a strictly Windows-mixer-free exclusive stream
/// (Review #8 A7/B9).
pub fn verify_status_exclusive(st: &ApDriverStatus) -> Result<(), String> {
    if st.abi_version != AP_STREAM_ABI_VERSION {
        return Err(format!(
            "capture driver ABI mismatch (driver={}, engine={AP_STREAM_ABI_VERSION})",
            st.abi_version
        ));
    }
    validate_stream_format(&st.format)?;
    if is_audiodg_name(&st.alloc_proc)
        || is_audiodg_name(&st.run_proc)
        || is_audiodg_name(&st.packet_proc)
    {
        return Err(format!(
            "Windows mixer (audiodg.exe) detected in data path (alloc={}, run={}, packet={})",
            proc_name_str(&st.alloc_proc),
            proc_name_str(&st.run_proc),
            proc_name_str(&st.packet_proc),
        ));
    }
    if is_system_or_idle_name(&st.alloc_proc) || is_system_or_idle_name(&st.run_proc) {
        return Err(format!(
            "unverified kernel/system caller in data path (alloc={}, run={})",
            proc_name_str(&st.alloc_proc),
            proc_name_str(&st.run_proc),
        ));
    }
    if st.exclusive_verified == 0 {
        return Err("kernel driver did not certify stream as exclusive".to_string());
    }
    Ok(())
}

/// Convert raw little-endian stereo integer PCM bytes into pure IEEE-754 `f64` samples in `[-1.0, +1.0)`.
/// Uses the exact same full-scale normalization as `FlacBlockReader`: `sample / 2^(bits - 1)`.
/// Rejects partial frames (Review #8 B9).
pub fn decode_integer_pcm_to_f64(
    raw: &[u8],
    fmt: &ApStreamFormat,
    left: &mut Vec<f64>,
    right: &mut Vec<f64>,
) -> Result<usize, String> {
    validate_stream_format(fmt)?;
    let frame_bytes = (fmt.channels as usize) * (fmt.bits_per_sample as usize) / 8;
    if raw.len() % frame_bytes != 0 {
        return Err(format!(
            "partial PCM frame: {} bytes is not a multiple of {frame_bytes}",
            raw.len()
        ));
    }
    const INV_16: f64 = 1.0 / 32_768.0;
    const INV_24: f64 = 1.0 / 8_388_608.0;
    const INV_32: f64 = 1.0 / 2_147_483_648.0;

    let before = left.len();
    match fmt.bits_per_sample {
        16 => {
            for frame in raw.chunks_exact(4) {
                let l = i16::from_le_bytes([frame[0], frame[1]]) as f64 * INV_16;
                let r = i16::from_le_bytes([frame[2], frame[3]]) as f64 * INV_16;
                left.push(l);
                right.push(r);
            }
        }
        24 => {
            for frame in raw.chunks_exact(6) {
                let l_i32 = i32::from_le_bytes([0, frame[0], frame[1], frame[2]]) >> 8;
                let r_i32 = i32::from_le_bytes([0, frame[3], frame[4], frame[5]]) >> 8;
                left.push(l_i32 as f64 * INV_24);
                right.push(r_i32 as f64 * INV_24);
            }
        }
        32 => {
            for frame in raw.chunks_exact(8) {
                let l_i32 = i32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
                let r_i32 = i32::from_le_bytes([frame[4], frame[5], frame[6], frame[7]]);
                left.push(l_i32 as f64 * INV_32);
                right.push(r_i32 as f64 * INV_32);
            }
        }
        other => {
            return Err(format!("unsupported container bit depth: {other}-bit"));
        }
    }
    Ok(left.len() - before)
}

/// Shared queue between the decoupled kernel drainer thread and [`StreamCaptureSource`]
/// (Review #8 B4/B6).
struct DrainerQueue {
    chunks: VecDeque<(ApStreamFormat, Vec<u8>)>,
    queued_bytes: usize,
    end_reason: CaptureEndReason,
    error: Option<String>,
}

/// Live bounded-block [`AudioBlockSource`] reading untouched exclusive-mode integer PCM for one
/// stream generation from `\\.\ApExclusiveAudio`.
///
/// Spawns a decoupled background drainer thread on open so kernel ring draining starts immediately
/// even during heavy DSP/REW convolver setup (Review #8 B6), throttles kernel reads when engine
/// buffer fill exceeds [`CAPTURE_TARGET_ENGINE_FILL`] (Review #8 B4), never zeroes real audio on
/// `CHUNK_FLAG_SILENT_PACKET` (Review #8 B3), and stashes any new-generation chunk for the next
/// source (Review #8 B2).
pub struct StreamCaptureSource {
    generation: u32,
    spec: StreamSpec,
    cancel: Arc<AtomicBool>,
    drainer_stop: Arc<AtomicBool>,
    queue: Arc<(Mutex<DrainerQueue>, Condvar)>,
    drainer_thread: Option<thread::JoinHandle<()>>,
    pending_left: Vec<f64>,
    pending_right: Vec<f64>,
    pending_offset: usize,
    eof: bool,
    end_reason: CaptureEndReason,
}

impl StreamCaptureSource {
    pub fn open_generation(
        transport: SharedCaptureTransport,
        expected_generation: u32,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        Self::open_generation_with_hooks(transport, expected_generation, cancel, None, None)
    }

    pub fn open_generation_with_hooks(
        transport: SharedCaptureTransport,
        expected_generation: u32,
        cancel: Arc<AtomicBool>,
        engine_fill: Option<Arc<dyn Fn() -> Duration + Send + Sync>>,
        on_overrun: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    ) -> Result<Self, String> {
        let st = transport.get_status().map_err(|e| e.to_string())?;
        verify_status_exclusive(&st)?;
        if st.stream_generation != expected_generation {
            return Err(format!(
                "stream generation already advanced ({} -> {})",
                expected_generation, st.stream_generation
            ));
        }
        let bits = if st.format.valid_bits_per_sample > 0 {
            st.format.valid_bits_per_sample
        } else {
            st.format.bits_per_sample
        };
        let spec = StreamSpec {
            sample_rate_hz: st.format.sample_rate,
            channels: st.format.channels,
            bits_per_sample: bits,
        };
        let initial_overruns_lo = (st.ring_overruns & 0xFFFF_FFFF) as u32;

        let queue = Arc::new((
            Mutex::new(DrainerQueue {
                chunks: VecDeque::new(),
                queued_bytes: 0,
                end_reason: CaptureEndReason::Running,
                error: None,
            }),
            Condvar::new(),
        ));
        let drainer_stop = Arc::new(AtomicBool::new(false));

        let q_bg = Arc::clone(&queue);
        let stop_bg = Arc::clone(&drainer_stop);
        let cancel_bg = Arc::clone(&cancel);
        let handle = thread::Builder::new()
            .name(format!("ap-cap-drain-g{expected_generation}"))
            .spawn(move || {
                drainer_loop(
                    transport,
                    expected_generation,
                    st.format,
                    initial_overruns_lo,
                    stop_bg,
                    cancel_bg,
                    engine_fill,
                    on_overrun,
                    q_bg,
                );
            })
            .map_err(|e| format!("failed spawning capture drainer thread: {e}"))?;

        Ok(Self {
            generation: expected_generation,
            spec,
            cancel,
            drainer_stop,
            queue,
            drainer_thread: Some(handle),
            pending_left: Vec::with_capacity(32_768),
            pending_right: Vec::with_capacity(32_768),
            pending_offset: 0,
            eof: false,
            end_reason: CaptureEndReason::Running,
        })
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn end_reason(&self) -> CaptureEndReason {
        self.end_reason
    }

    fn take_pending(&mut self, max_frames: usize) -> Option<StereoF64Buffer> {
        let avail = self.pending_left.len().saturating_sub(self.pending_offset);
        if avail == 0 {
            return None;
        }
        let n = avail.min(max_frames);
        let end = self.pending_offset + n;
        let left = self.pending_left[self.pending_offset..end].to_vec();
        let right = self.pending_right[self.pending_offset..end].to_vec();
        self.pending_offset = end;
        if self.pending_offset >= self.pending_left.len() {
            self.pending_left.clear();
            self.pending_right.clear();
            self.pending_offset = 0;
        }
        Some(StereoF64Buffer {
            sample_rate_hz: self.spec.sample_rate_hz,
            left,
            right,
        })
    }
}

impl Drop for StreamCaptureSource {
    fn drop(&mut self) {
        self.drainer_stop.store(true, Ordering::SeqCst);
        self.queue.1.notify_all();
        if let Some(h) = self.drainer_thread.take() {
            let _ = h.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn drainer_loop(
    transport: SharedCaptureTransport,
    generation: u32,
    expected_fmt: ApStreamFormat,
    mut last_overruns_lo: u32,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    engine_fill: Option<Arc<dyn Fn() -> Duration + Send + Sync>>,
    on_overrun: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    queue: Arc<(Mutex<DrainerQueue>, Condvar)>,
) {
    let hdr_size = size_of::<ApPcmChunkHeader>();
    let mut io_buf = vec![0u8; hdr_size + IOCTL_PCM_PAYLOAD_BYTES];
    let mut paused_since: Option<Instant> = None;

    let finish = |reason: CaptureEndReason, err: Option<String>| {
        let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
        q.end_reason = reason;
        if err.is_some() {
            q.error = err;
        }
        queue.1.notify_all();
    };

    // 1. Drain any stashed first chunk for this generation (Review #8 B2) before issuing new IOCTLs.
    if let Some((hdr, bytes)) = transport.take_stash_for(generation) {
        if hdr.abi_version != AP_STREAM_ABI_VERSION {
            finish(
                CaptureEndReason::Error,
                Some(format!("capture driver ABI mismatch: {}", hdr.abi_version)),
            );
            return;
        }
        if hdr.stream_running != 0 && (hdr.exclusive_verified == 0 || hdr.format.is_float != 0) {
            finish(
                CaptureEndReason::Error,
                Some("Windows mixer or floating-point stream detected — exclusive capture aborted".to_string()),
            );
            return;
        }
        if !bytes.is_empty() {
            let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            q.queued_bytes += bytes.len();
            q.chunks.push_back((hdr.format, bytes));
            queue.1.notify_all();
        }
    }

    loop {
        if stop.load(Ordering::Acquire) || cancel.load(Ordering::Acquire) {
            finish(CaptureEndReason::Cancelled, None);
            return;
        }

        // Throttle kernel reads when engine output buffer is above target (Review #8 B4) OR
        // user-mode queue is full, so kernel ring fill reflects actual DAC consumption.
        let throttled = engine_fill
            .as_ref()
            .is_some_and(|f| f() > CAPTURE_TARGET_ENGINE_FILL);
        let user_full = {
            let q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            q.queued_bytes >= MAX_USER_QUEUE_BYTES
        };
        if throttled || user_full {
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        let (hdr, payload_len) = match transport.read_pcm(&mut io_buf) {
            Ok(v) => v,
            Err(err) => {
                finish(CaptureEndReason::Error, Some(err.to_string()));
                return;
            }
        };

        if hdr.abi_version != AP_STREAM_ABI_VERSION {
            finish(
                CaptureEndReason::Error,
                Some(format!("capture driver ABI mismatch: {}", hdr.abi_version)),
            );
            return;
        }

        // Report any kernel ring overrun deltas (Review #8 B6).
        // Guard against counter resets (delta >= 2^31) while still handling genuine u32 wraps.
        let ov_delta = hdr.ring_overruns_lo.wrapping_sub(last_overruns_lo);
        if ov_delta > 0 && ov_delta < (1u32 << 31) {
            last_overruns_lo = hdr.ring_overruns_lo;
            if let Some(ref cb) = on_overrun {
                cb(u64::from(ov_delta));
            }
        } else if hdr.ring_overruns_lo != last_overruns_lo {
            last_overruns_lo = hdr.ring_overruns_lo;
        }

        // Stream switched to a new track / sample rate! Stash any bytes belonging to the new
        // generation so zero samples are lost (Review #8 B2), then exit with GenerationChanged (B1).
        if hdr.stream_generation != generation {
            if payload_len > 0 {
                let bytes = io_buf[hdr_size..hdr_size + payload_len].to_vec();
                transport.stash_chunk(hdr, bytes);
            }
            finish(CaptureEndReason::GenerationChanged(hdr.stream_generation), None);
            return;
        }

        // Hard zero-mixer enforcement on every chunk.
        if hdr.stream_running != 0 && (hdr.exclusive_verified == 0 || hdr.format.is_float != 0) {
            finish(
                CaptureEndReason::Error,
                Some("Windows mixer or floating-point stream detected — exclusive capture aborted".to_string()),
            );
            return;
        }

        if payload_len > 0 {
            paused_since = None;
            // Review #8 B3: ALWAYS forward raw PCM bytes (never zero entire multi-packet read
            // when CHUNK_FLAG_SILENT_PACKET is set; the kernel already zero-filled silent packets).
            let bytes = io_buf[hdr_size..hdr_size + payload_len].to_vec();
            let fmt = if hdr.format.sample_rate != 0 {
                hdr.format
            } else {
                expected_fmt
            };
            let mut q = queue.0.lock().unwrap_or_else(|e| e.into_inner());
            q.queued_bytes += bytes.len();
            q.chunks.push_back((fmt, bytes));
            queue.1.notify_all();
            continue;
        }

        // No bytes available right now.
        if hdr.stream_running == 0 {
            let since = *paused_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= PAUSE_EOF_GRACE {
                finish(CaptureEndReason::Paused, None);
                return;
            }
        } else {
            paused_since = None;
        }

        thread::sleep(Duration::from_millis(4));
    }
}

impl AudioBlockSource for StreamCaptureSource {
    fn spec(&self) -> StreamSpec {
        self.spec
    }

    fn peak_meta(&self) -> TrackPeakMetadata {
        // Live stream has no tags -> unknown peak -> mandatory -3 dB upsampling safety + REW excess.
        TrackPeakMetadata::default()
    }

    fn is_streaming(&self) -> bool {
        true
    }

    fn total_frames(&self) -> Option<u64> {
        None
    }

    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
        if self.eof {
            return Ok(None);
        }
        if self.cancel.load(Ordering::Acquire) {
            self.eof = true;
            self.end_reason = CaptureEndReason::Cancelled;
            return Ok(None);
        }
        let target_frames = max_frames.max(1);

        loop {
            if self.cancel.load(Ordering::Acquire) {
                self.eof = true;
                self.end_reason = CaptureEndReason::Cancelled;
                return Ok(None);
            }

            let avail = self.pending_left.len().saturating_sub(self.pending_offset);
            if avail >= target_frames {
                return Ok(self.take_pending(target_frames));
            }

            let next_chunk = {
                let mut q = self.queue.0.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some((fmt, bytes)) = q.chunks.pop_front() {
                        q.queued_bytes = q.queued_bytes.saturating_sub(bytes.len());
                        break Ok(Some((fmt, bytes)));
                    }
                    if let Some(err) = q.error.take() {
                        self.end_reason = CaptureEndReason::Error;
                        self.eof = true;
                        break Err(err);
                    }
                    if q.end_reason != CaptureEndReason::Running {
                        self.end_reason = q.end_reason;
                        self.eof = true;
                        break Ok(None);
                    }
                    if self.cancel.load(Ordering::Acquire) {
                        self.end_reason = CaptureEndReason::Cancelled;
                        self.eof = true;
                        break Ok(None);
                    }
                    let (guard, _) = self
                        .queue
                        .1
                        .wait_timeout(q, Duration::from_millis(20))
                        .unwrap_or_else(|e| e.into_inner());
                    q = guard;
                }
            }?;

            match next_chunk {
                Some((fmt, bytes)) => {
                    decode_integer_pcm_to_f64(
                        &bytes,
                        &fmt,
                        &mut self.pending_left,
                        &mut self.pending_right,
                    )?;
                }
                None => {
                    // Drainer ended cleanly (GenerationChanged / Paused / Cancelled): flush any
                    // remaining decoded frames for the current generation before returning None!
                    return Ok(self.take_pending(target_frames));
                }
            }
        }
    }
}

/// Information emitted by [`StreamCaptureMonitor`] whenever a verified exclusive-mode stream
/// starts or switches native format/sample rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedExclusiveStream {
    pub generation: u32,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub valid_bits_per_sample: u16,
    pub caller_name: String,
    pub caller_pid: u32,
}

/// Background watcher thread that monitors `\\.\ApExclusiveAudio` at ~0% CPU and fires
/// `on_stream_started` whenever a verified exclusive-mode client (e.g. Tidal Desktop) starts
/// playback, while reporting any kernel-blocked `audiodg.exe` attempts.
pub struct StreamCaptureMonitor {
    transport: SharedCaptureTransport,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl StreamCaptureMonitor {
    /// Start monitoring the opt-in exclusive ACX virtual driver.
    /// Never fails if the driver is not installed — quietly retries every 2 s until installed.
    pub fn start<FStart, FBlocked>(on_stream_started: FStart, on_mixer_blocked: FBlocked) -> Self
    where
        FStart: Fn(DetectedExclusiveStream) + Send + 'static,
        FBlocked: Fn(u32, String, u32, &'static str) + Send + 'static,
    {
        #[cfg(windows)]
        let initial: Option<Box<dyn CaptureTransport>> = Win32CaptureTransport::try_open()
            .ok()
            .map(|t| Box::new(t) as Box<dyn CaptureTransport>);
        #[cfg(not(windows))]
        let initial: Option<Box<dyn CaptureTransport>> = None;

        Self::start_with_transport(
            SharedCaptureTransport::new(initial),
            true,
            on_stream_started,
            on_mixer_blocked,
        )
    }

    /// Start monitoring with an injected [`SharedCaptureTransport`] (used by tests and `start`).
    pub fn start_with_transport<FStart, FBlocked>(
        transport: SharedCaptureTransport,
        auto_reconnect_win32: bool,
        on_stream_started: FStart,
        on_mixer_blocked: FBlocked,
    ) -> Self
    where
        FStart: Fn(DetectedExclusiveStream) + Send + 'static,
        FBlocked: Fn(u32, String, u32, &'static str) + Send + 'static,
    {
        let transport_bg = transport.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("ap-stream-capture-mon".to_string())
            .spawn(move || {
                let mut last_dispatched_gen: u32 = 0;
                let mut last_rejected_count: u32 = 0;
                let mut consecutive_transient_errors: u32 = 0;
                let mut last_failed_retry = Instant::now() - Duration::from_secs(10);
                let mut last_reconnect_attempt = Instant::now() - Duration::from_secs(10);

                while !stop_bg.load(Ordering::Acquire) {
                    if !transport_bg.is_connected() {
                        if auto_reconnect_win32
                            && last_reconnect_attempt.elapsed() >= Duration::from_secs(2)
                        {
                            last_reconnect_attempt = Instant::now();
                            #[cfg(windows)]
                            if let Ok(t) = Win32CaptureTransport::try_open() {
                                transport_bg.replace(Some(Box::new(t)));
                                last_dispatched_gen = 0;
                                last_rejected_count = 0;
                                consecutive_transient_errors = 0;
                            }
                        }
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }

                    match transport_bg.get_status() {
                        Ok(st) => {
                            consecutive_transient_errors = 0;

                            if st.rejected_shared_opens > last_rejected_count {
                                let delta = st.rejected_shared_opens - last_rejected_count;
                                last_rejected_count = st.rejected_shared_opens;
                                on_mixer_blocked(
                                    delta,
                                    proc_name_str(&st.last_rejected_proc).to_string(),
                                    st.last_rejected_pid,
                                    stage_name(st.last_rejected_stage),
                                );
                            }

                            // If a previous dispatch for this generation failed, allow retry after
                            // 1 s backoff (Review #8 B5), unless the user explicitly paused it.
                            let failed_gen = transport_bg.take_failed_generation();
                            if failed_gen != 0 && failed_gen == last_dispatched_gen {
                                last_failed_retry = Instant::now();
                            }

                            if st.stream_running != 0 && verify_status_exclusive(&st).is_ok() {
                                let gen_id = st.stream_generation;
                                // If the engine producer is already actively consuming live capture,
                                // it follows generations internally with zero DAC flush (Review #8 B1).
                                if transport_bg.is_actively_consuming() {
                                    last_dispatched_gen = gen_id;
                                } else {
                                    let is_new_gen = gen_id != last_dispatched_gen;
                                    let user_paused_this =
                                        transport_bg.user_paused_generation() == gen_id;
                                    let retry_due = !is_new_gen
                                        && !user_paused_this
                                        && transport_bg.active_generation() != gen_id
                                        && failed_gen == gen_id
                                        && last_failed_retry.elapsed() >= Duration::from_secs(1);

                                    if (is_new_gen && !user_paused_this) || retry_due {
                                        last_dispatched_gen = gen_id;
                                        let caller = if st.packet_pid != 0 {
                                            (
                                                proc_name_str(&st.packet_proc).to_string(),
                                                st.packet_pid,
                                            )
                                        } else if st.run_pid != 0 {
                                            (proc_name_str(&st.run_proc).to_string(), st.run_pid)
                                        } else {
                                            (
                                                proc_name_str(&st.alloc_proc).to_string(),
                                                st.alloc_pid,
                                            )
                                        };
                                        let valid_bits = if st.format.valid_bits_per_sample > 0 {
                                            st.format.valid_bits_per_sample
                                        } else {
                                            st.format.bits_per_sample
                                        };
                                        on_stream_started(DetectedExclusiveStream {
                                            generation: gen_id,
                                            sample_rate_hz: st.format.sample_rate,
                                            channels: st.format.channels,
                                            bits_per_sample: st.format.bits_per_sample,
                                            valid_bits_per_sample: valid_bits,
                                            caller_name: caller.0,
                                            caller_pid: caller.1,
                                        });
                                    }
                                }
                            }
                        }
                        Err(CaptureError::Gone(_)) | Err(CaptureError::Abi(_)) => {
                            transport_bg.replace(None);
                            consecutive_transient_errors = 0;
                            last_reconnect_attempt = Instant::now();
                        }
                        Err(CaptureError::Transient(_)) => {
                            // Review #8 B7: tolerate transient IOCTL hiccups without dropping handle.
                            consecutive_transient_errors += 1;
                            if consecutive_transient_errors >= MAX_TRANSIENT_STATUS_ERRORS {
                                transport_bg.replace(None);
                                consecutive_transient_errors = 0;
                                last_reconnect_attempt = Instant::now();
                            }
                        }
                    }

                    thread::sleep(Duration::from_millis(40));
                }
            })
            .ok();

        Self {
            transport,
            stop,
            thread: handle,
        }
    }

    pub fn transport(&self) -> SharedCaptureTransport {
        self.transport.clone()
    }

    pub fn is_driver_connected(&self) -> bool {
        self.transport.is_connected()
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StreamCaptureMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ap_stream_protocol::{CHUNK_FLAG_SILENT_PACKET, CHUNK_FLAG_STREAM_RUNNING};
    use std::sync::atomic::AtomicUsize;

    struct MockTransport {
        status: ApDriverStatus,
        chunks: VecDeque<(ApPcmChunkHeader, Vec<u8>)>,
        status_errors: VecDeque<CaptureError>,
        read_calls: Arc<AtomicUsize>,
    }

    impl MockTransport {
        fn new(status: ApDriverStatus, chunks: VecDeque<(ApPcmChunkHeader, Vec<u8>)>) -> Self {
            Self {
                status,
                chunks,
                status_errors: VecDeque::new(),
                read_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl CaptureTransport for MockTransport {
        fn get_status(&mut self) -> Result<ApDriverStatus, CaptureError> {
            if let Some(err) = self.status_errors.pop_front() {
                return Err(err);
            }
            Ok(self.status)
        }
        fn set_config(&mut self, _cfg: ApDriverConfig) -> Result<(), CaptureError> {
            Ok(())
        }
        fn reset_ring(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
        fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), CaptureError> {
            self.read_calls.fetch_add(1, Ordering::SeqCst);
            let hdr_size = size_of::<ApPcmChunkHeader>();
            if let Some((mut hdr, data)) = self.chunks.pop_front() {
                let n = data.len().min(io_buf.len() - hdr_size);
                hdr.pcm_bytes = n as u32;
                unsafe {
                    core::ptr::write_unaligned(io_buf.as_mut_ptr().cast::<ApPcmChunkHeader>(), hdr);
                }
                io_buf[hdr_size..hdr_size + n].copy_from_slice(&data[..n]);
                Ok((hdr, n))
            } else {
                let hdr = ApPcmChunkHeader {
                    abi_version: AP_STREAM_ABI_VERSION,
                    stream_generation: self.status.stream_generation,
                    stream_running: 0,
                    exclusive_verified: self.status.exclusive_verified,
                    format: self.status.format,
                    pcm_bytes: 0,
                    ring_fill_after: 0,
                    flags: 0,
                    ring_overruns_lo: (self.status.ring_overruns & 0xFFFF_FFFF) as u32,
                };
                unsafe {
                    core::ptr::write_unaligned(io_buf.as_mut_ptr().cast::<ApPcmChunkHeader>(), hdr);
                }
                Ok((hdr, 0))
            }
        }
    }

    fn proc16(s: &str) -> [u8; 16] {
        let mut b = [0u8; 16];
        let n = s.len().min(15);
        b[..n].copy_from_slice(&s.as_bytes()[..n]);
        b
    }

    fn exclusive_status(rate: u32, bits: u16, valid: u16, gen_id: u32) -> ApDriverStatus {
        let mut st = ApDriverStatus::default();
        st.stream_generation = gen_id;
        st.stream_running = 1;
        st.exclusive_verified = 1;
        st.format = ApStreamFormat {
            sample_rate: rate,
            channels: 2,
            bits_per_sample: bits,
            valid_bits_per_sample: valid,
            block_align: 2 * (bits / 8),
            is_float: 0,
            packet_size_bytes: 8192,
            packet_count: 2,
            reserved: 0,
        };
        st.create_proc = proc16("audiodg.exe");
        st.alloc_proc = proc16("TIDALPlayer.ex");
        st.run_proc = proc16("TIDALPlayer.ex");
        st.packet_proc = proc16("TIDALPlayer.ex");
        st.alloc_pid = 4242;
        st.run_pid = 4242;
        st.packet_pid = 4242;
        st
    }

    #[test]
    fn verify_status_accepts_tidal_exclusive_and_rejects_audiodg_system_and_bad_format_a7_b9() {
        let ok_st = exclusive_status(96_000, 32, 24, 1);
        assert!(verify_status_exclusive(&ok_st).is_ok());

        let mut mixer_alloc = ok_st;
        mixer_alloc.alloc_proc = proc16("audiodg.exe");
        assert!(verify_status_exclusive(&mixer_alloc).is_err());

        let mut mixer_run = ok_st;
        mixer_run.run_proc = proc16("AUDIODG.EXE");
        assert!(verify_status_exclusive(&mixer_run).is_err());

        let mut sys_run = ok_st;
        sys_run.run_proc = proc16("System");
        assert!(verify_status_exclusive(&sys_run).is_err());

        let mut float_st = ok_st;
        float_st.format.is_float = 1;
        assert!(verify_status_exclusive(&float_st).is_err());

        // B9: inconsistent block_align and invalid bits rejected
        let mut bad_align = ok_st;
        bad_align.format.bits_per_sample = 24;
        bad_align.format.block_align = 8;
        assert!(verify_status_exclusive(&bad_align).is_err());

        let mut bad_valid = ok_st;
        bad_valid.format.bits_per_sample = 16;
        bad_valid.format.valid_bits_per_sample = 24;
        bad_valid.format.block_align = 4;
        assert!(verify_status_exclusive(&bad_valid).is_err());
    }

    #[test]
    fn decode_integer_pcm_is_exact_and_rejects_partial_frames_b9() {
        let fmt16 = ApStreamFormat {
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
            valid_bits_per_sample: 16,
            block_align: 4,
            ..Default::default()
        };
        let mut raw16 = Vec::new();
        raw16.extend_from_slice(&16_384_i16.to_le_bytes());
        raw16.extend_from_slice(&(-32_768_i16).to_le_bytes());
        let (mut l, mut r) = (Vec::new(), Vec::new());
        assert_eq!(decode_integer_pcm_to_f64(&raw16, &fmt16, &mut l, &mut r).unwrap(), 1);
        assert_eq!(l[0], 0.5);
        assert_eq!(r[0], -1.0);

        // Partial frame must error (B9)
        assert!(decode_integer_pcm_to_f64(&raw16[..3], &fmt16, &mut l, &mut r).is_err());

        let fmt24 = ApStreamFormat {
            sample_rate: 96_000,
            channels: 2,
            bits_per_sample: 24,
            valid_bits_per_sample: 24,
            block_align: 6,
            ..Default::default()
        };
        let l24 = 4_194_304_i32.to_le_bytes();
        let r24 = (-8_388_608_i32).to_le_bytes();
        let raw24 = [l24[0], l24[1], l24[2], r24[0], r24[1], r24[2]];
        l.clear();
        r.clear();
        assert_eq!(decode_integer_pcm_to_f64(&raw24, &fmt24, &mut l, &mut r).unwrap(), 1);
        assert_eq!(l[0], 0.5);
        assert_eq!(r[0], -1.0);

        let fmt32 = ApStreamFormat {
            sample_rate: 192_000,
            channels: 2,
            bits_per_sample: 32,
            valid_bits_per_sample: 24,
            block_align: 8,
            ..Default::default()
        };
        let mut raw32 = Vec::new();
        raw32.extend_from_slice(&(1_i32 << 30).to_le_bytes());
        raw32.extend_from_slice(&i32::MIN.to_le_bytes());
        l.clear();
        r.clear();
        assert_eq!(decode_integer_pcm_to_f64(&raw32, &fmt32, &mut l, &mut r).unwrap(), 1);
        assert_eq!(l[0], 0.5);
        assert_eq!(r[0], -1.0);
    }

    #[test]
    fn silent_flag_does_not_zero_real_samples_b3() {
        let st = exclusive_status(44_100, 16, 16, 1);
        let mut pcm = Vec::new();
        for _ in 0..4 {
            pcm.extend_from_slice(&16_384_i16.to_le_bytes());
            pcm.extend_from_slice(&(-16_384_i16).to_le_bytes());
        }
        let hdr = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 1,
            stream_running: 1,
            exclusive_verified: 1,
            format: st.format,
            pcm_bytes: pcm.len() as u32,
            ring_fill_after: 0,
            flags: CHUNK_FLAG_STREAM_RUNNING | CHUNK_FLAG_SILENT_PACKET,
            ring_overruns_lo: 0,
        };
        let mock = MockTransport::new(st, VecDeque::from([(hdr, pcm)]));
        let shared = SharedCaptureTransport::new(Some(Box::new(mock)));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut src = StreamCaptureSource::open_generation(shared, 1, cancel).unwrap();

        let b = src.read_next_block(16).unwrap().expect("must decode real samples");
        assert_eq!(b.left.len(), 4);
        assert_eq!(b.left[0], 0.5);
        assert_eq!(b.right[0], -0.5);
    }

    #[test]
    fn generation_change_payload_is_stashed_and_handed_to_next_source_b2() {
        let st7 = exclusive_status(44_100, 16, 16, 7);
        let st8 = exclusive_status(96_000, 16, 16, 8);
        let mut pcm7 = Vec::new();
        for _ in 0..4 {
            pcm7.extend_from_slice(&16_384_i16.to_le_bytes());
            pcm7.extend_from_slice(&(-16_384_i16).to_le_bytes());
        }
        let mut pcm8 = Vec::new();
        for _ in 0..6 {
            pcm8.extend_from_slice(&8_192_i16.to_le_bytes());
            pcm8.extend_from_slice(&(-8_192_i16).to_le_bytes());
        }
        let hdr7 = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 7,
            stream_running: 1,
            exclusive_verified: 1,
            format: st7.format,
            pcm_bytes: pcm7.len() as u32,
            ring_fill_after: 0,
            flags: CHUNK_FLAG_STREAM_RUNNING,
            ring_overruns_lo: 0,
        };
        let hdr8 = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 8,
            stream_running: 1,
            exclusive_verified: 1,
            format: st8.format,
            pcm_bytes: pcm8.len() as u32,
            ring_fill_after: 0,
            flags: CHUNK_FLAG_STREAM_RUNNING,
            ring_overruns_lo: 0,
        };
        struct DualGenMock {
            st7: ApDriverStatus,
            st8: ApDriverStatus,
            cur_gen: Arc<AtomicU32>,
            chunks: VecDeque<(ApPcmChunkHeader, Vec<u8>)>,
        }
        impl CaptureTransport for DualGenMock {
            fn get_status(&mut self) -> Result<ApDriverStatus, CaptureError> {
                if self.cur_gen.load(Ordering::Acquire) == 8 {
                    Ok(self.st8)
                } else {
                    Ok(self.st7)
                }
            }
            fn set_config(&mut self, _: ApDriverConfig) -> Result<(), CaptureError> { Ok(()) }
            fn reset_ring(&mut self) -> Result<(), CaptureError> { Ok(()) }
            fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), CaptureError> {
                let hdr_size = size_of::<ApPcmChunkHeader>();
                if let Some((mut hdr, data)) = self.chunks.pop_front() {
                    if hdr.stream_generation == 8 {
                        self.cur_gen.store(8, Ordering::Release);
                    }
                    let n = data.len();
                    hdr.pcm_bytes = n as u32;
                    unsafe { core::ptr::write_unaligned(io_buf.as_mut_ptr().cast(), hdr); }
                    io_buf[hdr_size..hdr_size + n].copy_from_slice(&data);
                    Ok((hdr, n))
                } else {
                    let hdr = ApPcmChunkHeader {
                        abi_version: AP_STREAM_ABI_VERSION,
                        stream_generation: self.cur_gen.load(Ordering::Acquire),
                        stream_running: 0,
                        exclusive_verified: 1,
                        format: self.st8.format,
                        pcm_bytes: 0,
                        ring_fill_after: 0,
                        flags: 0,
                        ring_overruns_lo: 0,
                    };
                    unsafe { core::ptr::write_unaligned(io_buf.as_mut_ptr().cast(), hdr); }
                    Ok((hdr, 0))
                }
            }
        }

        let mock = DualGenMock {
            st7,
            st8,
            cur_gen: Arc::new(AtomicU32::new(7)),
            chunks: VecDeque::from([(hdr7, pcm7), (hdr8, pcm8)]),
        };
        let shared = SharedCaptureTransport::new(Some(Box::new(mock)));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut src7 = StreamCaptureSource::open_generation(shared.clone(), 7, cancel.clone()).unwrap();

        let b1 = src7.read_next_block(16).unwrap().expect("expected 4 frames from gen 7");
        assert_eq!(b1.left.len(), 4);
        assert_eq!(b1.left[0], 0.5);
        assert!(src7.read_next_block(16).unwrap().is_none());
        assert_eq!(src7.end_reason(), CaptureEndReason::GenerationChanged(8));
        drop(src7);

        // Next source for gen 8 receives the stashed 6 frames intact!
        let mut src8 = StreamCaptureSource::open_generation(shared, 8, cancel).unwrap();
        assert_eq!(src8.spec().sample_rate_hz, 96_000);
        let b2 = src8.read_next_block(16).unwrap().expect("expected stashed 6 frames from gen 8");
        assert_eq!(b2.left.len(), 6);
        assert_eq!(b2.left[0], 0.25);
        assert_eq!(b2.right[0], -0.25);
    }

    #[test]
    fn source_throttles_reads_above_target_and_reports_overruns_b4_b6() {
        let st = exclusive_status(44_100, 16, 16, 3);
        let pcm = vec![0u8; 4 * 4];
        let hdr = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 3,
            stream_running: 1,
            exclusive_verified: 1,
            format: st.format,
            pcm_bytes: pcm.len() as u32,
            ring_fill_after: 0,
            flags: CHUNK_FLAG_STREAM_RUNNING,
            ring_overruns_lo: 5,
        };
        let mock = MockTransport::new(st, VecDeque::from([(hdr, pcm)]));
        let read_calls = Arc::clone(&mock.read_calls);
        let shared = SharedCaptureTransport::new(Some(Box::new(mock)));
        let cancel = Arc::new(AtomicBool::new(false));
        let throttle = Arc::new(AtomicBool::new(true));
        let throttle_probe = Arc::clone(&throttle);
        let overruns_seen = Arc::new(AtomicUsize::new(0));
        let ov_cb = Arc::clone(&overruns_seen);

        let mut src = StreamCaptureSource::open_generation_with_hooks(
            shared,
            3,
            cancel,
            Some(Arc::new(move || {
                if throttle_probe.load(Ordering::Acquire) {
                    Duration::from_secs(2)
                } else {
                    Duration::from_millis(200)
                }
            })),
            Some(Arc::new(move |delta| {
                ov_cb.fetch_add(delta as usize, Ordering::SeqCst);
            })),
        )
        .unwrap();

        // While engine fill > CAPTURE_TARGET_ENGINE_FILL, drainer must not read kernel ring!
        thread::sleep(Duration::from_millis(40));
        assert_eq!(read_calls.load(Ordering::SeqCst), 0);

        // Release throttle -> drainer reads chunk and reports overrun delta = 5!
        throttle.store(false, Ordering::Release);
        let blk = src.read_next_block(4).unwrap().expect("expected chunk");
        assert_eq!(blk.left.len(), 4);
        assert_eq!(overruns_seen.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn monitor_keeps_handle_on_single_transient_error_b7() {
        let st = exclusive_status(44_100, 16, 16, 11);
        let mut mock = MockTransport::new(st, VecDeque::new());
        mock.status_errors
            .push_back(CaptureError::Transient("temporary glitch".to_string()));
        let shared = SharedCaptureTransport::new(Some(Box::new(mock)));
        let started = Arc::new(AtomicUsize::new(0));
        let started_cb = Arc::clone(&started);

        let mon = StreamCaptureMonitor::start_with_transport(
            shared.clone(),
            false,
            move |_| {
                started_cb.fetch_add(1, Ordering::SeqCst);
            },
            |_, _, _, _| {},
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && started.load(Ordering::SeqCst) == 0 {
            thread::sleep(Duration::from_millis(15));
        }
        assert!(shared.is_connected(), "single transient error must NOT drop handle");
        assert_eq!(started.load(Ordering::SeqCst), 1);
        mon.stop();
    }
}
