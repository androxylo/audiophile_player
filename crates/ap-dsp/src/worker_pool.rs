//! Persistent zero-dependency scoped worker pools for real-time-safe parallel DSP (Review #7 F12).
//!
//! Eliminates per-block OS thread creation (`std::thread::scope` / `CreateThread` + 2 MB stack
//! allocation per block) in [`crate::convolution`] and [`crate::upsampler_fft`] while preserving
//! **bit-identical** output (`0 ULPs` difference vs sequential execution).
//!
//! ## Deadlock Freedom
//! Uses two strictly ordered persistent pools forming an acyclic DAG:
//! 1. `channel_pool` (L/R stereo fork via [`join_stereo`]): runs the R channel on the calling
//!    thread and dispatches the L channel to a long-lived stereo worker.
//! 2. `mac_pool` (disjoint frequency-bin MAC slices via [`for_each_mac_slice`]): runs slice 0 on
//!    the calling thread and dispatches slices `1..k` to long-lived MAC workers.
//!
//! Because `channel_pool` workers only submit to `mac_pool` and `mac_pool` workers never submit
//! nested tasks, pool exhaustion deadlock is impossible.

use std::any::Any;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

/// Minimum `nonzero_partitions * spec_len` work threshold before fanning out MAC bin slices.
pub const MAC_PARALLEL_MIN_WORK: usize = 131_072;

/// Total number of persistent DSP worker OS threads spawned across process lifetime (telemetry
/// for asserting zero thread creation after warmup in tests).
static WORKER_SPAWN_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Return total persistent DSP worker threads ever spawned in this process.
pub fn worker_spawn_count() -> usize {
    WORKER_SPAWN_COUNT.load(Ordering::Relaxed)
}

/// Recommended MAC slice count for a single channel when L/R already run concurrently, avoiding
/// CPU oversubscription while keeping slice 0 on the calling thread.
pub fn mac_slice_count(parallel_stereo: bool) -> usize {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    if parallel_stereo {
        (cores / 2).clamp(1, 4)
    } else {
        cores.clamp(1, 4)
    }
}

type ErasedJob = Box<dyn FnOnce() + Send + 'static>;

struct PoolQueue {
    jobs: VecDeque<ErasedJob>,
}

struct PersistentPool {
    queue: Arc<(Mutex<PoolQueue>, Condvar)>,
    num_workers: usize,
}

impl PersistentPool {
    fn new(name_prefix: &'static str, num_workers: usize) -> Self {
        let num_workers = num_workers.max(1);
        let queue = Arc::new((
            Mutex::new(PoolQueue {
                jobs: VecDeque::with_capacity(64),
            }),
            Condvar::new(),
        ));
        for idx in 0..num_workers {
            let q = Arc::clone(&queue);
            WORKER_SPAWN_COUNT.fetch_add(1, Ordering::Relaxed);
            thread::Builder::new()
                .name(format!("{name_prefix}-{idx}"))
                .spawn(move || loop {
                    let job = {
                        let (lock, cv) = &*q;
                        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while st.jobs.is_empty() {
                            st = cv.wait(st).unwrap_or_else(|e| e.into_inner());
                        }
                        st.jobs.pop_front().unwrap()
                    };
                    job();
                })
                .expect("failed to spawn persistent DSP worker thread");
        }
        Self { queue, num_workers }
    }

    fn submit(&self, job: ErasedJob) {
        let (lock, cv) = &*self.queue;
        let mut st = lock.lock().unwrap_or_else(|e| e.into_inner());
        st.jobs.push_back(job);
        cv.notify_one();
    }
}

fn channel_pool() -> &'static PersistentPool {
    static POOL: OnceLock<PersistentPool> = OnceLock::new();
    POOL.get_or_init(|| PersistentPool::new("ap-dsp-ch", 2))
}

fn mac_pool() -> &'static PersistentPool {
    static POOL: OnceLock<PersistentPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        // Up to 8 persistent MAC workers (covers 2 stereo channels x 3 offloaded slices cleanly).
        PersistentPool::new("ap-dsp-mac", cores.clamp(2, 8))
    })
}

struct ScopeLatch {
    remaining: Mutex<usize>,
    cv: Condvar,
    panic_payload: Mutex<Option<Box<dyn Any + Send + 'static>>>,
}

impl ScopeLatch {
    fn new(count: usize) -> Arc<Self> {
        Arc::new(Self {
            remaining: Mutex::new(count),
            cv: Condvar::new(),
            panic_payload: Mutex::new(None),
        })
    }

    fn record_panic(&self, payload: Box<dyn Any + Send + 'static>) {
        let mut guard = self.panic_payload.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(payload);
        }
    }

    fn complete_one(&self) {
        let mut rem = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
        *rem = rem.saturating_sub(1);
        if *rem == 0 {
            self.cv.notify_all();
        }
    }

    fn wait_all(&self) {
        let mut rem = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
        while *rem > 0 {
            rem = self.cv.wait(rem).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn take_panic(&self) -> Option<Box<dyn Any + Send + 'static>> {
        self.panic_payload
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

/// RAII guard guaranteeing all dispatched scoped workers complete BEFORE the caller stack frame
/// unwinds — even if the caller's inline closure panics!
struct WaitGuard {
    latch: Arc<ScopeLatch>,
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.latch.wait_all();
    }
}

/// Run `left_fn` on the persistent stereo worker pool while executing `right_fn` inline on the
/// calling thread, joining cleanly before returning and propagating any panic.
pub fn join_stereo<FL, FR, RL, RR>(left_fn: FL, right_fn: FR) -> (RL, RR)
where
    FL: FnOnce() -> RL + Send,
    FR: FnOnce() -> RR,
    RL: Send,
{
    let pool = channel_pool();
    let latch = ScopeLatch::new(1);
    let left_slot: Mutex<Option<RL>> = Mutex::new(None);

    // SAFETY: `guard` waits unconditionally on `latch` in `Drop` before `join_stereo` returns or
    // unwinds, so neither `left_fn` nor `&left_slot` can ever outlive this stack frame.
    let left_slot_ptr = &left_slot as *const Mutex<Option<RL>> as usize;
    let latch_worker = Arc::clone(&latch);

    // Erase the lifetime of `left_fn` strictly inside the `WaitGuard`-protected scope.
    let boxed: Box<dyn FnOnce() + Send + '_> = Box::new(move || {
        let res = catch_unwind(AssertUnwindSafe(left_fn));
        match res {
            Ok(val) => {
                // SAFETY: `left_slot` lives on `join_stereo`'s stack frame until `latch` hits 0.
                let slot = unsafe { &*(left_slot_ptr as *const Mutex<Option<RL>>) };
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(val);
            }
            Err(payload) => latch_worker.record_panic(payload),
        }
        latch_worker.complete_one();
    });
    let static_job: ErasedJob = unsafe { std::mem::transmute(boxed) };

    let guard = WaitGuard {
        latch: Arc::clone(&latch),
    };
    pool.submit(static_job);

    let right_res = right_fn();
    drop(guard);

    if let Some(payload) = latch.take_panic() {
        resume_unwind(payload);
    }
    let left_res = left_slot
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .expect("join_stereo worker completed without storing result");
    (left_res, right_res)
}

/// Split `buf` into up to `max_slices` disjoint chunks, running chunk 0 inline on the calling
/// thread and dispatching chunks `1..` to the persistent MAC worker pool (`0 ULPs` difference vs
/// sequential execution).
pub fn for_each_mac_slice<T, F>(buf: &mut [T], max_slices: usize, f: F)
where
    T: Send,
    F: Fn(usize, usize, &mut [T]) + Sync,
{
    let slices = max_slices.min(mac_pool().num_workers + 1).max(1);
    if slices <= 1 || buf.len() < 512 {
        f(0, 0, buf);
        return;
    }

    let chunk_len = buf.len().div_ceil(slices);
    let mut iter = buf.chunks_mut(chunk_len);
    let Some(first_chunk) = iter.next() else {
        return;
    };
    let rest: Vec<(usize, usize, &mut [T])> = iter
        .enumerate()
        .map(|(i, chunk)| {
            let slice_idx = i + 1;
            let start = slice_idx * chunk_len;
            (slice_idx, start, chunk)
        })
        .collect();

    if rest.is_empty() {
        f(0, 0, first_chunk);
        return;
    }

    let pool = mac_pool();
    let latch = ScopeLatch::new(rest.len());
    let guard = WaitGuard {
        latch: Arc::clone(&latch),
    };
    let f_ref = &f;

    for (slice_idx, start, chunk) in rest {
        let latch_w = Arc::clone(&latch);
        // SAFETY: `guard` waits for all `rest.len()` jobs to complete in `Drop` before this
        // function returns or unwinds, so `chunk` and `f_ref` remain valid for the job's lifetime.
        let boxed: Box<dyn FnOnce() + Send + '_> = Box::new(move || {
            let res = catch_unwind(AssertUnwindSafe(|| {
                f_ref(slice_idx, start, chunk);
            }));
            if let Err(payload) = res {
                latch_w.record_panic(payload);
            }
            latch_w.complete_one();
        });
        let static_job: ErasedJob = unsafe { std::mem::transmute(boxed) };
        pool.submit(static_job);
    }

    // Execute slice 0 inline on the calling thread!
    f_ref(0, 0, first_chunk);
    drop(guard);

    if let Some(payload) = latch.take_panic() {
        resume_unwind(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_pool_spawns_once_and_produces_exact_slice_results() {
        // Warmup call initializes persistent pools.
        let (a, b) = join_stereo(|| 21 * 2, || 10 + 5);
        assert_eq!((a, b), (42, 15));
        let mut warm = vec![0usize; 4096];
        for_each_mac_slice(&mut warm, 4, |_idx, start, chunk| {
            for (i, slot) in chunk.iter_mut().enumerate() {
                *slot = start + i;
            }
        });
        let count_after_warmup = worker_spawn_count();
        assert!(count_after_warmup > 0);

        // 1,000 subsequent dispatches MUST spawn ZERO new OS threads!
        for _ in 0..1_000 {
            let mut data = vec![0usize; 2048];
            for_each_mac_slice(&mut data, 4, |_idx, start, chunk| {
                for (i, slot) in chunk.iter_mut().enumerate() {
                    *slot = (start + i) * 3 + 1;
                }
            });
            assert_eq!(data[0], 1);
            assert_eq!(data[2047], 2047 * 3 + 1);
        }
        assert_eq!(
            worker_spawn_count(),
            count_after_warmup,
            "F12: persistent worker pool must not spawn any threads after warmup"
        );
    }
}
