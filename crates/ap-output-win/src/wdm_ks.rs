//! Audirvana-style Direct Windows Kernel Streaming (`WDM-KS`) WaveRT backend.
//!
//! Communicates directly with `usbaudio2.sys` (or any WDM-KS WaveRT audio driver) via:
//! `SetupDiGetClassDevsW` -> `CreateFileW` -> `KsCreatePin` (`ksuser.dll`) ->
//! `KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION` ->
//! `KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT` -> `KSSTATE_RUN`.
//!
//! Bypasses BOTH `audiodg.exe` (Windows audio mixer) AND `AudioSes.dll` (WASAPI).
//! Windows Shared Mode is intentionally unimplemented and impossible to fall back to.

use std::sync::atomic::{AtomicBool, Ordering, fence};
#[cfg(windows)]
use std::sync::OnceLock;
use std::sync::Arc;

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;

use crate::hygiene::{MemoryLockGuard, MmcssGuard, OptimizationLevel};
use crate::sink::{OutputError, PlaybackStats, ZeroMixerAudioSink};

#[cfg(windows)]
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DIREG_DEV, HDEVINFO, SP_DEVINFO_DATA,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SPDRP_DEVICEDESC,
    SPDRP_FRIENDLYNAME, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiOpenDeviceInterfaceRegKey,
};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BUSY, ERROR_GEN_FAILURE, ERROR_INSUFFICIENT_BUFFER,
    ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_NO_SYSTEM_RESOURCES, ERROR_NOT_READY,
    ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
#[cfg(windows)]
use windows_sys::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
#[cfg(windows)]
use windows_sys::Win32::Media::KernelStreaming::{
    IOCTL_KS_PROPERTY, KSCATEGORY_AUDIO, KSDATAFORMAT, KSDATAFORMAT_0,
    KSDATAFORMAT_SPECIFIER_WAVEFORMATEX, KSDATAFORMAT_SUBTYPE_PCM, KSDATAFORMAT_TYPE_AUDIO,
    KSIDENTIFIER, KSIDENTIFIER_0, KSIDENTIFIER_0_0, KSINTERFACE_STANDARD_LOOPED_STREAMING,
    KSINTERFACE_STANDARD_STREAMING, KSINTERFACESETID_Standard, KSMEDIUM_STANDARD_DEVIO,
    KSMEDIUMSETID_Standard, KSMULTIPLE_ITEM, KSP_PIN, KSP_PIN_0, KSPIN_CINSTANCES,
    KSPIN_COMMUNICATION_BOTH, KSPIN_COMMUNICATION_SINK, KSPIN_CONNECT, KSPIN_DATAFLOW_IN,
    KSPRIORITY, KSPRIORITY_NORMAL, KSPROPERTY_CONNECTION_STATE, KSPROPERTY_PIN_CINSTANCES,
    KSPROPERTY_PIN_COMMUNICATION, KSPROPERTY_PIN_CTYPES, KSPROPERTY_PIN_DATAFLOW,
    KSPROPERTY_PIN_INTERFACES, KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION,
    KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT,
    KSPROPERTY_RTAUDIO_UNREGISTER_NOTIFICATION_EVENT, KSPROPERTY_TYPE_GET, KSPROPERTY_TYPE_SET,
    KSPROPSETID_Connection, KSPROPSETID_Pin, KSPROPSETID_RtAudio, KSRTAUDIO_BUFFER,
    KSRTAUDIO_BUFFER_PROPERTY_WITH_NOTIFICATION, KSRTAUDIO_NOTIFICATION_EVENT_PROPERTY,
    KSSTATE_ACQUIRE, KSSTATE_PAUSE, KSSTATE_RUN, KSSTATE_STOP, SPEAKER_FRONT_LEFT,
    SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::DeviceIoControl;
#[cfg(windows)]
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
#[cfg(windows)]
use windows_sys::Win32::System::Registry::{
    KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey, RegQueryValueExW,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Discovered Windows WDM-KS audio render filter and pin.
#[derive(Debug, Clone)]
pub struct KsRenderDevice {
    pub friendly_name: String,
    pub device_path: String,
    pub render_pin_id: u32,
    pub supports_looped_wavert: bool,
}

/// Exact Win32 `KSPIN_CONNECT` immediately followed by `KSDATAFORMAT_WAVEFORMATEXTENSIBLE`.
#[cfg(windows)]
#[repr(C)]
pub struct KsPinConnectBlob {
    connect: KSPIN_CONNECT,
    data_format: KSDATAFORMAT,
    wave_format: WAVEFORMATEXTENSIBLE,
}

#[cfg(windows)]
impl KsPinConnectBlob {
    /// Construct a Win32-ABI-exact PCM `KSPIN_CONNECT` + `KSDATAFORMAT_WAVEFORMATEXTENSIBLE` blob.
    pub fn new_pcm(
        pin_id: u32,
        sample_rate_hz: u32,
        channels: u16,
        wire_format: DacWireFormat,
        looped_wavert: bool,
    ) -> Self {
        let (bits_per_sample, valid_bits) = match wire_format {
            DacWireFormat::Int32 => (32_u16, 32_u16),
            DacWireFormat::Int24In32 => (32_u16, 24_u16),
            DacWireFormat::Int24Packed => (24_u16, 24_u16),
            DacWireFormat::Int16 => (16_u16, 16_u16),
        };
        let block_align = channels * (bits_per_sample / 8);
        let avg_bytes_per_sec = sample_rate_hz * u32::from(block_align);
        let format_size =
            (core::mem::size_of::<KSDATAFORMAT>() + core::mem::size_of::<WAVEFORMATEXTENSIBLE>())
                as u32;

        let iface_id = if looped_wavert {
            KSINTERFACE_STANDARD_LOOPED_STREAMING
        } else {
            KSINTERFACE_STANDARD_STREAMING
        };

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

        // Zero-initialize the entire blob first and write individual fields in place so every
        // padding byte (such as bytes 52..56 after `KSPIN_CONNECT.PinId` on x64) is guaranteed
        // initialized to 0, keeping `as_bytes()` completely sound.
        let mut uninit = core::mem::MaybeUninit::<Self>::zeroed();
        let ptr = uninit.as_mut_ptr();
        unsafe {
            core::ptr::addr_of_mut!((*ptr).connect.Interface).write(ks_identifier(
                KSINTERFACESETID_Standard,
                iface_id as u32,
                0,
            ));
            core::ptr::addr_of_mut!((*ptr).connect.Medium).write(ks_identifier(
                KSMEDIUMSETID_Standard,
                KSMEDIUM_STANDARD_DEVIO as u32,
                0,
            ));
            core::ptr::addr_of_mut!((*ptr).connect.PinId).write(pin_id);
            core::ptr::addr_of_mut!((*ptr).connect.PinToHandle).write(core::ptr::null_mut());
            core::ptr::addr_of_mut!((*ptr).connect.Priority).write(KSPRIORITY {
                PriorityClass: KSPRIORITY_NORMAL,
                PrioritySubClass: 1,
            });

            core::ptr::addr_of_mut!((*ptr).data_format).write(KSDATAFORMAT {
                Anonymous: KSDATAFORMAT_0 {
                    FormatSize: format_size,
                    Flags: 0,
                    SampleSize: u32::from(block_align),
                    Reserved: 0,
                    MajorFormat: KSDATAFORMAT_TYPE_AUDIO,
                    SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
                    Specifier: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
                },
            });

            core::ptr::addr_of_mut!((*ptr).wave_format).write(WAVEFORMATEXTENSIBLE {
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
            });

            uninit.assume_init()
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(
                (self as *const Self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }

    pub fn as_connect_ptr(&self) -> *const KSPIN_CONNECT {
        &self.connect
    }

    pub fn format_size(&self) -> u32 {
        unsafe { self.data_format.Anonymous.FormatSize }
    }

    pub fn block_align(&self) -> u16 {
        let f = self.wave_format.Format;
        f.nBlockAlign
    }

    pub fn bits_per_sample(&self) -> u16 {
        let f = self.wave_format.Format;
        f.wBitsPerSample
    }

    pub fn valid_bits_per_sample(&self) -> u16 {
        unsafe { self.wave_format.Samples.wValidBitsPerSample }
    }

    pub fn avg_bytes_per_sec(&self) -> u32 {
        let f = self.wave_format.Format;
        f.nAvgBytesPerSec
    }

    pub fn channel_mask(&self) -> u32 {
        let mask = self.wave_format.dwChannelMask;
        mask
    }
}

/// Pure, hardware-independent WaveRT cyclic ring-buffer frame encoder & pump.
///
/// Packs stereo MSB-aligned `[i32; 2]` frames into the exact little-endian integer
/// wire container (`Int32`, `Int24In32`, `Int24Packed`, `Int16`) and zero-pads the
/// tail cleanly once the source buffer is exhausted.
pub struct WaveRtFramePump<'a> {
    frames: &'a [[i32; 2]],
    cursor: usize,
    wire_format: DacWireFormat,
}

impl<'a> WaveRtFramePump<'a> {
    pub fn new(frames: &'a [[i32; 2]], wire_format: DacWireFormat) -> Self {
        Self {
            frames,
            cursor: 0,
            wire_format,
        }
    }

    pub fn bytes_per_frame(&self) -> usize {
        self.wire_format.bytes_per_frame()
    }

    pub fn is_exhausted(&self) -> bool {
        self.cursor >= self.frames.len()
    }

    pub fn frames_written(&self) -> usize {
        self.cursor
    }

    /// Fill `dst` with integer PCM bytes from the remaining source frames, zero-padding any remainder.
    /// Returns the number of real audio frames written (excluding trailing silence).
    pub fn fill_slice(&mut self, dst: &mut [u8]) -> usize {
        let bpf = self.bytes_per_frame();
        let max_frames = dst.len() / bpf;
        let remaining = self.frames.len().saturating_sub(self.cursor);
        let to_write = max_frames.min(remaining);

        let mut offset = 0;
        for &[l, r] in &self.frames[self.cursor..self.cursor + to_write] {
            encode_frame(&mut dst[offset..offset + bpf], l, r, self.wire_format);
            offset += bpf;
        }
        self.cursor += to_write;

        if offset < dst.len() {
            dst[offset..].fill(0);
        }
        to_write
    }

    /// Fill `dst` with a smooth linear fade-out to silence so mid-track stops are click-free.
    pub fn fill_fade_out_slice(&mut self, dst: &mut [u8]) -> usize {
        let written = self.fill_slice(dst);
        apply_linear_fade_out_bytes(dst, written, self.wire_format);
        written
    }
}

/// Internal chunk source abstraction shared by `play_full_memory` and `play_block_stream`.
pub trait WaveRtPumpSource {
    fn bytes_per_frame(&self) -> usize;
    fn frames_written(&self) -> usize;
    fn fill_slice(&mut self, dst: &mut [u8]) -> usize;
    fn fill_fade_out_slice(&mut self, dst: &mut [u8]) -> usize;
    fn take_error(&mut self) -> Option<OutputError> {
        None
    }
}

impl WaveRtPumpSource for WaveRtFramePump<'_> {
    fn bytes_per_frame(&self) -> usize {
        WaveRtFramePump::bytes_per_frame(self)
    }

    fn frames_written(&self) -> usize {
        WaveRtFramePump::frames_written(self)
    }

    fn fill_slice(&mut self, dst: &mut [u8]) -> usize {
        WaveRtFramePump::fill_slice(self, dst)
    }

    fn fill_fade_out_slice(&mut self, dst: &mut [u8]) -> usize {
        WaveRtFramePump::fill_fade_out_slice(self, dst)
    }
}

/// Streaming WaveRT ring-buffer pump that pulls bounded `InterleavedInt32Buffer` blocks
/// lazily across block boundaries with zero gaps and click-free fade-out on stop.
pub struct StreamingWaveRtPump<F>
where
    F: FnMut() -> Option<InterleavedInt32Buffer>,
{
    next_block: F,
    current: Option<InterleavedInt32Buffer>,
    cursor: usize,
    expected_sample_rate_hz: u32,
    wire_format: DacWireFormat,
    total_frames_written: usize,
    eof: bool,
    pending_error: Option<OutputError>,
}

impl<F> StreamingWaveRtPump<F>
where
    F: FnMut() -> Option<InterleavedInt32Buffer>,
{
    pub fn new(expected_sample_rate_hz: u32, wire_format: DacWireFormat, next_block: F) -> Self {
        Self {
            next_block,
            current: None,
            cursor: 0,
            expected_sample_rate_hz,
            wire_format,
            total_frames_written: 0,
            eof: false,
            pending_error: None,
        }
    }

    fn ensure_current(&mut self) -> bool {
        while !self.eof {
            if let Some(buf) = &self.current {
                if self.cursor < buf.frames.len() {
                    return true;
                }
            }
            match (self.next_block)() {
                Some(buf) if !buf.frames.is_empty() => {
                    if buf.sample_rate_hz != self.expected_sample_rate_hz {
                        self.current = None;
                        self.cursor = 0;
                        self.eof = true;
                        self.pending_error = Some(OutputError::InvalidBuffer(format!(
                            "stream block sample rate mismatch: pin is {} Hz, block is {} Hz",
                            self.expected_sample_rate_hz, buf.sample_rate_hz
                        )));
                        return false;
                    }
                    self.current = Some(buf);
                    self.cursor = 0;
                    return true;
                }
                Some(_) => continue, // skip empty block
                None => {
                    self.current = None;
                    self.cursor = 0;
                    self.eof = true;
                    return false;
                }
            }
        }
        false
    }
}

impl<F> WaveRtPumpSource for StreamingWaveRtPump<F>
where
    F: FnMut() -> Option<InterleavedInt32Buffer>,
{
    fn bytes_per_frame(&self) -> usize {
        self.wire_format.bytes_per_frame()
    }

    fn frames_written(&self) -> usize {
        self.total_frames_written
    }

    fn fill_slice(&mut self, dst: &mut [u8]) -> usize {
        let bpf = self.bytes_per_frame();
        let max_frames = dst.len() / bpf;
        let mut written_total = 0;

        while written_total < max_frames {
            if !self.ensure_current() {
                break;
            }
            let buf = self.current.as_ref().unwrap();
            let remaining = buf.frames.len() - self.cursor;
            let to_copy = (max_frames - written_total).min(remaining);

            let mut offset = written_total * bpf;
            for &[l, r] in &buf.frames[self.cursor..self.cursor + to_copy] {
                encode_frame(&mut dst[offset..offset + bpf], l, r, self.wire_format);
                offset += bpf;
            }
            self.cursor += to_copy;
            written_total += to_copy;
            self.total_frames_written += to_copy;
        }

        let used_bytes = written_total * bpf;
        if used_bytes < dst.len() {
            dst[used_bytes..].fill(0);
        }
        written_total
    }

    fn fill_fade_out_slice(&mut self, dst: &mut [u8]) -> usize {
        let written = self.fill_slice(dst);
        apply_linear_fade_out_bytes(dst, written, self.wire_format);
        written
    }

    fn take_error(&mut self) -> Option<OutputError> {
        self.pending_error.take()
    }
}

/// Round an MSB-aligned signed 32-bit PCM sample to its top 24 bits (unbiased round-to-nearest-even)
/// with the low 8 bits guaranteed zero and positive full-scale clamped cleanly without wraparound.
/// Already-quantized 24-bit samples (low byte == 0) pass through bit-identically.
#[inline]
fn round_i32_to_top_24(sample: i32) -> i32 {
    let mut q = i64::from(sample) >> 8;
    let rem = sample & 0xFF;
    if rem > 0x80 || (rem == 0x80 && (q & 1) != 0) {
        q += 1;
    }
    let clamped = q.clamp(-0x80_0000, 0x7F_FFFF) as i32;
    clamped << 8
}

/// Round an MSB-aligned signed 32-bit PCM sample to signed 16-bit (unbiased round-to-nearest-even)
/// with clean saturation. Already-quantized 16-bit samples (low 16 bits == 0) pass through bit-identically.
#[inline]
fn round_i32_to_top_16(sample: i32) -> i16 {
    let mut q = i64::from(sample) >> 16;
    let rem = sample & 0xFFFF;
    if rem > 0x8000 || (rem == 0x8000 && (q & 1) != 0) {
        q += 1;
    }
    q.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

#[inline]
fn encode_frame(dst: &mut [u8], l: i32, r: i32, wire_format: DacWireFormat) {
    match wire_format {
        DacWireFormat::Int32 => {
            dst[0..4].copy_from_slice(&l.to_le_bytes());
            dst[4..8].copy_from_slice(&r.to_le_bytes());
        }
        DacWireFormat::Int24In32 => {
            let l24 = round_i32_to_top_24(l);
            let r24 = round_i32_to_top_24(r);
            dst[0..4].copy_from_slice(&l24.to_le_bytes());
            dst[4..8].copy_from_slice(&r24.to_le_bytes());
        }
        DacWireFormat::Int24Packed => {
            let lb = round_i32_to_top_24(l).to_le_bytes();
            let rb = round_i32_to_top_24(r).to_le_bytes();
            dst[0..3].copy_from_slice(&lb[1..4]);
            dst[3..6].copy_from_slice(&rb[1..4]);
        }
        DacWireFormat::Int16 => {
            let l16 = round_i32_to_top_16(l);
            let r16 = round_i32_to_top_16(r);
            dst[0..2].copy_from_slice(&l16.to_le_bytes());
            dst[2..4].copy_from_slice(&r16.to_le_bytes());
        }
    }
}

fn apply_linear_fade_out_bytes(dst: &mut [u8], written: usize, wire_format: DacWireFormat) {
    if written == 0 {
        return;
    }
    let bpf = wire_format.bytes_per_frame();
    for i in 0..written {
        let gain = 1.0 - (i as f64) / (written as f64);
        let off = i * bpf;
        match wire_format {
            DacWireFormat::Int32 => {
                let l = i32::from_le_bytes(dst[off..off + 4].try_into().unwrap());
                let r = i32::from_le_bytes(dst[off + 4..off + 8].try_into().unwrap());
                let lf = ((l as f64) * gain).round() as i32;
                let rf = ((r as f64) * gain).round() as i32;
                dst[off..off + 4].copy_from_slice(&lf.to_le_bytes());
                dst[off + 4..off + 8].copy_from_slice(&rf.to_le_bytes());
            }
            DacWireFormat::Int24In32 => {
                let l = i32::from_le_bytes(dst[off..off + 4].try_into().unwrap());
                let r = i32::from_le_bytes(dst[off + 4..off + 8].try_into().unwrap());
                let lf = round_i32_to_top_24(((l as f64) * gain).round() as i32);
                let rf = round_i32_to_top_24(((r as f64) * gain).round() as i32);
                dst[off..off + 4].copy_from_slice(&lf.to_le_bytes());
                dst[off + 4..off + 8].copy_from_slice(&rf.to_le_bytes());
            }
            DacWireFormat::Int24Packed => {
                for ch_off in [off, off + 3] {
                    let s =
                        i32::from_le_bytes([0, dst[ch_off], dst[ch_off + 1], dst[ch_off + 2]]);
                    let sf = round_i32_to_top_24(((s as f64) * gain).round() as i32).to_le_bytes();
                    dst[ch_off..ch_off + 3].copy_from_slice(&sf[1..4]);
                }
            }
            DacWireFormat::Int16 => {
                for ch_off in [off, off + 2] {
                    let s = i16::from_le_bytes(dst[ch_off..ch_off + 2].try_into().unwrap());
                    let sf = ((s as f64) * gain).round() as i16;
                    dst[ch_off..ch_off + 2].copy_from_slice(&sf.to_le_bytes());
                }
            }
        }
    }
}

/// Classify a Win32 `KsCreatePin` failure code, highlighting busy/locked DACs (e.g. Audirvana).
pub fn classify_ks_pin_error(
    os_error: u32,
    device_name: &str,
    sample_rate_hz: u32,
    wire_format: DacWireFormat,
) -> OutputError {
    #[cfg(windows)]
    let is_busy = matches!(
        os_error,
        ERROR_BUSY
            | ERROR_SHARING_VIOLATION
            | ERROR_NO_SYSTEM_RESOURCES
            | ERROR_ACCESS_DENIED
            | ERROR_NOT_READY
    );
    #[cfg(not(windows))]
    let is_busy = matches!(os_error, 5 | 21 | 32 | 170 | 1450);

    if is_busy {
        return OutputError::DeviceBusy {
            device: device_name.to_string(),
            os_error,
            detail: "KsCreatePin denied because the kernel audio pin is currently in use"
                .to_string(),
        };
    }

    #[cfg(windows)]
    let is_gen_failure = os_error == ERROR_GEN_FAILURE;
    #[cfg(not(windows))]
    let is_gen_failure = os_error == 31;

    if is_gen_failure {
        return OutputError::DeviceBusy {
            device: device_name.to_string(),
            os_error,
            detail: "KsCreatePin returned ERROR_GEN_FAILURE (device busy/locked or pin unavailable)"
                .to_string(),
        };
    }

    OutputError::UnsupportedFormat {
        sample_rate_hz,
        wire_format,
        os_error,
        detail: "KsCreatePin rejected requested PCM format or pin parameters".to_string(),
    }
}

/// Pure, unit-testable WDM-KS device selection policy.
///
/// - When `device_filter` is `Some(sub)`, selects the first device matching `sub` (case-insensitive).
/// - When `device_filter` is `None` (zero-config default mode), prefers dedicated USB audiophile
///   DACs (Schiit/Unison/Gungnir/USB UAC2 WaveRT endpoints) over monitor HDMI/DisplayPort audio,
///   falling back gracefully to any available WDM-KS render device.
pub fn select_wdm_ks_device(
    devices: &[KsRenderDevice],
    device_filter: Option<&str>,
) -> Result<KsRenderDevice, OutputError> {
    if devices.is_empty() {
        return Err(OutputError::DeviceNotFound(
            "no Windows WDM-KS audio render devices found".to_string(),
        ));
    }

    let wavert_devices: Vec<&KsRenderDevice> = devices
        .iter()
        .filter(|d| d.supports_looped_wavert)
        .collect();
    if wavert_devices.is_empty() {
        return Err(OutputError::DeviceNotFound(
            "no Windows WDM-KS looped WaveRT audio render devices found".to_string(),
        ));
    }

    if let Some(filter) = device_filter {
        let needle = filter.to_lowercase();
        return wavert_devices
            .into_iter()
            .find(|d| d.friendly_name.to_lowercase().contains(&needle))
            .cloned()
            .ok_or_else(|| {
                OutputError::DeviceNotFound(format!(
                    "no WDM-KS looped WaveRT render device matching '{filter}'"
                ))
            });
    }

    let score = |d: &KsRenderDevice| -> i32 {
        let name = d.friendly_name.to_lowercase();
        let path = d.device_path.to_lowercase();
        let mut s = 0;
        if name.contains("gungnir") || name.contains("unison") || name.contains("schiit") {
            s += 100;
        }
        if path.contains("\\usb#") {
            s += 40;
        }
        if path.contains("msft_wave") {
            s += 30; // Microsoft UAC2 usbaudio2.sys WaveRT driver
        }
        if d.supports_looped_wavert {
            s += 20;
        }
        if name.contains("hdmi") || name.contains("displayport") || name.contains("nvidia") {
            s -= 50;
        }
        s
    };

    wavert_devices
        .into_iter()
        .max_by_key(|d| score(d))
        .cloned()
        .ok_or_else(|| OutputError::DeviceNotFound("no WDM-KS render device available".to_string()))
}

/// Direct Windows Kernel Streaming (`WDM-KS`) WaveRT output sink.
#[derive(Debug)]
pub struct WdmKsSink {
    device: KsRenderDevice,
    optimization: OptimizationLevel,
    stop_flag: Arc<AtomicBool>,
}

impl WdmKsSink {
    /// Locate a WDM-KS render device whose friendly name contains `device_filter` (case-insensitive).
    pub fn open(device_filter: &str) -> Result<Self, OutputError> {
        Self::open_with_optimization(device_filter, OptimizationLevel::High)
    }

    pub fn open_with_optimization(
        device_filter: &str,
        optimization: OptimizationLevel,
    ) -> Result<Self, OutputError> {
        Self::open_best(Some(device_filter), optimization)
    }

    /// Open the requested WDM-KS device, or auto-select the highest-priority audiophile DAC
    /// when `device_filter` is `None` (default zero-option mode).
    pub fn open_best(
        device_filter: Option<&str>,
        optimization: OptimizationLevel,
    ) -> Result<Self, OutputError> {
        let devices = enumerate_wdm_ks_render_devices()?;
        let found = select_wdm_ks_device(&devices, device_filter)?;
        Ok(Self {
            device: found,
            optimization,
            stop_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Return a thread-safe stop handle that can gracefully interrupt active playback
    /// (with a click-free fade-out) from a signal handler, timer, or HTTP Stop button.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop_flag)
    }

    pub fn device(&self) -> &KsRenderDevice {
        &self.device
    }

    /// Probe whether the hardware DAC accepts `(sample_rate_hz, wire_format)` via live `KsCreatePin`.
    /// Immediately closes the kernel pin via RAII on return.
    #[cfg(windows)]
    pub fn probe_format(
        &self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
    ) -> Result<(), OutputError> {
        let filter = open_ks_filter(&self.device.device_path)?;
        // Probe with the exact same looped WaveRT interface used during playback.
        let _pin = create_ks_render_pin(
            filter.raw(),
            &self.device,
            sample_rate_hz,
            wire_format,
            true,
        )?;
        Ok(())
    }
    /// Stream bounded `InterleavedInt32Buffer` blocks from `next_block` directly to the DAC
    /// via Windows Kernel Streaming (`WDM-KS` WaveRT) without ever requiring the entire track
    /// to live in RAM at once. Closes/releases the kernel pin cleanly via RAII on return.
    #[cfg(windows)]
    pub fn play_block_stream<F>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> Option<InterleavedInt32Buffer>,
    {
        let mut pump = StreamingWaveRtPump::new(sample_rate_hz, wire_format, next_block);
        self.run_wavert_pump(sample_rate_hz, wire_format, &mut pump)
    }

    #[cfg(windows)]
    fn run_wavert_pump(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        pump: &mut dyn WaveRtPumpSource,
    ) -> Result<PlaybackStats, OutputError> {
        // Never clear `stop_flag` here — the caller arms (clears) it before checking pending
        // signals/commands so Ctrl+C, Pause, or Next arriving before open is never swallowed.
        if self.stop_flag.load(Ordering::SeqCst) {
            return Ok(PlaybackStats {
                device_name: self.device.friendly_name.clone(),
                sample_rate_hz,
                wire_format,
                frames_played: 0,
                wavert_buffer_bytes: 0,
                notifications: 0,
                mmcss_pro_audio: false,
            });
        }

        // 1. Elevate thread priority via MMCSS "Pro Audio" (reverted automatically via RAII).
        let mmcss = MmcssGuard::enter_pro_audio(self.optimization);

        // 2. Open KS filter and create WaveRT looped render pin directly via ksuser.dll.
        let filter = open_ks_filter(&self.device.device_path)?;
        let pin = create_ks_render_pin(
            filter.raw(),
            &self.device,
            sample_rate_hz,
            wire_format,
            true,
        )?;

        // 3. Allocate WaveRT cyclic buffer (~100 ms total = two ~50 ms notification halves).
        let bpf = pump.bytes_per_frame() as u32;
        let requested_bytes = ((sample_rate_hz / 10).max(256)) * bpf;
        let rt_buf = allocate_wavert_buffer_with_notification(pin.raw(), requested_bytes, 2)?;
        if rt_buf.BufferAddress.is_null() || rt_buf.ActualBufferSize < bpf * 2 {
            return Err(OutputError::Win32 {
                operation: "KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION",
                code: 0,
                detail: format!(
                    "driver returned invalid WaveRT buffer (ptr={:p}, size={})",
                    rt_buf.BufferAddress, rt_buf.ActualBufferSize
                ),
            });
        }

        let buf_len = rt_buf.ActualBufferSize as usize;
        let half_len = buf_len / 2;
        let call_barrier = rt_buf.CallMemoryBarrier != 0;

        // 4. Create & register Win32 hardware notification event (RAII unregistered on drop).
        let event_handle = create_auto_reset_event()?;
        let _notif_guard = WaveRtNotificationGuard::register(pin.raw(), event_handle.raw())?;

        // 5. Ensure pin returns to KSSTATE_STOP even if an error occurs mid-stream.
        let mut state_guard = KsPinStateGuard::new(pin.raw());

        // 6. Pre-fill BOTH halves of the cyclic WaveRT buffer before starting hardware DMA.
        let cyclic_slice =
            unsafe { core::slice::from_raw_parts_mut(rt_buf.BufferAddress.cast::<u8>(), buf_len) };
        pump.fill_slice(&mut cyclic_slice[0..half_len]);
        if let Some(err) = pump.take_error() {
            return Err(err);
        }
        pump.fill_slice(&mut cyclic_slice[half_len..half_len * 2]);
        if let Some(err) = pump.take_error() {
            return Err(err);
        }
        if call_barrier {
            fence(Ordering::SeqCst);
        }

        // 7. Transition pin: ACQUIRE -> PAUSE -> RUN.
        state_guard.set_state(KSSTATE_ACQUIRE)?;
        state_guard.set_state(KSSTATE_PAUSE)?;
        state_guard.set_state(KSSTATE_RUN)?;

        // 8. Event-driven WaveRT loop: each notification signals one completed half-buffer.
        let mut next_half: usize = 0;
        let mut trailing_silent_halves: usize = 0;
        let mut stopping_fade_done = false;
        let mut notifications: usize = 0;
        let mut pump_error: Option<OutputError> = None;

        loop {
            let wait_res = unsafe { WaitForSingleObject(event_handle.raw(), 1500) };
            if wait_res != WAIT_OBJECT_0 {
                return Err(OutputError::Win32 {
                    operation: "WaitForSingleObject(WaveRT)",
                    code: wait_res,
                    detail: "timed out waiting for WaveRT hardware notification event".to_string(),
                });
            }
            notifications += 1;

            let start = next_half * half_len;
            let end = start + half_len;

            if self.stop_flag.load(Ordering::SeqCst) {
                if !stopping_fade_done {
                    pump.fill_fade_out_slice(&mut cyclic_slice[start..end]);
                    stopping_fade_done = true;
                } else {
                    cyclic_slice[start..end].fill(0);
                    trailing_silent_halves += 1;
                }
                if call_barrier {
                    fence(Ordering::SeqCst);
                }
                next_half ^= 1;
                if trailing_silent_halves >= 1 {
                    break;
                }
                continue;
            }

            let written = pump.fill_slice(&mut cyclic_slice[start..end]);
            if call_barrier {
                fence(Ordering::SeqCst);
            }
            next_half ^= 1;

            if let Some(err) = pump.take_error() {
                pump_error = Some(err);
                break;
            }

            if written == 0 {
                trailing_silent_halves += 1;
                if trailing_silent_halves >= 2 {
                    break;
                }
            }
        }

        // 9. Clean stop: PAUSE -> STOP (then RAII drops unregister event, close pin & filter).
        let _ = state_guard.set_state(KSSTATE_PAUSE);
        let _ = state_guard.set_state(KSSTATE_STOP);

        if let Some(err) = pump_error {
            return Err(err);
        }

        Ok(PlaybackStats {
            device_name: self.device.friendly_name.clone(),
            sample_rate_hz,
            wire_format,
            frames_played: pump.frames_written(),
            wavert_buffer_bytes: rt_buf.ActualBufferSize,
            notifications,
            mmcss_pro_audio: mmcss.is_active(),
        })
    }
}

impl ZeroMixerAudioSink for WdmKsSink {
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
        self.run_wavert_pump(pcm.sample_rate_hz, wire_format, &mut pump)
    }

    #[cfg(not(windows))]
    fn play_full_memory(
        &mut self,
        _pcm: &InterleavedInt32Buffer,
        _wire_format: DacWireFormat,
    ) -> Result<PlaybackStats, OutputError> {
        Err(OutputError::DeviceNotFound(
            "WDM-KS is only supported on Windows".to_string(),
        ))
    }

    fn stop(&mut self) -> Result<(), OutputError> {
        self.stop_flag.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Enumerate all active Windows WDM-KS audio render devices (`KSCATEGORY_AUDIO`).
#[cfg(windows)]
pub fn enumerate_wdm_ks_render_devices() -> Result<Vec<KsRenderDevice>, OutputError> {
    let class_guid = KSCATEGORY_AUDIO;
    let dev_info = unsafe {
        SetupDiGetClassDevsW(
            &class_guid,
            core::ptr::null(),
            core::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if dev_info == INVALID_HANDLE_VALUE as isize {
        return Err(win32_err("SetupDiGetClassDevsW"));
    }
    let _info_guard = DevInfoGuard(dev_info);

    let mut results = Vec::new();
    let mut index: u32 = 0;

    loop {
        let mut iface_data: SP_DEVICE_INTERFACE_DATA = unsafe { core::mem::zeroed() };
        iface_data.cbSize = core::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

        let ok = unsafe {
            SetupDiEnumDeviceInterfaces(
                dev_info,
                core::ptr::null(),
                &class_guid,
                index,
                &mut iface_data,
            )
        };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(OutputError::Win32 {
                operation: "SetupDiEnumDeviceInterfaces",
                code: err,
                detail: "failed enumerating KS audio device interfaces".to_string(),
            });
        }
        index += 1;

        let mut dev_data: SP_DEVINFO_DATA = unsafe { core::mem::zeroed() };
        dev_data.cbSize = core::mem::size_of::<SP_DEVINFO_DATA>() as u32;

        let Some(device_path) = get_interface_device_path(dev_info, &mut iface_data, &mut dev_data)
        else {
            continue;
        };

        let friendly_name = get_friendly_name(dev_info, &mut iface_data, &mut dev_data)
            .unwrap_or_else(|| device_path.clone());

        // Open KS filter to inspect whether it exposes an audio render sink pin.
        let Ok(filter) = open_ks_filter(&device_path) else {
            continue;
        };
        if let Some((pin_id, looped)) = find_render_pin(filter.raw()) {
            results.push(KsRenderDevice {
                friendly_name,
                device_path,
                render_pin_id: pin_id,
                supports_looped_wavert: looped,
            });
        }
    }

    Ok(results)
}

#[cfg(not(windows))]
pub fn enumerate_wdm_ks_render_devices() -> Result<Vec<KsRenderDevice>, OutputError> {
    Ok(Vec::new())
}

// ---------------------------------------------------------------------------
// Internal Win32 Kernel Streaming RAII & IOCTL Helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn ks_identifier(set: windows_sys::core::GUID, id: u32, flags: u32) -> KSIDENTIFIER {
    KSIDENTIFIER {
        Anonymous: KSIDENTIFIER_0 {
            Anonymous: KSIDENTIFIER_0_0 {
                Set: set,
                Id: id,
                Flags: flags,
            },
        },
    }
}

#[cfg(windows)]
fn win32_err(operation: &'static str) -> OutputError {
    let code = unsafe { GetLastError() };
    OutputError::Win32 {
        operation,
        code,
        detail: format!("Win32 error code {code}"),
    }
}

#[cfg(windows)]
struct DevInfoGuard(HDEVINFO);
#[cfg(windows)]
impl Drop for DevInfoGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

#[cfg(windows)]
struct OwnedHandle(HANDLE);
#[cfg(windows)]
impl OwnedHandle {
    fn raw(&self) -> HANDLE {
        self.0
    }
}
#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = CloseHandle(self.0);
            }
            self.0 = core::ptr::null_mut();
        }
    }
}

#[cfg(windows)]
struct WaveRtNotificationGuard {
    pin: HANDLE,
    event: HANDLE,
}
#[cfg(windows)]
impl WaveRtNotificationGuard {
    fn register(pin: HANDLE, event: HANDLE) -> Result<Self, OutputError> {
        let prop = KSRTAUDIO_NOTIFICATION_EVENT_PROPERTY {
            Property: ks_identifier(
                KSPROPSETID_RtAudio,
                KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT as u32,
                KSPROPERTY_TYPE_SET,
            ),
            NotificationEvent: event,
        };
        ks_ioctl(pin, &prop, core::ptr::null_mut(), 0, "RegisterNotificationEvent")?;
        Ok(Self { pin, event })
    }
}
#[cfg(windows)]
impl Drop for WaveRtNotificationGuard {
    fn drop(&mut self) {
        let prop = KSRTAUDIO_NOTIFICATION_EVENT_PROPERTY {
            Property: ks_identifier(
                KSPROPSETID_RtAudio,
                KSPROPERTY_RTAUDIO_UNREGISTER_NOTIFICATION_EVENT as u32,
                KSPROPERTY_TYPE_SET,
            ),
            NotificationEvent: self.event,
        };
        let _ = ks_ioctl(
            self.pin,
            &prop,
            core::ptr::null_mut(),
            0,
            "UnregisterNotificationEvent",
        );
    }
}

#[cfg(windows)]
struct KsPinStateGuard {
    pin: HANDLE,
    active: bool,
}
#[cfg(windows)]
impl KsPinStateGuard {
    fn new(pin: HANDLE) -> Self {
        Self { pin, active: false }
    }

    fn set_state(&mut self, state: i32) -> Result<(), OutputError> {
        let prop = ks_identifier(
            KSPROPSETID_Connection,
            KSPROPERTY_CONNECTION_STATE as u32,
            KSPROPERTY_TYPE_SET,
        );
        let mut st = state;
        ks_ioctl(
            self.pin,
            &prop,
            (&mut st as *mut i32).cast(),
            core::mem::size_of::<i32>() as u32,
            "SetKsPinState",
        )?;
        self.active = state != KSSTATE_STOP;
        Ok(())
    }
}
#[cfg(windows)]
impl Drop for KsPinStateGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = self.set_state(KSSTATE_PAUSE);
            let _ = self.set_state(KSSTATE_STOP);
        }
    }
}

#[cfg(windows)]
fn ks_ioctl<T>(
    handle: HANDLE,
    in_prop: &T,
    out_ptr: *mut core::ffi::c_void,
    out_len: u32,
    op_name: &'static str,
) -> Result<u32, OutputError> {
    let mut returned: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_KS_PROPERTY,
            (in_prop as *const T).cast(),
            core::mem::size_of::<T>() as u32,
            out_ptr,
            out_len,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(win32_err(op_name));
    }
    Ok(returned)
}

#[cfg(windows)]
fn open_ks_filter(device_path: &str) -> Result<OwnedHandle, OutputError> {
    let wide: Vec<u16> = device_path.encode_utf16().chain(core::iter::once(0)).collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return Err(win32_err("CreateFileW(KS filter)"));
    }
    Ok(OwnedHandle(h))
}

#[cfg(windows)]
fn create_auto_reset_event() -> Result<OwnedHandle, OutputError> {
    let h = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
    if h.is_null() {
        return Err(win32_err("CreateEventW"));
    }
    Ok(OwnedHandle(h))
}

#[cfg(windows)]
type KsCreatePinFn =
    unsafe extern "system" fn(HANDLE, *const KSPIN_CONNECT, u32, *mut HANDLE) -> u32;

#[cfg(windows)]
fn resolve_ks_create_pin() -> Result<KsCreatePinFn, OutputError> {
    static KS_CREATE_PIN: OnceLock<Result<KsCreatePinFn, (&'static str, u32, &'static str)>> =
        OnceLock::new();
    match *KS_CREATE_PIN.get_or_init(|| {
        let dll_name: Vec<u16> = "ksuser.dll\0".encode_utf16().collect();
        // Load once strictly from System32 to avoid DLL search-order planting and refcount leaks.
        let module = unsafe {
            LoadLibraryExW(
                dll_name.as_ptr(),
                core::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return Err((
                "LoadLibraryExW(ksuser.dll)",
                unsafe { GetLastError() },
                "failed to load ksuser.dll from System32",
            ));
        }
        let Some(sym) = (unsafe { GetProcAddress(module, c"KsCreatePin".as_ptr().cast()) }) else {
            return Err((
                "GetProcAddress(KsCreatePin)",
                unsafe { GetLastError() },
                "KsCreatePin export missing from ksuser.dll",
            ));
        };
        Ok(unsafe { core::mem::transmute::<_, KsCreatePinFn>(sym) })
    }) {
        Ok(f) => Ok(f),
        Err((operation, code, detail)) => Err(OutputError::Win32 {
            operation,
            code,
            detail: detail.to_string(),
        }),
    }
}

#[cfg(windows)]
fn create_ks_render_pin(
    filter: HANDLE,
    device: &KsRenderDevice,
    sample_rate_hz: u32,
    wire_format: DacWireFormat,
    looped_wavert: bool,
) -> Result<OwnedHandle, OutputError> {
    let ks_create_pin = resolve_ks_create_pin()?;

    let blob = KsPinConnectBlob::new_pcm(
        device.render_pin_id,
        sample_rate_hz,
        2,
        wire_format,
        looped_wavert,
    );

    let mut pin_handle: HANDLE = core::ptr::null_mut();
    let status = unsafe {
        ks_create_pin(
            filter,
            blob.as_connect_ptr(),
            GENERIC_WRITE | GENERIC_READ,
            &mut pin_handle,
        )
    };
    if status != 0 || pin_handle.is_null() || pin_handle == INVALID_HANDLE_VALUE {
        // Confirm via KSPROPERTY_PIN_CINSTANCES whether another exclusive/KS client holds the pin.
        if let Some(inst) = query_pin_cinstances(filter, device.render_pin_id) {
            if inst.CurrentCount >= inst.PossibleCount {
                return Err(OutputError::DeviceBusy {
                    device: device.friendly_name.clone(),
                    os_error: status,
                    detail: format!(
                        "KS pin {} is at capacity ({}/{} instances active)",
                        device.render_pin_id, inst.CurrentCount, inst.PossibleCount
                    ),
                });
            }
        }
        return Err(classify_ks_pin_error(
            status,
            &device.friendly_name,
            sample_rate_hz,
            wire_format,
        ));
    }
    Ok(OwnedHandle(pin_handle))
}

#[cfg(windows)]
fn query_pin_cinstances(filter: HANDLE, pin_id: u32) -> Option<KSPIN_CINSTANCES> {
    let ksp = KSP_PIN {
        Property: ks_identifier(
            KSPROPSETID_Pin,
            KSPROPERTY_PIN_CINSTANCES as u32,
            KSPROPERTY_TYPE_GET,
        ),
        PinId: pin_id,
        Anonymous: KSP_PIN_0 { Reserved: 0 },
    };
    let mut out: KSPIN_CINSTANCES = unsafe { core::mem::zeroed() };
    ks_ioctl(
        filter,
        &ksp,
        (&mut out as *mut KSPIN_CINSTANCES).cast(),
        core::mem::size_of::<KSPIN_CINSTANCES>() as u32,
        "KSPROPERTY_PIN_CINSTANCES",
    )
    .ok()?;
    Some(out)
}

#[cfg(windows)]
fn allocate_wavert_buffer_with_notification(
    pin: HANDLE,
    requested_bytes: u32,
    notification_count: u32,
) -> Result<KSRTAUDIO_BUFFER, OutputError> {
    let prop = KSRTAUDIO_BUFFER_PROPERTY_WITH_NOTIFICATION {
        Property: ks_identifier(
            KSPROPSETID_RtAudio,
            KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION as u32,
            KSPROPERTY_TYPE_GET,
        ),
        BaseAddress: core::ptr::null_mut(),
        RequestedBufferSize: requested_bytes,
        NotificationCount: notification_count,
    };
    let mut out: KSRTAUDIO_BUFFER = unsafe { core::mem::zeroed() };
    ks_ioctl(
        pin,
        &prop,
        (&mut out as *mut KSRTAUDIO_BUFFER).cast(),
        core::mem::size_of::<KSRTAUDIO_BUFFER>() as u32,
        "KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION",
    )?;
    Ok(out)
}

#[cfg(windows)]
fn find_render_pin(filter: HANDLE) -> Option<(u32, bool)> {
    let prop = ks_identifier(
        KSPROPSETID_Pin,
        KSPROPERTY_PIN_CTYPES as u32,
        KSPROPERTY_TYPE_GET,
    );
    let mut pin_count: u32 = 0;
    ks_ioctl(
        filter,
        &prop,
        (&mut pin_count as *mut u32).cast(),
        core::mem::size_of::<u32>() as u32,
        "KSPROPERTY_PIN_CTYPES",
    )
    .ok()?;

    for pin_id in 0..pin_count {
        let Some(dataflow) = query_pin_u32(filter, pin_id, KSPROPERTY_PIN_DATAFLOW as u32) else {
            continue;
        };
        if dataflow != KSPIN_DATAFLOW_IN as u32 {
            continue;
        }
        let Some(comm) = query_pin_u32(filter, pin_id, KSPROPERTY_PIN_COMMUNICATION as u32) else {
            continue;
        };
        if comm != KSPIN_COMMUNICATION_SINK as u32 && comm != KSPIN_COMMUNICATION_BOTH as u32 {
            continue;
        }
        let (_has_streaming, has_looped) = query_pin_streaming_interfaces(filter, pin_id);
        // Playback exclusively uses looped WaveRT streaming; skip non-WaveRT pins so a
        // WaveCyclic/WavePci pin never wins selection or probes OK then fails playback.
        if has_looped {
            return Some((pin_id, true));
        }
    }
    None
}

#[cfg(windows)]
fn query_pin_u32(filter: HANDLE, pin_id: u32, prop_id: u32) -> Option<u32> {
    let ksp = KSP_PIN {
        Property: ks_identifier(KSPROPSETID_Pin, prop_id, KSPROPERTY_TYPE_GET),
        PinId: pin_id,
        Anonymous: KSP_PIN_0 { Reserved: 0 },
    };
    let mut val: u32 = 0;
    ks_ioctl(
        filter,
        &ksp,
        (&mut val as *mut u32).cast(),
        core::mem::size_of::<u32>() as u32,
        "QueryPinU32",
    )
    .ok()?;
    Some(val)
}

#[cfg(windows)]
fn query_pin_streaming_interfaces(filter: HANDLE, pin_id: u32) -> (bool, bool) {
    const MAX_INTERFACES_BYTES: usize = 64 * 1024;

    let ksp = KSP_PIN {
        Property: ks_identifier(
            KSPROPSETID_Pin,
            KSPROPERTY_PIN_INTERFACES as u32,
            KSPROPERTY_TYPE_GET,
        ),
        PinId: pin_id,
        Anonymous: KSP_PIN_0 { Reserved: 0 },
    };

    // Step 1: two-step size probe with a 0-length output buffer.
    let mut required_bytes: u32 = 0;
    let probe_ok = unsafe {
        DeviceIoControl(
            filter,
            IOCTL_KS_PROPERTY,
            (&ksp as *const KSP_PIN).cast(),
            core::mem::size_of::<KSP_PIN>() as u32,
            core::ptr::null_mut(),
            0,
            &mut required_bytes,
            core::ptr::null_mut(),
        )
    };
    let probe_err = if probe_ok == 0 {
        unsafe { GetLastError() }
    } else {
        0
    };

    let mut alloc_bytes = if (probe_ok != 0
        || probe_err == ERROR_MORE_DATA
        || probe_err == ERROR_INSUFFICIENT_BUFFER)
        && (required_bytes as usize) >= core::mem::size_of::<KSMULTIPLE_ITEM>()
    {
        (required_bytes as usize).min(MAX_INTERFACES_BYTES)
    } else {
        // Fallback initial buffer for drivers that reject 0-length size probes.
        512
    };

    // Back with `Vec<u64>` so the buffer is 8-byte aligned for KSMULTIPLE_ITEM / KSIDENTIFIER.
    let mut buf = vec![0u64; (alloc_bytes + 7) / 8];
    let mut bytes_returned: u32 = 0;
    let mut ok = unsafe {
        DeviceIoControl(
            filter,
            IOCTL_KS_PROPERTY,
            (&ksp as *const KSP_PIN).cast(),
            core::mem::size_of::<KSP_PIN>() as u32,
            buf.as_mut_ptr().cast(),
            (buf.len() * 8) as u32,
            &mut bytes_returned,
            core::ptr::null_mut(),
        )
    };

    // Retry once if the fallback 512-byte buffer was too small and the driver reported the exact size.
    if ok == 0 {
        let err = unsafe { GetLastError() };
        if (err == ERROR_MORE_DATA || err == ERROR_INSUFFICIENT_BUFFER)
            && (bytes_returned as usize) > buf.len() * 8
            && (bytes_returned as usize) <= MAX_INTERFACES_BYTES
        {
            alloc_bytes = bytes_returned as usize;
            buf = vec![0u64; (alloc_bytes + 7) / 8];
            bytes_returned = 0;
            ok = unsafe {
                DeviceIoControl(
                    filter,
                    IOCTL_KS_PROPERTY,
                    (&ksp as *const KSP_PIN).cast(),
                    core::mem::size_of::<KSP_PIN>() as u32,
                    buf.as_mut_ptr().cast(),
                    (buf.len() * 8) as u32,
                    &mut bytes_returned,
                    core::ptr::null_mut(),
                )
            };
        }
    }

    if ok == 0 || (bytes_returned as usize) < core::mem::size_of::<KSMULTIPLE_ITEM>() {
        // Never assume looped WaveRT support when the interface query fails.
        return (false, false);
    }

    let valid_bytes = (bytes_returned as usize).min(buf.len() * 8);
    let raw_bytes = buf.as_ptr().cast::<u8>();
    let header = unsafe { core::ptr::read_unaligned(raw_bytes.cast::<KSMULTIPLE_ITEM>()) };
    let count = header.Count as usize;
    let item_size = core::mem::size_of::<KSIDENTIFIER>();
    let base = core::mem::size_of::<KSMULTIPLE_ITEM>();

    let mut has_streaming = false;
    let mut has_looped = false;
    for i in 0..count {
        let off = base.saturating_add(i.saturating_mul(item_size));
        if off.saturating_add(item_size) > valid_bytes {
            break;
        }
        let ident = unsafe { core::ptr::read_unaligned(raw_bytes.add(off).cast::<KSIDENTIFIER>()) };
        let (set, id) = unsafe { (ident.Anonymous.Anonymous.Set, ident.Anonymous.Anonymous.Id) };
        if guid_eq(set, KSINTERFACESETID_Standard) {
            if id == KSINTERFACE_STANDARD_STREAMING as u32 {
                has_streaming = true;
            }
            if id == KSINTERFACE_STANDARD_LOOPED_STREAMING as u32 {
                has_looped = true;
            }
        }
    }
    (has_streaming, has_looped)
}

#[cfg(windows)]
fn guid_eq(a: windows_sys::core::GUID, b: windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

#[cfg(windows)]
fn get_interface_device_path(
    dev_info: HDEVINFO,
    iface_data: &mut SP_DEVICE_INTERFACE_DATA,
    dev_data: &mut SP_DEVINFO_DATA,
) -> Option<String> {
    let mut required: u32 = 0;
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            iface_data,
            core::ptr::null_mut(),
            0,
            &mut required,
            core::ptr::null_mut(),
        );
    }
    if required == 0 {
        return None;
    }
    let err = unsafe { GetLastError() };
    if err != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }

    // Allocate 4-byte-aligned backing buffer for SP_DEVICE_INTERFACE_DETAIL_DATA_W.
    let mut buf = vec![0u32; (required as usize + 3) / 4];
    let detail = buf.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    unsafe {
        (*detail).cbSize = core::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    }

    let ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            dev_info,
            iface_data,
            detail,
            required,
            core::ptr::null_mut(),
            dev_data,
        )
    };
    if ok == 0 {
        return None;
    }

    let path_ptr = unsafe { core::ptr::addr_of!((*detail).DevicePath).cast::<u16>() };
    let path_offset = (path_ptr as usize).saturating_sub(detail as usize);
    let max_bytes = (required as usize).min(buf.len() * 4).saturating_sub(path_offset);
    let max_units = max_bytes / 2;
    let slice = unsafe { core::slice::from_raw_parts(path_ptr, max_units) };
    Some(wide_slice_to_string(slice))
}

#[cfg(windows)]
fn get_friendly_name(
    dev_info: HDEVINFO,
    iface_data: &mut SP_DEVICE_INTERFACE_DATA,
    dev_data: &mut SP_DEVINFO_DATA,
) -> Option<String> {
    // 1. Prefer the KS interface registry key's "FriendlyName" (e.g. "Speakers (Gungnir 2 Unison 384)").
    let hkey = unsafe { SetupDiOpenDeviceInterfaceRegKey(dev_info, iface_data, 0, KEY_READ) };
    if !hkey.is_null() && hkey != INVALID_HANDLE_VALUE {
        let name = query_reg_sz(hkey, "FriendlyName");
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        if let Some(n) = name {
            if !n.is_empty() {
                return Some(n);
            }
        }
    }

    // 2. Fallback to device SPDRP_FRIENDLYNAME or SPDRP_DEVICEDESC.
    let _ = DIREG_DEV;
    for prop in [SPDRP_FRIENDLYNAME, SPDRP_DEVICEDESC] {
        if let Some(s) = query_dev_registry_string(dev_info, dev_data, prop) {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

#[cfg(windows)]
fn query_reg_sz(hkey: windows_sys::Win32::System::Registry::HKEY, value_name: &str) -> Option<String> {
    let wide_name: Vec<u16> = value_name.encode_utf16().chain(core::iter::once(0)).collect();
    let mut buf = vec![0u16; 256];
    let mut reg_type: u32 = 0;
    // Leave at least one guaranteed trailing u16 slot and always slice by `byte_len / 2` so a
    // non-NUL-terminated REG_SZ can never read out of bounds.
    let mut byte_len = ((buf.len() - 1) * 2) as u32;
    let status = unsafe {
        RegQueryValueExW(
            hkey,
            wide_name.as_ptr(),
            core::ptr::null(),
            &mut reg_type,
            buf.as_mut_ptr().cast(),
            &mut byte_len,
        )
    };
    if status != 0 || byte_len < 2 || (reg_type != REG_SZ && reg_type != REG_EXPAND_SZ) {
        return None;
    }
    let units = (byte_len as usize / 2).min(buf.len());
    Some(wide_slice_to_string(&buf[..units]))
}

#[cfg(windows)]
fn query_dev_registry_string(
    dev_info: HDEVINFO,
    dev_data: &mut SP_DEVINFO_DATA,
    prop: u32,
) -> Option<String> {
    let mut buf = vec![0u16; 256];
    let mut reg_type: u32 = 0;
    let mut required: u32 = 0;
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            dev_info,
            dev_data,
            prop,
            &mut reg_type,
            buf.as_mut_ptr().cast(),
            ((buf.len() - 1) * 2) as u32,
            &mut required,
        )
    };
    if ok == 0 || required < 2 || (reg_type != REG_SZ && reg_type != REG_EXPAND_SZ) {
        return None;
    }
    let units = (required as usize / 2).min(buf.len());
    Some(wide_slice_to_string(&buf[..units]))
}

fn wide_slice_to_string(slice: &[u16]) -> String {
    let end = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    String::from_utf16_lossy(&slice[..end])
}

#[cfg(test)]
mod tests {
    use super::{round_i32_to_top_16, round_i32_to_top_24, wide_slice_to_string};

    #[test]
    fn wide_slice_to_string_handles_unterminated_and_embedded_nul_buffers() {
        let unterminated: Vec<u16> = "Gungnir2".encode_utf16().collect();
        assert_eq!(wide_slice_to_string(&unterminated), "Gungnir2");

        let mut with_nul: Vec<u16> = "Gungnir2\0junk".encode_utf16().collect();
        assert_eq!(wide_slice_to_string(&with_nul), "Gungnir2");

        with_nul.clear();
        assert_eq!(wide_slice_to_string(&with_nul), "");
    }

    #[test]
    fn reduced_depth_rounding_is_unbiased_and_saturating() {
        // Exact multiples pass through bit-identically.
        assert_eq!(round_i32_to_top_24(0x1234_5600), 0x1234_5600);
        assert_eq!(round_i32_to_top_24(-0x0001_0000), -0x0001_0000);
        assert_eq!(round_i32_to_top_16(0x1234_0000), 0x1234);
        assert_eq!(round_i32_to_top_16(-0x0001_0000), -1);

        // Halfway ties round to even (eliminating -0.5 LSB DC bias).
        assert_eq!(round_i32_to_top_24(0x1234_5680), 0x1234_5600); // even stays
        assert_eq!(round_i32_to_top_24(0x1234_5780), 0x1234_5800); // odd rounds up
        assert_eq!(round_i32_to_top_24(0x1234_5681), 0x1234_5700);
        assert_eq!(round_i32_to_top_24(-127), 0);
        assert_eq!(round_i32_to_top_24(-128), 0); // -0.5 -> 0 (even)
        assert_eq!(round_i32_to_top_24(-129), -256);

        assert_eq!(round_i32_to_top_16(0x1234_8000), 0x1234);
        assert_eq!(round_i32_to_top_16(0x1235_8000), 0x1236);
        assert_eq!(round_i32_to_top_16(0x1234_8001), 0x1235);
        assert_eq!(round_i32_to_top_16(-32767), 0);
        assert_eq!(round_i32_to_top_16(-32769), -1);

        // Full-scale saturation never wraps around.
        assert_eq!(round_i32_to_top_24(i32::MAX), 0x7FFF_FF00);
        assert_eq!(round_i32_to_top_24(i32::MIN), i32::MIN);
        assert_eq!(round_i32_to_top_16(i32::MAX), i16::MAX);
        assert_eq!(round_i32_to_top_16(i32::MIN), i16::MIN);
    }
}
