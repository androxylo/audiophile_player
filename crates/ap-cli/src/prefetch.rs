// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Bounded-memory streaming PCM block buffer between the DSP Producer and WDM-KS Consumer.
//!
//! Designed for BOTH arbitrarily large FLAC files (multi-hour 32/384 masters) AND continuous
//! Milestone 2 live streams (Tidal):
//! - Strictly caps buffered PCM in RAM by BOTH duration (default: `12 s` at the active output
//!   sample rate) and hard frame ceiling (`384 kHz * 12 s` ≈ 36 MB max RAM — NEVER grows
//!   unbounded regardless of file length, and never leads by ~100 s at 44.1 kHz!).
//! - Supports a configurable **prebuffer watermark** (e.g. `2.0 s` of PCM ready before starting
//!   WDM-KS hardware DMA) so playback starts cleanly with zero hiccups.
//! - Epoch-guarded instant flush (`reset_and_flush`) on Pause or mode/folder switch drops stale
//!   buffered blocks immediately and discards in-flight producer work cleanly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ap_control::state::PlayModeLabel;
use ap_core::buffer::InterleavedInt32Buffer;
use ap_dsp::pipeline::PipelineReport;
use ap_output_win::hygiene::MemoryLockGuard;

/// Default buffered lead time held in RAM ahead of the DAC (`24 s` at any output rate).
/// Absorbs multi-second mid-stream DSP/convolution/disk/rate-switch spikes with zero underruns
/// while capping worst-case RAM to ~73.7 MB at 384 kHz stereo Int32.
pub const DEFAULT_MAX_BUFFERED_SECONDS: u64 = 24;

/// Hard maximum buffered PCM frames held in RAM ahead of the DAC (~24 s @ 384 kHz ≈ 73.7 MB).
pub const DEFAULT_MAX_BUFFERED_FRAMES: usize = 384_000 * (DEFAULT_MAX_BUFFERED_SECONDS as usize);

/// Hard look-ahead cap for Ultimate Mode (Mandate M1: strictly `<= 30 s` ahead of playhead,
/// `384 kHz * 30 s * 8 B = 92.16 MB` max RAM, `VirtualLock`-pinned per block).
pub const ULTIMATE_MAX_BUFFERED_SECONDS: u64 = 30;

/// Hard maximum buffered PCM frames in Ultimate Mode (~30 s @ 384 kHz ≈ 92.16 MB).
pub const ULTIMATE_MAX_BUFFERED_FRAMES: usize = 384_000 * (ULTIMATE_MAX_BUFFERED_SECONDS as usize);

/// Minimum cushion buffered before starting a new local WDM-KS stream (`4 s` at output rate).
pub const PREBUFFER_SECONDS: u32 = 4;

/// Pre-allocated deferred-drop queue capacity (`256` blocks >= 40 s @ 384 kHz / 65,536 frames)
/// so the real-time MMCSS WaveRT thread NEVER executes `VirtualUnlock`, `HeapFree`, or
/// `VirtualFree` during `recycle_consumed_block` (Review #7 F10).
const INITIAL_DEFERRED_DROP_CAPACITY: usize = 256;

/// Wrapper around [`MemoryLockGuard`] with optional `#[cfg(test)]` lifecycle hooks so unit tests
/// can assert exact unlock-before-free ordering and zero-lock-contention pinning (Review #7 F10).
struct PinGuard {
    _real: MemoryLockGuard,
    #[cfg(test)]
    on_drop: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl PinGuard {
    fn lock_slice(
        slice: &[[i32; 2]],
        #[cfg(test)] on_lock: Option<&Arc<dyn Fn() + Send + Sync>>,
        #[cfg(test)] on_unlock: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        #[cfg(test)]
        if let Some(cb) = on_lock {
            cb();
        }
        Self {
            _real: MemoryLockGuard::lock_slice(slice),
            #[cfg(test)]
            on_drop: on_unlock,
        }
    }
}

#[cfg(test)]
impl Drop for PinGuard {
    fn drop(&mut self) {
        if let Some(cb) = self.on_drop.take() {
            cb();
        }
    }
}

/// Ordered stream events flowing from the DSP Producer into the WDM-KS Consumer.
#[derive(Debug)]
pub enum StreamEvent {
    /// Marks the start of a track/stream (carries output sample rate & pipeline metadata).
    TrackStart {
        path: PathBuf,
        /// Originating UPnP URI (set for `PlayModeLabel::UpnpRenderer` so the consumer promotes
        /// `CurrentURI` and starts `RelTime` only when this track becomes audible, Review #5 #5).
        upnp_uri: Option<String>,
        /// Generation token when this UPnP track came from a gapless `SetNextAVTransportURI`
        /// prefetch (`None` for explicit `Play`/`Seek` restarts of `CurrentURI`, Review #6 #8/#17).
        upnp_next_gen: Option<u64>,
        mode_label: PlayModeLabel,
        report: PipelineReport,
        total_duration_s: Option<f64>,
        /// True when the preceding track's gapless upsampler/REW tail was overlap-added onto this
        /// track's opening; prevents a mid-track skip from leaking the skipped track's ending decay.
        had_gapless_carryover: bool,
    },
    /// One ready-to-play bounded integer PCM block.
    PcmBlock(InterleavedInt32Buffer),
    /// Clean end-of-track marker (signals WDM-KS Consumer to finish this track cleanly).
    TrackEnd,
    /// Sequential folder or single-file queue finished cleanly -> pause & release DAC.
    EndOfQueue(String),
    /// Configuration or fatal source error -> pause & log cleanly.
    FatalError(String),
    /// Producer paused after consecutive unreadable/unsupported files; carries the last attempted
    /// path so a subsequent `Next` resumes *after* the bad run instead of replaying it.
    ErrorThresholdPause {
        message: String,
        last_attempted: PathBuf,
    },
}

/// Result of popping the next PCM block for an active track.
#[derive(Debug)]
pub enum PopBlockResult {
    /// Next PCM block ready for hardware output.
    Block(InterleavedInt32Buffer),
    /// Clean end of the active track (or next track / terminal control marker reached).
    TrackEnd,
    /// Producer did not deliver a block before `timeout` elapsed — track is NOT finished.
    Underrun,
    /// Buffer epoch changed (skip/pause/mode change) or buffer was permanently closed.
    StaleOrClosed,
}

fn is_terminal_marker(ev: &StreamEvent) -> bool {
    matches!(
        ev,
        StreamEvent::TrackEnd
            | StreamEvent::EndOfQueue(_)
            | StreamEvent::FatalError(_)
            | StreamEvent::ErrorThresholdPause { .. }
    )
}

fn event_metrics(ev: &StreamEvent) -> (usize, u128, usize) {
    match ev {
        StreamEvent::PcmBlock(b) => {
            let rate = u128::from(b.sample_rate_hz.max(1));
            let dur_ns = (b.frames.len() as u128).saturating_mul(1_000_000_000) / rate;
            (b.frames.len(), dur_ns, 0)
        }
        other => (0, 0, usize::from(is_terminal_marker(other))),
    }
}

fn pcm_block_peak_u32(frames: &[[i32; 2]]) -> u32 {
    frames.iter().fold(0u32, |acc, &[l, r]| {
        acc.max(l.unsigned_abs()).max(r.unsigned_abs())
    })
}

struct BufferInner {
    epoch: u64,
    /// Active `VirtualLock` guards keyed by `frames.as_ptr() as usize` while a block is queued
    /// or currently playing on the RT DMA thread.
    /// NOTE: Declared BEFORE `events` and `deferred_drops` so Rust's deterministic drop order
    /// ALWAYS unlocks pages BEFORE freeing their backing buffers (Review #7 F10).
    pinned_guards: HashMap<usize, PinGuard>,
    /// Guards corresponding to `deferred_drops`, ALWAYS unlocked BEFORE `deferred_drops` are freed.
    deferred_guards: Vec<PinGuard>,
    /// Pre-allocated spare guard vector swapped in O(1) under lock (Review #8 C7: zero heap
    /// allocation while holding `self.inner`).
    spare_guards: Vec<PinGuard>,
    /// Pointer keys of PCM blocks currently popped and in-flight on the RT DMA thread (never
    /// unlocked by `flush_locked` / `close` until `recycle_consumed_block` returns them).
    in_flight_ptrs: HashSet<usize>,
    /// Queued `(event, precomputed_pcm_peak_u32)` tuples so the RT consumer updates live peak in
    /// O(1) without scanning PCM frames on the WaveRT thread.
    events: VecDeque<(StreamEvent, u32)>,
    buffered_frames: usize,
    buffered_duration_ns: u128,
    terminal_markers: usize,
    max_buffered_frames: usize,
    max_buffered_duration_ns: u128,
    /// Optional per-mode override capping buffered duration (e.g. ~3 s for live exclusive
    /// capture so clock drift never accumulates into a 24-30 s queue, Review #8 B4).
    epoch_duration_cap_ns: Option<u128>,
    /// Lowest remaining buffered duration observed while actively popping PCM blocks for the
    /// current track (resets on track start / flush; diagnostic headroom metric).
    track_min_buffered_ns: Option<u128>,
    /// Consumed PCM blocks handed off by the real-time WaveRT thread so large `Vec<[i32; 2]>`
    /// buffers are deallocated exclusively on the non-RT producer/control thread.
    deferred_drops: Vec<InterleavedInt32Buffer>,
    /// Pre-allocated spare drop vector swapped in O(1) under lock (Review #8 C7).
    spare_drops: Vec<InterleavedInt32Buffer>,
    closed: bool,
}

struct FlushPayload {
    epoch: u64,
    guards: Vec<PinGuard>,
    events: VecDeque<(StreamEvent, u32)>,
    drops: Vec<InterleavedInt32Buffer>,
    #[cfg(test)]
    on_free_phase: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl FlushPayload {
    /// Strictly unlock all pages BEFORE freeing any queued or deferred PCM buffers (Review #7 F10).
    fn drop_unlock_before_free(self) {
        let Self {
            epoch: _,
            guards,
            events,
            drops,
            #[cfg(test)]
            on_free_phase,
        } = self;
        drop(guards);
        #[cfg(test)]
        if let Some(cb) = on_free_phase {
            cb();
        }
        drop(events);
        drop(drops);
    }
}

impl BufferInner {
    /// Swap `deferred_guards` / `deferred_drops` with pre-allocated spares in O(1) under lock
    /// (Review #8 C7: zero heap allocation while holding `self.inner`, and both `deferred_*`
    /// queues permanently retain capacity >= `INITIAL_DEFERRED_DROP_CAPACITY`).
    fn swap_deferred_with_spares(&mut self) -> (Vec<PinGuard>, Vec<InterleavedInt32Buffer>) {
        let g = if self.deferred_guards.is_empty() {
            Vec::new()
        } else if self.spare_guards.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY {
            let mut out = std::mem::take(&mut self.spare_guards);
            std::mem::swap(&mut out, &mut self.deferred_guards);
            out
        } else {
            self.deferred_guards.drain(..).collect()
        };
        let d = if self.deferred_drops.is_empty() {
            Vec::new()
        } else if self.spare_drops.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY {
            let mut out = std::mem::take(&mut self.spare_drops);
            std::mem::swap(&mut out, &mut self.deferred_drops);
            out
        } else {
            self.deferred_drops.drain(..).collect()
        };
        (g, d)
    }

    /// Return cleared spare vectors back under lock so subsequent drains remain allocation-free.
    fn restore_spares(
        &mut self,
        mut guards: Vec<PinGuard>,
        mut drops: Vec<InterleavedInt32Buffer>,
    ) {
        guards.clear();
        drops.clear();
        if guards.capacity() > self.spare_guards.capacity() {
            self.spare_guards = guards;
        }
        if drops.capacity() > self.spare_drops.capacity() {
            self.spare_drops = drops;
        }
    }

    fn flush_locked(
        &mut self,
        #[cfg(test)] on_free_phase: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> FlushPayload {
        self.epoch = self.epoch.wrapping_add(1);
        let events = std::mem::take(&mut self.events);
        self.buffered_frames = 0;
        self.buffered_duration_ns = 0;
        self.terminal_markers = 0;
        self.track_min_buffered_ns = None;

        // Review #7 re-review R6: Use `drain(..)` instead of `std::mem::take` so `deferred_guards`
        // and `deferred_drops` permanently retain their pre-allocated capacity (>= 256) and the RT
        // thread NEVER heap-allocates during `recycle_consumed_block`!
        let mut guards: Vec<PinGuard> = if self.deferred_guards.is_empty() {
            Vec::new()
        } else {
            self.deferred_guards.drain(..).collect()
        };
        // Remove and unlock ONLY guards for queued (non-in-flight) blocks; preserve the guard for
        // any block currently being read by the RT DMA thread until `recycle_consumed_block`!
        let mut release_keys = Vec::new();
        for (ev, _) in &events {
            if let StreamEvent::PcmBlock(b) = ev {
                let ptr_key = b.frames.as_ptr() as usize;
                if !self.in_flight_ptrs.contains(&ptr_key) {
                    release_keys.push(ptr_key);
                }
            }
        }
        for key in release_keys {
            if let Some(g) = self.pinned_guards.remove(&key) {
                guards.push(g);
            }
        }
        let drops: Vec<InterleavedInt32Buffer> = if self.deferred_drops.is_empty() {
            Vec::new()
        } else {
            self.deferred_drops.drain(..).collect()
        };
        FlushPayload {
            epoch: self.epoch,
            guards,
            events,
            drops,
            #[cfg(test)]
            on_free_phase,
        }
    }

    fn push_back_event(&mut self, ev: StreamEvent, peak_u32: u32) {
        let (frames, dur_ns, term) = event_metrics(&ev);
        self.buffered_frames += frames;
        self.buffered_duration_ns = self.buffered_duration_ns.saturating_add(dur_ns);
        self.terminal_markers += term;
        self.events.push_back((ev, peak_u32));
    }

    fn push_front_event(&mut self, ev: StreamEvent) {
        let peak_u32 = match &ev {
            StreamEvent::PcmBlock(b) => pcm_block_peak_u32(&b.frames),
            _ => 0,
        };
        let (frames, dur_ns, term) = event_metrics(&ev);
        self.buffered_frames += frames;
        self.buffered_duration_ns = self.buffered_duration_ns.saturating_add(dur_ns);
        self.terminal_markers += term;
        self.events.push_front((ev, peak_u32));
    }

    fn pop_front_event(&mut self) -> Option<(StreamEvent, u32)> {
        let (ev, peak_u32) = self.events.pop_front()?;
        let (frames, dur_ns, term) = event_metrics(&ev);
        self.buffered_frames = self.buffered_frames.saturating_sub(frames);
        self.buffered_duration_ns = self.buffered_duration_ns.saturating_sub(dur_ns);
        self.terminal_markers = self.terminal_markers.saturating_sub(term);
        Some((ev, peak_u32))
    }

    fn is_prebuffer_ready(&self, target_frames: usize) -> bool {
        self.buffered_frames >= target_frames || self.terminal_markers > 0
    }
}

/// Thread-safe, memory-bounded streaming block buffer with epoch invalidation.
pub struct BoundedBlockStreamBuffer {
    inner: Mutex<BufferInner>,
    cv: Condvar,
    /// Read outside `inner` so `VirtualLock` runs lock-free without blocking the RT thread (F10).
    pin_blocks: AtomicBool,
    #[cfg(test)]
    test_on_lock: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    test_on_unlock: Mutex<Option<Arc<dyn Fn(usize) + Send + Sync>>>,
    #[cfg(test)]
    test_on_free_phase: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl BoundedBlockStreamBuffer {
    /// Create a bounded buffer capped by both `DEFAULT_MAX_BUFFERED_SECONDS` (12 s at the
    /// active output sample rate) and the hard frame ceiling `max_buffered_frames`.
    pub fn new(max_buffered_frames: usize) -> Arc<Self> {
        Self::with_limits(
            max_buffered_frames,
            Duration::from_secs(DEFAULT_MAX_BUFFERED_SECONDS),
        )
    }

    /// Create a bounded buffer with explicit frame ceiling and duration cap.
    pub fn with_limits(max_buffered_frames: usize, max_buffered_duration: Duration) -> Arc<Self> {
        Self::with_limits_and_pinning(max_buffered_frames, max_buffered_duration, false)
    }

    /// Create a bounded buffer with explicit frame ceiling, duration cap, and optional
    /// per-block `VirtualLock` RAM pinning (`U11` Ultimate Mode).
    pub fn with_limits_and_pinning(
        max_buffered_frames: usize,
        max_buffered_duration: Duration,
        pin_blocks: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BufferInner {
                epoch: 0,
                pinned_guards: HashMap::with_capacity(INITIAL_DEFERRED_DROP_CAPACITY),
                deferred_guards: Vec::with_capacity(INITIAL_DEFERRED_DROP_CAPACITY),
                spare_guards: Vec::with_capacity(INITIAL_DEFERRED_DROP_CAPACITY),
                in_flight_ptrs: HashSet::with_capacity(8),
                events: VecDeque::new(),
                buffered_frames: 0,
                buffered_duration_ns: 0,
                terminal_markers: 0,
                max_buffered_frames: max_buffered_frames.max(65_536),
                max_buffered_duration_ns: max_buffered_duration.as_nanos().max(1),
                epoch_duration_cap_ns: None,
                track_min_buffered_ns: None,
                deferred_drops: Vec::with_capacity(INITIAL_DEFERRED_DROP_CAPACITY),
                spare_drops: Vec::with_capacity(INITIAL_DEFERRED_DROP_CAPACITY),
                closed: false,
            }),
            cv: Condvar::new(),
            pin_blocks: AtomicBool::new(pin_blocks),
            #[cfg(test)]
            test_on_lock: Mutex::new(None),
            #[cfg(test)]
            test_on_unlock: Mutex::new(None),
            #[cfg(test)]
            test_on_free_phase: Mutex::new(None),
        })
    }

    pub fn current_epoch(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).epoch
    }

    /// Set or clear a tighter per-mode duration cap (e.g. `Some(3 s)` for live capture,
    /// `None` for normal 24 s / Ultimate 30 s local playback; Review #8 B4).
    pub fn set_epoch_duration_cap(&self, cap: Option<Duration>) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.epoch_duration_cap_ns = cap.map(|d| d.as_nanos().max(1));
        self.cv.notify_all();
    }

    /// Current buffered audio duration in seconds ahead of the DAC (used by F9 W2 startup budget
    /// and live capture clock-slaving throttle).
    pub fn buffered_duration_seconds(&self) -> f64 {
        let ns = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffered_duration_ns;
        (ns as f64) / 1_000_000_000.0
    }

    /// Reset per-track buffer low-water telemetry right before starting hardware DMA.
    pub fn reset_track_low_water(&self) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.track_min_buffered_ns = Some(st.buffered_duration_ns);
    }

    /// Lowest remaining buffered duration (ms) observed during the current track's RT pops.
    #[allow(dead_code)]
    pub fn track_min_buffered_ms(&self) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .track_min_buffered_ns
            .map(|ns| (ns / 1_000_000) as u64)
    }

    /// Hand a consumed PCM block off the real-time MMCSS WaveRT thread for deferred deallocation
    /// on a non-RT thread. NEVER runs `VirtualUnlock`, `HeapFree`, or `VirtualFree` on the caller
    /// (RT) thread even under sustained W2 producer stalls (Review #7 F10).
    pub fn recycle_consumed_block(&self, buf: InterleavedInt32Buffer) {
        let ptr_key = buf.frames.as_ptr() as usize;
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.in_flight_ptrs.remove(&ptr_key);
        if let Some(g) = st.pinned_guards.remove(&ptr_key) {
            st.deferred_guards.push(g);
        }
        st.deferred_drops.push(buf);
    }

    /// Explicitly unlock and drop any deferred consumed PCM blocks on a non-RT thread.
    /// Review #7 re-review R6 + Review #8 C7: O(1) swap with pre-allocated spares under lock
    /// (zero heap allocations while holding `self.inner`), unlock/free strictly outside lock,
    /// then return the capacity-retaining spare vectors!
    pub fn drain_deferred_drops(&self) {
        let (mut guards, mut drops) = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.swap_deferred_with_spares()
        };
        if guards.capacity() == 0 && drops.capacity() == 0 {
            return;
        }
        // Unlock pages BEFORE dropping backing buffers, strictly outside `self.inner`.
        guards.clear();
        drops.clear();
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.restore_spares(guards, drops);
    }

    #[cfg(test)]
    pub fn buffered_frames(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffered_frames
    }

    #[cfg(test)]
    pub fn deferred_drop_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deferred_drops
            .len()
    }

    /// Increment epoch and immediately drop all queued PCM blocks (frees RAM on Pause / switch).
    pub fn reset_and_flush(&self) -> u64 {
        #[cfg(test)]
        let on_free = self.test_on_free_phase.lock().unwrap().clone();
        let payload = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let res = st.flush_locked(
                #[cfg(test)]
                on_free,
            );
            self.cv.notify_all();
            res
        };
        let ep = payload.epoch;
        payload.drop_unlock_before_free();
        ep
    }

    /// Re-queue a popped event at the front if `expected_epoch` is still active and open.
    /// Used so a pending command arriving right after `pop_for_epoch_timeout` does not silently
    /// drop a `TrackStart` marker before `skip_to_next_track_or_flush` inspects it.
    pub fn push_front_for_epoch(&self, expected_epoch: u64, event: StreamEvent) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return false;
        }
        st.push_front_event(event);
        self.cv.notify_all();
        true
    }

    /// Skip the currently playing track cleanly:
    /// - If `skip_front_track_start` is true and the front event is already a `TrackStart`
    ///   (rapid chained `Next` clicks before the pre-buffered track started), drops that
    ///   `TrackStart` first and returns its path in `skipped_front_path`, then skips to the
    ///   following `TrackStart` / terminal marker (or flushes if none is buffered yet).
    /// - If the buffer already holds the next track's `TrackStart` (or `EndOfQueue` / `FatalError`)
    ///   at `expected_epoch`, drops only the skipped track's remaining blocks and returns
    ///   `(true, expected_epoch, skipped_front_path)` so the pre-buffered next track plays
    ///   immediately with 0 ms wait.
    /// - Otherwise flushes everything, increments epoch, and returns
    ///   `(false, new_epoch, skipped_front_path)` so the producer aborts the remainder of the
    ///   current track and starts the next track immediately.
    pub fn skip_to_next_track_or_flush(
        &self,
        expected_epoch: u64,
        skip_front_track_start: bool,
    ) -> (bool, u64, Option<PathBuf>) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut skipped_front_path = None;
        if st.epoch == expected_epoch {
            if skip_front_track_start
                && matches!(st.events.front(), Some((StreamEvent::TrackStart { .. }, _)))
            {
                if let Some((StreamEvent::TrackStart { path, .. }, _)) = st.pop_front_event() {
                    skipped_front_path = Some(path);
                }
            }
            if let Some(next_idx) = st.events.iter().position(|(ev, _)| {
                matches!(
                    ev,
                    StreamEvent::TrackStart { .. }
                        | StreamEvent::EndOfQueue(_)
                        | StreamEvent::FatalError(_)
                        | StreamEvent::ErrorThresholdPause { .. }
                )
            }) {
                // Review #3 Finding #21: if we are actually skipping unplayed audio from the
                // preceding track (`next_idx > 0` or we just dropped a front TrackStart) AND the
                // buffered next track already has the skipped track's ending REW/upsampler tail
                // baked into its opening, flush instead of reusing so the user never hears the
                // skipped track's ending decay at the start of the next track.
                let next_has_baked_carryover = matches!(
                    st.events.get(next_idx),
                    Some((
                        StreamEvent::TrackStart {
                            had_gapless_carryover: true,
                            ..
                        },
                        _
                    ))
                );
                if !(next_has_baked_carryover && (next_idx > 0 || skipped_front_path.is_some())) {
                    let mut drop_frames = 0usize;
                    let mut drop_dur_ns = 0u128;
                    let mut drop_terms = 0usize;
                    let drained: Vec<(StreamEvent, u32)> = st.events.drain(..next_idx).collect();
                    for (ev, _) in drained {
                        let (frames, dur_ns, term) = event_metrics(&ev);
                        drop_frames += frames;
                        drop_dur_ns = drop_dur_ns.saturating_add(dur_ns);
                        drop_terms += term;
                        if let StreamEvent::PcmBlock(b) = ev {
                            let ptr_key = b.frames.as_ptr() as usize;
                            if let Some(g) = st.pinned_guards.remove(&ptr_key) {
                                st.deferred_guards.push(g);
                            }
                            st.deferred_drops.push(b);
                        }
                    }
                    st.buffered_frames = st.buffered_frames.saturating_sub(drop_frames);
                    st.buffered_duration_ns = st.buffered_duration_ns.saturating_sub(drop_dur_ns);
                    st.terminal_markers = st.terminal_markers.saturating_sub(drop_terms);
                    self.cv.notify_all();
                    return (true, st.epoch, skipped_front_path);
                }
            }
        }
        #[cfg(test)]
        let on_free = self.test_on_free_phase.lock().unwrap().clone();
        let payload = st.flush_locked(
            #[cfg(test)]
            on_free,
        );
        self.cv.notify_all();
        drop(st);
        let ep = payload.epoch;
        payload.drop_unlock_before_free();
        (false, ep, skipped_front_path)
    }

    /// Permanently close the buffer for graceful shutdown, waking all blocked threads.
    pub fn close(&self) {
        #[cfg(test)]
        let on_free = self.test_on_free_phase.lock().unwrap().clone();
        let payload = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
            let p = st.flush_locked(
                #[cfg(test)]
                on_free,
            );
            self.cv.notify_all();
            p
        };
        payload.drop_unlock_before_free();
    }

    /// Producer side: push a stream event, applying backpressure whenever buffered PCM reaches
    /// either the duration cap (`24 s` normal / `30 s` ultimate) or `max_buffered_frames`.
    ///
    /// Review #7 F10: `VirtualLock` pinning runs strictly OUTSIDE `self.inner` so the RT consumer
    /// thread never contends on page-locking or working-set expansion syscalls.
    pub fn push_for_epoch(&self, epoch: u64, event: StreamEvent) -> bool {
        let (frames_in_event, dur_ns_in_event, _) = event_metrics(&event);
        // Precompute PCM block peak on the producer thread (outside `self.inner`) so the RT
        // WaveRT thread only executes a single lock-free `AtomicU32::fetch_max` per block!
        let block_peak_u32 = match &event {
            StreamEvent::PcmBlock(b) => pcm_block_peak_u32(&b.frames),
            _ => 0,
        };

        // Pin PCM pages BEFORE acquiring `self.inner` so RT pops never stall on `VirtualLock`!
        let mut pre_pinned: Option<(usize, PinGuard)> =
            if self.pin_blocks.load(Ordering::Relaxed) {
                if let StreamEvent::PcmBlock(ref b) = event {
                    if !b.frames.is_empty() {
                        let ptr_key = b.frames.as_ptr() as usize;
                        #[cfg(test)]
                        let on_lock = self.test_on_lock.lock().unwrap().clone();
                        #[cfg(test)]
                        let on_unlock = self.test_on_unlock.lock().unwrap().clone().map(|cb| {
                            let f: Arc<dyn Fn() + Send + Sync> = Arc::new(move || cb(ptr_key));
                            f
                        });
                        let guard = PinGuard::lock_slice(
                            &b.frames,
                            #[cfg(test)]
                            on_lock.as_ref(),
                            #[cfg(test)]
                            on_unlock,
                        );
                        Some((ptr_key, guard))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };

        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guards, mut drops) = st.swap_deferred_with_spares();
        loop {
            if st.closed || st.epoch != epoch {
                drop(st);
                drop(pre_pinned);
                drop(event);
                guards.clear();
                drops.clear();
                if guards.capacity() > 0 || drops.capacity() > 0 {
                    let mut st2 = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    st2.restore_spares(guards, drops);
                }
                return false;
            }
            // Control markers pass immediately; PCM blocks respect both duration and RAM caps
            // (always allowing at least 1 block if no PCM frames are currently buffered).
            let effective_dur_cap = st
                .epoch_duration_cap_ns
                .map_or(st.max_buffered_duration_ns, |c| c.min(st.max_buffered_duration_ns));
            if frames_in_event == 0
                || st.buffered_frames == 0
                || (st.buffered_frames + frames_in_event <= st.max_buffered_frames
                    && st.buffered_duration_ns.saturating_add(dur_ns_in_event) <= effective_dur_cap)
            {
                if let Some((ptr_key, guard)) = pre_pinned.take() {
                    st.pinned_guards.insert(ptr_key, guard);
                }
                st.push_back_event(event, block_peak_u32);
                if guards.is_empty() && drops.is_empty() {
                    let (g2, d2) = st.swap_deferred_with_spares();
                    if guards.capacity() == 0 {
                        guards = g2;
                    } else {
                        st.restore_spares(g2, Vec::new());
                    }
                    if drops.capacity() == 0 {
                        drops = d2;
                    } else {
                        st.restore_spares(Vec::new(), d2);
                    }
                }
                self.cv.notify_all();
                drop(st);
                guards.clear();
                drops.clear();
                if guards.capacity() > 0 || drops.capacity() > 0 {
                    let mut st2 = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                    st2.restore_spares(guards, drops);
                }
                return true;
            }
            if !drops.is_empty() || !guards.is_empty() {
                drop(st);
                guards.clear();
                drops.clear();
                st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                st.restore_spares(guards, drops);
                let (g2, d2) = st.swap_deferred_with_spares();
                guards = g2;
                drops = d2;
                continue;
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            if guards.is_empty() && drops.is_empty() {
                st.restore_spares(guards, drops);
                let (g2, d2) = st.swap_deferred_with_spares();
                guards = g2;
                drops = d2;
            }
        }
    }

    /// Consumer side: wait up to `timeout` for the next stream event in `expected_epoch`.
    pub fn pop_for_epoch_timeout(
        &self,
        expected_epoch: u64,
        timeout: Duration,
    ) -> Option<StreamEvent> {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return None;
        }
        let (mut st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| {
                !s.closed && s.epoch == expected_epoch && s.events.is_empty()
            })
            .unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return None;
        }
        let (ev, _) = st.pop_front_event()?;
        // Review #7 re-review R1: `pop_for_epoch_timeout` is the non-RT control loop pop (only
        // `pop_track_pcm_locked` marks blocks in-flight for RT playback). If an orphaned `PcmBlock`
        // is popped here, release its `VirtualLock` guard immediately outside `st` BEFORE returning
        // `ev` so dropping an orphan block can never leak a pinned guard or free locked pages!
        let orphan_guard = if let StreamEvent::PcmBlock(ref b) = ev {
            if !b.frames.is_empty() {
                let ptr_key = b.frames.as_ptr() as usize;
                st.pinned_guards.remove(&ptr_key)
            } else {
                None
            }
        } else {
            None
        };
        self.cv.notify_all();
        drop(st);
        drop(orphan_guard);
        Some(ev)
    }

    /// Consumer side: wait until at least `target_frames` of PCM are pre-buffered in RAM
    /// (or a `TrackEnd` / terminal event is already queued) before opening WDM-KS DMA,
    /// eliminating startup hiccups. Returns `None` if epoch changed or closed.
    pub fn wait_prebuffer_watermark(
        &self,
        expected_epoch: u64,
        target_frames: usize,
        poll_slice: Duration,
    ) -> Option<bool> {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return None;
        }
        let (st, _) = self
            .cv
            .wait_timeout_while(st, poll_slice, |s| {
                !s.closed && s.epoch == expected_epoch && !s.is_prebuffer_ready(target_frames)
            })
            .unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return None;
        }
        Some(st.is_prebuffer_ready(target_frames))
    }

    /// Consumer side (real-time WaveRT audio thread): immediately pop the next PCM block
    /// for this track without ever sleeping on the condvar, returning `(result, block_peak_u32)`.
    pub fn try_pop_track_pcm_block_with_peak(
        &self,
        expected_epoch: u64,
    ) -> (PopBlockResult, u32) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return (PopBlockResult::StaleOrClosed, 0);
        }
        self.pop_track_pcm_locked(&mut st)
    }

    /// Consumer side (real-time WaveRT audio thread): immediately pop the next PCM block
    /// for this track without ever sleeping on the condvar.
    ///
    /// Returns `PopBlockResult::Underrun` immediately when the queue is empty so the RT loop
    /// can emit digital silence instead of blocking hardware DMA notifications.
    #[allow(dead_code)]
    pub fn try_pop_track_pcm_block(&self, expected_epoch: u64) -> PopBlockResult {
        self.try_pop_track_pcm_block_with_peak(expected_epoch).0
    }

    /// Consumer side (inside active track playback): pop the next PCM block for this track.
    ///
    /// Explicitly distinguishes clean `TrackEnd` from `Underrun` (timeout while producer is
    /// still decoding) and `StaleOrClosed` (epoch change / shutdown), so a temporary producer
    /// stall never silently truncates a track.
    #[allow(dead_code)]
    pub fn pop_track_pcm_block(
        &self,
        expected_epoch: u64,
        timeout: Duration,
    ) -> PopBlockResult {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return PopBlockResult::StaleOrClosed;
        }
        let (mut st, wait_res) = self
            .cv
            .wait_timeout_while(st, timeout, |s| {
                !s.closed && s.epoch == expected_epoch && s.events.is_empty()
            })
            .unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return PopBlockResult::StaleOrClosed;
        }
        if st.events.is_empty() && wait_res.timed_out() {
            return PopBlockResult::Underrun;
        }
        self.pop_track_pcm_locked(&mut st).0
    }

    fn pop_track_pcm_locked(&self, st: &mut BufferInner) -> (PopBlockResult, u32) {
        match st.events.front() {
            Some((StreamEvent::PcmBlock(_), _)) => {
                if let Some((StreamEvent::PcmBlock(b), peak_u32)) = st.pop_front_event() {
                    if !b.frames.is_empty() {
                        st.in_flight_ptrs.insert(b.frames.as_ptr() as usize);
                    }
                    if st.terminal_markers == 0 {
                        let rem = st.buffered_duration_ns;
                        st.track_min_buffered_ns = Some(match st.track_min_buffered_ns {
                            Some(prev) => prev.min(rem),
                            None => rem,
                        });
                    }
                    self.cv.notify_all();
                    (PopBlockResult::Block(b), peak_u32)
                } else {
                    (PopBlockResult::Underrun, 0)
                }
            }
            Some((StreamEvent::TrackEnd, _)) => {
                let _ = st.pop_front_event();
                self.cv.notify_all();
                (PopBlockResult::TrackEnd, 0)
            }
            // Leave next TrackStart / EndOfQueue / FatalError at the front for outer loop!
            Some(_) => (PopBlockResult::TrackEnd, 0),
            None => {
                st.track_min_buffered_ns = Some(0);
                (PopBlockResult::Underrun, 0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn make_block_at(rate_hz: u32, frames: usize) -> InterleavedInt32Buffer {
        InterleavedInt32Buffer {
            sample_rate_hz: rate_hz,
            frames: vec![[1, -1]; frames],
        }
    }

    fn make_block(frames: usize) -> InterleavedInt32Buffer {
        make_block_at(384_000, frames)
    }

    #[test]
    fn bounded_buffer_enforces_backpressure_and_streams_blocks_in_order() {
        // Small cap: 100,000 frames max; producer pushes 4 blocks of 50,000 frames (200,000 total)
        let buf = BoundedBlockStreamBuffer::new(100_000);
        let ep = buf.current_epoch();
        let b_prod = Arc::clone(&buf);

        let producer = thread::spawn(move || {
            for _ in 0..4 {
                assert!(b_prod.push_for_epoch(ep, StreamEvent::PcmBlock(make_block(50_000))));
            }
            assert!(b_prod.push_for_epoch(ep, StreamEvent::TrackEnd));
        });

        // Wait until watermark (100k frames) is buffered; verify producer did NOT exceed cap!
        while !buf
            .wait_prebuffer_watermark(ep, 100_000, Duration::from_millis(50))
            .unwrap()
        {}
        assert!(buf.buffered_frames() <= 100_000);

        // Drain all 4 blocks cleanly until TrackEnd
        let mut total = 0;
        loop {
            match buf.pop_track_pcm_block(ep, Duration::from_secs(1)) {
                PopBlockResult::Block(blk) => total += blk.frames.len(),
                PopBlockResult::TrackEnd => break,
                other => panic!("unexpected pop result: {other:?}"),
            }
        }
        assert_eq!(total, 200_000);
        assert_eq!(buf.buffered_frames(), 0);
        producer.join().unwrap();
    }

    #[test]
    fn time_based_cap_limits_low_rate_lead_to_default_seconds() {
        // At 44.1 kHz, DEFAULT_MAX_BUFFERED_SECONDS (24 s) is 1,058,400 frames — far less than
        // DEFAULT_MAX_BUFFERED_FRAMES (9.2M @ 384 kHz).
        let cap_secs = DEFAULT_MAX_BUFFERED_SECONDS as usize;
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.current_epoch();
        let b_prod = Arc::clone(&buf);

        // Producer tries to push (cap_secs + 8) seconds of 44.1 kHz audio.
        let producer = thread::spawn(move || {
            for _ in 0..(cap_secs + 8) {
                if !b_prod.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 44_100)))
                {
                    break;
                }
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if buf.buffered_frames() >= 44_100 * cap_secs {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(20));
        let buffered = buf.buffered_frames();
        assert!(
            buffered <= 44_100 * cap_secs,
            "44.1 kHz buffer should cap at <= {cap_secs} s, got {buffered}"
        );
        assert!(
            buffered >= 44_100 * (cap_secs - 1),
            "producer should have filled up to the {cap_secs} s cap, got {buffered}"
        );
        assert!(
            !producer.is_finished(),
            "producer should remain blocked on backpressure until buffer is drained or closed"
        );

        buf.close();
        producer.join().unwrap();
    }

    #[test]
    fn deferred_block_recycling_and_low_water_telemetry_work_off_rt_thread() {
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.current_epoch();

        // Push 4 blocks of 0.5 s @ 384 kHz (2.0 s total).
        for _ in 0..4 {
            assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block(192_000))));
        }
        buf.reset_track_low_water();
        assert_eq!(buf.track_min_buffered_ms(), Some(2_000));

        // Pop 2 blocks and recycle them as the RT thread would; drops stay deferred until producer runs.
        for _ in 0..2 {
            match buf.try_pop_track_pcm_block(ep) {
                PopBlockResult::Block(b) => buf.recycle_consumed_block(b),
                other => panic!("expected Block, got {other:?}"),
            }
        }
        assert_eq!(buf.deferred_drop_count(), 2);
        assert_eq!(buf.track_min_buffered_ms(), Some(1_000));

        // Next producer push automatically drains the deferred-drop queue off the RT thread.
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block(192_000))));
        assert_eq!(buf.deferred_drop_count(), 0);
    }

    #[test]
    fn pop_track_pcm_block_distinguishes_underrun_from_track_end() {
        let buf = BoundedBlockStreamBuffer::new(200_000);
        let ep = buf.current_epoch();

        // Empty queue -> timeout yields Underrun, NOT TrackEnd.
        assert!(matches!(
            buf.pop_track_pcm_block(ep, Duration::from_millis(15)),
            PopBlockResult::Underrun
        ));

        // Pushing a block afterwards still delivers it cleanly.
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block(10_000))));
        assert!(matches!(
            buf.pop_track_pcm_block(ep, Duration::from_millis(50)),
            PopBlockResult::Block(ref b) if b.frames.len() == 10_000
        ));

        // Pushing TrackEnd yields TrackEnd.
        assert!(buf.push_for_epoch(ep, StreamEvent::TrackEnd));
        assert!(matches!(
            buf.pop_track_pcm_block(ep, Duration::from_millis(50)),
            PopBlockResult::TrackEnd
        ));

        // Epoch change yields StaleOrClosed.
        let _ep1 = buf.reset_and_flush();
        assert!(matches!(
            buf.pop_track_pcm_block(ep, Duration::from_millis(50)),
            PopBlockResult::StaleOrClosed
        ));
    }

    #[test]
    fn reset_and_flush_frees_buffered_frames_and_rejects_stale_epoch_blocks() {
        let buf = BoundedBlockStreamBuffer::new(200_000);
        let ep0 = buf.current_epoch();
        assert!(buf.push_for_epoch(ep0, StreamEvent::PcmBlock(make_block(50_000))));
        assert_eq!(buf.buffered_frames(), 50_000);

        let ep1 = buf.reset_and_flush();
        assert_ne!(ep0, ep1);
        assert_eq!(buf.buffered_frames(), 0);
        assert!(!buf.push_for_epoch(ep0, StreamEvent::PcmBlock(make_block(10_000))));
        assert!(buf.push_for_epoch(ep1, StreamEvent::PcmBlock(make_block(10_000))));
        assert_eq!(buf.buffered_frames(), 10_000);
    }

    #[test]
    fn skip_to_next_track_reuses_prebuffered_next_track_or_flushes_mid_track() {
        let buf = BoundedBlockStreamBuffer::new(300_000);
        let ep0 = buf.current_epoch();

        // Case 1: mid-track (no next TrackStart queued yet) -> flushes and advances epoch.
        assert!(buf.push_for_epoch(ep0, StreamEvent::PcmBlock(make_block(40_000))));
        let (reused, ep1, skipped) = buf.skip_to_next_track_or_flush(ep0, false);
        assert!(!reused);
        assert_ne!(ep1, ep0);
        assert!(skipped.is_none());
        assert_eq!(buf.buffered_frames(), 0);

        // Case 2: tail of track A + EndOfQueue/next marker already buffered -> skips track A blocks, keeps next!
        assert!(buf.push_for_epoch(ep1, StreamEvent::PcmBlock(make_block(30_000))));
        assert!(buf.push_for_epoch(ep1, StreamEvent::TrackEnd));
        assert!(buf.push_for_epoch(ep1, StreamEvent::EndOfQueue("next-marker".into())));
        let (reused2, ep2, skipped2) = buf.skip_to_next_track_or_flush(ep1, false);
        assert!(reused2);
        assert_eq!(ep2, ep1);
        assert!(skipped2.is_none());
        assert_eq!(buf.buffered_frames(), 0);
        assert!(
            buf.wait_prebuffer_watermark(ep1, 100_000, Duration::from_millis(10))
                == Some(true),
            "terminal marker counter must stay > 0 for retained EndOfQueue"
        );
        assert!(matches!(
            buf.pop_for_epoch_timeout(ep1, Duration::from_millis(50)),
            Some(StreamEvent::EndOfQueue(_))
        ));
        // After popping the EndOfQueue marker, prebuffer should no longer report ready.
        assert_eq!(
            buf.wait_prebuffer_watermark(ep1, 100_000, Duration::from_millis(10)),
            Some(false)
        );

        // Case 3: push_front_for_epoch preserves event order and terminal marker accounting.
        assert!(buf.push_front_for_epoch(ep1, StreamEvent::EndOfQueue("requeued".into())));
        assert_eq!(
            buf.wait_prebuffer_watermark(ep1, 100_000, Duration::from_millis(10)),
            Some(true)
        );
        assert!(matches!(
            buf.pop_for_epoch_timeout(ep1, Duration::from_millis(50)),
            Some(StreamEvent::EndOfQueue(ref s)) if s == "requeued"
        ));
    }

    #[test]
    fn try_pop_track_pcm_block_is_nonblocking_and_recovers_from_underrun() {
        let buf = BoundedBlockStreamBuffer::new(100_000);
        let ep = buf.current_epoch();

        // Empty queue returns Underrun immediately (no condvar sleep).
        let started = std::time::Instant::now();
        assert!(matches!(
            buf.try_pop_track_pcm_block(ep),
            PopBlockResult::Underrun
        ));
        assert!(started.elapsed() < Duration::from_millis(20));

        // Once producer pushes a block and TrackEnd, non-blocking pops drain them in order.
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block(128))));
        assert!(buf.push_for_epoch(ep, StreamEvent::TrackEnd));

        match buf.try_pop_track_pcm_block(ep) {
            PopBlockResult::Block(b) => assert_eq!(b.frames.len(), 128),
            other => panic!("expected Block, got {other:?}"),
        }
        assert!(matches!(
            buf.try_pop_track_pcm_block(ep),
            PopBlockResult::TrackEnd
        ));

        // Epoch mismatch or close reports StaleOrClosed immediately.
        let _ = buf.reset_and_flush();
        assert!(matches!(
            buf.try_pop_track_pcm_block(ep),
            PopBlockResult::StaleOrClosed
        ));
    }

    #[test]
    fn ultimate_30s_pinned_buffer_enforces_m1_cap_and_unlocks_off_rt_thread() {
        let buf = BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        );
        let ep = buf.current_epoch();

        // Push 2 blocks of 1 s @ 44.1 kHz, pop & recycle -> deferred drops + guards clean up cleanly.
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 44_100))));
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 44_100))));
        if let PopBlockResult::Block(b) = buf.try_pop_track_pcm_block(ep) {
            buf.recycle_consumed_block(b);
        } else {
            panic!("expected pinned block");
        }
        assert_eq!(buf.deferred_drop_count(), 1);
        buf.drain_deferred_drops();
        assert_eq!(buf.deferred_drop_count(), 0);
        buf.close();
    }

    #[test]
    fn recycle_overflow_never_drops_on_caller_thread() {
        use std::sync::atomic::AtomicUsize;

        let buf = BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        );
        let unlocks = Arc::new(AtomicUsize::new(0));
        let u_clone = Arc::clone(&unlocks);
        *buf.test_on_unlock.lock().unwrap() = Some(Arc::new(move |_| {
            u_clone.fetch_add(1, Ordering::SeqCst);
        }));

        let ep = buf.current_epoch();
        // Push, pop, and recycle 64 blocks (far above old 16-block cap) without draining:
        // ZERO unlocks/drops may run on the recycling (RT) thread!
        for _ in 0..64 {
            assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 1_024))));
            if let PopBlockResult::Block(b) = buf.try_pop_track_pcm_block(ep) {
                buf.recycle_consumed_block(b);
            } else {
                panic!("expected block");
            }
            // Note: push_for_epoch drains deferred drops on producer side, so push 64 first instead!
        }
        buf.drain_deferred_drops();
        unlocks.store(0, Ordering::SeqCst);

        // Now push 64 blocks all at once, then pop & recycle all 64 on the RT side!
        for _ in 0..64 {
            assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 1_024))));
        }
        assert_eq!(unlocks.load(Ordering::SeqCst), 0);
        for _ in 0..64 {
            if let PopBlockResult::Block(b) = buf.try_pop_track_pcm_block(ep) {
                buf.recycle_consumed_block(b);
            } else {
                panic!("expected block");
            }
        }
        assert_eq!(
            unlocks.load(Ordering::SeqCst),
            0,
            "F10: recycle_consumed_block must NEVER unlock/drop on the RT thread"
        );
        assert_eq!(buf.deferred_drop_count(), 64);

        buf.drain_deferred_drops();
        assert_eq!(unlocks.load(Ordering::SeqCst), 64);
        assert_eq!(buf.deferred_drop_count(), 0);
    }

    #[test]
    fn flush_unlocks_queued_blocks_and_preserves_in_flight_guard_until_recycled() {
        let buf = BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        );
        let unlocked_ptrs = Arc::new(Mutex::new(Vec::<usize>::new()));
        let u_clone = Arc::clone(&unlocked_ptrs);
        *buf.test_on_unlock.lock().unwrap() = Some(Arc::new(move |ptr| {
            u_clone.lock().unwrap().push(ptr);
        }));

        let ep = buf.current_epoch();
        let b_inflight = make_block_at(44_100, 2_048);
        let ptr_inflight = b_inflight.frames.as_ptr() as usize;
        let b_queued = make_block_at(44_100, 2_048);
        let ptr_queued = b_queued.frames.as_ptr() as usize;

        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(b_inflight)));
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(b_queued)));

        // RT thread pops the first block (now in-flight!).
        let popped = match buf.try_pop_track_pcm_block(ep) {
            PopBlockResult::Block(b) => b,
            other => panic!("expected block, got {other:?}"),
        };
        assert_eq!(popped.frames.as_ptr() as usize, ptr_inflight);

        // Flush while `popped` is in-flight: queued block unlocks cleanly, in-flight block stays pinned!
        let _ = buf.reset_and_flush();
        {
            let log = unlocked_ptrs.lock().unwrap();
            assert!(log.contains(&ptr_queued), "queued block must be unlocked on flush");
            assert!(
                !log.contains(&ptr_inflight),
                "F10: in-flight RT block must stay pinned across flush until recycled!"
            );
        }

        // Now RT finishes with `popped` and recycles it -> unlocks only on off-RT drain.
        buf.recycle_consumed_block(popped);
        assert!(!unlocked_ptrs.lock().unwrap().contains(&ptr_inflight));
        buf.drain_deferred_drops();
        assert!(unlocked_ptrs.lock().unwrap().contains(&ptr_inflight));
    }

    #[test]
    fn push_does_not_hold_lock_during_pin() {
        let buf = BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        );
        let saw_unlocked = Arc::new(AtomicBool::new(false));
        let buf_weak = Arc::downgrade(&buf);
        let flag = Arc::clone(&saw_unlocked);
        *buf.test_on_lock.lock().unwrap() = Some(Arc::new(move || {
            if let Some(b) = buf_weak.upgrade() {
                // `try_lock()` MUST succeed from inside `lock_slice`, proving `inner` is NOT held!
                if b.inner.try_lock().is_ok() {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        }));

        let ep = buf.current_epoch();
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 1_024))));
        assert!(
            saw_unlocked.load(Ordering::SeqCst),
            "F10: inner mutex must NOT be held during VirtualLock"
        );
    }

    #[test]
    fn orphan_pcm_pop_does_not_leak_pin_guard_or_inflight_r1() {
        let buf = BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        );
        let unlocked = Arc::new(Mutex::new(Vec::<usize>::new()));
        let u_clone = Arc::clone(&unlocked);
        *buf.test_on_unlock.lock().unwrap() = Some(Arc::new(move |ptr| {
            u_clone.lock().unwrap().push(ptr);
        }));

        let ep = buf.current_epoch();
        let blk = make_block_at(44_100, 2_048);
        let ptr_key = blk.frames.as_ptr() as usize;
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(blk)));

        // Non-RT control loop pops an orphaned PcmBlock via `pop_for_epoch_timeout`:
        // guard MUST be unlocked BEFORE `ev` is returned/dropped, and `in_flight_ptrs` stays empty!
        let ev = buf
            .pop_for_epoch_timeout(ep, Duration::from_millis(50))
            .expect("expected orphan PcmBlock");
        assert!(
            unlocked.lock().unwrap().contains(&ptr_key),
            "R1: orphan PcmBlock popped via pop_for_epoch_timeout must be unlocked before drop"
        );
        {
            let st = buf.inner.lock().unwrap();
            assert!(
                st.in_flight_ptrs.is_empty(),
                "R1: pop_for_epoch_timeout must never leak keys into in_flight_ptrs"
            );
            assert!(
                st.pinned_guards.is_empty(),
                "R1: pinned_guards must not retain orphan block guard"
            );
        }
        drop(ev);
    }

    #[test]
    fn flush_unlocks_before_free_order_f10() {
        let buf = BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        );
        let order = Arc::new(Mutex::new(Vec::<(&'static str, usize)>::new()));
        let o1 = Arc::clone(&order);
        *buf.test_on_unlock.lock().unwrap() = Some(Arc::new(move |ptr| {
            o1.lock().unwrap().push(("unlock", ptr));
        }));
        let o2 = Arc::clone(&order);
        *buf.test_on_free_phase.lock().unwrap() = Some(Arc::new(move || {
            o2.lock().unwrap().push(("free_phase", 0));
        }));

        let ep = buf.current_epoch();
        let mut ptrs = Vec::new();
        for _ in 0..4 {
            let b = make_block_at(44_100, 2_048);
            ptrs.push(b.frames.as_ptr() as usize);
            assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(b)));
        }

        let _ = buf.reset_and_flush();

        let log = order.lock().unwrap();
        let free_idx = log
            .iter()
            .position(|&(ev, _)| ev == "free_phase")
            .expect("free_phase marker must fire on flush");
        assert_eq!(free_idx, 4, "all 4 unlocks must occur strictly BEFORE free_phase: {log:?}");
        for p in ptrs {
            assert!(
                log[..free_idx].iter().any(|&(ev, k)| ev == "unlock" && k == p),
                "queued block {p:#x} must unlock before buffer free"
            );
        }
    }

    #[test]
    fn deferred_queues_retain_capacity_after_drain_r6() {
        let buf = BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        );
        let ep = buf.current_epoch();
        for _ in 0..8 {
            assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 1_024))));
        }
        for _ in 0..4 {
            if let PopBlockResult::Block(b) = buf.try_pop_track_pcm_block(ep) {
                buf.recycle_consumed_block(b);
            }
        }
        buf.drain_deferred_drops();
        {
            let st = buf.inner.lock().unwrap();
            assert!(
                st.deferred_guards.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY,
                "R6: deferred_guards lost pre-allocated capacity after drain_deferred_drops"
            );
            assert!(
                st.deferred_drops.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY,
                "R6: deferred_drops lost pre-allocated capacity after drain_deferred_drops"
            );
            assert!(
                st.spare_guards.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY,
                "C7: spare_guards lost pre-allocated capacity after swap-and-restore"
            );
            assert!(
                st.spare_drops.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY,
                "C7: spare_drops lost pre-allocated capacity after swap-and-restore"
            );
        }

        // Next push drains any recycled blocks and flush drains remaining blocks — capacity MUST stick!
        for _ in 0..2 {
            if let PopBlockResult::Block(b) = buf.try_pop_track_pcm_block(ep) {
                buf.recycle_consumed_block(b);
            }
        }
        assert!(buf.push_for_epoch(ep, StreamEvent::PcmBlock(make_block_at(44_100, 1_024))));
        let _ = buf.reset_and_flush();
        {
            let st = buf.inner.lock().unwrap();
            assert!(
                st.deferred_guards.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY,
                "R6: deferred_guards lost pre-allocated capacity after push/flush"
            );
            assert!(
                st.deferred_drops.capacity() >= INITIAL_DEFERRED_DROP_CAPACITY,
                "R6: deferred_drops lost pre-allocated capacity after push/flush"
            );
        }
    }
}
