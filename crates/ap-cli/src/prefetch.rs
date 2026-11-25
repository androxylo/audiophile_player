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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
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

/// Maximum consumed PCM blocks held in the deferred-drop queue so the real-time MMCSS WaveRT
/// thread never invokes `HeapFree` / `VirtualFree` on large PCM allocations.
const MAX_DEFERRED_DROP_BLOCKS: usize = 16;

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

struct BufferInner {
    epoch: u64,
    events: VecDeque<StreamEvent>,
    buffered_frames: usize,
    buffered_duration_ns: u128,
    terminal_markers: usize,
    max_buffered_frames: usize,
    max_buffered_duration_ns: u128,
    /// Lowest remaining buffered duration observed while actively popping PCM blocks for the
    /// current track (resets on track start / flush; diagnostic headroom metric).
    track_min_buffered_ns: Option<u128>,
    /// Consumed PCM blocks handed off by the real-time WaveRT thread so large `Vec<[i32; 2]>`
    /// buffers are deallocated exclusively on the non-RT producer/control thread.
    deferred_drops: Vec<InterleavedInt32Buffer>,
    /// When `true` (Ultimate Mode `U11`), each queued PCM block is `VirtualLock`-pinned on the
    /// producer thread and unlocked strictly off-RT on drop/flush.
    pin_blocks: bool,
    /// Active `VirtualLock` guards keyed by `frames.as_ptr() as usize` while a block is queued
    /// or currently playing on the RT DMA thread.
    pinned_guards: HashMap<usize, MemoryLockGuard>,
    /// Guards corresponding to `deferred_drops`, unlocked BEFORE `deferred_drops` are freed.
    deferred_guards: Vec<MemoryLockGuard>,
    closed: bool,
}

impl BufferInner {
    fn flush_locked(&mut self) -> (u64, Vec<MemoryLockGuard>, Vec<InterleavedInt32Buffer>) {
        self.epoch = self.epoch.wrapping_add(1);
        self.events.clear();
        self.buffered_frames = 0;
        self.buffered_duration_ns = 0;
        self.terminal_markers = 0;
        self.track_min_buffered_ns = None;
        let mut guards = std::mem::take(&mut self.deferred_guards);
        guards.extend(self.pinned_guards.drain().map(|(_, g)| g));
        let drops = std::mem::take(&mut self.deferred_drops);
        (self.epoch, guards, drops)
    }

    fn push_back_event(&mut self, ev: StreamEvent) {
        let (frames, dur_ns, term) = event_metrics(&ev);
        self.buffered_frames += frames;
        self.buffered_duration_ns = self.buffered_duration_ns.saturating_add(dur_ns);
        self.terminal_markers += term;
        self.events.push_back(ev);
    }

    fn push_front_event(&mut self, ev: StreamEvent) {
        let (frames, dur_ns, term) = event_metrics(&ev);
        self.buffered_frames += frames;
        self.buffered_duration_ns = self.buffered_duration_ns.saturating_add(dur_ns);
        self.terminal_markers += term;
        self.events.push_front(ev);
    }

    fn pop_front_event(&mut self) -> Option<StreamEvent> {
        let ev = self.events.pop_front()?;
        let (frames, dur_ns, term) = event_metrics(&ev);
        self.buffered_frames = self.buffered_frames.saturating_sub(frames);
        self.buffered_duration_ns = self.buffered_duration_ns.saturating_sub(dur_ns);
        self.terminal_markers = self.terminal_markers.saturating_sub(term);
        Some(ev)
    }

    fn is_prebuffer_ready(&self, target_frames: usize) -> bool {
        self.buffered_frames >= target_frames || self.terminal_markers > 0
    }
}

/// Thread-safe, memory-bounded streaming block buffer with epoch invalidation.
pub struct BoundedBlockStreamBuffer {
    inner: Mutex<BufferInner>,
    cv: Condvar,
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
                events: VecDeque::new(),
                buffered_frames: 0,
                buffered_duration_ns: 0,
                terminal_markers: 0,
                max_buffered_frames: max_buffered_frames.max(65_536),
                max_buffered_duration_ns: max_buffered_duration.as_nanos().max(1),
                track_min_buffered_ns: None,
                deferred_drops: Vec::with_capacity(MAX_DEFERRED_DROP_BLOCKS),
                pin_blocks,
                pinned_guards: HashMap::new(),
                deferred_guards: Vec::with_capacity(MAX_DEFERRED_DROP_BLOCKS),
                closed: false,
            }),
            cv: Condvar::new(),
        })
    }

    pub fn current_epoch(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).epoch
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
    /// on the producer thread (avoids `HeapFree` / `VirtualFree` / `VirtualUnlock` on the RT thread).
    pub fn recycle_consumed_block(&self, buf: InterleavedInt32Buffer) {
        let ptr_key = buf.frames.as_ptr() as usize;
        let (overflow_guard, overflow_buf) = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let guard = st.pinned_guards.remove(&ptr_key);
            if st.deferred_drops.len() < MAX_DEFERRED_DROP_BLOCKS {
                if let Some(g) = guard {
                    st.deferred_guards.push(g);
                }
                st.deferred_drops.push(buf);
                (None, None)
            } else {
                (guard, Some(buf))
            }
        };
        // Unlock before freeing if overflow occurred.
        drop(overflow_guard);
        drop(overflow_buf);
    }

    /// Explicitly unlock and drop any deferred consumed PCM blocks on a non-RT thread.
    pub fn drain_deferred_drops(&self) {
        let (guards, drops) = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            (
                std::mem::take(&mut st.deferred_guards),
                std::mem::take(&mut st.deferred_drops),
            )
        };
        drop(guards);
        drop(drops);
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
        let (ep, guards, drops) = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let res = st.flush_locked();
            self.cv.notify_all();
            res
        };
        drop(guards);
        drop(drops);
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
                && matches!(st.events.front(), Some(StreamEvent::TrackStart { .. }))
            {
                if let Some(StreamEvent::TrackStart { path, .. }) = st.pop_front_event() {
                    skipped_front_path = Some(path);
                }
            }
            if let Some(next_idx) = st.events.iter().position(|ev| {
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
                    Some(StreamEvent::TrackStart {
                        had_gapless_carryover: true,
                        ..
                    })
                );
                if !(next_has_baked_carryover && (next_idx > 0 || skipped_front_path.is_some())) {
                    let mut drop_frames = 0usize;
                    let mut drop_dur_ns = 0u128;
                    let mut drop_terms = 0usize;
                    let drained: Vec<StreamEvent> = st.events.drain(..next_idx).collect();
                    for ev in drained {
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
        let (ep, guards, drops) = st.flush_locked();
        self.cv.notify_all();
        drop(st);
        drop(guards);
        drop(drops);
        (false, ep, skipped_front_path)
    }

    /// Permanently close the buffer for graceful shutdown, waking all blocked threads.
    pub fn close(&self) {
        let (guards, drops) = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.closed = true;
            st.events.clear();
            st.buffered_frames = 0;
            st.buffered_duration_ns = 0;
            st.terminal_markers = 0;
            st.track_min_buffered_ns = None;
            let mut g = std::mem::take(&mut st.deferred_guards);
            g.extend(st.pinned_guards.drain().map(|(_, guard)| guard));
            let d = std::mem::take(&mut st.deferred_drops);
            self.cv.notify_all();
            (g, d)
        };
        drop(guards);
        drop(drops);
    }

    /// Producer side: push a stream event, applying backpressure whenever buffered PCM reaches
    /// either the duration cap (`24 s` normal / `30 s` ultimate) or `max_buffered_frames`.
    /// Also drains any RT-recycled PCM blocks so `VirtualUnlock` / `HeapFree` runs here off-RT.
    pub fn push_for_epoch(&self, epoch: u64, event: StreamEvent) -> bool {
        let (frames_in_event, dur_ns_in_event, _) = event_metrics(&event);
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut guards = std::mem::take(&mut st.deferred_guards);
        let mut drops = std::mem::take(&mut st.deferred_drops);
        let should_pin = st.pin_blocks;
        loop {
            if st.closed || st.epoch != epoch {
                drop(st);
                drop(guards);
                drop(drops);
                return false;
            }
            // Control markers pass immediately; PCM blocks respect both duration and RAM caps
            // (always allowing at least 1 block if no PCM frames are currently buffered).
            if frames_in_event == 0
                || st.buffered_frames == 0
                || (st.buffered_frames + frames_in_event <= st.max_buffered_frames
                    && st.buffered_duration_ns.saturating_add(dur_ns_in_event)
                        <= st.max_buffered_duration_ns)
            {
                if should_pin {
                    if let StreamEvent::PcmBlock(ref b) = event {
                        if !b.frames.is_empty() {
                            let ptr_key = b.frames.as_ptr() as usize;
                            let guard = MemoryLockGuard::lock_slice(&b.frames);
                            st.pinned_guards.insert(ptr_key, guard);
                        }
                    }
                }
                st.push_back_event(event);
                if !st.deferred_guards.is_empty() {
                    guards.append(&mut st.deferred_guards);
                }
                if !st.deferred_drops.is_empty() {
                    drops.append(&mut st.deferred_drops);
                }
                self.cv.notify_all();
                drop(st);
                drop(guards);
                drop(drops);
                return true;
            }
            if !drops.is_empty() || !guards.is_empty() {
                drop(st);
                guards.clear();
                drops.clear();
                st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if !st.deferred_guards.is_empty() {
                    guards.append(&mut st.deferred_guards);
                }
                if !st.deferred_drops.is_empty() {
                    drops.append(&mut st.deferred_drops);
                }
                continue;
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            if !st.deferred_guards.is_empty() {
                guards.append(&mut st.deferred_guards);
            }
            if !st.deferred_drops.is_empty() {
                drops.append(&mut st.deferred_drops);
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
        let ev = st.pop_front_event()?;
        self.cv.notify_all();
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
    /// for this track without ever sleeping on the condvar.
    ///
    /// Returns `PopBlockResult::Underrun` immediately when the queue is empty so the RT loop
    /// can emit digital silence instead of blocking hardware DMA notifications.
    pub fn try_pop_track_pcm_block(&self, expected_epoch: u64) -> PopBlockResult {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return PopBlockResult::StaleOrClosed;
        }
        self.pop_track_pcm_locked(&mut st)
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
        self.pop_track_pcm_locked(&mut st)
    }

    fn pop_track_pcm_locked(&self, st: &mut BufferInner) -> PopBlockResult {
        match st.events.front() {
            Some(StreamEvent::PcmBlock(_)) => {
                if let Some(StreamEvent::PcmBlock(b)) = st.pop_front_event() {
                    if st.terminal_markers == 0 {
                        let rem = st.buffered_duration_ns;
                        st.track_min_buffered_ns = Some(match st.track_min_buffered_ns {
                            Some(prev) => prev.min(rem),
                            None => rem,
                        });
                    }
                    self.cv.notify_all();
                    PopBlockResult::Block(b)
                } else {
                    PopBlockResult::Underrun
                }
            }
            Some(StreamEvent::TrackEnd) => {
                let _ = st.pop_front_event();
                self.cv.notify_all();
                PopBlockResult::TrackEnd
            }
            // Leave next TrackStart / EndOfQueue / FatalError at the front for outer loop!
            Some(_) => PopBlockResult::TrackEnd,
            None => {
                st.track_min_buffered_ns = Some(0);
                PopBlockResult::Underrun
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
}
