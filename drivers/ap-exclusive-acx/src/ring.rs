// Non-paged spinlock-protected kernel ring buffer, telemetry, and audiodg.exe zero-mixer gate.
// SPDX-License-Identifier: GPL-3.0-or-later

use core::ptr;

use ap_stream_protocol::{
    AP_STREAM_ABI_VERSION, ApDriverConfig, ApDriverStatus, ApPcmChunkHeader, ApStreamFormat,
    ApTraceEvent, CHUNK_FLAG_EOS, CHUNK_FLAG_SILENT_PACKET, CHUNK_FLAG_STREAM_RUNNING,
    STAGE_ALLOC_RT_PACKETS, STAGE_CREATE_STREAM, STAGE_DESTROY, STAGE_PAUSE, STAGE_RUN,
    STAGE_SET_RENDER_PACKET, TRACE_RING_LEN, is_audiodg_name,
};

use crate::acx_wdf::{POOL_FLAG_NON_PAGED, POOL_TAG, PsGetCurrentProcessId, STATUS_ACCESS_DENIED_VAL};
use crate::sys::{
    ExAllocatePool2, ExFreePoolWithTag, IoGetCurrentProcess, KIRQL, KSPIN_LOCK,
    KeAcquireSpinLockRaiseToDpc, KeInitializeSpinLock, KeQueryPerformanceCounter,
    KeReleaseSpinLock, LARGE_INTEGER, NTSTATUS, PsGetProcessImageFileName,
};

/// 4 MiB non-paged circular buffer (power of two; holds ~1.36 s @ 384 kHz 32-bit stereo).
pub const RING_CAPACITY: usize = 4 * 1024 * 1024;
const RING_MASK: usize = RING_CAPACITY - 1;

pub struct GlobalRingState {
    lock: KSPIN_LOCK,
    initialized: bool,
    ring_ptr: *mut u8,
    write_pos: usize,
    read_pos: usize,
    fill_bytes: usize,
    reader_connected: bool,
    last_read_qpc: i64,
    qpc_freq: i64,
    last_chunk_flags: u32,
    status: ApDriverStatus,
}

unsafe impl Sync for GlobalRingState {}

static mut G_STATE: GlobalRingState = GlobalRingState {
    lock: 0,
    initialized: false,
    ring_ptr: ptr::null_mut(),
    write_pos: 0,
    read_pos: 0,
    fill_bytes: 0,
    reader_connected: false,
    last_read_qpc: 0,
    qpc_freq: 10_000_000,
    last_chunk_flags: 0,
    status: ApDriverStatus {
        abi_version: AP_STREAM_ABI_VERSION,
        stream_generation: 0,
        stream_running: 0,
        exclusive_verified: 0,
        strict_reject_audiodg: 1,
        format: ApStreamFormat {
            sample_rate: 0,
            channels: 0,
            bits_per_sample: 0,
            valid_bits_per_sample: 0,
            block_align: 0,
            is_float: 0,
            packet_size_bytes: 0,
            packet_count: 0,
            reserved: 0,
        },
        ring_capacity_bytes: RING_CAPACITY as u32,
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
        last_rejected_stage: 0,
        timer_adjust_ppm: 0,
        create_proc: [0; 16],
        alloc_proc: [0; 16],
        run_proc: [0; 16],
        packet_proc: [0; 16],
        last_rejected_proc: [0; 16],
        signal_processing_mode: [0; 16],
        trace_seq: 0,
        trace_reserved: 0,
        trace: [ApTraceEvent {
            seq: 0,
            stage: 0,
            pid: 0,
            sample_rate: 0,
            bits: 0,
            rejected: 0,
            proc_name: [0; 16],
        }; TRACE_RING_LEN],
    },
};

struct SpinGuard {
    lock: *mut KSPIN_LOCK,
    old_irql: KIRQL,
}

impl SpinGuard {
    #[inline]
    unsafe fn acquire(lock: *mut KSPIN_LOCK) -> Self {
        let old_irql = unsafe { KeAcquireSpinLockRaiseToDpc(lock) };
        Self { lock, old_irql }
    }
}

impl Drop for SpinGuard {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            KeReleaseSpinLock(self.lock, self.old_irql);
        }
    }
}

#[inline]
unsafe fn push_trace_locked(
    st: *mut GlobalRingState,
    stage: u32,
    pid: u32,
    proc_name: [u8; 16],
    rejected: bool,
) {
    unsafe {
        let next_seq = (*st).status.trace_seq.wrapping_add(1);
        (*st).status.trace_seq = next_seq;
        let idx = ((next_seq - 1) as usize) % TRACE_RING_LEN;
        (*st).status.trace[idx] = ApTraceEvent {
            seq: next_seq,
            stage,
            pid,
            sample_rate: (*st).status.format.sample_rate,
            bits: (*st).status.format.bits_per_sample,
            rejected: if rejected { 1 } else { 0 },
            proc_name,
        };
    }
}

pub unsafe fn capture_current_proc() -> (u32, [u8; 16]) {
    let mut name = [0u8; 16];
    let pid = unsafe { PsGetCurrentProcessId() } as usize as u32;
    let eproc = unsafe { IoGetCurrentProcess() };
    if !eproc.is_null() {
        let p = unsafe { PsGetProcessImageFileName(eproc) };
        if !p.is_null() {
            for i in 0..15 {
                let b = unsafe { *p.add(i) };
                name[i] = b;
                if b == 0 {
                    break;
                }
            }
        }
    }
    (pid, name)
}

pub unsafe fn init_global_ring() -> bool {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        if (*st).initialized {
            return !(*st).ring_ptr.is_null();
        }
        KeInitializeSpinLock(ptr::addr_of_mut!((*st).lock));
        let mut freq = LARGE_INTEGER::default();
        let _ = KeQueryPerformanceCounter(&mut freq);
        if freq.QuadPart > 0 {
            (*st).qpc_freq = freq.QuadPart;
        }
        let buf = ExAllocatePool2(POOL_FLAG_NON_PAGED, RING_CAPACITY as u64, POOL_TAG);
        if buf.is_null() {
            return false;
        }
        (*st).ring_ptr = buf.cast::<u8>();
        (*st).initialized = true;
        true
    }
}

pub unsafe fn cleanup_global_ring() {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        if !(*st).initialized {
            return;
        }
        let ptr_to_free = {
            let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
            (*st).write_pos = 0;
            (*st).read_pos = 0;
            (*st).fill_bytes = 0;
            (*st).status.ring_fill_bytes = 0;
            (*st).status.stream_running = 0;
            let p = (*st).ring_ptr;
            (*st).ring_ptr = ptr::null_mut();
            p
        };
        if !ptr_to_free.is_null() {
            ExFreePoolWithTag(ptr_to_free.cast(), POOL_TAG);
        }
        (*st).initialized = false;
    }
}

pub unsafe fn qpc_frequency() -> i64 {
    unsafe { (*ptr::addr_of!(G_STATE)).qpc_freq }
}

pub unsafe fn set_reader_connected(connected: bool) {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).reader_connected = connected;
        if connected {
            (*st).write_pos = 0;
            (*st).read_pos = 0;
            (*st).fill_bytes = 0;
            (*st).status.ring_fill_bytes = 0;
        }
    }
}

pub unsafe fn reset_ring_and_counters(reset_all_counters: bool) {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).write_pos = 0;
        (*st).read_pos = 0;
        (*st).fill_bytes = 0;
        (*st).status.ring_fill_bytes = 0;
        (*st).status.ring_overruns = 0;
        (*st).status.ring_underruns = 0;
        if reset_all_counters {
            (*st).status.rejected_shared_opens = 0;
            (*st).status.packets_completed = 0;
            (*st).status.render_packets_set = 0;
            (*st).status.bytes_written_to_ring = 0;
            (*st).status.bytes_read_from_ring = 0;
        }
    }
}

pub unsafe fn apply_config(cfg: ApDriverConfig) {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).status.strict_reject_audiodg = if cfg.strict_reject_audiodg != 0 { 1 } else { 0 };
        if cfg.reset_counters != 0 {
            (*st).status.rejected_shared_opens = 0;
            (*st).status.ring_overruns = 0;
            (*st).status.ring_underruns = 0;
            (*st).status.packets_completed = 0;
            (*st).status.render_packets_set = 0;
        }
    }
}

pub unsafe fn get_status_snapshot() -> ApDriverStatus {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        let mut snap = (*st).status;
        snap.ring_fill_bytes = (*st).fill_bytes as u32;
        snap
    }
}

/// Hard zero-mixer gate:
/// - `CreateStream` / endpoint-activation `AllocateRtPackets` may be brokered by `audiodg.exe`
///   or `svchost.exe` (zero audio flows until `Run`/`SetRenderPacket`).
/// - `Run` and `SetRenderPacket` from `audiodg.exe` are ALWAYS rejected with `STATUS_ACCESS_DENIED`
///   when `strict_reject_audiodg != 0`, so the Windows mixer can never start playback or deliver
///   a single mixed sample.
pub unsafe fn check_and_record_caller(stage: u32) -> Result<(u32, [u8; 16]), NTSTATUS> {
    let (pid, proc_name) = unsafe { capture_current_proc() };
    let is_mixer = is_audiodg_name(&proc_name);
    let is_streaming_stage = stage == STAGE_RUN || stage == STAGE_SET_RENDER_PACKET;

    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));

        if is_mixer && is_streaming_stage && (*st).status.strict_reject_audiodg != 0 {
            (*st).status.rejected_shared_opens = (*st).status.rejected_shared_opens.wrapping_add(1);
            (*st).status.last_rejected_pid = pid;
            (*st).status.last_rejected_proc = proc_name;
            (*st).status.last_rejected_stage = stage;
            push_trace_locked(st, stage, pid, proc_name, true);
            return Err(STATUS_ACCESS_DENIED_VAL);
        }

        match stage {
            STAGE_CREATE_STREAM => {
                (*st).status.create_pid = pid;
                (*st).status.create_proc = proc_name;
                push_trace_locked(st, stage, pid, proc_name, false);
            }
            STAGE_ALLOC_RT_PACKETS => {
                (*st).status.alloc_pid = pid;
                (*st).status.alloc_proc = proc_name;
                push_trace_locked(st, stage, pid, proc_name, false);
            }
            STAGE_RUN => {
                (*st).status.run_pid = pid;
                (*st).status.run_proc = proc_name;
                push_trace_locked(st, stage, pid, proc_name, false);
            }
            STAGE_SET_RENDER_PACKET => {
                let first_pkt = (*st).status.packet_pid == 0;
                (*st).status.packet_pid = pid;
                (*st).status.packet_proc = proc_name;
                (*st).status.render_packets_set = (*st).status.render_packets_set.wrapping_add(1);
                if first_pkt {
                    push_trace_locked(st, stage, pid, proc_name, false);
                }
            }
            _ => {}
        }

        if is_mixer && is_streaming_stage {
            (*st).status.exclusive_verified = 0;
        }
    }
    Ok((pid, proc_name))
}

pub unsafe fn on_stream_created(fmt: ApStreamFormat, proc_mode_guid: [u8; 16]) {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).status.streams_created = (*st).status.streams_created.wrapping_add(1);
        (*st).status.format = fmt;
        (*st).status.signal_processing_mode = proc_mode_guid;
        (*st).status.alloc_pid = 0;
        (*st).status.alloc_proc = [0; 16];
        (*st).status.run_pid = 0;
        (*st).status.run_proc = [0; 16];
        (*st).status.packet_pid = 0;
        (*st).status.packet_proc = [0; 16];
        (*st).status.exclusive_verified = 0;
    }
}

pub unsafe fn on_stream_packets_allocated(packet_count: u32, packet_size: u32) {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).status.format.packet_count = packet_count;
        (*st).status.format.packet_size_bytes = packet_size;
    }
}

pub unsafe fn on_stream_run(fmt: ApStreamFormat) {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).status.stream_generation = (*st).status.stream_generation.wrapping_add(1);
        (*st).status.stream_running = 1;
        (*st).status.format = fmt;
        (*st).write_pos = 0;
        (*st).read_pos = 0;
        (*st).fill_bytes = 0;
        (*st).status.ring_fill_bytes = 0;
        (*st).last_chunk_flags = CHUNK_FLAG_STREAM_RUNNING;
        let data_path_is_exclusive = (*st).status.alloc_pid != 0
            && (*st).status.run_pid != 0
            && !is_audiodg_name(&(*st).status.alloc_proc)
            && !is_audiodg_name(&(*st).status.run_proc)
            && fmt.is_float == 0;
        (*st).status.exclusive_verified = if data_path_is_exclusive { 1 } else { 0 };
    }
}

pub unsafe fn on_stream_paused() {
    let (pid, proc_name) = unsafe { capture_current_proc() };
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).status.stream_running = 0;
        (*st).last_chunk_flags = 0;
        push_trace_locked(st, STAGE_PAUSE, pid, proc_name, false);
    }
}

pub unsafe fn on_stream_destroyed() {
    let (pid, proc_name) = unsafe { capture_current_proc() };
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));
        (*st).status.streams_destroyed = (*st).status.streams_destroyed.wrapping_add(1);
        (*st).status.stream_running = 0;
        push_trace_locked(st, STAGE_DESTROY, pid, proc_name, false);
    }
}

/// Push one completed WaveRT render packet into the 4 MiB ring buffer.
/// Returns a smooth clock-slaving adjustment in ppm (-20,000..=+20,000) when the user-mode
/// engine is actively pulling (slaves Tidal seamlessly to the DAC's hardware crystal clock).
pub unsafe fn push_packet_bytes(
    src: *const u8,
    len: usize,
    is_silent: bool,
    is_eos: bool,
    packet_size: usize,
) -> i32 {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));

        (*st).status.packets_completed = (*st).status.packets_completed.wrapping_add(1);
        let mut flags = CHUNK_FLAG_STREAM_RUNNING;
        if is_silent {
            flags |= CHUNK_FLAG_SILENT_PACKET;
        }
        if is_eos {
            flags |= CHUNK_FLAG_EOS;
        }
        (*st).last_chunk_flags = flags;

        if (*st).status.exclusive_verified == 0 || (*st).ring_ptr.is_null() || len == 0 {
            return 0;
        }

        let copy_len = len.min(RING_CAPACITY);
        let space = RING_CAPACITY - (*st).fill_bytes;
        if copy_len > space {
            let block_align = ((*st).status.format.block_align as usize).max(1);
            let mut drop_bytes = copy_len - space;
            // Keep dropped prefix frame-aligned so stereo channels can NEVER swap on overrun!
            let rem = drop_bytes % block_align;
            if rem != 0 {
                drop_bytes = (drop_bytes + (block_align - rem)).min((*st).fill_bytes);
            }
            (*st).read_pos = ((*st).read_pos + drop_bytes) & RING_MASK;
            (*st).fill_bytes -= drop_bytes;
            if (*st).reader_connected {
                (*st).status.ring_overruns = (*st).status.ring_overruns.wrapping_add(1);
            }
        }

        let first = copy_len.min(RING_CAPACITY - (*st).write_pos);
        let second = copy_len - first;
        if is_silent || src.is_null() {
            ptr::write_bytes((*st).ring_ptr.add((*st).write_pos), 0, first);
            if second > 0 {
                ptr::write_bytes((*st).ring_ptr, 0, second);
            }
        } else {
            ptr::copy_nonoverlapping(src, (*st).ring_ptr.add((*st).write_pos), first);
            if second > 0 {
                ptr::copy_nonoverlapping(src.add(first), (*st).ring_ptr, second);
            }
        }

        (*st).write_pos = ((*st).write_pos + copy_len) & RING_MASK;
        (*st).fill_bytes += copy_len;
        (*st).status.ring_fill_bytes = (*st).fill_bytes as u32;
        (*st).status.bytes_written_to_ring =
            (*st).status.bytes_written_to_ring.wrapping_add(copy_len as u64);

        let now_qpc = KeQueryPerformanceCounter(ptr::null_mut()).QuadPart;
        let reader_active = (*st).reader_connected
            && (*st).last_read_qpc > 0
            && (now_qpc - (*st).last_read_qpc) < ((*st).qpc_freq / 2);

        let adj_ppm = if reader_active && packet_size > 0 {
            let target = (packet_size * 4).clamp(16_384, 524_288) as i64;
            let err = ((*st).fill_bytes as i64) - target;
            ((err * 20_000) / target).clamp(-20_000, 20_000) as i32
        } else {
            0
        };
        (*st).status.timer_adjust_ppm = adj_ppm;
        adj_ppm
    }
}

pub unsafe fn read_pcm_chunk(dst_pcm: *mut u8, max_pcm_bytes: usize) -> (ApPcmChunkHeader, usize) {
    unsafe {
        let st = ptr::addr_of_mut!(G_STATE);
        let _g = SpinGuard::acquire(ptr::addr_of_mut!((*st).lock));

        (*st).last_read_qpc = KeQueryPerformanceCounter(ptr::null_mut()).QuadPart;
        let block_align = ((*st).status.format.block_align as usize).max(1);
        let mut to_read = (*st).fill_bytes.min(max_pcm_bytes);
        to_read -= to_read % block_align;

        if to_read > 0 && !(*st).ring_ptr.is_null() && !dst_pcm.is_null() {
            let first = to_read.min(RING_CAPACITY - (*st).read_pos);
            let second = to_read - first;
            ptr::copy_nonoverlapping((*st).ring_ptr.add((*st).read_pos), dst_pcm, first);
            if second > 0 {
                ptr::copy_nonoverlapping((*st).ring_ptr, dst_pcm.add(first), second);
            }
            (*st).read_pos = ((*st).read_pos + to_read) & RING_MASK;
            (*st).fill_bytes -= to_read;
            (*st).status.ring_fill_bytes = (*st).fill_bytes as u32;
            (*st).status.bytes_read_from_ring =
                (*st).status.bytes_read_from_ring.wrapping_add(to_read as u64);
        }

        let hdr = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: (*st).status.stream_generation,
            stream_running: (*st).status.stream_running,
            exclusive_verified: (*st).status.exclusive_verified,
            format: (*st).status.format,
            pcm_bytes: to_read as u32,
            ring_fill_after: (*st).fill_bytes as u32,
            flags: (*st).last_chunk_flags,
            reserved: 0,
        };
        (hdr, to_read)
    }
}
