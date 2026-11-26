// ACX 1.1 WaveRT Render Stream implementation in pure `#![no_std]` Rust with per-stream spinlock.
// SPDX-License-Identifier: GPL-3.0-or-later

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use ap_stream_protocol::{
    ApStreamFormat, STAGE_ALLOC_RT_PACKETS, STAGE_CREATE_STREAM, STAGE_RUN,
    STAGE_SET_RENDER_PACKET,
};

use crate::acx_wdf::*;
use crate::ring;
use crate::sys::*;

const MAX_RT_PACKETS: usize = 8;
const MAX_ACTIVE_STREAMS: usize = 8;
const HNS_PER_SEC: u64 = 10_000_000;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
const KSSTREAM_EOS_FLAG: u32 = 0x40;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum StreamState {
    Stop = 0,
    Pause = 1,
    Run = 2,
}

#[derive(Clone, Copy, Default)]
struct SlotMeta {
    flags: u32,
    eos_len: u32,
    valid: bool,
}

pub struct RenderStream {
    lock: KSPIN_LOCK,
    stream: ACXSTREAM,
    format: ApStreamFormat,
    avg_bytes_per_sec: u32,
    block_align: u32,
    state: StreamState,
    timer: WDFTIMER,
    packet_count: u32,
    packet_size: u32,
    first_packet_offset: u32,
    packet_bases: [*mut u8; MAX_RT_PACKETS],
    packet_data_ptrs: [*mut u8; MAX_RT_PACKETS],
    slot_meta: [SlotMeta; MAX_RT_PACKETS],
    current_packet: AtomicU32,
    bytes_consumed: AtomicU64,
    start_time_hns: u64,
    start_pos_bytes: u64,
    glitch_adjust_hns: u64,
    clock_adj_ppm: AtomicI32,
    qpc_freq: i64,
}

struct RegistrySlot {
    stream: ACXSTREAM,
    timer: WDFTIMER,
    engine: *mut RenderStream,
}

static mut REG_LOCK: KSPIN_LOCK = 0;
static mut REG_SLOTS: [RegistrySlot; MAX_ACTIVE_STREAMS] = [
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
    RegistrySlot { stream: ptr::null_mut(), timer: ptr::null_mut(), engine: ptr::null_mut() },
];

pub unsafe fn init_stream_registry() {
    unsafe {
        KeInitializeSpinLock(ptr::addr_of_mut!(REG_LOCK));
    }
}

unsafe fn register_stream(stream: ACXSTREAM, engine: *mut RenderStream) -> bool {
    unsafe {
        let irql = KeAcquireSpinLockRaiseToDpc(ptr::addr_of_mut!(REG_LOCK));
        let mut ok = false;
        let slots = ptr::addr_of_mut!(REG_SLOTS);
        for slot in (*slots).iter_mut() {
            if slot.engine.is_null() {
                slot.stream = stream;
                slot.timer = ptr::null_mut();
                slot.engine = engine;
                ok = true;
                break;
            }
        }
        KeReleaseSpinLock(ptr::addr_of_mut!(REG_LOCK), irql);
        ok
    }
}

unsafe fn bind_timer_to_stream(stream: ACXSTREAM, timer: WDFTIMER) {
    unsafe {
        let irql = KeAcquireSpinLockRaiseToDpc(ptr::addr_of_mut!(REG_LOCK));
        let slots = ptr::addr_of_mut!(REG_SLOTS);
        for slot in (*slots).iter_mut() {
            if slot.stream == stream {
                slot.timer = timer;
                break;
            }
        }
        KeReleaseSpinLock(ptr::addr_of_mut!(REG_LOCK), irql);
    }
}

unsafe fn lookup_by_stream(stream: ACXSTREAM) -> *mut RenderStream {
    unsafe {
        let irql = KeAcquireSpinLockRaiseToDpc(ptr::addr_of_mut!(REG_LOCK));
        let mut found = ptr::null_mut();
        let slots = ptr::addr_of!(REG_SLOTS);
        for slot in (*slots).iter() {
            if slot.stream == stream {
                found = slot.engine;
                break;
            }
        }
        KeReleaseSpinLock(ptr::addr_of_mut!(REG_LOCK), irql);
        found
    }
}

unsafe fn lookup_by_timer(timer: WDFTIMER) -> *mut RenderStream {
    unsafe {
        let irql = KeAcquireSpinLockRaiseToDpc(ptr::addr_of_mut!(REG_LOCK));
        let mut found = ptr::null_mut();
        let slots = ptr::addr_of!(REG_SLOTS);
        for slot in (*slots).iter() {
            if slot.timer == timer {
                found = slot.engine;
                break;
            }
        }
        KeReleaseSpinLock(ptr::addr_of_mut!(REG_LOCK), irql);
        found
    }
}

unsafe fn unregister_stream(stream: ACXSTREAM) -> *mut RenderStream {
    unsafe {
        let irql = KeAcquireSpinLockRaiseToDpc(ptr::addr_of_mut!(REG_LOCK));
        let mut found = ptr::null_mut();
        let slots = ptr::addr_of_mut!(REG_SLOTS);
        for slot in (*slots).iter_mut() {
            if slot.stream == stream {
                found = slot.engine;
                slot.stream = ptr::null_mut();
                slot.timer = ptr::null_mut();
                slot.engine = ptr::null_mut();
                break;
            }
        }
        KeReleaseSpinLock(ptr::addr_of_mut!(REG_LOCK), irql);
        found
    }
}

#[inline]
fn qpc_to_hns(qpc: i64, freq: i64) -> u64 {
    if qpc <= 0 || freq <= 0 {
        return 0;
    }
    ((qpc as u128 * HNS_PER_SEC as u128) / (freq as u128)) as u64
}

impl RenderStream {
    unsafe fn allocate_rt_packets(
        &mut self,
        packet_count: ULONG,
        packet_size: ULONG,
        out_packets: *mut PACX_RTPACKET,
    ) -> NTSTATUS {
        if packet_count == 0
            || (packet_count as usize) > MAX_RT_PACKETS
            || packet_size == 0
            || out_packets.is_null()
        {
            return STATUS_INVALID_PARAMETER_VAL;
        }

        let arr_bytes = (packet_count as usize) * size_of::<ACX_RTPACKET>();
        let packets = unsafe { ExAllocatePool2(POOL_FLAG_NON_PAGED, arr_bytes as u64, POOL_TAG) }
            as PACX_RTPACKET;
        if packets.is_null() {
            return STATUS_INSUFFICIENT_RESOURCES_VAL;
        }

        let page = PAGE_SIZE as usize;
        let alloc_bytes = ((packet_size as usize) + page - 1) & !(page - 1);
        let first_offset = (alloc_bytes - packet_size as usize) as ULONG;

        let mut made = 0usize;
        let mut status = STATUS_SUCCESS_VAL;
        for i in 0..(packet_count as usize) {
            unsafe {
                *packets.add(i) = acx_rtpacket_init();
                let buf = ExAllocatePool2(POOL_FLAG_NON_PAGED, alloc_bytes as u64, POOL_TAG);
                if buf.is_null() {
                    status = STATUS_INSUFFICIENT_RESOURCES_VAL;
                    break;
                }
                let mdl = IoAllocateMdl(buf, alloc_bytes as ULONG, 0, 1, ptr::null_mut());
                if mdl.is_null() {
                    ExFreePoolWithTag(buf, POOL_TAG);
                    status = STATUS_INSUFFICIENT_RESOURCES_VAL;
                    break;
                }
                MmBuildMdlForNonPagedPool(mdl);
                let pkt = &mut *packets.add(i);
                pkt.RtPacketBuffer = wdf_memory_descriptor_init_mdl(mdl, alloc_bytes as ULONG);
                pkt.RtPacketSize = packet_size;
                pkt.RtPacketOffset = if i == 0 { first_offset } else { 0 };

                self.packet_bases[i] = buf.cast::<u8>();
                self.packet_data_ptrs[i] =
                    buf.cast::<u8>().add(if i == 0 { first_offset as usize } else { 0 });
                self.slot_meta[i] = SlotMeta::default();
            }
            made += 1;
        }

        if !nt_success(status) {
            unsafe {
                self.free_rt_packets(packets, made as ULONG);
            }
            return status;
        }

        self.packet_count = packet_count;
        self.packet_size = packet_size;
        self.first_packet_offset = first_offset;
        self.format.packet_count = packet_count;
        self.format.packet_size_bytes = packet_size;
        unsafe {
            *out_packets = packets;
            ring::on_stream_packets_allocated(packet_count, packet_size);
        }
        STATUS_SUCCESS_VAL
    }

    unsafe fn free_rt_packets(&mut self, packets: PACX_RTPACKET, packet_count: ULONG) {
        if !packets.is_null() {
            for i in 0..(packet_count as usize).min(MAX_RT_PACKETS) {
                unsafe {
                    let pkt = &*packets.add(i);
                    let mdl = pkt.RtPacketBuffer.u.MdlType.Mdl;
                    if !mdl.is_null() {
                        let base = self.packet_bases[i];
                        IoFreeMdl(mdl);
                        if !base.is_null() {
                            ExFreePoolWithTag(base.cast(), POOL_TAG);
                        }
                    }
                    self.packet_bases[i] = ptr::null_mut();
                    self.packet_data_ptrs[i] = ptr::null_mut();
                    self.slot_meta[i] = SlotMeta::default();
                }
            }
            unsafe {
                ExFreePoolWithTag(packets.cast(), POOL_TAG);
            }
        }
        self.packet_count = 0;
        self.packet_size = 0;
    }

    unsafe fn prepare_hardware(&mut self) -> NTSTATUS {
        if self.state == StreamState::Pause {
            return STATUS_SUCCESS_VAL;
        }
        if self.state != StreamState::Stop {
            return STATUS_INVALID_STATE_TRANSITION_VAL;
        }

        let mut timer_cfg = wdf_timer_config_init(Some(evt_stream_timer));
        let mut attrs = wdf_object_attributes_init();
        attrs.ParentObject = self.stream as WDFOBJECT;
        let mut timer: WDFTIMER = ptr::null_mut();
        let st = unsafe { wdf_timer_create(&mut timer_cfg, &mut attrs, &mut timer) };
        if !nt_success(st) {
            return st;
        }
        unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            self.timer = timer;
            self.state = StreamState::Pause;
            KeReleaseSpinLock(&mut self.lock, irql);
            bind_timer_to_stream(self.stream, timer);
        }
        STATUS_SUCCESS_VAL
    }

    unsafe fn release_hardware(&mut self) -> NTSTATUS {
        let (was_running, timer) = unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            let prev = self.state;
            let t = self.timer;
            self.state = StreamState::Stop;
            self.timer = ptr::null_mut();
            self.glitch_adjust_hns = 0;
            KeReleaseSpinLock(&mut self.lock, irql);
            (prev == StreamState::Run, t)
        };
        if !timer.is_null() {
            unsafe {
                wdf_timer_stop(timer, 1);
                KeFlushQueuedDpcs();
                bind_timer_to_stream(self.stream, ptr::null_mut());
                wdf_object_delete(timer as WDFOBJECT);
            }
        }
        if was_running {
            unsafe {
                ring::on_stream_paused();
            }
        }
        self.current_packet.store(0, Ordering::Release);
        self.bytes_consumed.store(0, Ordering::Release);
        STATUS_SUCCESS_VAL
    }

    unsafe fn run(&mut self) -> NTSTATUS {
        let now_qpc = unsafe { KeQueryPerformanceCounter(ptr::null_mut()).QuadPart };
        let fmt = unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            if self.state == StreamState::Run {
                KeReleaseSpinLock(&mut self.lock, irql);
                return STATUS_SUCCESS_VAL;
            }
            if self.state != StreamState::Pause {
                KeReleaseSpinLock(&mut self.lock, irql);
                return STATUS_INVALID_STATE_TRANSITION_VAL;
            }
            self.start_time_hns = qpc_to_hns(now_qpc, self.qpc_freq);
            self.start_pos_bytes = self.bytes_consumed.load(Ordering::Acquire);
            self.glitch_adjust_hns = 0;
            self.clock_adj_ppm.store(0, Ordering::Release);
            self.state = StreamState::Run;
            let f = self.format;
            KeReleaseSpinLock(&mut self.lock, irql);
            f
        };

        unsafe {
            ring::on_stream_run(fmt);
            self.schedule_next_tick();
        }
        STATUS_SUCCESS_VAL
    }

    unsafe fn pause(&mut self) -> NTSTATUS {
        let timer = unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            if self.state == StreamState::Pause {
                KeReleaseSpinLock(&mut self.lock, irql);
                return STATUS_SUCCESS_VAL;
            }
            if self.state != StreamState::Run {
                KeReleaseSpinLock(&mut self.lock, irql);
                return STATUS_INVALID_STATE_TRANSITION_VAL;
            }
            self.state = StreamState::Pause;
            let t = self.timer;
            KeReleaseSpinLock(&mut self.lock, irql);
            t
        };
        if !timer.is_null() {
            unsafe {
                wdf_timer_stop(timer, 1);
            }
        }
        unsafe {
            ring::on_stream_paused();
        }
        STATUS_SUCCESS_VAL
    }

    unsafe fn set_render_packet(
        &mut self,
        packet: ULONG,
        flags: ULONG,
        eos_length: ULONG,
    ) -> NTSTATUS {
        unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            let cur = self.current_packet.load(Ordering::Acquire);
            if self.state == StreamState::Run && packet < cur {
                KeReleaseSpinLock(&mut self.lock, irql);
                return STATUS_DATA_LATE_ERROR_VAL;
            }
            if self.packet_count == 0 {
                KeReleaseSpinLock(&mut self.lock, irql);
                return STATUS_INVALID_DEVICE_STATE_VAL;
            }
            if packet > cur.saturating_add(self.packet_count) {
                KeReleaseSpinLock(&mut self.lock, irql);
                return STATUS_DATA_OVERRUN_VAL;
            }
            let idx = (packet as usize) % (self.packet_count as usize);
            self.slot_meta[idx] = SlotMeta {
                flags,
                eos_len: eos_length,
                valid: true,
            };
            KeReleaseSpinLock(&mut self.lock, irql);
        }
        STATUS_SUCCESS_VAL
    }

    unsafe fn on_timer_tick(&mut self) {
        let (cur, data_ptr, packet_size, meta) = unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            if self.state != StreamState::Run || self.packet_count == 0 || self.packet_size == 0 {
                KeReleaseSpinLock(&mut self.lock, irql);
                return;
            }
            let cur = self.current_packet.load(Ordering::Acquire);
            let idx = (cur as usize) % (self.packet_count as usize);
            let data_ptr = self.packet_data_ptrs[idx];
            let pkt_size = self.packet_size as usize;
            let meta = self.slot_meta[idx];
            self.slot_meta[idx].valid = false;
            KeReleaseSpinLock(&mut self.lock, irql);
            (cur, data_ptr, pkt_size, meta)
        };

        let is_silent = meta.valid && ((meta.flags & AUDCLNT_BUFFERFLAGS_SILENT) != 0);
        let is_eos = meta.valid && ((meta.flags & KSSTREAM_EOS_FLAG) != 0);
        let valid_len = if is_eos && (meta.eos_len as usize) <= packet_size {
            meta.eos_len as usize
        } else {
            packet_size
        };

        let adj_ppm = unsafe {
            ring::push_packet_bytes(data_ptr, valid_len, is_silent, is_eos, packet_size)
        };
        self.clock_adj_ppm.store(adj_ppm, Ordering::Release);

        if !data_ptr.is_null() {
            unsafe {
                ptr::write_bytes(data_ptr, 0, packet_size);
            }
        }

        let completed = cur as u64;
        self.current_packet.store(cur.wrapping_add(1), Ordering::Release);
        self.bytes_consumed
            .fetch_add(packet_size as u64, Ordering::AcqRel);

        let qpc = unsafe { KeQueryPerformanceCounter(ptr::null_mut()).QuadPart as u64 };
        unsafe {
            let _ = acx_rt_stream_notify_packet_complete(self.stream, completed, qpc);
            self.schedule_next_tick();
        }
    }

    unsafe fn schedule_next_tick(&mut self) {
        let (timer, due_hns) = unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            if self.state != StreamState::Run || self.timer.is_null() || self.avg_bytes_per_sec == 0
            {
                KeReleaseSpinLock(&mut self.lock, irql);
                return;
            }
            let next_pkt = self.current_packet.load(Ordering::Acquire) as u64 + 1;
            let next_pos_bytes = next_pkt * (self.packet_size as u64);
            let since_resume = next_pos_bytes.saturating_sub(self.start_pos_bytes);
            let nominal_hns = ((since_resume as u128 * HNS_PER_SEC as u128)
                / (self.avg_bytes_per_sec as u128)) as u64;

            let target_hns = self
                .start_time_hns
                .saturating_add(self.glitch_adjust_hns)
                .saturating_add(nominal_hns);

            let now_qpc = KeQueryPerformanceCounter(ptr::null_mut()).QuadPart;
            let now_hns = qpc_to_hns(now_qpc, self.qpc_freq);

            let mut delta_hns: i64 = (target_hns as i64) - (now_hns as i64);
            let pkt_hns = ((self.packet_size as u64 * HNS_PER_SEC) / (self.avg_bytes_per_sec as u64))
                .max(10_000) as i64;

            let adj_ppm = self.clock_adj_ppm.load(Ordering::Acquire) as i64;
            if adj_ppm != 0 {
                let nudge = (pkt_hns * adj_ppm) / 1_000_000;
                delta_hns += nudge;
                if nudge >= 0 {
                    self.glitch_adjust_hns = self.glitch_adjust_hns.saturating_add(nudge as u64);
                } else {
                    self.glitch_adjust_hns =
                        self.glitch_adjust_hns.saturating_sub((-nudge) as u64);
                }
            }

            if delta_hns < 5_000 {
                let late = (5_000 - delta_hns) as u64;
                self.glitch_adjust_hns = self.glitch_adjust_hns.saturating_add(late);
                delta_hns = 5_000;
            }

            let t = self.timer;
            KeReleaseSpinLock(&mut self.lock, irql);
            (t, delta_hns)
        };

        unsafe {
            let _ = wdf_timer_start(timer, -due_hns);
        }
    }

    unsafe fn get_presentation_position(
        &mut self,
        pos_blocks: *mut ULONGLONG,
        qpc_pos: *mut ULONGLONG,
    ) -> NTSTATUS {
        let now_qpc = unsafe { KeQueryPerformanceCounter(ptr::null_mut()).QuadPart };
        let (total_bytes, ba) = unsafe {
            let irql = KeAcquireSpinLockRaiseToDpc(&mut self.lock);
            let tb = if self.state == StreamState::Run && self.avg_bytes_per_sec > 0 {
                let now_hns = qpc_to_hns(now_qpc, self.qpc_freq);
                let elapsed_hns = now_hns
                    .saturating_sub(self.start_time_hns)
                    .saturating_sub(self.glitch_adjust_hns);
                let interp = self.start_pos_bytes
                    + ((elapsed_hns as u128 * self.avg_bytes_per_sec as u128)
                        / (HNS_PER_SEC as u128)) as u64;
                interp.max(self.bytes_consumed.load(Ordering::Acquire))
            } else {
                self.bytes_consumed.load(Ordering::Acquire)
            };
            let ba = (self.block_align as u64).max(1);
            KeReleaseSpinLock(&mut self.lock, irql);
            (tb, ba)
        };

        unsafe {
            *pos_blocks = total_bytes / ba;
            *qpc_pos = now_qpc as u64;
        }
        STATUS_SUCCESS_VAL
    }
}

// ---- ACX & WDF Callbacks ----

pub unsafe extern "C" fn evt_circuit_create_stream(
    device: WDFDEVICE,
    circuit: ACXCIRCUIT,
    _pin: ACXPIN,
    mut stream_init: PACXSTREAM_INIT,
    stream_format: ACXDATAFORMAT,
    signal_processing_mode: *const GUID,
    _var_arguments: ACXOBJECTBAG,
) -> NTSTATUS {
    if let Err(st) = unsafe { ring::check_and_record_caller(STAGE_CREATE_STREAM) } {
        return st;
    }

    let (sample_rate, channels, bits, valid_bits, block_align, avg_bytes, subformat) = unsafe {
        (
            acx_dataformat_get_sample_rate(stream_format),
            acx_dataformat_get_channels(stream_format),
            acx_dataformat_get_bits_per_sample(stream_format) as u16,
            acx_dataformat_get_valid_bits_per_sample(stream_format) as u16,
            acx_dataformat_get_block_align(stream_format) as u16,
            acx_dataformat_get_avg_bytes_per_sec(stream_format),
            acx_dataformat_get_subformat(stream_format),
        )
    };
    let is_pcm = guid_eq(&subformat, &GUID_KSDATAFORMAT_SUBTYPE_PCM);
    if !is_pcm || channels != 2 || sample_rate < 44_100 || sample_rate > 384_000 {
        return STATUS_NOT_SUPPORTED_VAL;
    }

    let proc_mode_bytes = if !signal_processing_mode.is_null() {
        guid_to_bytes(unsafe { &*signal_processing_mode })
    } else {
        [0u8; 16]
    };

    let fmt = ApStreamFormat {
        sample_rate,
        channels,
        bits_per_sample: bits,
        valid_bits_per_sample: if valid_bits == 0 { bits } else { valid_bits },
        block_align,
        is_float: 0,
        packet_size_bytes: 0,
        packet_count: 0,
        reserved: 0,
    };

    let mut stream_cb = acx_stream_callbacks_init();
    stream_cb.EvtAcxStreamPrepareHardware = Some(evt_stream_prepare_hardware);
    stream_cb.EvtAcxStreamReleaseHardware = Some(evt_stream_release_hardware);
    stream_cb.EvtAcxStreamRun = Some(evt_stream_run);
    stream_cb.EvtAcxStreamPause = Some(evt_stream_pause);
    let st = unsafe { acx_stream_init_assign_stream_callbacks(stream_init, &mut stream_cb) };
    if !nt_success(st) {
        return st;
    }

    let mut rt_cb = acx_rt_stream_callbacks_init();
    rt_cb.EvtAcxStreamGetHwLatency = Some(evt_stream_get_hw_latency);
    rt_cb.EvtAcxStreamAllocateRtPackets = Some(evt_stream_allocate_rt_packets);
    rt_cb.EvtAcxStreamFreeRtPackets = Some(evt_stream_free_rt_packets);
    rt_cb.EvtAcxStreamSetRenderPacket = Some(evt_stream_set_render_packet);
    rt_cb.EvtAcxStreamGetCurrentPacket = Some(evt_stream_get_current_packet);
    rt_cb.EvtAcxStreamGetPresentationPosition = Some(evt_stream_get_presentation_position);
    let st = unsafe { acx_stream_init_assign_rt_stream_callbacks(stream_init, &mut rt_cb) };
    if !nt_success(st) {
        return st;
    }

    unsafe {
        acx_stream_init_set_rt_supports_notifications(stream_init);
    }

    let mut attrs = wdf_object_attributes_init();
    attrs.EvtDestroyCallback = Some(evt_stream_destroy);
    let mut stream: ACXSTREAM = ptr::null_mut();
    let st = unsafe { acx_rt_stream_create(device, circuit, &mut attrs, &mut stream_init, &mut stream) };
    if !nt_success(st) {
        return st;
    }

    let raw = unsafe {
        ExAllocatePool2(POOL_FLAG_NON_PAGED, size_of::<RenderStream>() as u64, POOL_TAG)
    } as *mut RenderStream;
    if raw.is_null() {
        return STATUS_INSUFFICIENT_RESOURCES_VAL;
    }

    unsafe {
        raw.write(RenderStream {
            lock: 0,
            stream,
            format: fmt,
            avg_bytes_per_sec: avg_bytes,
            block_align: block_align as u32,
            state: StreamState::Stop,
            timer: ptr::null_mut(),
            packet_count: 0,
            packet_size: 0,
            first_packet_offset: 0,
            packet_bases: [ptr::null_mut(); MAX_RT_PACKETS],
            packet_data_ptrs: [ptr::null_mut(); MAX_RT_PACKETS],
            slot_meta: [SlotMeta::default(); MAX_RT_PACKETS],
            current_packet: AtomicU32::new(0),
            bytes_consumed: AtomicU64::new(0),
            start_time_hns: 0,
            start_pos_bytes: 0,
            glitch_adjust_hns: 0,
            clock_adj_ppm: AtomicI32::new(0),
            qpc_freq: ring::qpc_frequency(),
        });
        KeInitializeSpinLock(ptr::addr_of_mut!((*raw).lock));

        if !register_stream(stream, raw) {
            ExFreePoolWithTag(raw.cast(), POOL_TAG);
            return STATUS_INSUFFICIENT_RESOURCES_VAL;
        }
        ring::on_stream_created(fmt, proc_mode_bytes);
    }

    STATUS_SUCCESS_VAL
}

unsafe extern "C" fn evt_stream_destroy(obj: WDFOBJECT) {
    let stream = obj as ACXSTREAM;
    let engine = unsafe { unregister_stream(stream) };
    if !engine.is_null() {
        unsafe {
            let _ = (*engine).release_hardware();
            ExFreePoolWithTag(engine.cast(), POOL_TAG);
            ring::on_stream_destroyed();
        }
    }
}

unsafe extern "C" fn evt_stream_get_hw_latency(
    _stream: ACXSTREAM,
    fifo_size: *mut ULONG,
    delay: *mut ULONG,
) -> NTSTATUS {
    unsafe {
        if !fifo_size.is_null() {
            *fifo_size = 0;
        }
        if !delay.is_null() {
            *delay = 0;
        }
    }
    STATUS_SUCCESS_VAL
}

unsafe extern "C" fn evt_stream_allocate_rt_packets(
    stream: ACXSTREAM,
    packet_count: ULONG,
    packet_size: ULONG,
    packets: *mut PACX_RTPACKET,
) -> NTSTATUS {
    if let Err(st) = unsafe { ring::check_and_record_caller(STAGE_ALLOC_RT_PACKETS) } {
        return st;
    }
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() {
        return STATUS_INVALID_DEVICE_STATE_VAL;
    }
    unsafe { (*engine).allocate_rt_packets(packet_count, packet_size, packets) }
}

unsafe extern "C" fn evt_stream_free_rt_packets(
    stream: ACXSTREAM,
    packets: PACX_RTPACKET,
    packet_count: ULONG,
) {
    let engine = unsafe { lookup_by_stream(stream) };
    if !engine.is_null() {
        unsafe {
            (*engine).free_rt_packets(packets, packet_count);
        }
    }
}

unsafe extern "C" fn evt_stream_prepare_hardware(stream: ACXSTREAM) -> NTSTATUS {
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() {
        return STATUS_INVALID_DEVICE_STATE_VAL;
    }
    unsafe { (*engine).prepare_hardware() }
}

unsafe extern "C" fn evt_stream_release_hardware(stream: ACXSTREAM) -> NTSTATUS {
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() {
        return STATUS_SUCCESS_VAL;
    }
    unsafe { (*engine).release_hardware() }
}

unsafe extern "C" fn evt_stream_run(stream: ACXSTREAM) -> NTSTATUS {
    if let Err(st) = unsafe { ring::check_and_record_caller(STAGE_RUN) } {
        return st;
    }
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() {
        return STATUS_INVALID_DEVICE_STATE_VAL;
    }
    unsafe { (*engine).run() }
}

unsafe extern "C" fn evt_stream_pause(stream: ACXSTREAM) -> NTSTATUS {
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() {
        return STATUS_SUCCESS_VAL;
    }
    unsafe { (*engine).pause() }
}

unsafe extern "C" fn evt_stream_set_render_packet(
    stream: ACXSTREAM,
    packet: ULONG,
    flags: ULONG,
    eos_length: ULONG,
) -> NTSTATUS {
    if let Err(st) = unsafe { ring::check_and_record_caller(STAGE_SET_RENDER_PACKET) } {
        return st;
    }
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() {
        return STATUS_INVALID_DEVICE_STATE_VAL;
    }
    unsafe { (*engine).set_render_packet(packet, flags, eos_length) }
}

unsafe extern "C" fn evt_stream_get_current_packet(
    stream: ACXSTREAM,
    current_packet: PULONG,
) -> NTSTATUS {
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() || current_packet.is_null() {
        return STATUS_INVALID_DEVICE_STATE_VAL;
    }
    unsafe {
        *current_packet = (*engine).current_packet.load(Ordering::Acquire);
    }
    STATUS_SUCCESS_VAL
}

unsafe extern "C" fn evt_stream_get_presentation_position(
    stream: ACXSTREAM,
    position_in_blocks: PULONGLONG,
    qpc_position: PULONGLONG,
) -> NTSTATUS {
    let engine = unsafe { lookup_by_stream(stream) };
    if engine.is_null() || position_in_blocks.is_null() || qpc_position.is_null() {
        return STATUS_INVALID_DEVICE_STATE_VAL;
    }
    unsafe { (*engine).get_presentation_position(position_in_blocks, qpc_position) }
}

unsafe extern "C" fn evt_stream_timer(timer: WDFTIMER) {
    let engine = unsafe { lookup_by_timer(timer) };
    if !engine.is_null() {
        unsafe {
            (*engine).on_timer_tick();
        }
    }
}
