//! Audirvana-style Direct Windows Kernel Streaming (`WDM-KS`) WaveRT backend.
//!
//! Communicates directly with `usbaudio2.sys` (or any WDM-KS WaveRT audio driver) via:
//! `SetupDiGetClassDevsW` -> `CreateFileW` -> `KsCreatePin` (`ksuser.dll`) ->
//! `KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION` ->
//! `KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT` -> `KSSTATE_RUN`.
//!
//! Bypasses BOTH `audiodg.exe` (Windows audio mixer) AND `AudioSes.dll` (WASAPI).
//! Windows Shared Mode is intentionally unimplemented and impossible to fall back to.

use core::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering, fence};
#[cfg(windows)]
use std::sync::OnceLock;
use std::sync::Arc;
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

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
    IOCTL_KS_PROPERTY, KSAUDIO_POSITION, KSCATEGORY_AUDIO, KSDATAFORMAT, KSDATAFORMAT_0,
    KSDATAFORMAT_SPECIFIER_WAVEFORMATEX, KSDATAFORMAT_SUBTYPE_PCM, KSDATAFORMAT_TYPE_AUDIO,
    KSIDENTIFIER, KSIDENTIFIER_0, KSIDENTIFIER_0_0, KSINTERFACE_STANDARD_LOOPED_STREAMING,
    KSINTERFACE_STANDARD_STREAMING, KSINTERFACESETID_Standard, KSMEDIUM_STANDARD_DEVIO,
    KSMEDIUMSETID_Standard, KSMULTIPLE_ITEM, KSP_PIN, KSP_PIN_0, KSPIN_CINSTANCES,
    KSPIN_COMMUNICATION_BOTH, KSPIN_COMMUNICATION_SINK, KSPIN_CONNECT, KSPIN_DATAFLOW_IN,
    KSPRIORITY, KSPRIORITY_NORMAL, KSPROPERTY_AUDIO_POSITION, KSPROPERTY_CONNECTION_STATE,
    KSPROPERTY_PIN_CINSTANCES, KSPROPERTY_PIN_COMMUNICATION, KSPROPERTY_PIN_CTYPES,
    KSPROPERTY_PIN_DATAFLOW, KSPROPERTY_PIN_INTERFACES,
    KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION, KSPROPERTY_RTAUDIO_POSITIONREGISTER,
    KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT,
    KSPROPERTY_RTAUDIO_UNREGISTER_NOTIFICATION_EVENT, KSPROPERTY_TYPE_GET, KSPROPERTY_TYPE_SET,
    KSPROPSETID_Audio, KSPROPSETID_Connection, KSPROPSETID_Pin, KSPROPSETID_RtAudio,
    KSRTAUDIO_BUFFER, KSRTAUDIO_BUFFER_PROPERTY_WITH_NOTIFICATION, KSRTAUDIO_HWREGISTER,
    KSRTAUDIO_HWREGISTER_PROPERTY, KSRTAUDIO_NOTIFICATION_EVENT_PROPERTY, KSSTATE_ACQUIRE,
    KSSTATE_PAUSE, KSSTATE_RUN, KSSTATE_STOP, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT,
    WAVE_FORMAT_EXTENSIBLE,
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
struct KsPinConnectLayout {
    connect: KSPIN_CONNECT,
    data_format: KSDATAFORMAT,
    wave_format: WAVEFORMATEXTENSIBLE,
}

#[cfg(windows)]
const KS_PIN_CONNECT_BLOB_SIZE: usize = core::mem::size_of::<KsPinConnectLayout>();

/// Aligned, fully initialized byte representation of `KSPIN_CONNECT` + `KSDATAFORMAT_WAVEFORMATEXTENSIBLE`.
///
/// Storing the ABI blob as an aligned byte array guarantees every padding byte (such as
/// bytes 52..56 after `KSPIN_CONNECT.PinId` on x64) stays initialized across typed moves,
/// making [`KsPinConnectBlob::as_bytes`] 100% sound without `unsafe`.
#[cfg(windows)]
#[repr(C, align(8))]
pub struct KsPinConnectBlob {
    bytes: [u8; KS_PIN_CONNECT_BLOB_SIZE],
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
        const _: () = assert!(
            core::mem::align_of::<KsPinConnectBlob>() >= core::mem::align_of::<KsPinConnectLayout>()
        );
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

        // Start with all bytes explicitly zeroed in the byte array; write only initialized
        // fields so implicit struct padding bytes remain initialized 0u8 even after moves.
        let mut blob = Self {
            bytes: [0u8; KS_PIN_CONNECT_BLOB_SIZE],
        };
        let ptr = blob.bytes.as_mut_ptr().cast::<KsPinConnectLayout>();
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
        }
        blob
    }

    #[inline]
    fn layout(&self) -> &KsPinConnectLayout {
        unsafe { &*self.bytes.as_ptr().cast::<KsPinConnectLayout>() }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_connect_ptr(&self) -> *const KSPIN_CONNECT {
        self.bytes.as_ptr().cast::<KSPIN_CONNECT>()
    }

    pub fn format_size(&self) -> u32 {
        unsafe { self.layout().data_format.Anonymous.FormatSize }
    }

    pub fn block_align(&self) -> u16 {
        let f = self.layout().wave_format.Format;
        f.nBlockAlign
    }

    pub fn bits_per_sample(&self) -> u16 {
        let f = self.layout().wave_format.Format;
        f.wBitsPerSample
    }

    pub fn valid_bits_per_sample(&self) -> u16 {
        unsafe { self.layout().wave_format.Samples.wValidBitsPerSample }
    }

    pub fn avg_bytes_per_sec(&self) -> u32 {
        let f = self.layout().wave_format.Format;
        f.nAvgBytesPerSec
    }

    pub fn channel_mask(&self) -> u32 {
        let mask = self.layout().wave_format.dwChannelMask;
        mask
    }
}

/// Stereo-decorrelated TPDF dither state for reduced-depth wire requantization (`Int24*`, `Int16`).
///
/// Stays completely inactive (`active == false`) as long as all incoming MSB-aligned `i32`
/// samples already have zero discarded lower bits (preserving bit-perfect pass-through for
/// already-quantized 24-bit or 16-bit DSP streams without double-dithering). Latches active
/// as soon as any sample carries non-zero sub-LSB precision, applying continuous unbiased
/// 1-LSB TPDF dither before rounding and clamping.
#[derive(Debug, Clone)]
struct WireDitherState {
    prng_l: u64,
    prng_r: u64,
    active: bool,
}

impl Default for WireDitherState {
    fn default() -> Self {
        Self {
            prng_l: 0x9E37_79B9_7F4A_7C15,
            prng_r: 0xD1B5_4A32_D192_ED03,
            active: false,
        }
    }
}

impl WireDitherState {
    #[inline]
    fn next_u32(state: &mut u64) -> u32 {
        // Fast SplitMix64 step (zero heap allocation, full 64-bit period, statistically clean).
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 32) as u32
    }

    /// Generate a 1-LSB TPDF sample in `[-1.0, +1.0]` (in units of target LSB) for channel `ch`.
    #[inline]
    fn next_tpdf_lsb(&mut self, ch: usize) -> f64 {
        const INV_2_32: f64 = 1.0 / 4_294_967_296.0;
        let st = if ch == 0 {
            &mut self.prng_l
        } else {
            &mut self.prng_r
        };
        let u1 = (Self::next_u32(st) as f64) * INV_2_32;
        let u2 = (Self::next_u32(st) as f64) * INV_2_32;
        u1 - u2
    }

    #[inline]
    fn quantize_to_top_24(&mut self, sample: i32, ch: usize) -> i32 {
        if !self.active {
            if (sample & 0xFF) == 0 {
                return sample;
            }
            self.active = true;
        }
        let dither = self.next_tpdf_lsb(ch);
        let scaled = (sample as f64) / 256.0 + dither;
        let q = scaled.round() as i64;
        (q.clamp(-0x80_0000, 0x7F_FFFF) as i32) << 8
    }

    #[inline]
    fn quantize_to_top_16(&mut self, sample: i32, ch: usize) -> i16 {
        if !self.active {
            if (sample & 0xFFFF) == 0 {
                return (sample >> 16) as i16;
            }
            self.active = true;
        }
        let dither = self.next_tpdf_lsb(ch);
        let scaled = (sample as f64) / 65536.0 + dither;
        let q = scaled.round() as i64;
        q.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
    }

    /// Dither and quantize an `f64` sample (on the full signed `i32` scale, e.g., after applying
    /// a stop fade-out gain ramp) directly into top-24-bit MSB-aligned `i32` in a single stage.
    #[inline]
    fn quantize_f64_to_top_24(&mut self, sample_i32_scale: f64, ch: usize) -> i32 {
        self.active = true;
        let dither = self.next_tpdf_lsb(ch);
        let q = (sample_i32_scale / 256.0 + dither).round() as i64;
        (q.clamp(-0x80_0000, 0x7F_FFFF) as i32) << 8
    }

    /// Dither and quantize an `f64` sample (on the full signed `i32` scale, e.g., after applying
    /// a stop fade-out gain ramp) directly into signed `i16` in a single stage.
    #[inline]
    fn quantize_f64_to_top_16(&mut self, sample_i32_scale: f64, ch: usize) -> i16 {
        self.active = true;
        let dither = self.next_tpdf_lsb(ch);
        let q = (sample_i32_scale / 65536.0 + dither).round() as i64;
        q.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
    }
}

/// Shared frame encoder used by both `WaveRtFramePump` and `StreamingWaveRtPump`.
#[inline]
fn encode_frames_into(
    dst: &mut [u8],
    frames: &[[i32; 2]],
    wire_format: DacWireFormat,
    dither: &mut WireDitherState,
) -> usize {
    let bpf = wire_format.bytes_per_frame();
    let mut offset = 0;
    for &[l, r] in frames {
        encode_frame(&mut dst[offset..offset + bpf], l, r, wire_format, dither);
        offset += bpf;
    }
    offset
}

/// Apply a linear stop fade-out ramp (`1.0 -> 0.0`) directly to MSB-aligned `i32` source samples
/// *before* wire encoding so reduced-depth `Int24*` and `Int16` formats run TPDF dither once
/// after gain attenuation rather than re-rounding already-quantized wire bytes.
fn encode_fade_out_frames_into(
    dst: &mut [u8],
    frames: &[[i32; 2]],
    wire_format: DacWireFormat,
    dither: &mut WireDitherState,
) -> usize {
    let total = frames.len();
    if total == 0 {
        return 0;
    }
    let bpf = wire_format.bytes_per_frame();
    let mut offset = 0;
    for (i, &[l, r]) in frames.iter().enumerate() {
        let gain = 1.0 - (i as f64) / (total as f64);
        let lf = f64::from(l) * gain;
        let rf = f64::from(r) * gain;
        let slot = &mut dst[offset..offset + bpf];
        match wire_format {
            DacWireFormat::Int32 => {
                let l32 = (lf.round() as i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                let r32 = (rf.round() as i64).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                slot[0..4].copy_from_slice(&l32.to_le_bytes());
                slot[4..8].copy_from_slice(&r32.to_le_bytes());
            }
            DacWireFormat::Int24In32 => {
                let l24 = dither.quantize_f64_to_top_24(lf, 0);
                let r24 = dither.quantize_f64_to_top_24(rf, 1);
                slot[0..4].copy_from_slice(&l24.to_le_bytes());
                slot[4..8].copy_from_slice(&r24.to_le_bytes());
            }
            DacWireFormat::Int24Packed => {
                let lb = dither.quantize_f64_to_top_24(lf, 0).to_le_bytes();
                let rb = dither.quantize_f64_to_top_24(rf, 1).to_le_bytes();
                slot[0..3].copy_from_slice(&lb[1..4]);
                slot[3..6].copy_from_slice(&rb[1..4]);
            }
            DacWireFormat::Int16 => {
                let l16 = dither.quantize_f64_to_top_16(lf, 0);
                let r16 = dither.quantize_f64_to_top_16(rf, 1);
                slot[0..2].copy_from_slice(&l16.to_le_bytes());
                slot[2..4].copy_from_slice(&r16.to_le_bytes());
            }
        }
        offset += bpf;
    }
    offset
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
    dither: WireDitherState,
}

impl<'a> WaveRtFramePump<'a> {
    pub fn new(frames: &'a [[i32; 2]], wire_format: DacWireFormat) -> Self {
        Self {
            frames,
            cursor: 0,
            wire_format,
            dither: WireDitherState::default(),
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

        let offset = encode_frames_into(
            dst,
            &self.frames[self.cursor..self.cursor + to_write],
            self.wire_format,
            &mut self.dither,
        );
        self.cursor += to_write;

        if offset < dst.len() {
            dst[offset..].fill(0);
        }
        to_write
    }

    /// Fill `dst` with a smooth linear fade-out to silence so mid-track stops are click-free.
    ///
    /// Applies the linear gain ramp to the MSB-aligned `i32` source frames *before* reduced-depth
    /// encoding so `Int24*` and `Int16` formats run TPDF dither once after attenuation.
    pub fn fill_fade_out_slice(&mut self, dst: &mut [u8]) -> usize {
        let bpf = self.bytes_per_frame();
        let max_frames = dst.len() / bpf;
        let remaining = self.frames.len().saturating_sub(self.cursor);
        let to_write = max_frames.min(remaining);

        let offset = encode_fade_out_frames_into(
            dst,
            &self.frames[self.cursor..self.cursor + to_write],
            self.wire_format,
            &mut self.dither,
        );
        self.cursor += to_write;

        if offset < dst.len() {
            dst[offset..].fill(0);
        }
        to_write
    }
}

/// Non-blocking result returned by a streaming PCM block source for WaveRT playback.
#[derive(Debug)]
pub enum StreamBlock {
    /// A ready block of PCM frames at the pin's sample rate.
    Ready(InterleavedInt32Buffer),
    /// Producer temporarily has no block ready (underrun) — output silence without ending the track.
    Underrun,
    /// Clean end-of-track (or playback epoch closed).
    EndOfTrack,
}

impl From<Option<InterleavedInt32Buffer>> for StreamBlock {
    fn from(opt: Option<InterleavedInt32Buffer>) -> Self {
        match opt {
            Some(buf) => Self::Ready(buf),
            None => Self::EndOfTrack,
        }
    }
}

/// Internal chunk source abstraction shared by `play_full_memory` and `play_block_stream`.
pub trait WaveRtPumpSource {
    fn wire_format(&self) -> DacWireFormat;

    fn bytes_per_frame(&self) -> usize {
        self.wire_format().bytes_per_frame()
    }

    fn frames_written(&self) -> usize;
    fn is_eof(&self) -> bool;
    fn fill_slice(&mut self, dst: &mut [u8]) -> usize;

    fn fill_fade_out_slice(&mut self, dst: &mut [u8]) -> usize;

    fn take_error(&mut self) -> Option<OutputError> {
        None
    }
}

impl WaveRtPumpSource for WaveRtFramePump<'_> {
    fn wire_format(&self) -> DacWireFormat {
        self.wire_format
    }

    fn frames_written(&self) -> usize {
        WaveRtFramePump::frames_written(self)
    }

    fn is_eof(&self) -> bool {
        WaveRtFramePump::is_exhausted(self)
    }

    fn fill_slice(&mut self, dst: &mut [u8]) -> usize {
        WaveRtFramePump::fill_slice(self, dst)
    }

    fn fill_fade_out_slice(&mut self, dst: &mut [u8]) -> usize {
        WaveRtFramePump::fill_fade_out_slice(self, dst)
    }
}

/// Streaming WaveRT ring-buffer pump that pulls bounded `InterleavedInt32Buffer` blocks
/// lazily across block boundaries with zero gaps, non-blocking underrun silence, and
/// click-free fade-out on stop.
pub struct StreamingWaveRtPump<F, B = StreamBlock>
where
    F: FnMut() -> B,
    B: Into<StreamBlock>,
{
    next_block: F,
    current: Option<InterleavedInt32Buffer>,
    cursor: usize,
    expected_sample_rate_hz: u32,
    wire_format: DacWireFormat,
    dither: WireDitherState,
    total_frames_written: usize,
    eof: bool,
    pending_error: Option<OutputError>,
    _marker: PhantomData<fn() -> B>,
}

impl<F, B> StreamingWaveRtPump<F, B>
where
    F: FnMut() -> B,
    B: Into<StreamBlock>,
{
    pub fn new(expected_sample_rate_hz: u32, wire_format: DacWireFormat, next_block: F) -> Self {
        Self {
            next_block,
            current: None,
            cursor: 0,
            expected_sample_rate_hz,
            wire_format,
            dither: WireDitherState::default(),
            total_frames_written: 0,
            eof: false,
            pending_error: None,
            _marker: PhantomData,
        }
    }

    fn ensure_current(&mut self) -> bool {
        while !self.eof {
            if let Some(buf) = &self.current {
                if self.cursor < buf.frames.len() {
                    return true;
                }
            }
            match (self.next_block)().into() {
                StreamBlock::Ready(buf) if !buf.frames.is_empty() => {
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
                StreamBlock::Ready(_) => continue, // skip empty block
                StreamBlock::Underrun => {
                    // Temporary non-blocking underrun: emit silence for this half without
                    // marking EOF so playback recovers seamlessly once the producer catches up.
                    self.current = None;
                    self.cursor = 0;
                    return false;
                }
                StreamBlock::EndOfTrack => {
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

impl<F, B> WaveRtPumpSource for StreamingWaveRtPump<F, B>
where
    F: FnMut() -> B,
    B: Into<StreamBlock>,
{
    fn wire_format(&self) -> DacWireFormat {
        self.wire_format
    }

    fn frames_written(&self) -> usize {
        self.total_frames_written
    }

    fn is_eof(&self) -> bool {
        self.eof
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

            let dst_off = written_total * bpf;
            encode_frames_into(
                &mut dst[dst_off..dst_off + to_copy * bpf],
                &buf.frames[self.cursor..self.cursor + to_copy],
                self.wire_format,
                &mut self.dither,
            );
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
        let bpf = self.bytes_per_frame();
        let max_frames = dst.len() / bpf;
        let mut raw_frames: Vec<[i32; 2]> = Vec::with_capacity(max_frames);

        while raw_frames.len() < max_frames {
            if !self.ensure_current() {
                break;
            }
            let buf = self.current.as_ref().unwrap();
            let remaining = buf.frames.len() - self.cursor;
            let to_copy = (max_frames - raw_frames.len()).min(remaining);
            raw_frames.extend_from_slice(&buf.frames[self.cursor..self.cursor + to_copy]);
            self.cursor += to_copy;
            self.total_frames_written += to_copy;
        }

        let offset = encode_fade_out_frames_into(
            dst,
            &raw_frames,
            self.wire_format,
            &mut self.dither,
        );
        if offset < dst.len() {
            dst[offset..].fill(0);
        }
        raw_frames.len()
    }

    fn take_error(&mut self) -> Option<OutputError> {
        self.pending_error.take()
    }
}

/// Frame-aligned two-half layout for a cyclic WaveRT DMA buffer.
///
/// Guarantees both halves (`[0..half_len]` and `[half_len..buf_len]`) start and end on exact
/// frame boundaries and cover 100% of `[0..buf_len)` with zero unwritten gap bytes, even when a
/// driver returns an odd number of frames. Rejects sizes that are not a multiple of `bpf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaveRtRingLayout {
    buf_len: usize,
    half_len: usize,
    bpf: usize,
}

impl WaveRtRingLayout {
    pub fn new(buf_len: usize, bpf: usize) -> Result<Self, OutputError> {
        if bpf == 0 || buf_len < bpf * 2 || buf_len % bpf != 0 {
            return Err(OutputError::Win32 {
                operation: "KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION",
                code: 0,
                detail: format!(
                    "driver returned non-frame-aligned or too-small WaveRT buffer (size={buf_len}, bytes_per_frame={bpf})"
                ),
            });
        }
        let half_len = (buf_len / 2) / bpf * bpf;
        if half_len == 0 || buf_len - half_len < bpf {
            return Err(OutputError::Win32 {
                operation: "KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION",
                code: 0,
                detail: format!(
                    "driver returned degenerate WaveRT half size (size={buf_len}, half_len={half_len}, bpf={bpf})"
                ),
            });
        }
        Ok(Self {
            buf_len,
            half_len,
            bpf,
        })
    }

    pub fn buf_len(&self) -> usize {
        self.buf_len
    }

    pub fn half_len(&self) -> usize {
        self.half_len
    }

    pub fn bytes_per_frame(&self) -> usize {
        self.bpf
    }

    pub fn half_range(&self, half_idx: usize) -> core::ops::Range<usize> {
        if half_idx == 0 {
            0..self.half_len
        } else {
            self.half_len..self.buf_len
        }
    }

    /// Given the hardware DMA play cursor byte offset, return the half index that DMA is
    /// NOT currently reading (safe to overwrite without tearing or in-phase ticking).
    pub fn safe_write_half_for_play_byte(&self, play_offset_bytes: u64) -> usize {
        let pos = (play_offset_bytes % (self.buf_len as u64)) as usize;
        if pos < self.half_len {
            1 // DMA is reading half 0 -> safely write half 1
        } else {
            0 // DMA is reading half 1 -> safely write half 0
        }
    }
    /// Returns true if `play_offset_bytes` lies strictly inside the "override zone" of
    /// `toggle_half` (i.e. inside `toggle_half` and at least `guard` bytes before the boundary
    /// into `toggle_half ^ 1`), meaning DMA is genuinely active inside `toggle_half` rather
    /// than merely lagging slightly behind the expected half boundary.
    pub fn is_clearly_inside_half(&self, half_idx: usize, play_offset_bytes: u64) -> bool {
        let pos = (play_offset_bytes % (self.buf_len as u64)) as usize;
        if half_idx == 0 {
            let guard = (self.half_len / 4).max(self.bpf);
            let limit = self.half_len.saturating_sub(guard);
            pos < limit
        } else {
            let half1_len = self.buf_len - self.half_len;
            let guard = (half1_len / 4).max(self.bpf);
            let limit = self.buf_len.saturating_sub(guard);
            pos >= self.half_len && pos < limit
        }
    }
}

/// Outcome of processing one WaveRT hardware notification event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveRtStepOutcome {
    Continue,
    Finished,
}

/// Whether the hardware DMA position cursor wraps modulo `buf_len` or increases monotonically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionWrapMode {
    /// Auto-detect wrapping if a position reading ever decreases within `0..buf_len`.
    Auto,
    /// Known to wrap modulo `buf_len` (e.g., `KSPROPERTY_RTAUDIO_POSITIONREGISTER`).
    Modular,
    /// Known to be a monotonically increasing cumulative byte counter.
    Monotonic,
}

/// Locks onto a single `KSAUDIO_POSITION` cursor (`WriteOffset` vs `PlayOffset`) on the first
/// non-zero sample so modulo-`buf_len` wraps to `0` never mix two cursors with different latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPositionCursorSelector {
    mode: AudioPositionCursorMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AudioPositionCursorMode {
    Undecided,
    WriteOffset,
    PlayOffset,
}

impl Default for AudioPositionCursorSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPositionCursorSelector {
    pub fn new() -> Self {
        Self {
            mode: AudioPositionCursorMode::Undecided,
        }
    }

    /// Select a consistent byte cursor from `(play_offset, write_offset)`.
    ///
    /// Once `write_offset > 0` is observed, `WriteOffset` is locked for the lifetime of the stream
    /// (including legitimate `0` readings on modulo wrap). If only `play_offset > 0` is observed,
    /// `PlayOffset` is locked for the lifetime of the stream.
    pub fn select(&mut self, play_offset: u64, write_offset: u64) -> u64 {
        match self.mode {
            AudioPositionCursorMode::Undecided => {
                if write_offset > 0 {
                    self.mode = AudioPositionCursorMode::WriteOffset;
                    write_offset
                } else if play_offset > 0 {
                    self.mode = AudioPositionCursorMode::PlayOffset;
                    play_offset
                } else {
                    0
                }
            }
            AudioPositionCursorMode::WriteOffset => write_offset,
            AudioPositionCursorMode::PlayOffset => play_offset,
        }
    }

    pub fn is_write_offset(&self) -> bool {
        self.mode == AudioPositionCursorMode::WriteOffset
    }
}

/// Pure, hardware-independent WaveRT cyclic half-buffer state machine.
///
/// Manages:
/// - Position-authoritative, lag-compensated half-boundary tracking with a 1/4-half pre-boundary
///   guard band and wall-clock (`Instant`) sanity checks. Late-but-not-coalesced wakes never
///   trigger false coalescing or invert half phase, genuine coalesced wakes resynchronize on
///   every wake (self-healing), and `Unavailable` position sources fall back to wall-clock
///   boundary tracking when timing is provided.
/// - Stop fade-out that waits for the fade half to finish playing (`trailing_silent_halves >= 2`).
/// - Non-blocking underrun handling (emits digital silence without ending playback).
/// - Accurate `frames_played` counting only real audio frames whose half completed DMA playback.
pub struct WaveRtLoopController {
    layout: WaveRtRingLayout,
    next_expected_half: usize,
    last_written_half: Option<usize>,
    last_raw_pos: Option<u64>,
    last_unwrapped_pos: Option<u64>,
    last_boundary_idx: u64,
    lag_bytes: u64,
    wrap_mode: PositionWrapMode,
    observed_wrap: bool,
    half_duration: Option<Duration>,
    last_boundary_instant: Option<Duration>,
    half_audio_frames: [usize; 2],
    completed_audio_frames: usize,
    trailing_silent_halves: usize,
    stopping_fade_done: bool,
}

impl WaveRtLoopController {
    pub fn new(layout: WaveRtRingLayout) -> Self {
        Self {
            layout,
            next_expected_half: 0,
            last_written_half: None,
            last_raw_pos: None,
            last_unwrapped_pos: None,
            last_boundary_idx: 0,
            lag_bytes: 0,
            wrap_mode: PositionWrapMode::Auto,
            observed_wrap: false,
            half_duration: None,
            last_boundary_instant: None,
            half_audio_frames: [0, 0],
            completed_audio_frames: 0,
            trailing_silent_halves: 0,
            stopping_fade_done: false,
        }
    }

    /// Configure the nominal duration of one half-buffer (for wall-clock wrap disambiguation and
    /// `Unavailable` position-reader coalescing recovery).
    pub fn with_half_duration(mut self, half_duration: Duration) -> Self {
        if !half_duration.is_zero() {
            self.half_duration = Some(half_duration);
        }
        self
    }

    /// Configure whether the hardware position cursor wraps modulo `buf_len` or is monotonic.
    pub fn with_wrap_mode(mut self, wrap_mode: PositionWrapMode) -> Self {
        self.wrap_mode = wrap_mode;
        self
    }

    pub fn layout(&self) -> WaveRtRingLayout {
        self.layout
    }

    /// Number of real audio frames whose DMA half has finished playing to the DAC.
    pub fn frames_played(&self) -> usize {
        self.completed_audio_frames
    }

    /// Pre-fill both cyclic halves before transitioning the KS pin to `KSSTATE_RUN`.
    pub fn prefill(
        &mut self,
        cyclic_slice: &mut [u8],
        pump: &mut dyn WaveRtPumpSource,
    ) -> Result<(), OutputError> {
        let r0 = self.layout.half_range(0);
        let w0 = pump.fill_slice(&mut cyclic_slice[r0]);
        if let Some(err) = pump.take_error() {
            return Err(err);
        }
        let r1 = self.layout.half_range(1);
        let w1 = pump.fill_slice(&mut cyclic_slice[r1]);
        if let Some(err) = pump.take_error() {
            return Err(err);
        }
        self.half_audio_frames = [w0, w1];
        self.last_written_half = Some(1);
        self.last_raw_pos = None;
        self.last_unwrapped_pos = None;
        self.last_boundary_idx = 0;
        self.lag_bytes = 0;
        self.observed_wrap = false;
        self.last_boundary_instant = Some(Duration::ZERO);
        Ok(())
    }

    fn boundary_idx_for_byte(&self, unwrapped_byte: u64) -> u64 {
        let buf = self.layout.buf_len() as u64;
        let half = self.layout.half_len() as u64;
        let rings = unwrapped_byte / buf;
        let rem = unwrapped_byte % buf;
        rings
            .saturating_mul(2)
            .saturating_add(u64::from(rem >= half))
    }

    fn boundary_byte(&self, boundary_idx: u64) -> u64 {
        let buf = self.layout.buf_len() as u64;
        let half = self.layout.half_len() as u64;
        (boundary_idx / 2)
            .saturating_mul(buf)
            .saturating_add((boundary_idx % 2) * half)
    }

    fn advance_boundary_clock(
        &mut self,
        now_since_run: Option<Duration>,
        halves_advanced: u64,
    ) {
        let (Some(now), Some(half_dur)) = (now_since_run, self.half_duration) else {
            return;
        };
        let prev_t = self.last_boundary_instant.unwrap_or(Duration::ZERO);
        let step = half_dur.saturating_mul(halves_advanced.min(u32::MAX as u64) as u32);
        let ideal = prev_t.saturating_add(step);
        let new_t = if now < ideal {
            // Wake arrived slightly before estimated boundary (crystal faster than QPC or jitter).
            now
        } else {
            let overshoot = now.saturating_sub(ideal);
            if overshoot <= half_dur / 4 {
                // On-time wake: gently PLL-track crystal drift without reacting to late spikes.
                ideal.saturating_add(overshoot / 8)
            } else {
                // Late wake: keep the nominal boundary reference intact so the next on-time wake
                // sees the true inter-boundary elapsed interval.
                ideal
            }
        };
        self.last_boundary_instant = Some(new_t);
    }

    fn step_wall_clock_fallback(&mut self, now_since_run: Option<Duration>) -> (usize, bool) {
        let halves_elapsed = match (now_since_run, self.half_duration) {
            (Some(now), Some(half_dur)) => {
                let prev_t = self.last_boundary_instant.unwrap_or(Duration::ZERO);
                let since = now.saturating_sub(prev_t);
                let two_halves = half_dur.saturating_mul(2);
                if since >= two_halves {
                    ((since.as_nanos() / half_dur.as_nanos()) as u64).max(2)
                } else {
                    1
                }
            }
            _ => 1,
        };
        self.advance_boundary_clock(now_since_run, halves_elapsed);
        self.last_boundary_idx = self.last_boundary_idx.saturating_add(halves_elapsed);
        let write_half = ((self.last_boundary_idx as usize) & 1) ^ 1;
        (write_half, halves_elapsed >= 2)
    }

    /// Decide which half to write and whether at least one notification was coalesced.
    ///
    /// Tracks unwrapped, lag-compensated DMA byte position and counts actual half-boundary
    /// crossings rather than rounding position deltas. A late-but-not-coalesced wake (still in
    /// the expected DMA half) writes the opposite half and does NOT falsely credit unplayed
    /// frames. Coalesced wakes (whether on monotonic or modular cursors, including wake #1)
    /// self-heal immediately and keep `next_expected_half` locked to hardware DMA.
    fn select_write_half(
        &mut self,
        play_offset_bytes: Option<u64>,
        now_since_run: Option<Duration>,
    ) -> (usize, bool) {
        let Some(pos) = play_offset_bytes else {
            return self.step_wall_clock_fallback(now_since_run);
        };

        let half = self.layout.half_len() as u64;
        let buf = self.layout.buf_len() as u64;
        let guard = (self.layout.half_len() / 4).max(self.layout.bytes_per_frame()) as u64;

        let clock_since_boundary = match (now_since_run, self.half_duration) {
            (Some(now), Some(half_dur)) => {
                let prev_t = self.last_boundary_instant.unwrap_or(Duration::ZERO);
                Some((now.saturating_sub(prev_t), half_dur))
            }
            _ => None,
        };
        let clock_allows_full_ring_wrap: Option<bool> = clock_since_boundary.map(|(since, hd)| {
            // Require >= 1.5 halves since last boundary before treating a small modular delta
            // as a full-ring (2-half) wrap.
            let threshold = hd.saturating_add(hd / 2);
            since >= threshold
        });

        let unwrapped: u64 = match (self.last_raw_pos, self.last_unwrapped_pos) {
            (None, _) => {
                // First wake after KSSTATE_RUN (DMA started at byte 0).
                if pos < half {
                    let is_coalesced_modular_first_wake = match clock_allows_full_ring_wrap {
                        Some(true) => true,
                        Some(false) => false,
                        None => {
                            self.wrap_mode == PositionWrapMode::Modular
                                && pos < half.saturating_sub(guard)
                        }
                    };
                    if is_coalesced_modular_first_wake {
                        self.lag_bytes = 0;
                        self.observed_wrap = true;
                        buf.saturating_add(pos)
                    } else {
                        // Normal first wake with a lagging PlayOffset cursor (e.g. usbaudio2.sys).
                        self.lag_bytes = half - pos;
                        pos
                    }
                } else {
                    self.lag_bytes = 0;
                    pos
                }
            }
            (Some(prev_raw), Some(prev_unwrapped)) => {
                // Stuck-at-zero cursor guard: degrade cleanly to wall-clock / toggle fallback.
                if prev_raw == 0 && pos == 0 && self.lag_bytes == half {
                    self.last_raw_pos = Some(pos);
                    return self.step_wall_clock_fallback(now_since_run);
                }
                let is_modular = self.wrap_mode == PositionWrapMode::Modular || self.observed_wrap;
                if pos >= prev_raw {
                    let raw_delta = pos - prev_raw;
                    let is_full_ring_wrap = prev_raw < buf
                        && pos < buf
                        && raw_delta < half / 2
                        && self.wrap_mode != PositionWrapMode::Monotonic
                        && match clock_allows_full_ring_wrap {
                            Some(true) => is_modular || self.lag_bytes == 0,
                            Some(false) => false,
                            None => {
                                is_modular
                                    && self.lag_bytes == 0
                                    && self.layout.is_clearly_inside_half(
                                        self.next_expected_half,
                                        pos,
                                    )
                            }
                        };
                    if is_full_ring_wrap {
                        let extra_rings = match clock_since_boundary {
                            Some((since, hd)) => {
                                let h = (since.as_nanos() / hd.as_nanos()) as u64;
                                (h / 2).max(1)
                            }
                            None => 1,
                        };
                        self.observed_wrap = true;
                        prev_unwrapped
                            .saturating_add(raw_delta)
                            .saturating_add(extra_rings.saturating_mul(buf))
                    } else {
                        prev_unwrapped.saturating_add(raw_delta)
                    }
                } else if prev_raw < buf && pos < buf && self.wrap_mode != PositionWrapMode::Monotonic
                {
                    // Modular wrap around buf_len.
                    self.observed_wrap = true;
                    let ring_delta = (buf - prev_raw).saturating_add(pos);
                    let extra_rings = match clock_since_boundary {
                        Some((since, hd)) if since >= hd.saturating_mul(3) => {
                            let h = (since.as_nanos() / hd.as_nanos()) as u64;
                            let base_halves = if ring_delta >= half { 1 } else { 0 };
                            h.saturating_sub(base_halves) / 2
                        }
                        _ => 0,
                    };
                    prev_unwrapped
                        .saturating_add(ring_delta)
                        .saturating_add(extra_rings.saturating_mul(buf))
                } else {
                    // 32-bit counter wrap or non-monotonic quirk: step 1 half cleanly.
                    self.boundary_byte(self.last_boundary_idx.saturating_add(1))
                        .saturating_sub(self.lag_bytes)
                }
            }
            _ => pos,
        };

        let adj_pos = unwrapped.saturating_add(self.lag_bytes);
        let raw_bidx = self.boundary_idx_for_byte(adj_pos);
        let next_expected_bidx = self.last_boundary_idx.saturating_add(1);
        let next_expected_byte = self.boundary_byte(next_expected_bidx);

        let (new_bidx, coalesced) = if raw_bidx >= next_expected_bidx.saturating_add(1) {
            // Crossed >= 2 half boundaries: genuine coalescing; resync directly to DMA half.
            (raw_bidx, true)
        } else if raw_bidx == next_expected_bidx {
            // Exactly 1 half boundary crossed (including late-but-not-coalesced wakes).
            (raw_bidx, false)
        } else {
            // Position is still short of `next_expected_byte`.
            let shortfall = next_expected_byte.saturating_sub(adj_pos);
            if shortfall <= guard {
                // Inside pre-boundary jitter guard band: trust expected half boundary.
                (next_expected_bidx, false)
            } else {
                // Clearly inside `raw_bidx`'s half (outside guard band): self-heal by writing
                // the half opposite DMA rather than overwriting the active DMA half.
                (raw_bidx, false)
            }
        };

        let halves_advanced = new_bidx.saturating_sub(self.last_boundary_idx).max(1);
        self.advance_boundary_clock(now_since_run, halves_advanced);
        self.last_raw_pos = Some(pos);
        self.last_unwrapped_pos = Some(unwrapped);
        self.last_boundary_idx = new_bidx;

        let write_half = ((new_bidx as usize) & 1) ^ 1;
        (write_half, coalesced)
    }

    /// Process one WaveRT half-buffer completion notification without wall-clock timing.
    ///
    /// Returns the half index that was written alongside `Continue` / `Finished`.
    pub fn on_notification(
        &mut self,
        cyclic_slice: &mut [u8],
        pump: &mut dyn WaveRtPumpSource,
        stop_requested: bool,
        play_offset_bytes: Option<u64>,
    ) -> Result<(usize, WaveRtStepOutcome), OutputError> {
        self.on_notification_at(cyclic_slice, pump, stop_requested, play_offset_bytes, None)
    }

    /// Process one WaveRT half-buffer completion notification with monotonic wall-clock time
    /// elapsed since `KSSTATE_RUN`.
    pub fn on_notification_at(
        &mut self,
        cyclic_slice: &mut [u8],
        pump: &mut dyn WaveRtPumpSource,
        stop_requested: bool,
        play_offset_bytes: Option<u64>,
        now_since_run: Option<Duration>,
    ) -> Result<(usize, WaveRtStepOutcome), OutputError> {
        let (write_half, coalesced) = self.select_write_half(play_offset_bytes, now_since_run);
        self.next_expected_half = write_half ^ 1;

        if coalesced {
            // Coalesced notification (>= 2 boundaries crossed): both halves completed DMA.
            self.completed_audio_frames += self.half_audio_frames[0] + self.half_audio_frames[1];
            self.half_audio_frames = [0, 0];
        } else {
            self.completed_audio_frames += self.half_audio_frames[write_half];
            self.half_audio_frames[write_half] = 0;
        }
        self.last_written_half = Some(write_half);

        let range = self.layout.half_range(write_half);

        if stop_requested {
            if !self.stopping_fade_done {
                let written = pump.fill_fade_out_slice(&mut cyclic_slice[range]);
                self.half_audio_frames[write_half] = written;
                self.stopping_fade_done = true;
                self.trailing_silent_halves = 0;
                return Ok((write_half, WaveRtStepOutcome::Continue));
            }
            cyclic_slice[range].fill(0);
            self.half_audio_frames[write_half] = 0;
            self.trailing_silent_halves += 1;
            let outcome = if self.trailing_silent_halves >= 2 {
                WaveRtStepOutcome::Finished
            } else {
                WaveRtStepOutcome::Continue
            };
            return Ok((write_half, outcome));
        }

        let written = pump.fill_slice(&mut cyclic_slice[range]);
        self.half_audio_frames[write_half] = written;

        if let Some(err) = pump.take_error() {
            return Err(err);
        }

        if written == 0 {
            if pump.is_eof() {
                self.trailing_silent_halves += 1;
                if self.trailing_silent_halves >= 2 {
                    return Ok((write_half, WaveRtStepOutcome::Finished));
                }
            } else {
                // Non-blocking producer underrun: digital silence was written into this half;
                // stay alive until audio resumes or stop_requested becomes true.
                self.trailing_silent_halves = 0;
            }
        } else {
            self.trailing_silent_halves = 0;
        }

        Ok((write_half, WaveRtStepOutcome::Continue))
    }
}

/// Round an MSB-aligned signed 32-bit PCM sample to its top 24 bits (unbiased round-to-nearest-even)
/// with the low 8 bits guaranteed zero and positive full-scale clamped cleanly without wraparound.
/// Already-quantized 24-bit samples (low byte == 0) pass through bit-identically.
#[cfg(test)]
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
#[cfg(test)]
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
fn encode_frame(
    dst: &mut [u8],
    l: i32,
    r: i32,
    wire_format: DacWireFormat,
    dither: &mut WireDitherState,
) {
    match wire_format {
        DacWireFormat::Int32 => {
            dst[0..4].copy_from_slice(&l.to_le_bytes());
            dst[4..8].copy_from_slice(&r.to_le_bytes());
        }
        DacWireFormat::Int24In32 => {
            let l24 = dither.quantize_to_top_24(l, 0);
            let r24 = dither.quantize_to_top_24(r, 1);
            dst[0..4].copy_from_slice(&l24.to_le_bytes());
            dst[4..8].copy_from_slice(&r24.to_le_bytes());
        }
        DacWireFormat::Int24Packed => {
            let lb = dither.quantize_to_top_24(l, 0).to_le_bytes();
            let rb = dither.quantize_to_top_24(r, 1).to_le_bytes();
            dst[0..3].copy_from_slice(&lb[1..4]);
            dst[3..6].copy_from_slice(&rb[1..4]);
        }
        DacWireFormat::Int16 => {
            let l16 = dither.quantize_to_top_16(l, 0);
            let r16 = dither.quantize_to_top_16(r, 1);
            dst[0..2].copy_from_slice(&l16.to_le_bytes());
            dst[2..4].copy_from_slice(&r16.to_le_bytes());
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

    /// Re-arm the sink after a previous `stop()` so subsequent playback calls proceed.
    pub fn arm(&mut self) {
        self.stop_flag.store(false, Ordering::SeqCst);
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
    ///
    /// `next_block` may return either `StreamBlock` (recommended for non-blocking RT loops with
    /// explicit `Underrun` silence recovery) or `Option<InterleavedInt32Buffer>`.
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
        self.run_wavert_pump(sample_rate_hz, wire_format, &mut pump)
    }

    /// Empirically probe live WaveRT notification (`GET` vs `SET`) and hardware position
    /// properties (`POSITIONREGISTER` and `KSPROPERTY_AUDIO_POSITION`) on the opened DAC.
    #[cfg(windows)]
    pub fn probe_wavert_hardware_properties(
        &self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
    ) -> Result<WaveRtHardwareProbe, OutputError> {
        let filter = open_ks_filter(&self.device.device_path)?;
        let pin = create_ks_render_pin(
            filter.raw(),
            &self.device,
            sample_rate_hz,
            wire_format,
            true,
        )?;
        let bpf = wire_format.bytes_per_frame() as u32;
        let requested_bytes = ((sample_rate_hz / 10).max(256)) * bpf;
        let rt_buf = allocate_wavert_buffer_with_notification(pin.raw(), requested_bytes, 2)?;
        let layout = WaveRtRingLayout::new(rt_buf.ActualBufferSize as usize, bpf as usize)?;

        let ev_get = create_auto_reset_event()?;
        let reg_get = register_notification_event_raw(pin.raw(), ev_get.raw(), KSPROPERTY_TYPE_GET);
        let unreg_get = if reg_get.is_ok() {
            unregister_notification_event_raw(pin.raw(), ev_get.raw(), KSPROPERTY_TYPE_GET)
        } else {
            Err(OutputError::Win32 {
                operation: "UnregisterNotificationEvent(GET)",
                code: 0,
                detail: "skipped because REGISTER(GET) failed".to_string(),
            })
        };

        let ev_set = create_auto_reset_event()?;
        let reg_set = register_notification_event_raw(pin.raw(), ev_set.raw(), KSPROPERTY_TYPE_SET);
        let unreg_set = if reg_set.is_ok() {
            unregister_notification_event_raw(pin.raw(), ev_set.raw(), KSPROPERTY_TYPE_SET)
        } else {
            Err(OutputError::Win32 {
                operation: "UnregisterNotificationEvent(SET)",
                code: 0,
                detail: "skipped because REGISTER(SET) failed".to_string(),
            })
        };

        let position_register_supported = query_position_register(pin.raw()).is_some();
        let audio_position_supported = query_audio_position(pin.raw()).is_some();

        // Stream 4 silent notifications to observe live PlayOffset/WriteOffset % buf_len behavior.
        // `allocate_wavert_buffer_with_notification` already guarantees `BufferAddress` is non-null.
        let cyclic_slice = unsafe {
            core::slice::from_raw_parts_mut(rt_buf.BufferAddress.cast::<u8>(), layout.buf_len())
        };
        cyclic_slice.fill(0);
        if rt_buf.CallMemoryBarrier != 0 {
            fence(Ordering::SeqCst);
        }

        let ev_run = create_auto_reset_event()?;
        let _notif = WaveRtNotificationGuard::register(pin.raw(), ev_run.raw())?;
        let mut pos_reader = WaveRtPositionReader::open(pin.raw());
        let mut state_guard = KsPinStateGuard::new(pin.raw());
        state_guard.set_state(KSSTATE_ACQUIRE)?;
        state_guard.set_state(KSSTATE_PAUSE)?;
        state_guard.set_state(KSSTATE_RUN)?;

        let mut sample_play_offsets = Vec::with_capacity(4);
        let mut sample_write_offsets = Vec::with_capacity(4);
        for _ in 0..4 {
            let wait_res = unsafe { WaitForSingleObject(ev_run.raw(), 1500) };
            if wait_res != WAIT_OBJECT_0 {
                break;
            }
            if let Some(pos) = pos_reader.read_position_bytes() {
                sample_play_offsets.push((pos, layout.safe_write_half_for_play_byte(pos)));
            }
            if let Some(ap) = query_audio_position(pin.raw()) {
                sample_write_offsets.push((ap.PlayOffset, ap.WriteOffset));
            }
        }

        let _ = state_guard.set_state(KSSTATE_PAUSE);
        let _ = state_guard.set_state(KSSTATE_STOP);

        Ok(WaveRtHardwareProbe {
            buffer_bytes: layout.buf_len(),
            half_bytes: layout.half_len(),
            register_get_ok: reg_get.is_ok(),
            unregister_get_ok: unreg_get.is_ok(),
            register_set_ok: reg_set.is_ok(),
            unregister_set_ok: unreg_set.is_ok(),
            position_register_supported,
            audio_position_supported,
            sample_play_offsets,
            sample_write_offsets,
        })
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
        // `allocate_wavert_buffer_with_notification` verifies `BufferAddress` is non-null.
        let bpf = pump.bytes_per_frame() as u32;
        let requested_bytes = ((sample_rate_hz / 10).max(256)) * bpf;
        let rt_buf = allocate_wavert_buffer_with_notification(pin.raw(), requested_bytes, 2)?;
        let layout = WaveRtRingLayout::new(rt_buf.ActualBufferSize as usize, bpf as usize)?;
        let call_barrier = rt_buf.CallMemoryBarrier != 0;

        // 4. Create & register Win32 hardware notification event (RAII unregistered on drop).
        let event_handle = create_auto_reset_event()?;
        let _notif_guard = WaveRtNotificationGuard::register(pin.raw(), event_handle.raw())?;

        // 5. Probe hardware play-cursor reader (POSITIONREGISTER -> AUDIO_POSITION -> wall-clock fallback).
        let mut pos_reader = WaveRtPositionReader::open(pin.raw());

        // 6. Ensure pin returns to KSSTATE_STOP even if an error occurs mid-stream.
        let mut state_guard = KsPinStateGuard::new(pin.raw());

        // 7. Pre-fill BOTH frame-aligned halves before starting hardware DMA.
        let cyclic_slice = unsafe {
            core::slice::from_raw_parts_mut(rt_buf.BufferAddress.cast::<u8>(), layout.buf_len())
        };
        let half_frames = layout.half_len() / layout.bytes_per_frame();
        let half_duration =
            Duration::from_secs_f64(half_frames as f64 / f64::from(sample_rate_hz.max(1)));
        let mut controller = WaveRtLoopController::new(layout)
            .with_half_duration(half_duration)
            .with_wrap_mode(if pos_reader.is_modular() {
                PositionWrapMode::Modular
            } else {
                PositionWrapMode::Auto
            });
        controller.prefill(cyclic_slice, pump)?;
        if call_barrier {
            fence(Ordering::SeqCst);
        }

        // 8. Transition pin: ACQUIRE -> PAUSE -> RUN.
        state_guard.set_state(KSSTATE_ACQUIRE)?;
        state_guard.set_state(KSSTATE_PAUSE)?;
        state_guard.set_state(KSSTATE_RUN)?;
        let run_start = Instant::now();

        // 9. Event-driven WaveRT loop: each notification signals a completed half-buffer.
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

            let stop_requested = self.stop_flag.load(Ordering::SeqCst);
            let play_offset = pos_reader.read_position_bytes();
            let now_since_run = run_start.elapsed();

            match controller.on_notification_at(
                cyclic_slice,
                pump,
                stop_requested,
                play_offset,
                Some(now_since_run),
            ) {
                Ok((_, outcome)) => {
                    if call_barrier {
                        fence(Ordering::SeqCst);
                    }
                    if outcome == WaveRtStepOutcome::Finished {
                        break;
                    }
                }
                Err(err) => {
                    if call_barrier {
                        fence(Ordering::SeqCst);
                    }
                    pump_error = Some(err);
                    break;
                }
            }
        }

        // 10. Clean stop: PAUSE -> STOP (then RAII drops unregister event, close pin & filter).
        let _ = state_guard.set_state(KSSTATE_PAUSE);
        let _ = state_guard.set_state(KSSTATE_STOP);

        if let Some(err) = pump_error {
            return Err(err);
        }

        Ok(PlaybackStats {
            device_name: self.device.friendly_name.clone(),
            sample_rate_hz,
            wire_format,
            frames_played: controller.frames_played(),
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

    fn arm(&mut self) {
        WdmKsSink::arm(self);
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

/// Empirical probe summary of a live WDM-KS WaveRT driver's notification & position support.
#[derive(Debug, Clone)]
pub struct WaveRtHardwareProbe {
    pub buffer_bytes: usize,
    pub half_bytes: usize,
    pub register_get_ok: bool,
    pub unregister_get_ok: bool,
    pub register_set_ok: bool,
    pub unregister_set_ok: bool,
    pub position_register_supported: bool,
    pub audio_position_supported: bool,
    pub sample_play_offsets: Vec<(u64, usize)>,
    /// Raw `(PlayOffset, WriteOffset)` pairs from `KSPROPERTY_AUDIO_POSITION` on each wake.
    pub sample_write_offsets: Vec<(u64, u64)>,
}

#[cfg(windows)]
fn register_notification_event_raw(
    pin: HANDLE,
    event: HANDLE,
    flags: u32,
) -> Result<(), OutputError> {
    let prop = KSRTAUDIO_NOTIFICATION_EVENT_PROPERTY {
        Property: ks_identifier(
            KSPROPSETID_RtAudio,
            KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT as u32,
            flags,
        ),
        NotificationEvent: event,
    };
    ks_ioctl(pin, &prop, core::ptr::null_mut(), 0, "RegisterNotificationEvent")?;
    Ok(())
}

#[cfg(windows)]
fn unregister_notification_event_raw(
    pin: HANDLE,
    event: HANDLE,
    flags: u32,
) -> Result<(), OutputError> {
    let prop = KSRTAUDIO_NOTIFICATION_EVENT_PROPERTY {
        Property: ks_identifier(
            KSPROPSETID_RtAudio,
            KSPROPERTY_RTAUDIO_UNREGISTER_NOTIFICATION_EVENT as u32,
            flags,
        ),
        NotificationEvent: event,
    };
    ks_ioctl(pin, &prop, core::ptr::null_mut(), 0, "UnregisterNotificationEvent")?;
    Ok(())
}

#[cfg(windows)]
struct WaveRtNotificationGuard {
    pin: HANDLE,
    event: HANDLE,
    property_flags: u32,
}
#[cfg(windows)]
impl WaveRtNotificationGuard {
    fn register(pin: HANDLE, event: HANDLE) -> Result<Self, OutputError> {
        // Per the WDK WaveRT property usage table (Get: Yes, Set: No) and PortAudio pa_win_wdmks,
        // KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT is a GET property. Try KSPROPERTY_TYPE_GET
        // first, falling back to KSPROPERTY_TYPE_SET only if a driver rejects GET, and reuse the
        // accepted flag for UNREGISTER on drop.
        match register_notification_event_raw(pin, event, KSPROPERTY_TYPE_GET) {
            Ok(()) => Ok(Self {
                pin,
                event,
                property_flags: KSPROPERTY_TYPE_GET,
            }),
            Err(get_err) => match register_notification_event_raw(pin, event, KSPROPERTY_TYPE_SET) {
                Ok(()) => Ok(Self {
                    pin,
                    event,
                    property_flags: KSPROPERTY_TYPE_SET,
                }),
                Err(_) => Err(get_err),
            },
        }
    }
}
#[cfg(windows)]
impl Drop for WaveRtNotificationGuard {
    fn drop(&mut self) {
        let _ = unregister_notification_event_raw(self.pin, self.event, self.property_flags);
    }
}

#[cfg(windows)]
fn query_position_register(pin: HANDLE) -> Option<(*const core::ffi::c_void, u32)> {
    let prop = KSRTAUDIO_HWREGISTER_PROPERTY {
        Property: ks_identifier(
            KSPROPSETID_RtAudio,
            KSPROPERTY_RTAUDIO_POSITIONREGISTER as u32,
            KSPROPERTY_TYPE_GET,
        ),
        BaseAddress: core::ptr::null_mut(),
    };
    let mut reg: KSRTAUDIO_HWREGISTER = unsafe { core::mem::zeroed() };
    let ok = ks_ioctl(
        pin,
        &prop,
        (&mut reg as *mut KSRTAUDIO_HWREGISTER).cast(),
        core::mem::size_of::<KSRTAUDIO_HWREGISTER>() as u32,
        "KSPROPERTY_RTAUDIO_POSITIONREGISTER",
    );
    if ok.is_ok() && !reg.Register.is_null() && matches!(reg.Width, 32 | 64) {
        Some((reg.Register.cast_const(), reg.Width))
    } else {
        None
    }
}

#[cfg(windows)]
fn query_audio_position(pin: HANDLE) -> Option<KSAUDIO_POSITION> {
    let prop = ks_identifier(
        KSPROPSETID_Audio,
        KSPROPERTY_AUDIO_POSITION as u32,
        KSPROPERTY_TYPE_GET,
    );
    let mut pos: KSAUDIO_POSITION = unsafe { core::mem::zeroed() };
    let ok = ks_ioctl(
        pin,
        &prop,
        (&mut pos as *mut KSAUDIO_POSITION).cast(),
        core::mem::size_of::<KSAUDIO_POSITION>() as u32,
        "KSPROPERTY_AUDIO_POSITION",
    );
    ok.ok().map(|_| pos)
}

/// Hardware DMA/play-cursor reader for a live WaveRT KS pin (must not outlive `pin`).
#[cfg(windows)]
enum WaveRtPositionReader {
    Register {
        ptr: *const core::ffi::c_void,
        width_bits: u32,
    },
    AudioPositionIoctl {
        pin: HANDLE,
        selector: AudioPositionCursorSelector,
    },
    Unavailable,
}

#[cfg(windows)]
impl WaveRtPositionReader {
    fn open(pin: HANDLE) -> Self {
        if let Some((ptr, width_bits)) = query_position_register(pin) {
            return Self::Register { ptr, width_bits };
        }
        if let Some(initial) = query_audio_position(pin) {
            let mut selector = AudioPositionCursorSelector::new();
            let _ = selector.select(initial.PlayOffset, initial.WriteOffset);
            return Self::AudioPositionIoctl { pin, selector };
        }
        Self::Unavailable
    }

    fn is_modular(&self) -> bool {
        matches!(self, Self::Register { .. })
    }

    fn read_position_bytes(&mut self) -> Option<u64> {
        match self {
            Self::Register { ptr, width_bits } => unsafe {
                match *width_bits {
                    32 => Some(u64::from(core::ptr::read_volatile((*ptr).cast::<u32>()))),
                    64 => Some(core::ptr::read_volatile((*ptr).cast::<u64>())),
                    _ => None,
                }
            },
            Self::AudioPositionIoctl { pin, selector } => {
                // Lock onto one consistent cursor (`WriteOffset` once seen non-zero, otherwise
                // `PlayOffset`) so legitimate modulo wraps to 0 never mix two cursors.
                query_audio_position(*pin)
                    .map(|pos| selector.select(pos.PlayOffset, pos.WriteOffset))
            }
            Self::Unavailable => None,
        }
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
    if out.BufferAddress.is_null() {
        return Err(OutputError::Win32 {
            operation: "KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION",
            code: 0,
            detail: format!(
                "driver returned null WaveRT buffer (size={})",
                out.ActualBufferSize
            ),
        });
    }
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
