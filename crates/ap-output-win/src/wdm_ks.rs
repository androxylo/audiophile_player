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

    /// Number of non-blocking producer underrun events encountered during streaming.
    fn underrun_events(&self) -> usize {
        0
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
/// lazily across block boundaries with zero gaps, non-blocking underrun silence, deferred
/// off-RT-thread block recycling, and click-free fade-out on stop.
pub struct StreamingWaveRtPump<F, B = StreamBlock>
where
    F: FnMut() -> B,
    B: Into<StreamBlock>,
{
    next_block: F,
    recycler: Option<Box<dyn FnMut(InterleavedInt32Buffer)>>,
    current: Option<InterleavedInt32Buffer>,
    cursor: usize,
    expected_sample_rate_hz: u32,
    wire_format: DacWireFormat,
    dither: WireDitherState,
    total_frames_written: usize,
    underrun_events: usize,
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
            recycler: None,
            current: None,
            cursor: 0,
            expected_sample_rate_hz,
            wire_format,
            dither: WireDitherState::default(),
            total_frames_written: 0,
            underrun_events: 0,
            eof: false,
            pending_error: None,
            _marker: PhantomData,
        }
    }

    /// Attach a callback that receives consumed `InterleavedInt32Buffer` blocks so the real-time
    /// MMCSS WaveRT thread never invokes `HeapFree` / `VirtualFree` on large PCM buffers.
    pub fn with_recycler<R>(mut self, recycler: R) -> Self
    where
        R: FnMut(InterleavedInt32Buffer) + 'static,
    {
        self.recycler = Some(Box::new(recycler));
        self
    }

    fn release_current(&mut self) {
        if let Some(old) = self.current.take() {
            if let Some(ref mut recycle) = self.recycler {
                recycle(old);
            }
        }
        self.cursor = 0;
    }

    fn ensure_current(&mut self) -> bool {
        while !self.eof {
            if let Some(buf) = &self.current {
                if self.cursor < buf.frames.len() {
                    return true;
                }
            }
            self.release_current();
            match (self.next_block)().into() {
                StreamBlock::Ready(buf) if !buf.frames.is_empty() => {
                    if buf.sample_rate_hz != self.expected_sample_rate_hz {
                        let bad_rate = buf.sample_rate_hz;
                        if let Some(ref mut recycle) = self.recycler {
                            recycle(buf);
                        }
                        self.eof = true;
                        self.pending_error = Some(OutputError::InvalidBuffer(format!(
                            "stream block sample rate mismatch: pin is {} Hz, block is {} Hz",
                            self.expected_sample_rate_hz, bad_rate
                        )));
                        return false;
                    }
                    self.current = Some(buf);
                    self.cursor = 0;
                    return true;
                }
                StreamBlock::Ready(empty) => {
                    if let Some(ref mut recycle) = self.recycler {
                        recycle(empty);
                    }
                    continue; // skip empty block
                }
                StreamBlock::Underrun => {
                    // Temporary non-blocking underrun: emit silence for this half without
                    // marking EOF so playback recovers seamlessly once the producer catches up.
                    self.underrun_events = self.underrun_events.saturating_add(1);
                    return false;
                }
                StreamBlock::EndOfTrack => {
                    self.eof = true;
                    return false;
                }
            }
        }
        false
    }
}

impl<F, B> Drop for StreamingWaveRtPump<F, B>
where
    F: FnMut() -> B,
    B: Into<StreamBlock>,
{
    fn drop(&mut self) {
        self.release_current();
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

    fn underrun_events(&self) -> usize {
        self.underrun_events
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
    /// Spurious or duplicate wake before the next half-buffer boundary; no half was overwritten
    /// and no frames were credited.
    Spurious,
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

const LAG_HISTORY_WINDOW: usize = 8;
const CLOCK_ANCHOR_BLOCK: usize = 16;
const WAKE_RING_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaveRtWakeDecision {
    Write { write_half: usize, coalesced: bool },
    Spurious,
}

/// Pure, hardware-independent WaveRT cyclic half-buffer state machine.
///
/// Manages:
/// - Position-authoritative, continuously lag-tracked half-boundary detection with a 1/4-half
///   pre-boundary guard band, unified wall-clock multi-ring unwrapping, and spurious-wake
///   rejection. Lag is derived strictly from position (`boundary - unwrapped`) so `KSSTATE_RUN`
///   startup delays `D` and wake jitter never contaminate `lag_bytes` (Review #6 #2/#4/#5/#12).
/// - Robust lower-envelope linear clock model `(clock_ref_bidx, clock_ref_ns, effective_half_ns)`
///   that tracks both fine bidirectional crystal drift (±600 ppm) and non-nominal hardware wake
///   cadences (e.g. `usbaudio2.sys` ~52.86 ms vs nominal 50 ms) even under multi-ms jitter,
///   without absorbing wake latency into the boundary reference (Review #6 #4/#6/#11).
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
    lag_history: [u64; LAG_HISTORY_WINDOW],
    lag_history_len: usize,
    lag_history_cursor: usize,
    wrap_mode: PositionWrapMode,
    observed_wrap: bool,
    /// True once any hardware position sample has been observed strictly inside a half
    /// (`pos % half != 0`). Remains `false` for boundary-quantized drivers such as
    /// `usbaudio2.sys` that snap `KSAUDIO_POSITION` to exact `0` / `half` multiples.
    observed_intra_half_pos: bool,
    half_duration: Option<Duration>,
    /// Phase-continuous lower-envelope wall-clock boundary model:
    /// `ideal_boundary_ns(b) = clock_ref_ns + (b - clock_ref_bidx) * effective_half_ns`.
    clock_ref_bidx: u64,
    clock_ref_ns: i128,
    effective_half_ns: i128,
    baseline_wake_latency_ns: Option<i128>,
    anchor_block_min_err_ns: i128,
    anchor_block_min_wake: Option<(u64, i128)>,
    anchor_block_count: usize,
    anchor_blocks_completed: u64,
    /// Earliest-block lowest-residual wake `(bidx, now_ns)` used for long-horizon cadence slope.
    cadence_ref_min_wake: Option<(u64, i128)>,
    /// Ring of consecutive clean single-half `(bidx, now_ns)` wakes for early cadence detection.
    wake_ring: [(u64, i128); WAKE_RING_LEN],
    wake_ring_len: usize,
    wake_ring_cursor: usize,
    last_wake_instant: Option<Duration>,
    half_audio_frames: [usize; 2],
    completed_audio_frames: usize,
    spurious_wakes: usize,
    coalesced_wakes: usize,
    underrun_halves: usize,
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
            lag_history: [0; LAG_HISTORY_WINDOW],
            lag_history_len: 0,
            lag_history_cursor: 0,
            wrap_mode: PositionWrapMode::Auto,
            observed_wrap: false,
            observed_intra_half_pos: false,
            half_duration: None,
            clock_ref_bidx: 0,
            clock_ref_ns: 0,
            effective_half_ns: 0,
            baseline_wake_latency_ns: None,
            anchor_block_min_err_ns: 0,
            anchor_block_min_wake: None,
            anchor_block_count: 0,
            anchor_blocks_completed: 0,
            cadence_ref_min_wake: None,
            wake_ring: [(0, 0); WAKE_RING_LEN],
            wake_ring_len: 0,
            wake_ring_cursor: 0,
            last_wake_instant: None,
            half_audio_frames: [0, 0],
            completed_audio_frames: 0,
            spurious_wakes: 0,
            coalesced_wakes: 0,
            underrun_halves: 0,
            trailing_silent_halves: 0,
            stopping_fade_done: false,
        }
    }

    /// Configure the nominal duration of one half-buffer (for wall-clock wrap disambiguation and
    /// `Unavailable` position-reader coalescing recovery).
    pub fn with_half_duration(mut self, half_duration: Duration) -> Self {
        if !half_duration.is_zero() {
            self.half_duration = Some(half_duration);
            self.effective_half_ns = half_duration.as_nanos() as i128;
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

    /// Number of spurious/early notifications ignored without writing or crediting frames.
    pub fn spurious_wakes(&self) -> usize {
        self.spurious_wakes
    }

    /// Number of coalesced notifications (`>= 2` half-boundaries crossed between wakes).
    pub fn coalesced_wakes(&self) -> usize {
        self.coalesced_wakes
    }

    /// Number of DMA half-buffer refills that received zero frames from a non-EOF producer.
    pub fn underrun_halves(&self) -> usize {
        self.underrun_halves
    }

    /// Current estimated hardware play-cursor lag in bytes (strictly `0` for boundary-quantized
    /// drivers like `usbaudio2.sys`).
    pub fn lag_bytes(&self) -> u64 {
        self.lag_bytes
    }

    /// Currently tracked effective half-buffer wake period (initialized from `with_half_duration`
    /// and adapted when hardware cadence consistently differs from nominal).
    pub fn effective_half_duration(&self) -> Option<Duration> {
        if self.effective_half_ns > 0 {
            Some(Duration::from_nanos(self.effective_half_ns as u64))
        } else {
            self.half_duration
        }
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
        self.lag_history = [0; LAG_HISTORY_WINDOW];
        self.lag_history_len = 0;
        self.lag_history_cursor = 0;
        self.observed_wrap = false;
        self.observed_intra_half_pos = false;
        self.clock_ref_bidx = 0;
        self.clock_ref_ns = 0;
        self.effective_half_ns = self
            .half_duration
            .map_or(0, |d| d.as_nanos() as i128);
        self.baseline_wake_latency_ns = None;
        self.anchor_block_min_err_ns = 0;
        self.anchor_block_min_wake = None;
        self.anchor_block_count = 0;
        self.anchor_blocks_completed = 0;
        self.cadence_ref_min_wake = None;
        self.wake_ring = [(0, 0); WAKE_RING_LEN];
        self.wake_ring_len = 0;
        self.wake_ring_cursor = 0;
        self.last_wake_instant = Some(Duration::ZERO);
        self.spurious_wakes = 0;
        self.coalesced_wakes = 0;
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

    /// Active tracked half-buffer period in nanoseconds (defaults to nominal `half_dur`).
    fn effective_half_ns_for(&self, half_dur: Duration) -> i128 {
        if self.effective_half_ns > 0 {
            self.effective_half_ns
        } else {
            half_dur.as_nanos() as i128
        }
    }

    /// Ideal monotonic time (ns since `KSSTATE_RUN`) for half-boundary `boundary_idx`.
    fn ideal_boundary_ns(&self, boundary_idx: u64, half_dur: Duration) -> i128 {
        let half_ns = self.effective_half_ns_for(half_dur);
        let delta_bidx = (boundary_idx as i128) - (self.clock_ref_bidx as i128);
        self.clock_ref_ns
            .saturating_add(delta_bidx.saturating_mul(half_ns))
            .max(0)
    }

    /// Record a non-coalesced lag sample and recompute `self.lag_bytes` robustly.
    ///
    /// Uses the 2nd-largest sample in the sliding window (once >= 3 samples exist; max before
    /// that). This converges immediately upward when wake #1 was late (underestimated lag) AND
    /// rejects an isolated stale/coarse outlier sample (overestimated lag) within 2 on-time wakes.
    fn record_lag_sample(&mut self, sample: u64) {
        let half = self.layout.half_len() as u64;
        let clamped = sample.min(half);
        self.lag_history[self.lag_history_cursor] = clamped;
        self.lag_history_cursor = (self.lag_history_cursor + 1) % LAG_HISTORY_WINDOW;
        if self.lag_history_len < LAG_HISTORY_WINDOW {
            self.lag_history_len += 1;
        }
        if self.lag_history_len == 0 {
            self.lag_bytes = 0;
            return;
        }
        let mut sorted = [0u64; LAG_HISTORY_WINDOW];
        sorted[..self.lag_history_len].copy_from_slice(&self.lag_history[..self.lag_history_len]);
        sorted[..self.lag_history_len].sort_unstable();
        self.lag_bytes = if self.lag_history_len >= 3 {
            sorted[self.lag_history_len - 2]
        } else {
            sorted[self.lag_history_len - 1]
        };
    }

    /// Record a non-coalesced cursor lag observation derived strictly from hardware position
    /// (`boundary_byte.saturating_sub(unwrapped)`).
    ///
    /// Never adds `run_start`-anchored wall-clock wake latency to lag (Review #6 #2/#4/#5/#12):
    /// - Zero-lag position registers and boundary-quantized cursors (`pos % half == 0`) always
    ///   stay strictly at `lag_bytes = 0`, regardless of `KSSTATE_RUN` startup delay `D` or
    ///   non-nominal hardware cadence.
    /// - Continuous lagging cursors converge upward via the 2nd-largest of the last 8 samples.
    /// - Heavily delayed wakes (`>= eff_half / 4` past ideal) only record if they raise our lag
    ///   estimate (or history is empty), preventing a late-wake burst from evicting true lag.
    fn record_observed_lag(
        &mut self,
        now_since_run: Option<Duration>,
        boundary_idx: u64,
        unwrapped: u64,
    ) {
        let b_byte = self.boundary_byte(boundary_idx);
        let observed = b_byte.saturating_sub(unwrapped);
        let is_late_wake = match (now_since_run, self.half_duration) {
            (Some(now), Some(hd)) if !hd.is_zero() => {
                let eff_half = self.effective_half_ns_for(hd);
                let ideal_ns = self.ideal_boundary_ns(boundary_idx, hd);
                (now.as_nanos() as i128).saturating_sub(ideal_ns) >= eff_half / 4
            }
            _ => false,
        };
        if !is_late_wake || self.lag_history_len == 0 || observed > self.lag_bytes {
            self.record_lag_sample(observed);
        }
    }

    fn push_clean_wake(&mut self, boundary_idx: u64, now_ns: i128) {
        if self.wake_ring_len > 0 {
            let prev_slot = (self.wake_ring_cursor + WAKE_RING_LEN - 1) % WAKE_RING_LEN;
            let (prev_bidx, _) = self.wake_ring[prev_slot];
            if boundary_idx != prev_bidx.saturating_add(1) {
                self.wake_ring_len = 0;
                self.wake_ring_cursor = 0;
            }
        }
        self.wake_ring[self.wake_ring_cursor] = (boundary_idx, now_ns);
        self.wake_ring_cursor = (self.wake_ring_cursor + 1) % WAKE_RING_LEN;
        if self.wake_ring_len < WAKE_RING_LEN {
            self.wake_ring_len += 1;
        }
    }

    fn recent_consecutive_wakes(&self, count: usize) -> Option<[(u64, i128); WAKE_RING_LEN]> {
        if self.wake_ring_len < count || count > WAKE_RING_LEN {
            return None;
        }
        let mut out = [(0u64, 0i128); WAKE_RING_LEN];
        let start = (self.wake_ring_cursor + WAKE_RING_LEN - count) % WAKE_RING_LEN;
        for (i, slot) in out.iter_mut().enumerate().take(count) {
            *slot = self.wake_ring[(start + i) % WAKE_RING_LEN];
        }
        Some(out)
    }

    /// Update the lower-envelope linear wall-clock boundary model bidirectionally:
    /// - Never runs blind 4-wake startup absorption (Review #6 #4).
    /// - Snaps `clock_ref_ns` back immediately whenever any clean wake arrives earlier than
    ///   `ideal_boundary_ns`, preventing the anchor from drifting forward into wake latency (#6).
    /// - Detects non-nominal hardware wake cadences (e.g. `usbaudio2.sys` 52.86 ms vs nominal
    ///   50 ms) both on steady low-jitter streams AND under multi-ms jitter via multi-step
    ///   consistent slopes + long-horizon block-minimum slope refinement (#11).
    /// - Tracks fine bidirectional crystal drift (±600 ppm) over 16-wake blocks against
    ///   `baseline_wake_latency_ns` without absorbing steady wake latency.
    fn update_clock_anchor(
        &mut self,
        now_since_run: Option<Duration>,
        boundary_idx: u64,
        coalesced: bool,
        is_first_wake: bool,
    ) {
        let (Some(now), Some(half_dur)) = (now_since_run, self.half_duration) else {
            return;
        };
        if half_dur.is_zero() {
            return;
        }
        let now_ns = now.as_nanos() as i128;
        let nom_half_ns = half_dur.as_nanos() as i128;
        let eff_half_ns = self.effective_half_ns_for(half_dur);
        let ideal_ns = self.ideal_boundary_ns(boundary_idx, half_dur);
        let err_from_ideal = now_ns - ideal_ns;

        if is_first_wake && !coalesced {
            if err_from_ideal < 0 {
                // DMA started before `run_start` was captured: anchor directly to first wake.
                self.clock_ref_bidx = 0;
                self.clock_ref_ns = err_from_ideal;
                self.baseline_wake_latency_ns = Some(0);
            } else if err_from_ideal > eff_half_ns / 2 {
                // Large startup delay D (> half/2) before first notification: anchor at wake #1
                // so subsequent lower-latency wakes immediately snap `clock_ref_ns` down to D.
                self.clock_ref_bidx = boundary_idx;
                self.clock_ref_ns = now_ns;
                self.baseline_wake_latency_ns = Some(0);
            } else {
                self.baseline_wake_latency_ns = Some(err_from_ideal);
            }
            let post_err = now_ns - self.ideal_boundary_ns(boundary_idx, half_dur);
            self.anchor_block_min_err_ns = post_err;
            self.anchor_block_min_wake = Some((boundary_idx, now_ns));
            self.anchor_block_count = 1;
            self.wake_ring_len = 0;
            self.wake_ring_cursor = 0;
            self.push_clean_wake(boundary_idx, now_ns);
            return;
        }

        if coalesced {
            self.wake_ring_len = 0;
            self.wake_ring_cursor = 0;
            if err_from_ideal < 0 {
                let deficit = -err_from_ideal;
                let adj = deficit.min(eff_half_ns / 2);
                self.clock_ref_ns = self.clock_ref_ns.saturating_sub(adj);
            }
            return;
        }

        self.push_clean_wake(boundary_idx, now_ns);

        // 1. Early non-nominal cadence detection (works for both steady low-jitter drivers like
        //    Gungnir 2 and jittered `Unavailable` streams like U(0, 10) ms, Review #6 #11).
        let min_cadence = (nom_half_ns * 88) / 100;
        let max_cadence = (nom_half_ns * 112) / 100;
        let mut cadence_reanchored = false;

        // Fast steady-cadence lock during initial warmup only (5 consecutive wakes with <= 2%
        // 1-step spread and > 1.0% deviation from current effective_half_ns). Post-warmup, only
        // long-horizon block-minimum slope refinement may adjust `effective_half_ns`, preventing
        // a random 5-wake jitter ramp at wake 1800 from corrupting a locked period!
        if self.anchor_blocks_completed == 0 {
            if let Some(w) = self.recent_consecutive_wakes(5) {
                let mut dts = [0i128; 4];
                for i in 0..4 {
                    dts[i] = w[i + 1].1 - w[i].1;
                }
                dts.sort_unstable();
                let spread = dts[3] - dts[0];
                let med_dt = (dts[1] + dts[2]) / 2;
                if spread <= (nom_half_ns / 50).max(1)
                    && med_dt >= min_cadence
                    && med_dt <= max_cadence
                    && (med_dt - eff_half_ns).abs() > (nom_half_ns / 100).max(1)
                {
                    let mut best = w[0];
                    let mut best_res = best.1 - (best.0 as i128).saturating_mul(med_dt);
                    for &cand in &w[1..5] {
                        let res = cand.1 - (cand.0 as i128).saturating_mul(med_dt);
                        if res < best_res {
                            best_res = res;
                            best = cand;
                        }
                    }
                    let base_lat = self
                        .baseline_wake_latency_ns
                        .unwrap_or(0)
                        .clamp(0, med_dt / 2);
                    self.clock_ref_bidx = best.0;
                    self.clock_ref_ns = best.1.saturating_sub(base_lat);
                    self.effective_half_ns = med_dt;
                    self.cadence_ref_min_wake = Some(best);
                    self.anchor_block_count = 0;
                    self.anchor_block_min_err_ns = 0;
                    self.anchor_block_min_wake = None;
                    cadence_reanchored = true;
                }
            }
        }

        // Jitter-tolerant warmup cadence lock (10 consecutive wakes):
        // Use exact integer OLS slope over all 10 wakes plus both disjoint 5-wake halves
        // (`ols5(w[0..5])`, `ols5(w[5..10])`) and max residual check. A genuine non-nominal
        // cadence (±4..8%, even with U(0, 10) ms jitter) produces consistent slope in BOTH halves
        // with bounded residuals, whereas pure U(0.1, 0.6*half) jitter fails the two-half/residual
        // consistency check (fixes Review #6 Findings #6 & #11 simultaneously).
        if !cadence_reanchored && self.anchor_blocks_completed == 0 {
            if let Some(w) = self.recent_consecutive_wakes(10) {
                let t0 = w[0].1;
                let y = [
                    w[0].1 - t0,
                    w[1].1 - t0,
                    w[2].1 - t0,
                    w[3].1 - t0,
                    w[4].1 - t0,
                    w[5].1 - t0,
                    w[6].1 - t0,
                    w[7].1 - t0,
                    w[8].1 - t0,
                    w[9].1 - t0,
                ];
                let ols5_first = (-2 * y[0] - y[1] + y[3] + 2 * y[4]) / 10;
                let ols5_second = (-2 * y[5] - y[6] + y[8] + 2 * y[9]) / 10;
                // sum_{i=0..9} (2i - 9) * y_i / (2 * 82.5) = sum / 165
                let ols10 = (-9 * y[0] - 7 * y[1] - 5 * y[2] - 3 * y[3] - y[4]
                    + y[5]
                    + 3 * y[6]
                    + 5 * y[7]
                    + 7 * y[8]
                    + 9 * y[9])
                    / 165;
                let mean_y: i128 = y.iter().sum::<i128>() / 10;
                // Intercept at i=0: mean_y - 4.5 * ols10
                let intercept = mean_y - (9 * ols10) / 2;
                let max_res = (0..10)
                    .map(|i| (y[i] - (intercept + (i as i128) * ols10)).abs())
                    .max()
                    .unwrap_or(0);

                let one_pct = nom_half_ns / 100; // 1.0%
                let two_half_pct = nom_half_ns / 40; // 2.5%
                let min_err_first = w[0..5]
                    .iter()
                    .map(|&(k, t)| t - (k as i128).saturating_mul(eff_half_ns))
                    .min()
                    .unwrap_or(0);
                let min_err_second = w[5..10]
                    .iter()
                    .map(|&(k, t)| t - (k as i128).saturating_mul(eff_half_ns))
                    .min()
                    .unwrap_or(0);
                let envelope_shift = min_err_second - min_err_first;
                let min_envelope_shift = nom_half_ns / 6; // >= 16.7% shift between 5-wake minima
                // For faster-than-nominal cadence (`both_below`), also require the latest wake to
                // have actually arrived strictly before nominal `k * nom_half` so a simple drop
                // from a late wake #1 to an on-time wake #10 under heavy jitter is never mistaken
                // for a faster hardware clock.
                let latest_ahead_of_nominal =
                    w[9].1 < (w[9].0 as i128).saturating_mul(nom_half_ns) - (nom_half_ns / 8);
                let both_above = ols5_first > eff_half_ns + one_pct
                    && ols5_second > eff_half_ns + one_pct
                    && envelope_shift >= min_envelope_shift;
                let both_below = ols5_first < eff_half_ns - one_pct
                    && ols5_second < eff_half_ns - one_pct
                    && envelope_shift <= -min_envelope_shift
                    && latest_ahead_of_nominal;

                if (both_above || both_below)
                    && (ols10 - eff_half_ns).abs() > two_half_pct
                    && max_res <= (nom_half_ns * 16) / 100
                    && ols10 >= min_cadence
                    && ols10 <= max_cadence
                {
                    let mut best = w[0];
                    let mut best_res = best.1 - (best.0 as i128).saturating_mul(ols10);
                    for &cand in &w[1..10] {
                        let res = cand.1 - (cand.0 as i128).saturating_mul(ols10);
                        if res < best_res {
                            best_res = res;
                            best = cand;
                        }
                    }
                    self.clock_ref_bidx = best.0;
                    self.clock_ref_ns = best.1;
                    self.effective_half_ns = ols10;
                    self.baseline_wake_latency_ns = Some(0);
                    self.cadence_ref_min_wake = Some(best);
                    self.anchor_block_count = 0;
                    self.anchor_block_min_err_ns = 0;
                    self.anchor_block_min_wake = None;
                    cadence_reanchored = true;
                }
            }
        }

        if cadence_reanchored {
            return;
        }

        // 2. Immediate early-wake lower-envelope snap-down: NEVER let `ideal_boundary_ns` stay
        //    ahead of genuine on-time wakes (fixes Review #6 #4 & #6).
        if err_from_ideal < 0 {
            self.clock_ref_ns = self.clock_ref_ns.saturating_add(err_from_ideal);
            self.baseline_wake_latency_ns = Some(0);
        } else if self.anchor_blocks_completed == 0 {
            self.baseline_wake_latency_ns = Some(
                self.baseline_wake_latency_ns
                    .map_or(err_from_ideal, |b| b.min(err_from_ideal))
                    .min(eff_half_ns / 2),
            );
        }

        let post_err = now_ns - self.ideal_boundary_ns(boundary_idx, half_dur);
        let raw_res = now_ns - (boundary_idx as i128).saturating_mul(eff_half_ns);
        let is_lower_raw = match self.anchor_block_min_wake {
            None => true,
            Some((prev_k, prev_t)) => {
                raw_res < prev_t - (prev_k as i128).saturating_mul(eff_half_ns)
            }
        };
        if self.anchor_block_count == 0 || post_err < self.anchor_block_min_err_ns {
            self.anchor_block_min_err_ns = post_err;
        }
        if self.anchor_block_count == 0 || is_lower_raw {
            self.anchor_block_min_wake = Some((boundary_idx, now_ns));
        }
        self.anchor_block_count += 1;

        if self.anchor_block_count >= CLOCK_ANCHOR_BLOCK {
            let block_min_err = self.anchor_block_min_err_ns;
            let block_min_wake = self.anchor_block_min_wake;
            self.anchor_block_count = 0;
            self.anchor_block_min_err_ns = 0;
            self.anchor_block_min_wake = None;
            self.anchor_blocks_completed = self.anchor_blocks_completed.saturating_add(1);

            // 3. Long-horizon block-minimum slope refinement (handles ±8% cadences under heavy
            //    jitter AND converges sub-0.1% period accuracy without false triggers on U(0.1,30)).
            if let (Some((ref_k, ref_t)), Some((cur_k, cur_t))) =
                (self.cadence_ref_min_wake, block_min_wake)
            {
                let dk = cur_k.saturating_sub(ref_k);
                if dk >= CLOCK_ANCHOR_BLOCK as u64 {
                    let long_slope = (cur_t - ref_t) / (dk as i128);
                    let total_drift = (long_slope - eff_half_ns)
                        .abs()
                        .saturating_mul(dk as i128);
                    let refine_threshold = if dk >= 48 {
                        eff_half_ns / 4
                    } else {
                        eff_half_ns / 2
                    };
                    if long_slope >= min_cadence
                        && long_slope <= max_cadence
                        && total_drift > refine_threshold
                    {
                        let base_lat = self
                            .baseline_wake_latency_ns
                            .unwrap_or(0)
                            .clamp(0, long_slope / 4);
                        self.effective_half_ns = long_slope;
                        self.clock_ref_bidx = cur_k;
                        self.clock_ref_ns = cur_t.saturating_sub(base_lat);
                        return;
                    }
                }
            } else if block_min_wake.is_some() {
                self.cadence_ref_min_wake = block_min_wake;
            }

            // 4. Fine bidirectional crystal drift tracking (±600 ppm max per 16-wake block).
            let base_lat = *self
                .baseline_wake_latency_ns
                .get_or_insert_with(|| block_min_err.clamp(0, eff_half_ns / 2));
            let err = block_min_err - base_lat;
            let max_drift_per_block =
                (((CLOCK_ANCHOR_BLOCK as i128) * eff_half_ns) / 1666).max(1);

            if err >= -max_drift_per_block && err <= max_drift_per_block {
                self.clock_ref_ns = self.clock_ref_ns.saturating_add(err);
            } else if err > max_drift_per_block {
                self.clock_ref_ns = self.clock_ref_ns.saturating_add(max_drift_per_block);
            } else {
                let new_base = block_min_err.max(0);
                self.baseline_wake_latency_ns = Some(new_base);
                if block_min_err < 0 {
                    self.clock_ref_ns = self.clock_ref_ns.saturating_add(block_min_err);
                }
            }
        }
    }

    fn step_wall_clock_fallback(&mut self, now_since_run: Option<Duration>) -> WaveRtWakeDecision {
        let is_first_wake = self.last_boundary_idx == 0;
        let next_bidx = self.last_boundary_idx.saturating_add(1);
        let new_bidx = match (now_since_run, self.half_duration) {
            (Some(now), Some(half_dur)) => {
                let now_ns = now.as_nanos() as i128;
                let half_ns = self.effective_half_ns_for(half_dur);
                let ideal_next_ns = self.ideal_boundary_ns(next_bidx, half_dur);
                let early_margin_ns = half_ns / 8;

                if is_first_wake {
                    // The very first hardware notification is boundary 1 even when `KSSTATE_RUN`
                    // has a delayed DMA start `D` (Review #6 follow-up).
                    1
                } else {
                    // Review #6 Findings #4 & #6: NEVER reject a wake as `Spurious` when the gap
                    // since the previous wake is >= half/2, and ALWAYS apply `early_margin_ns` so
                    // an on-time wake right after a late wake is never dropped! On wake #2 after
                    // a late startup anchor, require an even tighter duplicate gap (< half/8).
                    let gap_ns = self
                        .last_wake_instant
                        .map_or(half_ns, |prev| now.saturating_sub(prev).as_nanos() as i128);
                    let dup_gap_limit = if self.last_boundary_idx == 1 {
                        half_ns / 8
                    } else {
                        half_ns / 2
                    };
                    if gap_ns < dup_gap_limit && now_ns.saturating_add(early_margin_ns) < ideal_next_ns
                    {
                        return WaveRtWakeDecision::Spurious;
                    }

                    if now_ns < ideal_next_ns {
                        next_bidx
                    } else {
                        let delta_ns = now_ns.saturating_sub(self.clock_ref_ns);
                        let delta_halves = if delta_ns >= 0 {
                            delta_ns / half_ns
                        } else {
                            -((-delta_ns + half_ns - 1) / half_ns)
                        };
                        let computed = (self.clock_ref_bidx as i128)
                            .saturating_add(delta_halves)
                            .max(0) as u64;
                        computed.max(next_bidx)
                    }
                }
            }
            _ => next_bidx,
        };

        let halves_elapsed = new_bidx.saturating_sub(self.last_boundary_idx);
        let coalesced = halves_elapsed >= 2;
        self.update_clock_anchor(now_since_run, new_bidx, coalesced, is_first_wake);
        if let Some(now) = now_since_run {
            self.last_wake_instant = Some(now);
        }
        self.last_boundary_idx = new_bidx;
        let write_half = ((new_bidx as usize) & 1) ^ 1;
        WaveRtWakeDecision::Write {
            write_half,
            coalesced,
        }
    }

    /// Decide which half to write, whether notifications coalesced, or whether the wake is
    /// spurious (arrived before the next half boundary).
    fn select_write_half(
        &mut self,
        play_offset_bytes: Option<u64>,
        now_since_run: Option<Duration>,
    ) -> WaveRtWakeDecision {
        let Some(pos) = play_offset_bytes else {
            return self.step_wall_clock_fallback(now_since_run);
        };

        let half = self.layout.half_len() as u64;
        let buf = self.layout.buf_len() as u64;
        let guard = (self.layout.half_len() / 4).max(self.layout.bytes_per_frame()) as u64;
        let max_plausible_lag = (half / 2).max(guard);

        if pos % half != 0 {
            self.observed_intra_half_pos = true;
        }

        let clock_info = match (now_since_run, self.half_duration) {
            (Some(now), Some(hd)) if !hd.is_zero() => Some((now, hd)),
            _ => None,
        };

        let is_first_wake = self.last_raw_pos.is_none();
        let unwrapped: u64 = match (self.last_raw_pos, self.last_unwrapped_pos) {
            (None, _) => {
                // First wake after KSSTATE_RUN (DMA started at byte 0).
                if let Some((now, hd)) = clock_info {
                    let eff_half_ns = self.effective_half_ns_for(hd) as u128;
                    let unwrapped_first =
                        if pos < buf && self.wrap_mode != PositionWrapMode::Monotonic {
                            let expected_pos =
                                ((now.as_nanos() * u128::from(half)) / eff_half_ns) as u64;
                            if expected_pos > pos.saturating_add(buf / 2)
                                && now.as_nanos() >= eff_half_ns + eff_half_ns / 2
                            {
                                let extra_rings = (expected_pos - pos + buf / 2) / buf;
                                self.observed_wrap = true;
                                pos.saturating_add(extra_rings.saturating_mul(buf))
                            } else {
                                pos
                            }
                        } else {
                            pos
                        };

                    // Review #6 Findings #3 & #5: On a coalesced first wake (`now >= 2 * eff_half`)
                    // with a lagging `AUDIO_POSITION`/`Monotonic` cursor, the lagging cursor can
                    // sit within `max_plausible_lag` just below `clock_bidx`, putting `pos_bidx`
                    // one behind the true DMA boundary. Seed lag from `clock_bidx` when exact.
                    let pos_bidx = self.boundary_idx_for_byte(unwrapped_first);
                    let clock_bidx = (now.as_nanos() / eff_half_ns) as u64;
                    if now.as_nanos() >= eff_half_ns.saturating_mul(2)
                        && self.wrap_mode != PositionWrapMode::Modular
                        && clock_bidx == pos_bidx.saturating_add(1)
                    {
                        let target_byte = self.boundary_byte(clock_bidx);
                        if unwrapped_first < target_byte
                            && target_byte - unwrapped_first <= max_plausible_lag
                        {
                            self.record_lag_sample(target_byte - unwrapped_first);
                        } else {
                            self.record_observed_lag(Some(now), pos_bidx.max(1), unwrapped_first);
                        }
                    } else if pos_bidx <= 1 {
                        self.record_observed_lag(Some(now), 1, unwrapped_first);
                    } else {
                        self.lag_bytes = 0;
                    }
                    unwrapped_first
                } else if pos < half {
                    let is_coalesced_modular_first_wake = self.wrap_mode
                        == PositionWrapMode::Modular
                        && pos < half.saturating_sub(guard);
                    if is_coalesced_modular_first_wake {
                        self.lag_bytes = 0;
                        self.observed_wrap = true;
                        buf.saturating_add(pos)
                    } else {
                        self.record_lag_sample(half - pos);
                        pos
                    }
                } else {
                    self.record_lag_sample(0);
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
                let can_be_modular =
                    prev_raw < buf && pos < buf && self.wrap_mode != PositionWrapMode::Monotonic;

                if can_be_modular {
                    if pos < prev_raw {
                        self.observed_wrap = true;
                    }
                    let base_delta = if pos >= prev_raw {
                        pos - prev_raw
                    } else {
                        (buf - prev_raw).saturating_add(pos)
                    };

                    let extra_rings = if let Some((now, hd)) = clock_info {
                        let eff_half_ns = self.effective_half_ns_for(hd) as u128;
                        let prev_t = self.last_wake_instant.unwrap_or(Duration::ZERO);
                        let elapsed = now.saturating_sub(prev_t);
                        let expected_delta =
                            ((elapsed.as_nanos() * u128::from(half)) / eff_half_ns) as u64;
                        if expected_delta > base_delta.saturating_add(buf / 2)
                            && (is_modular
                                || self.observed_wrap
                                || elapsed.as_nanos() >= eff_half_ns + eff_half_ns / 2)
                        {
                            self.observed_wrap = true;
                            (expected_delta - base_delta + buf / 2) / buf
                        } else {
                            0
                        }
                    } else if pos >= prev_raw
                        && is_modular
                        && self.lag_bytes == 0
                        && base_delta < half / 2
                        && self
                            .layout
                            .is_clearly_inside_half(self.next_expected_half, pos)
                    {
                        // Clockless modular full-ring coalesce fallback.
                        1
                    } else {
                        0
                    };

                    prev_unwrapped
                        .saturating_add(base_delta)
                        .saturating_add(extra_rings.saturating_mul(buf))
                } else if pos >= prev_raw {
                    prev_unwrapped.saturating_add(pos - prev_raw)
                } else {
                    // 32-bit counter wrap or non-monotonic quirk: step 1 half cleanly.
                    self.boundary_byte(self.last_boundary_idx.saturating_add(1))
                        .saturating_sub(self.lag_bytes)
                }
            }
            _ => pos,
        };

        let next_expected_bidx = self.last_boundary_idx.saturating_add(1);
        let next_expected_byte = self.boundary_byte(next_expected_bidx);
        let adj_pos = unwrapped.saturating_add(self.lag_bytes);
        let mut raw_bidx = self.boundary_idx_for_byte(adj_pos);

        // Review #5 #13 + Review #6 #12: Small upper-boundary guard ONLY for genuinely lagging
        // continuous cursors (`lag_bytes > half / 32`) with wall-clock confirmation. Never fires
        // on zero-lag position registers (`lag_bytes == 0`) or boundary-quantized cursors!
        let min_lag_for_upper_guard =
            (half / 32).max((self.layout.bytes_per_frame() as u64).saturating_mul(2));
        if self.observed_intra_half_pos && self.lag_bytes > min_lag_for_upper_guard {
            if let Some((now, hd)) = clock_info {
                let candidate_next = raw_bidx.saturating_add(1);
                let next_byte = self.boundary_byte(candidate_next);
                let upper_margin = (half / 16).max(self.layout.bytes_per_frame() as u64);
                if adj_pos < next_byte
                    && next_byte - adj_pos <= upper_margin
                    && (now.as_nanos() as i128) >= self.ideal_boundary_ns(candidate_next, hd)
                {
                    raw_bidx = candidate_next;
                }
            }
        }

        // Guard against an initially overestimated stale lag sample falsely triggering coalescing
        // when the wall clock proves the subsequent boundary has not yet been reached.
        if raw_bidx == next_expected_bidx.saturating_add(1) {
            if let Some((now, hd)) = clock_info {
                let now_ns = now.as_nanos() as i128;
                let ideal_raw = self.ideal_boundary_ns(raw_bidx, hd);
                let early_margin = self.effective_half_ns_for(hd) / 8;
                if now_ns.saturating_add(early_margin) < ideal_raw
                    && unwrapped < self.boundary_byte(raw_bidx)
                {
                    raw_bidx = next_expected_bidx;
                }
            }
        }

        let (new_bidx, coalesced, record_lag) = if raw_bidx >= next_expected_bidx.saturating_add(1)
        {
            // Crossed >= 2 half boundaries: genuine coalescing; resync directly to DMA half.
            (raw_bidx, true, false)
        } else if raw_bidx == next_expected_bidx {
            // Exactly 1 half boundary crossed (including late-but-not-coalesced wakes).
            (raw_bidx, false, !is_first_wake)
        } else {
            // Position is still short of `next_expected_byte`.
            let shortfall = next_expected_byte.saturating_sub(adj_pos);
            if shortfall <= guard {
                // Inside pre-boundary jitter guard band: trust expected half boundary.
                (next_expected_bidx, false, !is_first_wake)
            } else {
                // Outside guard band: distinguish a genuine boundary wake whose true cursor lag
                // was underestimated (e.g. wake #1 arrived late) from a spurious/duplicate wake.
                // Review #5 #12: When the wall clock proves we are at `next_expected_bidx` (and
                // not yet at `next_expected_bidx + 1`), accept lags up to a full `half` (use the
                // stricter `half / 2` cap only when no wall clock is available).
                let candidate_lag = next_expected_byte.saturating_sub(unwrapped);
                let (clock_reached_boundary, effective_max_lag) = match clock_info {
                    Some((now, hd)) => {
                        let now_ns = now.as_nanos() as i128;
                        let ideal_next = self.ideal_boundary_ns(next_expected_bidx, hd);
                        let ideal_after =
                            self.ideal_boundary_ns(next_expected_bidx.saturating_add(1), hd);
                        let early_margin = self.effective_half_ns_for(hd) / 8;
                        let reached = now_ns.saturating_add(early_margin) >= ideal_next
                            && now_ns.saturating_add(early_margin) < ideal_after;
                        (reached, half)
                    }
                    None => (
                        self.last_unwrapped_pos
                            .map(|prev| unwrapped.saturating_sub(prev) >= half / 2)
                            .unwrap_or(false),
                        max_plausible_lag,
                    ),
                };

                if !is_first_wake
                    && candidate_lag <= effective_max_lag
                    && clock_reached_boundary
                {
                    (next_expected_bidx, false, true)
                } else {
                    // Spurious / duplicate wake before the next half boundary: do NOT overwrite
                    // the pending half and do NOT credit unplayed frames.
                    return WaveRtWakeDecision::Spurious;
                }
            }
        };

        if record_lag {
            self.record_observed_lag(now_since_run, new_bidx, unwrapped);
        }

        self.update_clock_anchor(now_since_run, new_bidx, coalesced, is_first_wake);
        if let Some(now) = now_since_run {
            self.last_wake_instant = Some(now);
        }
        self.last_raw_pos = Some(pos);
        self.last_unwrapped_pos = Some(unwrapped);
        self.last_boundary_idx = new_bidx;

        let write_half = ((new_bidx as usize) & 1) ^ 1;
        WaveRtWakeDecision::Write {
            write_half,
            coalesced,
        }
    }

    /// Process one WaveRT half-buffer completion notification without wall-clock timing.
    ///
    /// Returns the half index that was written alongside `Continue` / `Spurious` / `Finished`.
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
        let (write_half, coalesced) = match self.select_write_half(play_offset_bytes, now_since_run)
        {
            WaveRtWakeDecision::Write {
                write_half,
                coalesced,
            } => (write_half, coalesced),
            WaveRtWakeDecision::Spurious => {
                self.spurious_wakes = self.spurious_wakes.saturating_add(1);
                return Ok((
                    self.last_written_half.unwrap_or(0),
                    WaveRtStepOutcome::Spurious,
                ));
            }
        };
        self.next_expected_half = write_half ^ 1;

        if coalesced {
            // Coalesced notification (>= 2 boundaries crossed): both halves completed DMA.
            self.coalesced_wakes = self.coalesced_wakes.saturating_add(1);
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
                self.underrun_halves = self.underrun_halves.saturating_add(1);
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

/// Returns `true` if `dev` is Audiophile Player's own virtual exclusive ACX capture endpoint.
/// Excluding it prevents an accidental audio feedback loop when selecting the output DAC.
pub fn is_own_virtual_endpoint(dev: &KsRenderDevice) -> bool {
    let name = dev.friendly_name.to_ascii_lowercase();
    let path = dev.device_path.to_ascii_lowercase();
    path.contains("apexclusiveaudio")
        || path.contains("aptidaltest")
        || name.contains("audiophile player")
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
        .filter(|d| d.supports_looped_wavert && !is_own_virtual_endpoint(d))
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

    /// Stream bounded `InterleavedInt32Buffer` blocks like [`Self::play_block_stream`], handing
    /// consumed blocks to `recycle_block` so the real-time MMCSS WaveRT thread never frees large
    /// PCM buffers directly.
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
        let run_start = Instant::now();
        state_guard.set_state(KSSTATE_RUN)?;
        let ksstate_run_elapsed = run_start.elapsed();

        // Stream 24 silent notifications (~1.25 s) so live probes cross both the 16-wake
        // CLOCK_ANCHOR_BLOCK rollover and any cadence-drift threshold on real hardware.
        let mut sample_play_offsets = Vec::with_capacity(24);
        let mut sample_write_offsets = Vec::with_capacity(24);
        let mut sample_wake_elapsed = Vec::with_capacity(24);
        for _ in 0..24 {
            let wait_res = unsafe { WaitForSingleObject(ev_run.raw(), 1500) };
            if wait_res != WAIT_OBJECT_0 {
                break;
            }
            let elapsed = run_start.elapsed();
            sample_wake_elapsed.push(elapsed);
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
            ksstate_run_elapsed,
            sample_play_offsets,
            sample_write_offsets,
            sample_wake_elapsed,
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
                underrun_halves: 0,
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
        // Capture `run_start` right before `KSSTATE_RUN` so DMA cannot start meaningfully before
        // `run_start` (Review #5 #11).
        state_guard.set_state(KSSTATE_ACQUIRE)?;
        state_guard.set_state(KSSTATE_PAUSE)?;
        let run_start = Instant::now();
        state_guard.set_state(KSSTATE_RUN)?;

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
            underrun_halves: controller.underrun_halves().max(pump.underrun_events()),
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
    /// Wall-clock duration spent inside `set_state(KSSTATE_RUN)` after capturing `run_start`.
    pub ksstate_run_elapsed: Duration,
    pub sample_play_offsets: Vec<(u64, usize)>,
    /// Raw `(PlayOffset, WriteOffset)` pairs from `KSPROPERTY_AUDIO_POSITION` on each wake.
    pub sample_write_offsets: Vec<(u64, u64)>,
    /// Elapsed wall-clock time since `run_start` on each WaveRT hardware notification wake.
    pub sample_wake_elapsed: Vec<Duration>,
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
    use super::{
        KsRenderDevice, is_own_virtual_endpoint, round_i32_to_top_16, round_i32_to_top_24,
        select_wdm_ks_device, wide_slice_to_string,
    };

    #[test]
    fn own_virtual_capture_endpoint_is_excluded_from_dac_selection() {
        let virt = KsRenderDevice {
            friendly_name: "Line (Audiophile Player)".to_string(),
            device_path: r"\\?\root#devgen#0000#{6994ad04}\render0".to_string(),
            render_pin_id: 0,
            supports_looped_wavert: true,
        };
        let dac = KsRenderDevice {
            friendly_name: "Gungnir 2 Unison 384".to_string(),
            device_path: r"\\?\usb#vid_30be&pid_1022#0000#{6994ad04}\msft_wave".to_string(),
            render_pin_id: 0,
            supports_looped_wavert: true,
        };
        assert!(is_own_virtual_endpoint(&virt));
        assert!(!is_own_virtual_endpoint(&dac));
        assert!(select_wdm_ks_device(&[virt.clone()], None).is_err());
        let picked = select_wdm_ks_device(&[virt, dac], None).unwrap();
        assert_eq!(picked.friendly_name, "Gungnir 2 Unison 384");
    }

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
