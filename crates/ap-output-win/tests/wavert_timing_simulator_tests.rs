// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Causal virtual-timeline DMA simulator (`FakeWaveRtDevice`) and S1–S6 timing invariant tests
//! (`WDMKS_TIMING_TESTS_REVIEW.md` §4.2).
//!
//! Unlike parity-derived mocks, the simulated hardware DMA cursor advances continuously on an
//! independent virtual timeline (`rate * (1 + ppm / 1e6)`), tapping every frame out of the ring
//! buffer as the read pointer crosses it and verifying:
//! 1. Strictly `+1` frame index sequence on healthy runs (no repeats, skips, or reordering).
//! 2. Visible-failures guarantee: every tap anomaly is counted in `PlaybackStats`.
//! 3. Active-half protection (S2): rescue path never writes into the half DMA is currently reading.
//! 4. Mid-half underruns (S5) are counted in `partial_underrun_halves`.
//! 5. 32-bit monotonic cursor wrap near `u32::MAX` (S6) stays glitch-free.
//! 6. Seeded determinism and long ±600 ppm drift soak with zero uncounted deviations.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;
use ap_output_win::sink::{OutputError, PlaybackStats};
use ap_output_win::wdm_ks::{
    PositionWrapMode, StreamBlock, StreamingWaveRtPump, WaveRtClock, WaveRtDevice,
    WaveRtFramePump, WaveRtPositionSample, WaveRtRingLayout, WaveRtWaitResult,
    run_wavert_pump_with_device,
};

#[derive(Debug, Clone, Copy)]
enum SimPositionModel {
    None,
    /// `usbaudio2.sys`-style cursor quantized strictly to `0` or `half_bytes`.
    Quantized0Half,
    /// Continuous modular register wrapping at `buf_len` with optional FIFO lag.
    ContinuousModular { lag_bytes: u64 },
    /// Monotonic 32-bit byte counter starting near `start_offset` (wraps modulo 2^32, S6).
    Monotonic32 { start_offset: u64 },
}

#[derive(Debug, Clone)]
struct SimConfig {
    sample_rate_hz: u32,
    half_frames: usize,
    dma_ppm: f64,
    pos_model: SimPositionModel,
    /// Wake times relative to `start_run` in nanoseconds (if empty, auto-generated at `wake_period_ns`).
    explicit_wakes_ns: Vec<u64>,
    /// Override wake cadence (ns) when `explicit_wakes_ns` is empty (e.g. S1: 52.856 ms vs 50 ms DMA).
    wake_period_ns: Option<u64>,
    /// Optional seeded jitter (max ns) added to auto-generated wakes.
    jitter_max_ns: u64,
    /// Seed for deterministic PRNG.
    seed: u64,
    /// Max wakes before timeout guard (prevents infinite test hangs).
    max_wakes: usize,
}

struct SharedSimClock {
    now_ns: Rc<Cell<u64>>,
}

impl WaveRtClock for SharedSimClock {
    fn now_since_run(&self) -> Duration {
        Duration::from_nanos(self.now_ns.get())
    }
}

struct FakeWaveRtDevice {
    cfg: SimConfig,
    layout: WaveRtRingLayout,
    ring: Vec<u8>,
    now_ns: Rc<Cell<u64>>,
    next_wake_idx: usize,
    prng: u64,
    /// Total frames already consumed/tapped by the simulated DMA engine.
    dma_frames_consumed: u64,
    /// Exact frame index stamps read out of the ring by the causal DMA cursor.
    tapped_left_samples: Vec<i32>,
    /// Ring snapshots before each wake to detect active-half overwrites.
    ring_before_wake: Vec<u8>,
    active_half_at_wake: usize,
    active_half_overwrites: usize,
    running: bool,
}

impl FakeWaveRtDevice {
    fn new(cfg: SimConfig) -> (Self, SharedSimClock) {
        let bpf = DacWireFormat::Int32.bytes_per_frame();
        let buf_len = cfg.half_frames * 2 * bpf;
        let layout = WaveRtRingLayout::new(buf_len, bpf).unwrap();
        let now_ns = Rc::new(Cell::new(0));
        let clock = SharedSimClock {
            now_ns: Rc::clone(&now_ns),
        };
        let dev = Self {
            prng: cfg.seed.max(1),
            cfg,
            layout,
            ring: vec![0u8; buf_len],
            now_ns,
            next_wake_idx: 0,
            dma_frames_consumed: 0,
            tapped_left_samples: Vec::new(),
            ring_before_wake: vec![0u8; buf_len],
            active_half_at_wake: 0,
            active_half_overwrites: 0,
            running: false,
        };
        (dev, clock)
    }

    fn next_u64(&mut self) -> u64 {
        self.prng = self.prng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.prng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn dma_frames_at_ns(&self, t_ns: u64) -> u64 {
        let rate = f64::from(self.cfg.sample_rate_hz) * (1.0 + self.cfg.dma_ppm * 1e-6);
        ((t_ns as f64) * 1e-9 * rate).floor().max(0.0) as u64
    }

    /// Advance the causal DMA cursor up to `target_ns`, reading every frame the hardware clocks out.
    fn advance_dma_to(&mut self, target_ns: u64) {
        if !self.running {
            return;
        }
        let target_frames = self.dma_frames_at_ns(target_ns);
        let total_ring_frames = (self.cfg.half_frames * 2) as u64;
        let bpf = self.layout.bytes_per_frame();
        while self.dma_frames_consumed < target_frames {
            let ring_frame = (self.dma_frames_consumed % total_ring_frames) as usize;
            let off = ring_frame * bpf;
            let l = i32::from_le_bytes(self.ring[off..off + 4].try_into().unwrap());
            self.tapped_left_samples.push(l);
            self.dma_frames_consumed += 1;
        }
    }

    fn check_active_half_overwrite(&mut self) {
        if !self.running || self.next_wake_idx == 0 {
            return;
        }
        let active_range = self.layout.half_range(self.active_half_at_wake);
        if self.ring[active_range.clone()] != self.ring_before_wake[active_range] {
            self.active_half_overwrites += 1;
        }
    }

    fn next_wake_time_ns(&mut self) -> Option<u64> {
        if self.next_wake_idx >= self.cfg.max_wakes {
            return None;
        }
        let idx = self.next_wake_idx;
        self.next_wake_idx += 1;
        if !self.cfg.explicit_wakes_ns.is_empty() {
            return self.cfg.explicit_wakes_ns.get(idx).copied();
        }
        // Hardware WaveRT DMA interrupts fire on DMA half-buffer crossings (at `dma_rate`, never
        // before the DMA cursor crosses the boundary), while the host wall clock (`SharedSimClock`)
        // ticks at nominal `sample_rate_hz` — OR a caller can supply an explicit decoupled
        // `wake_period_ns` (e.g. nominal host timer) to test uncoupled wake-vs-DMA clock drift.
        let base = if let Some(period_ns) = self.cfg.wake_period_ns {
            period_ns.saturating_mul((idx + 1) as u64)
        } else {
            let dma_rate = f64::from(self.cfg.sample_rate_hz) * (1.0 + self.cfg.dma_ppm * 1e-6);
            ((((idx + 1) as f64) * (self.cfg.half_frames as f64) / dma_rate) * 1e9).ceil() as u64
        };
        let jitter = if self.cfg.jitter_max_ns > 0 {
            self.next_u64() % self.cfg.jitter_max_ns
        } else {
            0
        };
        Some(base.saturating_add(jitter))
    }
}

impl WaveRtDevice for FakeWaveRtDevice {
    fn device_name(&self) -> &str {
        "FakeWaveRtDevice"
    }

    fn layout(&self) -> WaveRtRingLayout {
        self.layout
    }

    fn actual_buffer_bytes(&self) -> u32 {
        self.layout.buf_len() as u32
    }

    fn is_modular_position(&self) -> bool {
        matches!(
            self.cfg.pos_model,
            SimPositionModel::Quantized0Half | SimPositionModel::ContinuousModular { .. }
        )
    }

    fn ring_mut(&mut self) -> &mut [u8] {
        &mut self.ring
    }

    fn wait_notification(&mut self, _timeout: Duration) -> WaveRtWaitResult {
        // Before advancing to the next wake, verify whether the previous wake wrote into the
        // half the DMA cursor was actively reading.
        self.check_active_half_overwrite();

        let Some(wake_ns) = self.next_wake_time_ns() else {
            return WaveRtWaitResult::Timeout;
        };
        let wake_ns = wake_ns.max(self.now_ns.get());
        self.advance_dma_to(wake_ns);
        self.now_ns.set(wake_ns);

        // Snapshot ring and record which half DMA is actively reading at this wake instant.
        let total_ring_frames = (self.cfg.half_frames * 2) as u64;
        let ring_frame = (self.dma_frames_consumed % total_ring_frames) as usize;
        self.active_half_at_wake = if ring_frame < self.cfg.half_frames {
            0
        } else {
            1
        };
        self.ring_before_wake.copy_from_slice(&self.ring);
        WaveRtWaitResult::Signaled
    }

    fn read_position(&mut self) -> Option<WaveRtPositionSample> {
        let bpf = self.layout.bytes_per_frame() as u64;
        let buf = self.layout.buf_len() as u64;
        let half = self.layout.half_len() as u64;
        let total_bytes = self.dma_frames_consumed.saturating_mul(bpf);
        match self.cfg.pos_model {
            SimPositionModel::None => None,
            SimPositionModel::Quantized0Half => {
                let in_ring = total_bytes % buf;
                let snapped = if in_ring < half { 0 } else { half };
                Some(WaveRtPositionSample {
                    offset_bytes: snapped,
                    wrap_mode: PositionWrapMode::Modular,
                })
            }
            SimPositionModel::ContinuousModular { lag_bytes } => {
                let lagged = total_bytes.saturating_sub(lag_bytes) % buf;
                Some(WaveRtPositionSample {
                    offset_bytes: lagged,
                    wrap_mode: PositionWrapMode::Modular,
                })
            }
            SimPositionModel::Monotonic32 { start_offset } => {
                let wrapped32 = (start_offset.wrapping_add(total_bytes)) & 0xFFFF_FFFF;
                Some(WaveRtPositionSample {
                    offset_bytes: wrapped32,
                    wrap_mode: PositionWrapMode::Auto,
                })
            }
        }
    }

    fn barrier(&self) {}

    fn start_run(&mut self) -> Result<(), OutputError> {
        self.running = true;
        Ok(())
    }

    fn stop_clean(&mut self) {
        self.check_active_half_overwrite();
        self.running = false;
    }
}

/// Analyze tapped left-channel frame stamps (`1..=total_frames`, with `0` = silence).
/// Review #10 TV4: also counts `mid_zeros` (zero frames occurring AFTER the first audio frame
/// and BEFORE the last audio frame) so mid-stream underrun dropouts are never silently stripped!
/// Returns `(total_non_zero, repeats, skips, non_monotonic, mid_zeros)`.
fn analyze_tapped_sequence(tapped: &[i32]) -> (usize, usize, usize, usize, usize) {
    let first_nz = tapped.iter().position(|&v| v != 0);
    let last_nz = tapped.iter().rposition(|&v| v != 0);
    let mid_zeros = match (first_nz, last_nz) {
        (Some(f), Some(l)) if l > f => tapped[f..=l].iter().filter(|&&v| v == 0).count(),
        _ => 0,
    };
    let non_zero: Vec<i32> = tapped.iter().copied().filter(|&v| v != 0).collect();
    if non_zero.is_empty() {
        return (0, 0, 0, 0, 0);
    }
    let mut repeats = 0usize;
    let mut skips = 0usize;
    let mut non_mono = 0usize;
    for w in non_zero.windows(2) {
        let diff = w[1] - w[0];
        if diff == 1 {
            continue;
        } else if diff <= 0 {
            repeats += 1;
            non_mono += 1;
        } else {
            skips += (diff - 1) as usize;
        }
    }
    (non_zero.len(), repeats, skips, non_mono, mid_zeros)
}

fn make_stamped_frames(n: usize) -> Vec<[i32; 2]> {
    (1..=n).map(|i| [i as i32, -(i as i32)]).collect()
}

fn run_sim(
    cfg: SimConfig,
    frames: &[[i32; 2]],
) -> (Result<PlaybackStats, OutputError>, FakeWaveRtDevice) {
    let sr = cfg.sample_rate_hz;
    let (mut dev, clock) = FakeWaveRtDevice::new(cfg);
    let mut pump = WaveRtFramePump::new(frames, DacWireFormat::Int32);
    let stop = AtomicBool::new(false);
    let res = run_wavert_pump_with_device(
        &mut dev,
        &clock,
        sr,
        DacWireFormat::Int32,
        &mut pump,
        &stop,
        true,
        false,
    );
    (res, dev)
}

#[test]
fn causal_dma_clean_quantized_and_continuous_streams_are_strictly_monotonic_zero_anomalies() {
    let half_frames = 2_400; // 50 ms @ 48 kHz
    let total_frames = half_frames * 20;
    let frames = make_stamped_frames(total_frames);

    for (label, model) in [
        ("quantized_0_half", SimPositionModel::Quantized0Half),
        (
            "continuous_modular",
            SimPositionModel::ContinuousModular { lag_bytes: 0 },
        ),
        (
            "continuous_lagging",
            SimPositionModel::ContinuousModular { lag_bytes: 2_048 },
        ),
        ("no_position", SimPositionModel::None),
    ] {
        let cfg = SimConfig {
            sample_rate_hz: 48_000,
            half_frames,
            dma_ppm: 0.0,
            pos_model: model,
            explicit_wakes_ns: Vec::new(),
            wake_period_ns: None,
            jitter_max_ns: 2_000_000, // up to 2 ms jitter
            seed: 0x2026_0001,
            max_wakes: 200,
        };
        let (res, dev) = run_sim(cfg, &frames);
        let stats = res.unwrap_or_else(|e| panic!("{label} failed: {e}"));
        let (nz, repeats, skips, non_mono, mid_zeros) = analyze_tapped_sequence(&dev.tapped_left_samples);
        assert_eq!(nz, total_frames, "{label}: wrong non-zero frame count");
        assert_eq!(repeats, 0, "{label}: unexpected repeats");
        assert_eq!(skips, 0, "{label}: unexpected skips");
        assert_eq!(non_mono, 0, "{label}: non-monotonic tap");
        assert_eq!(mid_zeros, 0, "{label}: unexpected mid-stream silence dropout");
        assert_eq!(
            dev.active_half_overwrites, 0,
            "{label}: overwrote active DMA half!"
        );
        assert!(
            !stats.has_timing_anomalies(),
            "{label}: unexpected anomaly counters: {}",
            stats.timing_summary()
        );
        assert!(stats.max_wake_interval_ns > 0);
        assert!(!stats.non_zero_histogram_bins().is_empty());
    }
}

#[test]
fn s1_mismatched_52_856ms_wake_cadence_vs_50ms_dma_counts_every_deviation() {
    // Issue #22 S1: wake cadence 52.856 ms vs true 50.000 ms DMA half with quantized 0/half cursor.
    // Every tap deviation MUST be made visible via PlaybackStats counters (zero silent glitches).
    let half_frames = 2_400; // 50.000 ms @ 48 kHz
    let total_frames = half_frames * 60;
    let frames = make_stamped_frames(total_frames);
    let cfg = SimConfig {
        sample_rate_hz: 48_000,
        half_frames,
        dma_ppm: 0.0,
        pos_model: SimPositionModel::Quantized0Half,
        explicit_wakes_ns: Vec::new(),
        wake_period_ns: Some(52_856_000), // +5.71% mismatch
        jitter_max_ns: 0,
        seed: 0x5101,
        max_wakes: 400,
    };
    let (res, dev) = run_sim(cfg, &frames);
    let stats = res.expect("S1 sim should complete cleanly");
    let (nz, repeats, skips, _non_mono, mid_zeros) = analyze_tapped_sequence(&dev.tapped_left_samples);
    // Review #10 TV5: unconditionally require real audio tapped AND visible anomaly reporting!
    assert!(nz > 0, "S1 must tap real audio frames");
    assert!(
        repeats > 0 || skips > 0 || mid_zeros > 0,
        "S1 +5.71% wake mismatch must produce observable slip (nz={nz})"
    );
    assert!(
        stats.has_timing_anomalies(),
        "S1 visible-failures violation: tap had repeats={repeats} skips={skips} mid_zeros={mid_zeros} but stats reported 0 anomalies ({})",
        stats.timing_summary()
    );
}

#[test]
fn s2_quantized_stuck_position_never_overwrites_active_dma_half() {
    // Issue #22 S2: wake arrives at 0.96*half on a boundary-quantized 0/half driver while DMA
    // is STILL in the old half (position hasn't flipped yet). The narrowed rescue guard must
    // reject this as Spurious rather than writing into the active half!
    let half_frames = 2_400; // 50 ms @ 48 kHz
    let total_frames = half_frames * 12;
    let frames = make_stamped_frames(total_frames);

    let mut wakes = Vec::new();
    // Normal wakes 1..4 at 50, 100, 150, 200 ms.
    for k in 1..=4_u64 {
        wakes.push(k * 50_000_000 + 500_000);
    }
    // Fault wake at 248 ms (0.96 half after 200 ms: DMA is STILL in half 0, quantized pos == 0!).
    wakes.push(248_000_000);
    // Resume normal wakes 5..20.
    for k in 5..=20_u64 {
        wakes.push(k * 50_000_000 + 500_000);
    }

    let cfg = SimConfig {
        sample_rate_hz: 48_000,
        half_frames,
        dma_ppm: 0.0,
        pos_model: SimPositionModel::Quantized0Half,
        explicit_wakes_ns: wakes,
        wake_period_ns: None,
        jitter_max_ns: 0,
        seed: 0x5202,
        max_wakes: 64,
    };
    let (res, dev) = run_sim(cfg, &frames);
    let stats = res.expect("S2 sim failed");
    assert_eq!(
        dev.active_half_overwrites, 0,
        "S2 regression: pump overwrote active DMA half during early quantized wake!"
    );
    assert!(
        stats.spurious_wakes >= 1,
        "expected early quantized wake to be classified as spurious, got {}",
        stats.timing_summary()
    );
    let (nz, repeats, skips, _, mid_zeros) = analyze_tapped_sequence(&dev.tapped_left_samples);
    assert_eq!(nz, total_frames);
    assert_eq!(repeats, 0);
    assert_eq!(skips, 0);
    assert_eq!(mid_zeros, 0);
}

#[test]
fn s3_merged_coalesced_events_are_counted_visibly() {
    // Issue #22 S3: drop wake #3 (150 ms) so wake #4 arrives after 2 full halves (200.5 ms).
    let half_frames = 2_400;
    let total_frames = half_frames * 12;
    let frames = make_stamped_frames(total_frames);

    let mut wakes = Vec::new();
    for k in 1..=20_u64 {
        if k == 3 {
            continue; // coalesced / dropped interrupt
        }
        wakes.push(k * 50_000_000 + 500_000);
    }

    let cfg = SimConfig {
        sample_rate_hz: 48_000,
        half_frames,
        dma_ppm: 0.0,
        pos_model: SimPositionModel::ContinuousModular { lag_bytes: 0 },
        explicit_wakes_ns: wakes,
        wake_period_ns: None,
        jitter_max_ns: 0,
        seed: 0x5303,
        max_wakes: 64,
    };
    let (res, _dev) = run_sim(cfg, &frames);
    let stats = res.expect("S3 sim failed");
    assert!(
        stats.coalesced_wakes >= 1,
        "S3: coalesced wake must be counted in PlaybackStats ({})",
        stats.timing_summary()
    );
    assert!(stats.max_wake_interval_ns >= 95_000_000);
}

#[test]
fn s5_mid_half_underrun_is_counted_in_partial_underrun_halves() {
    // Issue #22 S5: producer delivers only half of a DMA half then returns Underrun before resuming.
    let half_frames = 2_400;
    let cfg = SimConfig {
        sample_rate_hz: 48_000,
        half_frames,
        dma_ppm: 0.0,
        pos_model: SimPositionModel::Quantized0Half,
        explicit_wakes_ns: Vec::new(),
        wake_period_ns: None,
        jitter_max_ns: 0,
        seed: 0x5505,
        max_wakes: 64,
    };
    let (mut dev, clock) = FakeWaveRtDevice::new(cfg);

    let mut queue = VecDeque::from(vec![
        StreamBlock::Ready(InterleavedInt32Buffer {
            sample_rate_hz: 48_000,
            frames: make_stamped_frames(half_frames * 2), // fills both prefill halves
        }),
        StreamBlock::Ready(InterleavedInt32Buffer {
            sample_rate_hz: 48_000,
            frames: make_stamped_frames(half_frames / 2), // mid-half partial block!
        }),
        StreamBlock::Underrun, // forces partial silence padding in the second half of this half!
        StreamBlock::Ready(InterleavedInt32Buffer {
            sample_rate_hz: 48_000,
            frames: make_stamped_frames(half_frames * 2),
        }),
        StreamBlock::EndOfTrack,
    ]);
    let mut pump = StreamingWaveRtPump::new(48_000, DacWireFormat::Int32, move || {
        queue.pop_front().unwrap_or(StreamBlock::EndOfTrack)
    });
    let stop = AtomicBool::new(false);
    let stats = run_wavert_pump_with_device(
        &mut dev,
        &clock,
        48_000,
        DacWireFormat::Int32,
        &mut pump,
        &stop,
        true,
        false,
    )
    .expect("S5 sim failed");
    assert!(
        stats.partial_underrun_halves >= 1,
        "S5: mid-half underrun must increment partial_underrun_halves ({})",
        stats.timing_summary()
    );
    assert!(stats.has_timing_anomalies());
}

#[test]
fn s6_32bit_monotonic_position_wrap_near_u32_max_stays_clean() {
    // Issue #22 S6: 32-bit monotonic cursor starts 3 halves below u32::MAX and wraps mid-stream.
    let half_frames = 2_400;
    let half_bytes = (half_frames * DacWireFormat::Int32.bytes_per_frame()) as u64;
    let total_frames = half_frames * 16;
    let frames = make_stamped_frames(total_frames);

    let cfg = SimConfig {
        sample_rate_hz: 48_000,
        half_frames,
        dma_ppm: 0.0,
        pos_model: SimPositionModel::Monotonic32 {
            start_offset: (u32::MAX as u64) - (half_bytes * 3),
        },
        explicit_wakes_ns: Vec::new(),
        wake_period_ns: None,
        jitter_max_ns: 1_000_000,
        seed: 0x5606,
        max_wakes: 64,
    };
    let (res, dev) = run_sim(cfg, &frames);
    let stats = res.expect("S6 wrap sim failed");
    let (nz, repeats, skips, _, mid_zeros) = analyze_tapped_sequence(&dev.tapped_left_samples);
    assert_eq!(nz, total_frames);
    assert_eq!(repeats, 0, "S6 wrap caused repeat");
    assert_eq!(skips, 0, "S6 wrap caused skip");
    assert_eq!(mid_zeros, 0, "S6 wrap caused mid-stream dropout");
    assert_eq!(dev.active_half_overwrites, 0);
    assert!(!stats.has_timing_anomalies(), "{}", stats.timing_summary());
}

#[test]
fn causal_dma_drift_sweep_plus_minus_600ppm_and_determinism() {
    // Review #10 TV3:
    // 1) Hardware-coupled WaveRT interrupts over 2,000 halves (20 s virtual time @ 48 kHz with
    //    10 ms halves): at +-600 ppm, DMA drifts by +-12 ms (> 1.2 full halves!) relative to the
    //    nominal host QPC clock (`half_duration`), verifying `WaveRtNotificationController` tracks
    //    DMA rate cleanly with 0 repeats, 0 skips, 0 mid-stream zeros, and 0 anomalies.
    let half_frames = 480; // 10 ms @ 48 kHz -> 2,000 halves = 20 s virtual time (960k frames)
    let total_frames = half_frames * 2_000;
    let frames = make_stamped_frames(total_frames);

    for ppm in [-600.0_f64, -200.0, 0.0, 200.0, 600.0] {
        let cfg = SimConfig {
            sample_rate_hz: 48_000,
            half_frames,
            dma_ppm: ppm,
            pos_model: SimPositionModel::Quantized0Half,
            explicit_wakes_ns: Vec::new(),
            wake_period_ns: None,
            jitter_max_ns: 600_000, // 0.6 ms random jitter
            seed: 0xD1F7_0000 ^ (ppm.to_bits()),
            max_wakes: 2_200,
        };
        let (res1, dev1) = run_sim(cfg.clone(), &frames);
        let (res2, dev2) = run_sim(cfg, &frames);
        let stats1 = res1.unwrap_or_else(|e| panic!("ppm={ppm} failed: {e}"));
        let _stats2 = res2.unwrap();
        // Determinism: identical seed produces bit-identical DMA tap sequence.
        assert_eq!(
            dev1.tapped_left_samples, dev2.tapped_left_samples,
            "ppm={ppm}: non-deterministic DMA tap"
        );
        let (nz, repeats, skips, _, mid_zeros) = analyze_tapped_sequence(&dev1.tapped_left_samples);
        assert_eq!(nz, total_frames, "ppm={ppm}: frame count mismatch");
        assert_eq!(repeats, 0, "ppm={ppm}: repeat in tap");
        assert_eq!(skips, 0, "ppm={ppm}: skip in tap");
        assert_eq!(mid_zeros, 0, "ppm={ppm}: mid-stream silence dropout");
        assert_eq!(dev1.active_half_overwrites, 0, "ppm={ppm}: active overwrite");
        assert!(
            !stats1.has_timing_anomalies(),
            "ppm={ppm}: unexpected anomaly ({})",
            stats1.timing_summary()
        );
    }

    // 2) Decoupled nominal-rate wake timer vs drifted DMA clock (+-600 ppm): genuine clock slip
    //    between host timer wakes and DMA crossings MUST never silently corrupt audio without
    //    surfacing in `PlaybackStats::has_timing_anomalies()` (TV3 visible-failures invariant).
    let slip_half = 2_400; // 50 ms @ 48 kHz
    let slip_frames = make_stamped_frames(slip_half * 120);
    let nominal_period_ns = ((slip_half as f64) / 48_000.0 * 1e9).round() as u64;
    for ppm in [-600.0_f64, 600.0] {
        let cfg = SimConfig {
            sample_rate_hz: 48_000,
            half_frames: slip_half,
            dma_ppm: ppm,
            pos_model: SimPositionModel::Quantized0Half,
            explicit_wakes_ns: Vec::new(),
            wake_period_ns: Some(nominal_period_ns),
            jitter_max_ns: 1_000_000,
            seed: 0x5119_0000 ^ ppm.to_bits(),
            max_wakes: 300,
        };
        let (res, dev) = run_sim(cfg, &slip_frames);
        let stats = res.unwrap_or_else(|e| panic!("decoupled ppm={ppm} failed: {e}"));
        let (nz, repeats, skips, _, mid_zeros) = analyze_tapped_sequence(&dev.tapped_left_samples);
        assert!(nz > 0, "decoupled ppm={ppm}: empty tap");
        assert_eq!(
            dev.active_half_overwrites, 0,
            "decoupled ppm={ppm}: overwrote active DMA half!"
        );
        if repeats > 0 || skips > 0 || mid_zeros > 0 {
            assert!(
                stats.has_timing_anomalies(),
                "decoupled ppm={ppm}: silent glitch (repeats={repeats}, skips={skips}, mid_zeros={mid_zeros}) with zero reported anomalies!"
            );
        }
    }
}
