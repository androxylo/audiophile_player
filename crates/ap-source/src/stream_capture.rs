//! Live exclusive-mode kernel stream capture source (`\\.\ApExclusiveAudio`).
//!
//! Connects to Audiophile Player's opt-in pure-Rust KMDF + ACX 1.1 virtual render driver
//! (`drivers/ap-exclusive-acx`), enforces the hard zero-mixer invariant (`audiodg.exe` blocked
//! in kernel at `Run`/`SetRenderPacket` and double-checked in user mode), and feeds exact
//! pure-`f64` stereo blocks into [`AudioBlockSource`] with `is_streaming() == true`
//! (`-3 dB` unknown-peak headroom, dynamic rate following across `44.1..=384 kHz`).

use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;
use ap_stream_protocol::{
    AP_STREAM_ABI_VERSION, ApDriverConfig, ApDriverStatus, ApPcmChunkHeader, ApStreamFormat,
    CHUNK_FLAG_SILENT_PACKET, DEVICE_WIN32_PATH, IOCTL_AP_GET_STATUS, IOCTL_AP_READ_PCM,
    IOCTL_AP_RESET_RING, IOCTL_AP_SET_CONFIG, is_audiodg_name, proc_name_str, stage_name,
};

use crate::flac::AudioBlockSource;

/// Maximum raw PCM payload requested per `IOCTL_AP_READ_PCM` call (`128 KiB`).
const IOCTL_PCM_PAYLOAD_BYTES: usize = 131_072;

/// Idle time after `stream_running == 0` and empty ring before returning clean EOF (`Ok(None)`)
/// so the engine drains the REW tail and releases the WDM-KS DAC pin promptly on Tidal Pause/Stop.
const PAUSE_EOF_GRACE: Duration = Duration::from_millis(250);

/// Abstract transport over `\\.\ApExclusiveAudio` (implemented by Win32 IOCTL in production and
/// deterministic mocks in unit tests).
pub trait CaptureTransport: Send {
    fn get_status(&mut self) -> Result<ApDriverStatus, String>;
    fn set_config(&mut self, cfg: ApDriverConfig) -> Result<(), String>;
    fn reset_ring(&mut self) -> Result<(), String>;
    /// Read available PCM bytes into `io_buf` (`size_of::<ApPcmChunkHeader>() + payload`),
    /// returning `(header, payload_bytes)`.
    fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), String>;
}

/// Shared handle wrapper so the background [`StreamCaptureMonitor`] and active
/// [`StreamCaptureSource`] share the single exclusive `\\.\ApExclusiveAudio` kernel handle.
#[derive(Clone)]
pub struct SharedCaptureTransport {
    inner: Arc<Mutex<Option<Box<dyn CaptureTransport>>>>,
}

impl SharedCaptureTransport {
    pub fn new(transport: Option<Box<dyn CaptureTransport>>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(transport)),
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
    }

    pub fn get_status(&self) -> Result<ApDriverStatus, String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err("capture driver not connected".to_string());
        };
        t.get_status()
    }

    pub fn set_config(&self, cfg: ApDriverConfig) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err("capture driver not connected".to_string());
        };
        t.set_config(cfg)
    }

    pub fn reset_ring(&self) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err("capture driver not connected".to_string());
        };
        t.reset_ring()
    }

    pub fn read_pcm(&self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), String> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = g.as_mut() else {
            return Err("capture driver not connected".to_string());
        };
        t.read_pcm(io_buf)
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
    pub fn try_open() -> Result<Self, String> {
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
            return Err(format!("CreateFileW({DEVICE_WIN32_PATH}) error {err}"));
        }
        let mut transport = Self { handle };
        // Verify ABI and immediately enforce strict zero-mixer kernel rejection.
        let status = transport.get_status()?;
        if status.abi_version != AP_STREAM_ABI_VERSION {
            return Err(format!(
                "capture driver ABI mismatch (driver={}, engine={AP_STREAM_ABI_VERSION})",
                status.abi_version
            ));
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
    fn get_status(&mut self) -> Result<ApDriverStatus, String> {
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
            return Err(format!("IOCTL_AP_GET_STATUS failed (err={err})"));
        }
        if status.abi_version != AP_STREAM_ABI_VERSION {
            return Err(format!(
                "ABI mismatch: driver={} engine={AP_STREAM_ABI_VERSION}",
                status.abi_version
            ));
        }
        Ok(status)
    }

    fn set_config(&mut self, cfg: ApDriverConfig) -> Result<(), String> {
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
            return Err(format!("IOCTL_AP_SET_CONFIG failed (err={err})"));
        }
        Ok(())
    }

    fn reset_ring(&mut self) -> Result<(), String> {
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
            return Err(format!("IOCTL_AP_RESET_RING failed (err={err})"));
        }
        Ok(())
    }

    fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), String> {
        use std::ffi::c_void;
        use std::ptr;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::IO::DeviceIoControl;

        let hdr_size = size_of::<ApPcmChunkHeader>();
        if io_buf.len() < hdr_size {
            return Err("read_pcm buffer smaller than header".to_string());
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
            return Err(format!("IOCTL_AP_READ_PCM failed (err={err})"));
        }
        if (returned as usize) < hdr_size {
            return Err(format!("short IOCTL_AP_READ_PCM reply ({returned} bytes)"));
        }
        let hdr = unsafe { core::ptr::read_unaligned(io_buf.as_ptr().cast::<ApPcmChunkHeader>()) };
        let payload = (hdr.pcm_bytes as usize).min((returned as usize) - hdr_size);
        Ok((hdr, payload))
    }
}

/// Verify that a driver status snapshot represents a strictly Windows-mixer-free exclusive stream.
pub fn verify_status_exclusive(st: &ApDriverStatus) -> Result<(), String> {
    if st.abi_version != AP_STREAM_ABI_VERSION {
        return Err(format!(
            "capture driver ABI mismatch (driver={}, engine={AP_STREAM_ABI_VERSION})",
            st.abi_version
        ));
    }
    if st.format.is_float != 0 {
        return Err("rejected floating-point stream (integer PCM only)".to_string());
    }
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
    if st.exclusive_verified == 0 {
        return Err("kernel driver did not certify stream as exclusive".to_string());
    }
    if st.format.channels != 2 || st.format.sample_rate < 44_100 || st.format.sample_rate > 384_000
    {
        return Err(format!(
            "unsupported exclusive stream format: {} Hz, {} ch",
            st.format.sample_rate, st.format.channels
        ));
    }
    Ok(())
}

/// Convert raw little-endian stereo integer PCM bytes into pure IEEE-754 `f64` samples in `[-1.0, +1.0)`.
/// Uses the exact same full-scale normalization as `FlacBlockReader`: `sample / 2^(bits - 1)`.
pub fn decode_integer_pcm_to_f64(
    raw: &[u8],
    fmt: &ApStreamFormat,
    left: &mut Vec<f64>,
    right: &mut Vec<f64>,
) -> Result<usize, String> {
    if fmt.channels != 2 || fmt.is_float != 0 {
        return Err(format!(
            "unsupported PCM layout: channels={} is_float={}",
            fmt.channels, fmt.is_float
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
                // Sign-extend 24-bit signed little-endian integer into i32 via arithmetic shift.
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

/// Live bounded-block [`AudioBlockSource`] reading untouched exclusive-mode integer PCM for one
/// stream generation from `\\.\ApExclusiveAudio`.
pub struct StreamCaptureSource {
    transport: SharedCaptureTransport,
    generation: u32,
    spec: StreamSpec,
    format: ApStreamFormat,
    cancel: Arc<AtomicBool>,
    io_buf: Vec<u8>,
    pending_left: Vec<f64>,
    pending_right: Vec<f64>,
    pending_offset: usize,
    paused_since: Option<Instant>,
    eof: bool,
}

impl StreamCaptureSource {
    pub fn open_generation(
        transport: SharedCaptureTransport,
        expected_generation: u32,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let st = transport.get_status()?;
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
        Ok(Self {
            transport,
            generation: expected_generation,
            spec,
            format: st.format,
            cancel,
            io_buf: vec![0u8; size_of::<ApPcmChunkHeader>() + IOCTL_PCM_PAYLOAD_BYTES],
            pending_left: Vec::with_capacity(32_768),
            pending_right: Vec::with_capacity(32_768),
            pending_offset: 0,
            paused_since: None,
            eof: false,
        })
    }

    pub fn generation(&self) -> u32 {
        self.generation
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
        if self.eof || self.cancel.load(Ordering::Acquire) {
            return Ok(None);
        }
        let target_frames = max_frames.max(1);
        let hdr_size = size_of::<ApPcmChunkHeader>();

        loop {
            if self.cancel.load(Ordering::Acquire) {
                self.eof = true;
                return Ok(None);
            }

            let avail = self.pending_left.len().saturating_sub(self.pending_offset);
            if avail >= target_frames {
                return Ok(self.take_pending(target_frames));
            }

            let (hdr, payload_len) = self.transport.read_pcm(&mut self.io_buf)?;

            if hdr.abi_version != AP_STREAM_ABI_VERSION {
                return Err(format!("capture driver ABI mismatch: {}", hdr.abi_version));
            }

            // Stream switched to a new track / sample rate! Flush any pending frames for the old
            // generation first, then return clean EOF so the engine immediately starts the new one.
            if hdr.stream_generation != self.generation {
                self.eof = true;
                return Ok(self.take_pending(target_frames));
            }

            // Hard zero-mixer enforcement on every chunk.
            if hdr.stream_running != 0 && (hdr.exclusive_verified == 0 || hdr.format.is_float != 0)
            {
                self.eof = true;
                return Err(
                    "Windows mixer or floating-point stream detected — exclusive capture aborted"
                        .to_string(),
                );
            }

            if payload_len > 0 {
                self.paused_since = None;
                let payload = &self.io_buf[hdr_size..hdr_size + payload_len];
                if (hdr.flags & CHUNK_FLAG_SILENT_PACKET) != 0 {
                    let frames = payload_len / (self.format.block_align.max(1) as usize);
                    self.pending_left.resize(self.pending_left.len() + frames, 0.0);
                    self.pending_right.resize(self.pending_right.len() + frames, 0.0);
                } else {
                    decode_integer_pcm_to_f64(
                        payload,
                        &self.format,
                        &mut self.pending_left,
                        &mut self.pending_right,
                    )?;
                }
                continue;
            }

            // No bytes available right now.
            if hdr.stream_running == 0 {
                // If we already buffered partial frames before pause, flush them immediately.
                if let Some(buf) = self.take_pending(target_frames) {
                    return Ok(Some(buf));
                }
                let since = *self.paused_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= PAUSE_EOF_GRACE {
                    self.eof = true;
                    return Ok(None);
                }
            } else {
                self.paused_since = None;
            }

            thread::sleep(Duration::from_millis(4));
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
/// playback or switches sample rate, while reporting any kernel-blocked `audiodg.exe` attempts.
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
        let initial: Option<Box<dyn CaptureTransport>> =
            Win32CaptureTransport::try_open().ok().map(|t| Box::new(t) as Box<dyn CaptureTransport>);
        #[cfg(not(windows))]
        let initial: Option<Box<dyn CaptureTransport>> = None;

        let transport = SharedCaptureTransport::new(initial);
        let transport_bg = transport.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);

        let handle = thread::Builder::new()
            .name("ap-stream-capture-mon".to_string())
            .spawn(move || {
                let mut last_dispatched_gen: u32 = 0;
                let mut last_rejected_count: u32 = 0;
                let mut last_reconnect_attempt = Instant::now() - Duration::from_secs(10);

                while !stop_bg.load(Ordering::Acquire) {
                    if !transport_bg.is_connected() {
                        if last_reconnect_attempt.elapsed() >= Duration::from_secs(2) {
                            last_reconnect_attempt = Instant::now();
                            #[cfg(windows)]
                            if let Ok(t) = Win32CaptureTransport::try_open() {
                                transport_bg.replace(Some(Box::new(t)));
                                last_dispatched_gen = 0;
                                last_rejected_count = 0;
                            }
                        }
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }

                    match transport_bg.get_status() {
                        Ok(st) => {
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

                            if st.stream_running != 0
                                && st.stream_generation != last_dispatched_gen
                                && verify_status_exclusive(&st).is_ok()
                            {
                                last_dispatched_gen = st.stream_generation;
                                let caller = if st.packet_pid != 0 {
                                    (proc_name_str(&st.packet_proc).to_string(), st.packet_pid)
                                } else if st.run_pid != 0 {
                                    (proc_name_str(&st.run_proc).to_string(), st.run_pid)
                                } else {
                                    (proc_name_str(&st.alloc_proc).to_string(), st.alloc_pid)
                                };
                                let valid_bits = if st.format.valid_bits_per_sample > 0 {
                                    st.format.valid_bits_per_sample
                                } else {
                                    st.format.bits_per_sample
                                };
                                on_stream_started(DetectedExclusiveStream {
                                    generation: st.stream_generation,
                                    sample_rate_hz: st.format.sample_rate,
                                    channels: st.format.channels,
                                    bits_per_sample: st.format.bits_per_sample,
                                    valid_bits_per_sample: valid_bits,
                                    caller_name: caller.0,
                                    caller_pid: caller.1,
                                });
                            }
                        }
                        Err(_) => {
                            // Driver was restarted or uninstalled; drop stale handle and wait to reconnect.
                            transport_bg.replace(None);
                            last_reconnect_attempt = Instant::now();
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
    use std::collections::VecDeque;

    struct MockTransport {
        status: ApDriverStatus,
        chunks: VecDeque<(ApPcmChunkHeader, Vec<u8>)>,
    }

    impl CaptureTransport for MockTransport {
        fn get_status(&mut self) -> Result<ApDriverStatus, String> {
            Ok(self.status)
        }
        fn set_config(&mut self, _cfg: ApDriverConfig) -> Result<(), String> {
            Ok(())
        }
        fn reset_ring(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), String> {
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
                    reserved: 0,
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
        st.create_proc = proc16("audiodg.exe"); // brokered pin create is normal on Win11
        st.alloc_proc = proc16("TIDALPlayer.ex");
        st.run_proc = proc16("TIDALPlayer.ex");
        st.packet_proc = proc16("TIDALPlayer.ex");
        st.alloc_pid = 4242;
        st.run_pid = 4242;
        st.packet_pid = 4242;
        st
    }

    #[test]
    fn verify_status_accepts_tidal_exclusive_and_rejects_audiodg_data_path() {
        let ok_st = exclusive_status(96_000, 32, 24, 1);
        assert!(verify_status_exclusive(&ok_st).is_ok());

        let mut mixer_alloc = ok_st;
        mixer_alloc.alloc_proc = proc16("audiodg.exe");
        assert!(verify_status_exclusive(&mixer_alloc).is_err());

        let mut mixer_run = ok_st;
        mixer_run.run_proc = proc16("AUDIODG.EXE");
        assert!(verify_status_exclusive(&mixer_run).is_err());

        let mut float_st = ok_st;
        float_st.format.is_float = 1;
        assert!(verify_status_exclusive(&float_st).is_err());
    }

    #[test]
    fn decode_integer_pcm_is_exact_across_16_24_and_32_bit() {
        // 16-bit: +16384 -> +0.5, -32768 -> -1.0
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

        // 24-bit packed: +4194304 -> +0.5, -8388608 -> -1.0
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

        // 32-bit (and 24-in-32 MSB-aligned): +1073741824 -> +0.5, i32::MIN -> -1.0
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
    fn stream_capture_source_ends_cleanly_on_generation_change() {
        let st = exclusive_status(44_100, 16, 16, 7);
        let mut pcm = Vec::new();
        for _ in 0..4 {
            pcm.extend_from_slice(&16_384_i16.to_le_bytes());
            pcm.extend_from_slice(&(-16_384_i16).to_le_bytes());
        }
        let hdr_g7 = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 7,
            stream_running: 1,
            exclusive_verified: 1,
            format: st.format,
            pcm_bytes: pcm.len() as u32,
            ring_fill_after: 0,
            flags: 1,
            reserved: 0,
        };
        let hdr_g8 = ApPcmChunkHeader {
            stream_generation: 8, // rate change / next track!
            pcm_bytes: 0,
            ..hdr_g7
        };
        let mock = MockTransport {
            status: st,
            chunks: VecDeque::from([(hdr_g7, pcm), (hdr_g8, Vec::new())]),
        };
        let shared = SharedCaptureTransport::new(Some(Box::new(mock)));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut src = StreamCaptureSource::open_generation(shared, 7, cancel).unwrap();
        assert!(src.is_streaming());
        assert_eq!(src.spec().sample_rate_hz, 44_100);
        assert_eq!(src.spec().bits_per_sample, 16);

        let b1 = src.read_next_block(16).unwrap().expect("expected flushed 4 frames");
        assert_eq!(b1.left.len(), 4);
        assert_eq!(b1.left[0], 0.5);
        assert_eq!(b1.right[0], -0.5);

        // Generation advanced to 8 -> clean EOF so engine switches to new track/rate immediately.
        assert!(src.read_next_block(16).unwrap().is_none());
    }
}
