//! Shared `#![no_std]` ABI and pure-Rust host-testable timing/ring math between
//! `ap-exclusive-acx` (KMDF + ACX 1.1 kernel render driver) and `ap-source::stream_capture`
//! (user-mode engine reader).
//!
//! SPDX-License-Identifier: GPL-3.0-or-later
#![no_std]

use core::mem::{offset_of, size_of};

pub const AP_STREAM_ABI_VERSION: u32 = 3;

/// Win32 symbolic link exposed by the driver's exclusive control device.
pub const DEVICE_WIN32_PATH: &str = r"\\.\ApExclusiveAudio";

/// Maximum PCM payload returned per `IOCTL_AP_READ_PCM` call (caps spinlock hold time at DISPATCH,
/// Review #8 A12).
pub const MAX_READ_PCM_BYTES: usize = 256 * 1024;

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

/// Configure driver behavior (e.g. reset diagnostic counters; strict zero-mixer gate stays enforced).
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
    /// Kept for ABI compatibility; the kernel always enforces strict `audiodg.exe` rejection
    /// on `Run`/`SetRenderPacket` (cannot be weakened from user mode, Review #8 A1).
    pub strict_reject_audiodg: u32,
    /// Non-zero: reset diagnostic counters when applying config.
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
    /// `AllocateRtPackets` and `Run` are owned by a non-`audiodg.exe`, non-`System`
    /// exclusive-mode client (e.g. `TIDALPlayer.exe`).
    pub exclusive_verified: u32,
    pub strict_reject_audiodg: u32,
    /// Explicit zero-initialized padding before 8-byte aligned `format` (Review #8 A13/B8).
    pub _pad0: u32,
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
            _pad0: 0,
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
    /// Low 32 bits of the kernel ring overrun counter (Review #8 B6).
    pub ring_overruns_lo: u32,
}

// Compile-time frozen ABI layout invariants (zero implicit padding holes, Review #8 A13/B8).
const _: () = {
    assert!(size_of::<ApStreamFormat>() == 32);
    assert!(offset_of!(ApStreamFormat, sample_rate) == 0);
    assert!(offset_of!(ApStreamFormat, channels) == 4);
    assert!(offset_of!(ApStreamFormat, bits_per_sample) == 6);
    assert!(offset_of!(ApStreamFormat, valid_bits_per_sample) == 8);
    assert!(offset_of!(ApStreamFormat, block_align) == 10);
    assert!(offset_of!(ApStreamFormat, is_float) == 12);
    assert!(offset_of!(ApStreamFormat, packet_size_bytes) == 16);
    assert!(offset_of!(ApStreamFormat, packet_count) == 20);
    assert!(offset_of!(ApStreamFormat, reserved) == 24);

    assert!(size_of::<ApDriverConfig>() == 8);
    assert!(size_of::<ApTraceEvent>() == 36);
    assert!(offset_of!(ApTraceEvent, proc_name) == 20);

    assert!(size_of::<ApPcmChunkHeader>() == 64);
    assert!(offset_of!(ApPcmChunkHeader, format) == 16);
    assert!(offset_of!(ApPcmChunkHeader, pcm_bytes) == 48);
    assert!(offset_of!(ApPcmChunkHeader, ring_fill_after) == 52);
    assert!(offset_of!(ApPcmChunkHeader, flags) == 56);
    assert!(offset_of!(ApPcmChunkHeader, ring_overruns_lo) == 60);

    assert!(size_of::<ApDriverStatus>() == 832);
    assert!(offset_of!(ApDriverStatus, strict_reject_audiodg) == 16);
    assert!(offset_of!(ApDriverStatus, _pad0) == 20);
    assert!(offset_of!(ApDriverStatus, format) == 24);
    assert!(offset_of!(ApDriverStatus, ring_capacity_bytes) == 56);
    assert!(offset_of!(ApDriverStatus, packets_completed) == 64);
    assert!(offset_of!(ApDriverStatus, streams_created) == 112);
    assert!(offset_of!(ApDriverStatus, create_proc) == 152);
    assert!(offset_of!(ApDriverStatus, trace_seq) == 248);
    assert!(offset_of!(ApDriverStatus, trace) == 256);
};

/// Convert a 16-byte kernel process name buffer into a `&str` slice up to the first NUL.
pub fn proc_name_str(buf: &[u8; 16]) -> &str {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    core::str::from_utf8(&buf[..end]).unwrap_or("<non-utf8>")
}

fn eq_ascii_ci(buf: &[u8; 16], target: &[u8]) -> bool {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    if end != target.len() {
        return false;
    }
    let mut i = 0;
    while i < target.len() {
        let mut c = buf[i];
        if c >= b'A' && c <= b'Z' {
            c += b'a' - b'A';
        }
        if c != target[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Case-insensitive check for `audiodg.exe` (Windows Audio Device Graph Isolation / Mixer).
pub fn is_audiodg_name(buf: &[u8; 16]) -> bool {
    eq_ascii_ci(buf, b"audiodg.exe")
}

/// Returns `true` for kernel worker / system / empty process names so deferred WDF worker
/// threads fail closed instead of spoofing `exclusive_verified = 1` (Review #8 A7).
pub fn is_system_or_idle_name(buf: &[u8; 16]) -> bool {
    buf[0] == 0 || eq_ascii_ci(buf, b"system") || eq_ascii_ci(buf, b"idle")
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

/// Pure `#![no_std]` kernel driver timing, monotonic presentation position, packet bounds,
/// and alignment helpers (shared between `ap-exclusive-acx` and host unit tests; Review #8
/// A2, A3, A8, A9).
pub mod clock_math {
    pub const HNS_PER_SEC: u64 = 10_000_000;
    /// Maximum smooth DAC clock slaving rate (±500 ppm easily tracks any real quartz crystal
    /// ±50–100 ppm without oscillation; Review #8 A3).
    pub const MAX_SLAVING_PPM: i32 = 500;
    /// Minimum timer due delay (0.5 ms in 100 ns units).
    pub const MIN_DUE_HNS: i64 = 5_000;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RenderPacketError {
        InvalidState,
        Late,
        Overrun,
    }

    /// Strict ACX `SetRenderPacket` bounds validation (Review #8 A8):
    /// - When running, `packet <= cur` is late (`cur` is currently playing).
    /// - `packet >= cur + packet_count` wraps onto a playing/unconsumed slot (`Overrun`).
    #[inline]
    pub fn validate_render_packet(
        is_running: bool,
        cur: u32,
        packet_count: u32,
        packet: u32,
    ) -> Result<usize, RenderPacketError> {
        if packet_count == 0 {
            return Err(RenderPacketError::InvalidState);
        }
        if is_running && packet <= cur {
            return Err(RenderPacketError::Late);
        }
        if packet >= cur.saturating_add(packet_count) {
            return Err(RenderPacketError::Overrun);
        }
        Ok((packet as usize) % (packet_count as usize))
    }

    /// Round `len` down to a multiple of `block_align` (Review #8 A9).
    #[inline]
    pub const fn align_down(len: usize, block_align: usize) -> usize {
        if block_align <= 1 {
            len
        } else {
            len - (len % block_align)
        }
    }

    /// Compute a low-pass filtered clock-slaving rate in `[-MAX_SLAVING_PPM, +MAX_SLAVING_PPM]`
    /// from kernel ring fill vs target fill (Review #8 A3).
    ///
    /// Positive ppm slows the virtual producer down (ring too full); negative ppm speeds the
    /// virtual producer up (ring draining because the DAC runs faster than nominal QPC).
    #[inline]
    pub fn update_slaving_ppm(
        smoothed_ppm_x256: &mut i64,
        fill_bytes: usize,
        target_bytes: usize,
    ) -> i32 {
        let target = (target_bytes.max(16_384)) as i64;
        let err = (fill_bytes as i64) - target;
        let raw_ppm = ((err * (MAX_SLAVING_PPM as i64)) / target)
            .clamp(-(MAX_SLAVING_PPM as i64), MAX_SLAVING_PPM as i64);
        // 1st-order IIR low-pass (alpha = 1/8) in Q8 fixed-point prevents oscillation.
        let prev = *smoothed_ppm_x256;
        let next = (prev * 7 + raw_ppm * 256) / 8;
        *smoothed_ppm_x256 = next;
        (next / 256).clamp(-(MAX_SLAVING_PPM as i64), MAX_SLAVING_PPM as i64) as i32
    }

    /// Per-stream monotonic presentation position and signed-offset WaveRT packet scheduler
    /// (Review #8 A2 & A3).
    #[derive(Clone, Copy, Debug, Default)]
    pub struct StreamClockState {
        pub start_time_hns: u64,
        pub start_pos_bytes: u64,
        pub last_reported_pos_bytes: u64,
        /// Signed cumulative schedule offset in 100 ns units (`< 0` = sped up for a fast DAC;
        /// `> 0` = slowed down for a slow DAC or late DPC; Review #8 A3).
        pub glitch_adjust_hns: i64,
    }

    impl StreamClockState {
        pub fn on_stop(&mut self) {
            self.start_time_hns = 0;
            self.start_pos_bytes = 0;
            self.last_reported_pos_bytes = 0;
            self.glitch_adjust_hns = 0;
        }

        pub fn on_run(&mut self, now_hns: u64, bytes_consumed: u64) {
            self.start_time_hns = now_hns;
            // Resume from the frozen monotonic position so position never steps backwards (A2).
            self.start_pos_bytes = self.last_reported_pos_bytes.max(bytes_consumed);
            self.glitch_adjust_hns = 0;
        }

        pub fn on_pause(
            &mut self,
            now_hns: u64,
            avg_bytes_per_sec: u32,
            bytes_consumed: u64,
            packet_size: u32,
        ) {
            // Freeze `last_reported_pos_bytes` at the live position before entering Pause (A2).
            let _ = self.presentation_position_bytes(
                true,
                now_hns,
                avg_bytes_per_sec,
                bytes_consumed,
                packet_size,
            );
        }

        /// Compute the next timer due delay (`>= MIN_DUE_HNS`) and accumulate signed slaving
        /// adjustments so both fast (+ppm) and slow (-ppm) DAC crystals lock without drift (A3).
        pub fn compute_next_due_hns(
            &mut self,
            now_hns: u64,
            next_pkt_pos_bytes: u64,
            avg_bytes_per_sec: u32,
            packet_size: u32,
            adj_ppm: i32,
        ) -> i64 {
            if avg_bytes_per_sec == 0 {
                return MIN_DUE_HNS;
            }
            let since_resume = next_pkt_pos_bytes.saturating_sub(self.start_pos_bytes);
            let nominal_hns = ((since_resume as u128 * HNS_PER_SEC as u128)
                / (avg_bytes_per_sec as u128)) as i128;

            let pkt_hns = (((packet_size as u64) * HNS_PER_SEC) / (avg_bytes_per_sec as u64))
                .max(10_000) as i64;

            let clamped_ppm = adj_ppm.clamp(-MAX_SLAVING_PPM, MAX_SLAVING_PPM) as i64;
            if clamped_ppm != 0 {
                let nudge = (pkt_hns * clamped_ppm) / 1_000_000;
                self.glitch_adjust_hns = self.glitch_adjust_hns.saturating_add(nudge);
            }

            let target_hns: i128 =
                (self.start_time_hns as i128) + nominal_hns + (self.glitch_adjust_hns as i128);
            let mut delta_hns: i128 = target_hns - (now_hns as i128);

            if delta_hns < (MIN_DUE_HNS as i128) {
                let late = ((MIN_DUE_HNS as i128) - delta_hns).min(i64::MAX as i128) as i64;
                self.glitch_adjust_hns = self.glitch_adjust_hns.saturating_add(late);
                delta_hns = MIN_DUE_HNS as i128;
            }

            delta_hns.clamp(MIN_DUE_HNS as i128, (HNS_PER_SEC as i128) * 10) as i64
        }

        /// Strictly monotonic presentation position in bytes (Review #8 A2):
        /// - Uses signed `glitch_adjust_hns` consistently with the tick schedule.
        /// - Clamps interpolation to `<= bytes_consumed + packet_size` (never runs ahead on late DPC).
        /// - Never decreases across pauses, clock nudges, or late ticks.
        pub fn presentation_position_bytes(
            &mut self,
            is_running: bool,
            now_hns: u64,
            avg_bytes_per_sec: u32,
            bytes_consumed: u64,
            packet_size: u32,
        ) -> u64 {
            let computed = if is_running && avg_bytes_per_sec > 0 {
                let adjusted_start =
                    (self.start_time_hns as i128) + (self.glitch_adjust_hns as i128);
                let elapsed_hns = ((now_hns as i128) - adjusted_start).max(0) as u128;
                let interp = self.start_pos_bytes.saturating_add(
                    ((elapsed_hns * (avg_bytes_per_sec as u128)) / (HNS_PER_SEC as u128)) as u64,
                );
                let ceiling = bytes_consumed.saturating_add(packet_size as u64);
                interp.min(ceiling).max(bytes_consumed)
            } else {
                bytes_consumed
            };
            let mono = computed.max(self.last_reported_pos_bytes);
            self.last_reported_pos_bytes = mono;
            mono
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clock_math::*;

    fn to_proc16(s: &str) -> [u8; 16] {
        let mut out = [0u8; 16];
        let bytes = s.as_bytes();
        let n = bytes.len().min(15);
        out[..n].copy_from_slice(&bytes[..n]);
        out
    }

    #[test]
    fn audiodg_and_system_detection_are_case_insensitive_and_exact() {
        assert!(is_audiodg_name(&to_proc16("audiodg.exe")));
        assert!(is_audiodg_name(&to_proc16("AUDIODG.EXE")));
        assert!(is_audiodg_name(&to_proc16("AudioDg.Exe")));
        assert!(!is_audiodg_name(&to_proc16("TIDALPlayer.ex")));
        assert!(!is_audiodg_name(&to_proc16("audiodg.exe2")));
        assert!(!is_audiodg_name(&to_proc16("")));

        assert!(is_system_or_idle_name(&to_proc16("System")));
        assert!(is_system_or_idle_name(&to_proc16("SYSTEM")));
        assert!(is_system_or_idle_name(&to_proc16("Idle")));
        assert!(is_system_or_idle_name(&to_proc16("")));
        assert!(!is_system_or_idle_name(&to_proc16("TIDALPlayer.ex")));
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
    fn ioctl_codes_and_access_bits_match_method_buffered_layout() {
        assert_eq!(IOCTL_AP_GET_STATUS, 0x0022_6000);
        assert_eq!(IOCTL_AP_READ_PCM, 0x0022_6004);
        assert_eq!(IOCTL_AP_SET_CONFIG, 0x0022_A008);
        assert_eq!(IOCTL_AP_RESET_RING, 0x0022_A00C);
        // Verify METHOD_BUFFERED (low 2 bits == 0) and READ/WRITE access bits (bits 14..16)
        assert_eq!(IOCTL_AP_GET_STATUS & 0x3, 0);
        assert_eq!((IOCTL_AP_GET_STATUS >> 14) & 0x3, FILE_READ_ACCESS);
        assert_eq!((IOCTL_AP_READ_PCM >> 14) & 0x3, FILE_READ_ACCESS);
        assert_eq!((IOCTL_AP_SET_CONFIG >> 14) & 0x3, FILE_WRITE_ACCESS);
        assert_eq!((IOCTL_AP_RESET_RING >> 14) & 0x3, FILE_WRITE_ACCESS);
    }

    #[test]
    fn abi_struct_sizes_and_offsets_have_zero_implicit_padding() {
        assert_eq!(size_of::<ApStreamFormat>(), 32);
        assert_eq!(size_of::<ApDriverConfig>(), 8);
        assert_eq!(size_of::<ApTraceEvent>(), 36);
        assert_eq!(size_of::<ApPcmChunkHeader>(), 64);
        assert_eq!(size_of::<ApDriverStatus>(), 832);
        assert_eq!(offset_of!(ApDriverStatus, _pad0), 20);
        assert_eq!(offset_of!(ApDriverStatus, format), 24);
    }

    #[test]
    fn validate_render_packet_enforces_strict_late_and_overrun_bounds_a8() {
        // Running, cur=6, packet_count=2: only packet 7 is valid!
        assert_eq!(
            validate_render_packet(true, 6, 2, 5),
            Err(RenderPacketError::Late)
        );
        assert_eq!(
            validate_render_packet(true, 6, 2, 6),
            Err(RenderPacketError::Late)
        );
        assert_eq!(validate_render_packet(true, 6, 2, 7), Ok(1));
        assert_eq!(
            validate_render_packet(true, 6, 2, 8),
            Err(RenderPacketError::Overrun)
        );
        // Paused before first run (cur=0, count=2): pre-fill packets 0 and 1 are valid, 2 is overrun
        assert_eq!(validate_render_packet(false, 0, 2, 0), Ok(0));
        assert_eq!(validate_render_packet(false, 0, 2, 1), Ok(1));
        assert_eq!(
            validate_render_packet(false, 0, 2, 2),
            Err(RenderPacketError::Overrun)
        );
        assert_eq!(
            validate_render_packet(true, 0, 0, 0),
            Err(RenderPacketError::InvalidState)
        );
    }

    #[test]
    fn align_down_preserves_frame_boundaries_for_4_6_8_byte_frames_a9() {
        for ba in [4usize, 6, 8] {
            for len in [0usize, 1, 5, 6, 7, 13, 4096, 4 * 1024 * 1024] {
                let a = align_down(len, ba);
                assert_eq!(a % ba, 0);
                assert!(a <= len);
                assert!(len - a < ba);
            }
        }
    }

    #[test]
    fn presentation_position_is_strictly_monotonic_across_pause_nudges_and_late_ticks_a2() {
        let rate: u32 = 44_100 * 8; // 352,800 B/s (44.1k / 32-bit stereo)
        let pkt: u32 = 3_528; // 10 ms packet
        let mut clk = StreamClockState::default();
        let mut now_hns: u64 = 1_000_000;
        let mut consumed: u64 = 0;

        clk.on_run(now_hns, consumed);
        let mut prev = 0u64;

        // 1. Mid-packet query then pause then query while paused then resume
        now_hns += 70_000; // 7 ms into packet 0
        let p1 = clk.presentation_position_bytes(true, now_hns, rate, consumed, pkt);
        assert!(p1 > prev && p1 <= pkt as u64);
        prev = p1;

        clk.on_pause(now_hns, rate, consumed, pkt);
        let p_paused = clk.presentation_position_bytes(false, now_hns + 500_000, rate, consumed, pkt);
        assert!(p_paused >= prev, "pause must never move position backwards");
        prev = p_paused;

        // Resume after 50 ms pause
        now_hns += 500_000;
        clk.on_run(now_hns, consumed);

        // 2. 500 ticks with positive ppm nudges (+500 ppm) + occasional late DPC (+3 ms)
        for tick in 1..=500u64 {
            let due = clk.compute_next_due_hns(now_hns, tick * (pkt as u64), rate, pkt, 500);
            let jitter = if tick % 25 == 0 { 30_000 } else { 0 };
            now_hns += (due as u64) + jitter;
            let p_before = clk.presentation_position_bytes(true, now_hns, rate, consumed, pkt);
            assert!(
                p_before >= prev,
                "position regressed at tick {tick}: {p_before} < {prev}"
            );
            assert!(
                p_before <= consumed + (pkt as u64),
                "position exceeded active packet ceiling during late DPC"
            );
            prev = p_before;
            consumed += pkt as u64;
            let p_after = clk.presentation_position_bytes(true, now_hns, rate, consumed, pkt);
            assert!(p_after >= prev);
            prev = p_after;
        }
    }

    #[test]
    fn clock_slaving_tracks_both_fast_and_slow_dac_crystals_over_one_hour_a3() {
        let rate: u32 = 48_000 * 8; // 384,000 B/s
        let pkt: u32 = 3_840; // 10 ms packet
        let target: usize = 256 * 1024;
        let ring_cap: usize = 4 * 1024 * 1024;

        for dac_offset_ppm in [-100i64, 100i64] {
            let mut clk = StreamClockState::default();
            let mut smoothed_ppm: i64 = 0;
            let mut now_hns: u64 = 10_000_000;
            let mut fill_bytes: f64 = target as f64;
            let dac_bytes_per_hns =
                (rate as f64) * (1.0 + (dac_offset_ppm as f64) / 1_000_000.0) / (HNS_PER_SEC as f64);

            clk.on_run(now_hns, 0);

            // Simulate 1 full hour = 360,000 packets of 10 ms
            let mut min_fill = fill_bytes;
            let mut max_fill = fill_bytes;
            for tick in 1..=360_000u64 {
                let adj = update_slaving_ppm(&mut smoothed_ppm, fill_bytes as usize, target);
                let due = clk.compute_next_due_hns(now_hns, tick * (pkt as u64), rate, pkt, adj);
                now_hns += due as u64;
                // DAC drains over `due` hns, then producer delivers one packet
                fill_bytes -= (due as f64) * dac_bytes_per_hns;
                assert!(
                    fill_bytes > 0.0,
                    "underrun at tick {tick} for dac_offset_ppm={dac_offset_ppm}"
                );
                fill_bytes += pkt as f64;
                assert!(
                    fill_bytes < (ring_cap as f64),
                    "overrun at tick {tick} for dac_offset_ppm={dac_offset_ppm}"
                );
                if fill_bytes < min_fill {
                    min_fill = fill_bytes;
                }
                if fill_bytes > max_fill {
                    max_fill = fill_bytes;
                }
            }
            // Over a full hour at ±100 ppm, fill must stay tightly bounded around target!
            assert!(
                min_fill > (target as f64) * 0.5 && max_fill < (target as f64) * 1.5,
                "dac_offset_ppm={dac_offset_ppm}: fill drifted out of bounds [{min_fill:.0}, {max_fill:.0}] around {target}"
            );
        }
    }
}
