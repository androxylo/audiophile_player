//! Shared `#![no_std]` ABI between `ap-exclusive-acx` (KMDF + ACX 1.1 kernel render driver)
//! and `ap-source::stream_capture` (user-mode engine reader).
//!
//! SPDX-License-Identifier: GPL-3.0-or-later
#![no_std]

pub const AP_STREAM_ABI_VERSION: u32 = 2;

/// Win32 symbolic link exposed by the driver's exclusive control device.
pub const DEVICE_WIN32_PATH: &str = r"\\.\ApExclusiveAudio";

// CTL_CODE(DeviceType, Function, Method, Access)
//   ((DeviceType) << 16) | ((Access) << 14) | ((Function) << 2) | (Method)
const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
const METHOD_BUFFERED: u32 = 0;
const FILE_READ_ACCESS: u32 = 1;
const FILE_WRITE_ACCESS: u32 = 2;

const fn ctl_code(device_type: u32, function: u32, method: u32, access: u32) -> u32 {
    (device_type << 16) | (access << 14) | (function << 2) | method
}

/// Query full driver/stream status, caller process verification, and counters.
pub const IOCTL_AP_GET_STATUS: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_READ_ACCESS);

/// Read available PCM bytes from the kernel ring buffer (prefixed by [`ApPcmChunkHeader`]).
pub const IOCTL_AP_READ_PCM: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x801, METHOD_BUFFERED, FILE_READ_ACCESS);

/// Configure driver behavior (e.g. strict `audiodg.exe` rejection).
pub const IOCTL_AP_SET_CONFIG: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_WRITE_ACCESS);

/// Reset ring buffer and error counters.
pub const IOCTL_AP_RESET_RING: u32 =
    ctl_code(FILE_DEVICE_UNKNOWN, 0x803, METHOD_BUFFERED, FILE_WRITE_ACCESS);

/// Lifecycle / rejection stage constants.
pub const STAGE_NONE: u32 = 0;
pub const STAGE_CREATE_STREAM: u32 = 1;
pub const STAGE_ALLOC_RT_PACKETS: u32 = 2;
pub const STAGE_RUN: u32 = 3;
pub const STAGE_SET_RENDER_PACKET: u32 = 4;
pub const STAGE_PAUSE: u32 = 5;
pub const STAGE_DESTROY: u32 = 6;

pub const TRACE_RING_LEN: usize = 16;

/// Flags in [`ApPcmChunkHeader::flags`].
pub const CHUNK_FLAG_STREAM_RUNNING: u32 = 1 << 0;
pub const CHUNK_FLAG_EOS: u32 = 1 << 1;
pub const CHUNK_FLAG_SILENT_PACKET: u32 = 1 << 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApStreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub valid_bits_per_sample: u16,
    pub block_align: u16,
    pub is_float: u32,
    pub packet_size_bytes: u32,
    pub packet_count: u32,
    pub reserved: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApDriverConfig {
    /// Non-zero (default): reject any `Run` or `SetRenderPacket` from `audiodg.exe`
    /// with `STATUS_ACCESS_DENIED` so the Windows mixer can NEVER enter the pipeline.
    pub strict_reject_audiodg: u32,
    /// Non-zero: also reset diagnostic counters when applying config.
    pub reset_counters: u32,
}

impl Default for ApDriverConfig {
    fn default() -> Self {
        Self {
            strict_reject_audiodg: 1,
            reset_counters: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApTraceEvent {
    pub seq: u32,
    pub stage: u32,
    pub pid: u32,
    pub sample_rate: u32,
    pub bits: u16,
    pub rejected: u16,
    pub proc_name: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApDriverStatus {
    pub abi_version: u32,
    pub stream_generation: u32,
    pub stream_running: u32,
    /// 1 ONLY when the active stream has integer PCM (`is_float == 0`) and
    /// `AllocateRtPackets`, `Run`, and `SetRenderPacket` are owned by a non-`audiodg.exe`
    /// exclusive-mode client (e.g. `TIDALPlayer.exe`).
    pub exclusive_verified: u32,
    pub strict_reject_audiodg: u32,
    pub format: ApStreamFormat,
    pub ring_capacity_bytes: u32,
    pub ring_fill_bytes: u32,
    pub packets_completed: u64,
    pub render_packets_set: u64,
    pub bytes_written_to_ring: u64,
    pub bytes_read_from_ring: u64,
    pub ring_overruns: u64,
    pub ring_underruns: u64,
    pub streams_created: u32,
    pub streams_destroyed: u32,
    pub rejected_shared_opens: u32,
    pub create_pid: u32,
    pub alloc_pid: u32,
    pub run_pid: u32,
    pub packet_pid: u32,
    pub last_rejected_pid: u32,
    pub last_rejected_stage: u32,
    pub timer_adjust_ppm: i32,
    pub create_proc: [u8; 16],
    pub alloc_proc: [u8; 16],
    pub run_proc: [u8; 16],
    pub packet_proc: [u8; 16],
    pub last_rejected_proc: [u8; 16],
    pub signal_processing_mode: [u8; 16],
    pub trace_seq: u32,
    pub trace_reserved: u32,
    pub trace: [ApTraceEvent; TRACE_RING_LEN],
}

impl Default for ApDriverStatus {
    fn default() -> Self {
        Self {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 0,
            stream_running: 0,
            exclusive_verified: 0,
            strict_reject_audiodg: 1,
            format: ApStreamFormat::default(),
            ring_capacity_bytes: 0,
            ring_fill_bytes: 0,
            packets_completed: 0,
            render_packets_set: 0,
            bytes_written_to_ring: 0,
            bytes_read_from_ring: 0,
            ring_overruns: 0,
            ring_underruns: 0,
            streams_created: 0,
            streams_destroyed: 0,
            rejected_shared_opens: 0,
            create_pid: 0,
            alloc_pid: 0,
            run_pid: 0,
            packet_pid: 0,
            last_rejected_pid: 0,
            last_rejected_stage: STAGE_NONE,
            timer_adjust_ppm: 0,
            create_proc: [0; 16],
            alloc_proc: [0; 16],
            run_proc: [0; 16],
            packet_proc: [0; 16],
            last_rejected_proc: [0; 16],
            signal_processing_mode: [0; 16],
            trace_seq: 0,
            trace_reserved: 0,
            trace: [ApTraceEvent::default(); TRACE_RING_LEN],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ApPcmChunkHeader {
    pub abi_version: u32,
    pub stream_generation: u32,
    pub stream_running: u32,
    pub exclusive_verified: u32,
    pub format: ApStreamFormat,
    pub pcm_bytes: u32,
    pub ring_fill_after: u32,
    pub flags: u32,
    pub reserved: u32,
}

/// Convert a 16-byte kernel process name buffer into a `&str` slice up to the first NUL.
pub fn proc_name_str(buf: &[u8; 16]) -> &str {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..end]).unwrap_or("<non-utf8>")
}

/// Case-insensitive check for `audiodg.exe` (Windows Audio Device Graph Isolation / Mixer).
pub fn is_audiodg_name(buf: &[u8; 16]) -> bool {
    const TARGET: &[u8] = b"audiodg.exe";
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if end != TARGET.len() {
        return false;
    }
    let mut i = 0;
    while i < TARGET.len() {
        let mut c = buf[i];
        if c >= b'A' && c <= b'Z' {
            c += b'a' - b'A';
        }
        if c != TARGET[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Human-readable label for a lifecycle/rejection stage constant.
pub fn stage_name(stage: u32) -> &'static str {
    match stage {
        STAGE_CREATE_STREAM => "CreateStream",
        STAGE_ALLOC_RT_PACKETS => "AllocateRtPackets",
        STAGE_RUN => "Run",
        STAGE_SET_RENDER_PACKET => "SetRenderPacket",
        STAGE_PAUSE => "Pause",
        STAGE_DESTROY => "Destroy",
        _ => "None",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_proc16(s: &str) -> [u8; 16] {
        let mut out = [0u8; 16];
        let bytes = s.as_bytes();
        let n = bytes.len().min(15);
        out[..n].copy_from_slice(&bytes[..n]);
        out
    }

    #[test]
    fn audiodg_detection_is_case_insensitive_and_exact() {
        assert!(is_audiodg_name(&to_proc16("audiodg.exe")));
        assert!(is_audiodg_name(&to_proc16("AUDIODG.EXE")));
        assert!(is_audiodg_name(&to_proc16("AudioDg.Exe")));
        assert!(!is_audiodg_name(&to_proc16("TIDALPlayer.ex")));
        assert!(!is_audiodg_name(&to_proc16("audiodg.exe2")));
        assert!(!is_audiodg_name(&to_proc16("")));
    }

    #[test]
    fn proc_name_and_stage_helpers_work() {
        let p = to_proc16("TIDALPlayer.ex");
        assert_eq!(proc_name_str(&p), "TIDALPlayer.ex");
        assert_eq!(stage_name(STAGE_RUN), "Run");
        assert_eq!(stage_name(STAGE_SET_RENDER_PACKET), "SetRenderPacket");
        assert_eq!(stage_name(999), "None");
    }

    #[test]
    fn ioctl_codes_match_method_buffered_layout() {
        assert_eq!(IOCTL_AP_GET_STATUS, 0x0022_6000);
        assert_eq!(IOCTL_AP_READ_PCM, 0x0022_6004);
        assert_eq!(IOCTL_AP_SET_CONFIG, 0x0022_A008);
        assert_eq!(IOCTL_AP_RESET_RING, 0x0022_A00C);
    }
}
