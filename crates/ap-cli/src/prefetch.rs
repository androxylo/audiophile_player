//! Bounded-memory streaming PCM block buffer between the DSP Producer and WDM-KS Consumer.
//!
//! Designed for BOTH arbitrarily large FLAC files (multi-hour 32/384 masters) AND continuous
//! Milestone 2 live streams (Tidal):
//! - Strictly caps buffered PCM frames in RAM (default: `12 s @ 384 kHz` = `4,608,000` frames
//!   ≈ 36 MB max RAM — NEVER grows unbounded regardless of file length!).
//! - Supports a configurable **prebuffer watermark** (e.g. `2.0 s` of PCM ready before starting
//!   WDM-KS hardware DMA) so playback starts cleanly with zero hiccups.
//! - Epoch-guarded instant flush (`reset_and_flush`) on Pause or mode/folder switch drops stale
//!   buffered blocks immediately and discards in-flight producer work cleanly.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ap_control::state::PlayModeLabel;
use ap_core::buffer::InterleavedInt32Buffer;
use ap_dsp::pipeline::PipelineReport;

/// Maximum buffered PCM frames held in RAM ahead of the DAC (~12 s @ 384 kHz ≈ 36 MB).
pub const DEFAULT_MAX_BUFFERED_FRAMES: usize = 384_000 * 12;

/// Minimum cushion buffered before starting a new WDM-KS stream (~2 s at output rate).
pub const PREBUFFER_SECONDS: u32 = 2;

/// Ordered stream events flowing from the DSP Producer into the WDM-KS Consumer.
#[derive(Debug)]
pub enum StreamEvent {
    /// Marks the start of a track/stream (carries output sample rate & pipeline metadata).
    TrackStart {
        path: PathBuf,
        mode_label: PlayModeLabel,
        report: PipelineReport,
        total_duration_s: Option<f64>,
    },
    /// One ready-to-play bounded integer PCM block.
    PcmBlock(InterleavedInt32Buffer),
    /// Clean end-of-track marker (signals WDM-KS Consumer to finish this track cleanly).
    TrackEnd,
    /// Sequential folder or single-file queue finished cleanly -> pause & release DAC.
    EndOfQueue(String),
    /// Configuration or fatal source error -> pause & log cleanly.
    FatalError(String),
}

struct BufferInner {
    epoch: u64,
    events: VecDeque<StreamEvent>,
    buffered_frames: usize,
    max_buffered_frames: usize,
    closed: bool,
}

/// Thread-safe, memory-bounded streaming block buffer with epoch invalidation.
pub struct BoundedBlockStreamBuffer {
    inner: Mutex<BufferInner>,
    cv: Condvar,
}

impl BoundedBlockStreamBuffer {
    pub fn new(max_buffered_frames: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BufferInner {
                epoch: 0,
                events: VecDeque::new(),
                buffered_frames: 0,
                max_buffered_frames: max_buffered_frames.max(65_536),
                closed: false,
            }),
            cv: Condvar::new(),
        })
    }

    pub fn current_epoch(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).epoch
    }

    #[cfg(test)]
    pub fn buffered_frames(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .buffered_frames
    }

    /// Increment epoch and immediately drop all queued PCM blocks (frees RAM on Pause / switch).
    pub fn reset_and_flush(&self) -> u64 {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.epoch = st.epoch.wrapping_add(1);
        st.events.clear();
        st.buffered_frames = 0;
        let ep = st.epoch;
        self.cv.notify_all();
        ep
    }

    /// Permanently close the buffer for graceful shutdown, waking all blocked threads.
    pub fn close(&self) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.closed = true;
        st.events.clear();
        st.buffered_frames = 0;
        self.cv.notify_all();
    }

    /// Producer side: push a stream event, applying backpressure whenever buffered PCM frames
    /// reach `max_buffered_frames` (guarantees strictly bounded RAM on arbitrarily huge files).
    pub fn push_for_epoch(&self, epoch: u64, event: StreamEvent) -> bool {
        let frames_in_event = match &event {
            StreamEvent::PcmBlock(b) => b.frames.len(),
            _ => 0,
        };
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if st.closed || st.epoch != epoch {
                return false;
            }
            // Control markers pass immediately; PCM blocks respect the bounded RAM cap
            // (always allowing at least 1 block if buffer is currently empty).
            if frames_in_event == 0
                || st.buffered_frames == 0
                || st.buffered_frames + frames_in_event <= st.max_buffered_frames
            {
                st.buffered_frames += frames_in_event;
                st.events.push_back(event);
                self.cv.notify_all();
                return true;
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
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
        let ev = st.events.pop_front()?;
        if let StreamEvent::PcmBlock(ref b) = ev {
            st.buffered_frames = st.buffered_frames.saturating_sub(b.frames.len());
        }
        self.cv.notify_all();
        Some(ev)
    }

    /// Consumer side: wait until at least `target_frames` of PCM are pre-buffered in RAM
    /// (or a `TrackEnd` / terminal event is already queued) before opening WDM-KS DMA,
    /// eliminating startup hiccups. Returns `false` if epoch changed or closed.
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
        let is_ready = |s: &BufferInner| {
            s.buffered_frames >= target_frames
                || s.events.iter().any(|e| {
                    matches!(
                        e,
                        StreamEvent::TrackEnd
                            | StreamEvent::EndOfQueue(_)
                            | StreamEvent::FatalError(_)
                    )
                })
        };
        let (st, _) = self
            .cv
            .wait_timeout_while(st, poll_slice, |s| {
                !s.closed && s.epoch == expected_epoch && !is_ready(s)
            })
            .unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return None;
        }
        Some(is_ready(&st))
    }

    /// Consumer side (inside active track playback): pop the next PCM block for this track.
    /// Returns `None` cleanly on `TrackEnd`, epoch change, or shutdown.
    pub fn pop_track_pcm_block(
        &self,
        expected_epoch: u64,
        timeout: Duration,
    ) -> Option<InterleavedInt32Buffer> {
        loop {
            let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if st.closed || st.epoch != expected_epoch {
                return None;
            }
            let (mut st, timed_out) = self
                .cv
                .wait_timeout_while(st, timeout, |s| {
                    !s.closed && s.epoch == expected_epoch && s.events.is_empty()
                })
                .unwrap_or_else(|e| e.into_inner());
            if st.closed || st.epoch != expected_epoch || timed_out.timed_out() {
                return None;
            }
            match st.events.front() {
                Some(StreamEvent::PcmBlock(_)) => {
                    if let Some(StreamEvent::PcmBlock(b)) = st.events.pop_front() {
                        st.buffered_frames = st.buffered_frames.saturating_sub(b.frames.len());
                        self.cv.notify_all();
                        return Some(b);
                    }
                }
                Some(StreamEvent::TrackEnd) => {
                    let _ = st.events.pop_front();
                    self.cv.notify_all();
                    return None;
                }
                // Leave next TrackStart / EndOfQueue / FatalError at the front for outer loop!
                Some(_) | None => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn make_block(frames: usize) -> InterleavedInt32Buffer {
        InterleavedInt32Buffer {
            sample_rate_hz: 384_000,
            frames: vec![[1, -1]; frames],
        }
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
        while buf
            .wait_prebuffer_watermark(ep, 100_000, Duration::from_millis(50))
            .unwrap()
            == false
        {}
        assert!(buf.buffered_frames() <= 100_000);

        // Drain all 4 blocks cleanly until TrackEnd returns None
        let mut total = 0;
        while let Some(blk) = buf.pop_track_pcm_block(ep, Duration::from_secs(1)) {
            total += blk.frames.len();
        }
        assert_eq!(total, 200_000);
        assert_eq!(buf.buffered_frames(), 0);
        producer.join().unwrap();
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
}
