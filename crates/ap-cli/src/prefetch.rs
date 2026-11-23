//! Bounded look-ahead pre-buffering queue (`capacity = 1` prepared track in RAM).
//!
//! Eliminates hiccups & multi-second DSP gaps between consecutive tracks:
//! - A background **DSP Producer** thread decodes and runs the full `AudioPipeline`
//!   for Track `N+1` while the **WDM-KS Consumer** streams Track `N` to the DAC.
//! - Strictly bounded to 1 buffered track ahead so RAM never grows unbounded.
//! - Uses an `epoch` generation counter so Pause or any mode/folder change immediately
//!   flushes stale buffered tracks and discards in-flight stale renders cleanly.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Item handed from the DSP Producer to the WDM-KS Consumer.
#[derive(Debug)]
pub enum PrefetchEvent<T> {
    /// A fully processed track ready in RAM for immediate hiccup-free playback.
    Track(T),
    /// The active sequential folder or single-file queue finished cleanly -> pause & release DAC.
    EndOfQueue(String),
    /// A configuration or source error occurred -> pause & log cleanly.
    FatalError(String),
}

struct QueueInner<T> {
    epoch: u64,
    slot: Option<PrefetchEvent<T>>,
    closed: bool,
}

/// Bounded 1-slot epoch-guarded look-ahead buffer between DSP Producer and WDM-KS Consumer.
pub struct TrackPrefetchQueue<T> {
    inner: Mutex<QueueInner<T>>,
    cv: Condvar,
}

impl<T> TrackPrefetchQueue<T> {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(QueueInner {
                epoch: 0,
                slot: None,
                closed: false,
            }),
            cv: Condvar::new(),
        })
    }

    pub fn current_epoch(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).epoch
    }

    /// Increment epoch and drop any stale buffered track immediately (called on Pause or
    /// mode/folder switch). Returns the new epoch.
    pub fn reset_and_flush(&self) -> u64 {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.epoch = st.epoch.wrapping_add(1);
        st.slot = None;
        let ep = st.epoch;
        self.cv.notify_all();
        ep
    }

    /// Close the queue permanently for graceful process shutdown, waking all blocked threads.
    pub fn close(&self) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.closed = true;
        st.slot = None;
        self.cv.notify_all();
    }

    /// Producer side: wait until the 1-slot buffer is empty (or epoch changed / closed),
    /// then deposit `event` ONLY if `epoch` is still current.
    /// Returns `true` if deposited, `false` if discarded due to epoch change or shutdown.
    pub fn push_for_epoch(&self, epoch: u64, event: PrefetchEvent<T>) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if st.closed || st.epoch != epoch {
                return false;
            }
            if st.slot.is_none() {
                st.slot = Some(event);
                self.cv.notify_all();
                return true;
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Consumer side: wait up to `timeout` for the next buffered event matching `expected_epoch`.
    /// Returns `None` on timeout, epoch change, or shutdown so callers can also poll control events.
    pub fn pop_for_epoch_timeout(
        &self,
        expected_epoch: u64,
        timeout: Duration,
    ) -> Option<PrefetchEvent<T>> {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return None;
        }
        let (mut st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| {
                !s.closed && s.epoch == expected_epoch && s.slot.is_none()
            })
            .unwrap_or_else(|e| e.into_inner());
        if st.closed || st.epoch != expected_epoch {
            return None;
        }
        let ev = st.slot.take();
        if ev.is_some() {
            self.cv.notify_all();
        }
        ev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn producer_prefetches_next_track_while_consumer_is_busy_for_zero_gap_pop() {
        let q: Arc<TrackPrefetchQueue<String>> = TrackPrefetchQueue::new();
        let ep = q.current_epoch();
        let q_prod = Arc::clone(&q);

        let producer = thread::spawn(move || {
            assert!(q_prod.push_for_epoch(ep, PrefetchEvent::Track("track-1".into())));
            assert!(q_prod.push_for_epoch(ep, PrefetchEvent::Track("track-2".into())));
        });

        // Pop track 1 and simulate playing it for 60 ms while producer fills track 2 into RAM.
        match q.pop_for_epoch_timeout(ep, Duration::from_secs(1)) {
            Some(PrefetchEvent::Track(t)) => assert_eq!(t, "track-1"),
            other => panic!("unexpected: {other:?}"),
        }
        thread::sleep(Duration::from_millis(60));

        // Track 2 must already be buffered and pop instantaneously (< 20 ms).
        let t0 = Instant::now();
        match q.pop_for_epoch_timeout(ep, Duration::from_secs(1)) {
            Some(PrefetchEvent::Track(t)) => assert_eq!(t, "track-2"),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(t0.elapsed() < Duration::from_millis(20));

        producer.join().unwrap();
    }

    #[test]
    fn reset_and_flush_discards_stale_tracks_and_rejects_old_epoch_pushes() {
        let q: Arc<TrackPrefetchQueue<u32>> = TrackPrefetchQueue::new();
        let ep0 = q.current_epoch();
        assert!(q.push_for_epoch(ep0, PrefetchEvent::Track(100)));

        // Flush on Pause / mode switch -> bumps epoch and drops track 100
        let ep1 = q.reset_and_flush();
        assert_ne!(ep0, ep1);

        // Stale producer finishing work for ep0 must be rejected cleanly
        assert!(!q.push_for_epoch(ep0, PrefetchEvent::Track(101)));

        // New epoch push succeeds cleanly
        assert!(q.push_for_epoch(ep1, PrefetchEvent::Track(200)));
        match q.pop_for_epoch_timeout(ep1, Duration::from_secs(1)) {
            Some(PrefetchEvent::Track(v)) => assert_eq!(v, 200),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn close_wakes_blocked_consumer_cleanly() {
        let q: Arc<TrackPrefetchQueue<u32>> = TrackPrefetchQueue::new();
        let ep = q.current_epoch();
        let q2 = Arc::clone(&q);

        let consumer = thread::spawn(move || q2.pop_for_epoch_timeout(ep, Duration::from_secs(5)));
        thread::sleep(Duration::from_millis(25));
        q.close();
        assert!(consumer.join().unwrap().is_none());
    }
}
