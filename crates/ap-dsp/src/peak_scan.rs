// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! High-accuracy whole-file peak scanner for Ultimate Mode E3 (`Issue #23 §3.1`, P2a).
//!
//! Computes exact sample peak and high-accuracy true peak (`tp8x-32`: 8x oversampling,
//! 32-tap-per-phase Kaiser-windowed sinc with 3-point parabolic inter-sample peak refinement)
//! guaranteeing `<= 0.1 dB` under-read across `0..0.45 * fs` at 44.1/48/96/192 kHz.
//!
//! ## Two-Pass Gated Architecture (Cheap & Bounded RAM)
//! - **Pass A:** Streams decoded stereo blocks, tracks per-block and global sample peaks, and
//!   retains only blocks whose peak is within `6.02 dB` of the running global sample peak
//!   (along with exact overlap context on both edges). Earlier candidates falling below the
//!   rising gate are pruned immediately, and candidate RAM is bounded by flushing Pass B in
//!   parallel batches if a long constant-loudness track exceeds `MAX_BUFFERED_CANDIDATE_FRAMES`.
//! - **Pass B:** Evaluates the 8x polyphase Kaiser true-peak meter ONLY on qualifying blocks,
//!   split across the persistent low-priority DSP worker pool (`worker_pool::for_each_batch`),
//!   yielding the exact same maximum true peak as an ungated full-track scan.

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use ap_core::buffer::StereoF64Buffer;
use ap_core::filter_db::TRUE_PEAK_METER_8X_64;

use crate::upsampler::bessel_i0_f64;
use crate::worker_pool;

/// Oversampling factor of the E3 true-peak meter (`8x`).
pub const PEAK_SCAN_OVERSAMPLE: usize = 8;

/// Taps per polyphase branch of the E3 true-peak meter (`64`, i.e. 512-tap 8x prototype,
/// Review #10 §1.4 / HP-03: guarantees `<= 0.1 dB` under-read across `0..=0.45 * fs` even on
/// short fractional-phase Gaussian tone bursts).
pub const PEAK_SCAN_TAPS_PER_PHASE: usize = 64;

/// Half-length of each polyphase branch (`32` samples on each side of the center tap).
const HALF_TAPS: usize = PEAK_SCAN_TAPS_PER_PHASE / 2;

/// Overlap margin (`36` samples > `HALF_TAPS + 1`) retained on each side of a candidate block so
/// both the 64-tap FIR kernel and the +/-1 8x-sample parabolic refinement are exact across
/// arbitrary block boundaries.
const EDGE_CTX: usize = HALF_TAPS + 4;

/// Context window retained before/after each candidate block (`2 * EDGE_CTX = 72` samples) so
/// evaluation can extend `EDGE_CTX` samples into quiet neighbor blocks while still having full
/// `>= EDGE_CTX` real history for the 64-tap kernel.
const STORED_CTX: usize = 2 * EDGE_CTX;

/// Kaiser beta for the 64-tap/phase (`512`-tap 8x) true-peak prototype (`~76 dB` stopband,
/// half-transition `~0.037 * fs`, flat to `0.463 * fs` so `0.45 * fs` stays `< 0.05 dB` even at
/// mid-phase `4/8` on short bursts; Review #10 HP-03).
const KAISER_BETA: f64 = 7.5;

/// Pass-B gating threshold in dB below the global sample peak (`Issue #23 §3.1`).
/// An inter-sample peak > +6.02 dB above the sample peak is already treated as unrealistic
/// (`MAX_REALISTIC_TRUE_PEAK_DB`), so blocks strictly below `global_sp - 6.02 dB` cannot
/// exceed the true peak of the loudest blocks.
pub const PEAK_SCAN_GATE_DB: f64 = 6.02;

/// Conservative linear gate ratio (`0.495`, i.e. `-6.11 dB`) slightly below `10^(-6.02/20)`
/// (`0.50003`) to avoid floating-point boundary misses.
const GATE_LINEAR_RATIO: f64 = 0.495;

/// Upper bound on in-memory candidate frames before flushing a parallel Pass-B batch
/// (`2_000_000` stereo f64 frames ~= 32 MiB), keeping RAM bounded even on long brickwalled tracks.
pub const DEFAULT_MAX_BUFFERED_CANDIDATE_FRAMES: usize = 2_000_000;

/// Authoritative meter id stored in Schema-3 YAML alongside `true_peak_dbtp`.
pub const PEAK_SCAN_METER_ID: &str = TRUE_PEAK_METER_8X_64;

/// Output of a completed E3 whole-file peak scan (`ap_dsp::peak_scan`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeakScanResult {
    pub sample_peak_linear: f64,
    pub sample_peak_dbfs: f64,
    pub true_peak_linear: f64,
    pub true_peak_dbtp: f64,
    pub frames_scanned: u64,
    pub blocks_total: u64,
    pub blocks_oversampled: u64,
    pub meter_id: &'static str,
}

#[inline]
fn linear_to_db(v: f64) -> f64 {
    if v > 0.0 && v.is_finite() {
        20.0 * v.log10()
    } else {
        -180.0
    }
}

/// Precomputed 8x oversampling, 32-tap-per-phase Kaiser-windowed sinc true-peak meter (`tp8x-32`).
///
/// Phase `0` is exact sample identity (`x[i]`); phases `1..8` evaluate fractional offsets
/// `p / 8` between `x[i]` and `x[i + 1]`. Consecutive 8x output samples `(y_prev, y_curr, y_next)`
/// undergo 3-point quadratic vertex refinement whenever `|y_curr|` is a local extremum, eliminating
/// the 8x sampling-grid phase miss (`-0.136 dB` at `0.45 * fs`) down to `< 0.03 dB`.
pub struct TruePeakMeter8x {
    /// Polyphase coefficient bank for fractional phases `1..8` (`bank[p - 1]`).
    /// Index `k = 0..32` multiplies `samples[i - 15 + k]`, evaluating `x(i + p / 8)`.
    frac_bank: [[f64; PEAK_SCAN_TAPS_PER_PHASE]; PEAK_SCAN_OVERSAMPLE - 1],
}

static SHARED_METER_8X: OnceLock<TruePeakMeter8x> = OnceLock::new();

impl TruePeakMeter8x {
    /// Return a process-wide shared immutable instance of the precomputed `tp8x-32` meter.
    pub fn shared() -> &'static Self {
        SHARED_METER_8X.get_or_init(Self::new)
    }

    /// Design the 8x / 32-tap-per-phase Kaiser-windowed sinc polyphase bank.
    pub fn new() -> Self {
        let mut frac_bank = [[0.0_f64; PEAK_SCAN_TAPS_PER_PHASE]; PEAK_SCAN_OVERSAMPLE - 1];
        let inv_i0 = 1.0 / bessel_i0_f64(KAISER_BETA);
        let radius = HALF_TAPS as f64; // 16.0

        for p in 1..PEAK_SCAN_OVERSAMPLE {
            let frac = (p as f64) / (PEAK_SCAN_OVERSAMPLE as f64);
            let coeffs = &mut frac_bank[p - 1];
            let mut sum = 0.0_f64;
            for (k, slot) in coeffs.iter_mut().enumerate() {
                // Relative sample offset `m = k - 15` in `[-15..=16]`.
                // Evaluating continuous sinc at `t = i + frac` sums `x[i + m] * sinc(frac - m)`.
                let m = (k as isize) - ((HALF_TAPS as isize) - 1);
                let tau = frac - (m as f64);
                let sinc = if tau.abs() < 1e-15 {
                    1.0
                } else {
                    (PI * tau).sin() / (PI * tau)
                };
                let r = tau / radius;
                let win = if r.abs() < 1.0 {
                    bessel_i0_f64(KAISER_BETA * (1.0 - r * r).sqrt()) * inv_i0
                } else {
                    0.0
                };
                let v = sinc * win;
                *slot = v;
                sum += v;
            }
            if sum.abs() > 1e-12 {
                let inv_sum = 1.0 / sum;
                for c in coeffs.iter_mut() {
                    *c *= inv_sum;
                }
            }
        }

        Self { frac_bank }
    }

    /// Evaluate the 8x polyphase filter for fractional phase `p in 1..8` at integer index `i`
    /// inside `samples` (zero-padded outside `0..samples.len()`).
    #[inline]
    fn eval_phase(&self, samples: &[f64], i: usize, p_idx: usize) -> f64 {
        let coeffs = &self.frac_bank[p_idx];
        let n = samples.len();
        // Fast interior path: no bounds checks when `i >= 15` and `i + 16 < n`.
        if i >= HALF_TAPS - 1 && i + HALF_TAPS < n {
            let base = i - (HALF_TAPS - 1);
            let s = &samples[base..base + PEAK_SCAN_TAPS_PER_PHASE];
            let mut acc0 = 0.0_f64;
            let mut acc1 = 0.0_f64;
            let mut acc2 = 0.0_f64;
            let mut acc3 = 0.0_f64;
            let mut k = 0usize;
            while k < PEAK_SCAN_TAPS_PER_PHASE {
                acc0 += s[k] * coeffs[k];
                acc1 += s[k + 1] * coeffs[k + 1];
                acc2 += s[k + 2] * coeffs[k + 2];
                acc3 += s[k + 3] * coeffs[k + 3];
                k += 4;
            }
            (acc0 + acc1) + (acc2 + acc3)
        } else {
            let mut acc = 0.0_f64;
            let base = (i as isize) - ((HALF_TAPS as isize) - 1);
            for (k, &c) in coeffs.iter().enumerate() {
                let idx = base + (k as isize);
                if idx >= 0 && (idx as usize) < n {
                    acc += samples[idx as usize] * c;
                }
            }
            acc
        }
    }

    /// Scan a single channel `samples` over source-sample indices `[eval_start..eval_end]`,
    /// returning the maximum linear true-peak amplitude (including parabolic 3-point refinement
    /// across adjacent 8x output samples).
    pub fn scan_channel_range(&self, samples: &[f64], eval_start: usize, eval_end: usize) -> f64 {
        let n = samples.len();
        if n == 0 || eval_start >= eval_end || eval_start >= n {
            return 0.0;
        }
        let end = eval_end.min(n);

        // Track 3 consecutive 8x samples `(y_prev, y_curr, y_next)` to refine local extrema.
        // Seed `y_prev` with phase 7 of `eval_start - 1` when available so parabolic refinement
        // at `(eval_start, phase 0)` is exact across block splits!
        let mut y_prev = if eval_start > 0 {
            self.eval_phase(samples, eval_start - 1, PEAK_SCAN_OVERSAMPLE - 2)
        } else {
            0.0
        };
        let mut y_curr = samples[eval_start];
        let mut peak = y_curr.abs();

        #[inline(always)]
        fn push_8x(y_prev: &mut f64, y_curr: &mut f64, y_next: f64, peak: &mut f64) {
            let ac = y_curr.abs();
            if ac > *peak {
                *peak = ac;
            }
            // Refine strictly when `y_curr` is a local maximum or minimum of the 3-point stencil
            // and lies near/above the running peak (within 2% -> skips 99%+ of samples!).
            if ac > *peak * 0.96
                && ((*y_curr >= *y_prev && *y_curr >= y_next)
                    || (*y_curr <= *y_prev && *y_curr <= y_next))
            {
                let denom = *y_prev - 2.0 * *y_curr + y_next;
                if denom.abs() > 1e-18 {
                    let delta = 0.5 * (*y_prev - y_next) / denom;
                    if delta.abs() <= 0.5 + 1e-9 {
                        let refined = (*y_curr - 0.25 * (*y_prev - y_next) * delta).abs();
                        if refined > *peak {
                            *peak = refined;
                        }
                    }
                }
            }
            *y_prev = *y_curr;
            *y_curr = y_next;
        }

        // Fractional phases 1..8 of `eval_start`.
        for p_idx in 0..(PEAK_SCAN_OVERSAMPLE - 1) {
            let y_next = self.eval_phase(samples, eval_start, p_idx);
            push_8x(&mut y_prev, &mut y_curr, y_next, &mut peak);
        }

        // Remaining integer indices `(eval_start + 1)..end`.
        for i in (eval_start + 1)..end {
            push_8x(&mut y_prev, &mut y_curr, samples[i], &mut peak);
            for p_idx in 0..(PEAK_SCAN_OVERSAMPLE - 1) {
                let y_next = self.eval_phase(samples, i, p_idx);
                push_8x(&mut y_prev, &mut y_curr, y_next, &mut peak);
            }
        }

        // Finalize last fractional sample (`end - 1`, phase 7) against `samples[end]` (or 0.0).
        let next_after_end = if end < n { samples[end] } else { 0.0 };
        push_8x(&mut y_prev, &mut y_curr, next_after_end, &mut peak);
        // Check `y_curr` itself only if `end < n` wasn't outside our range; we already included
        // `end - 1` phase 7 in `peak` via `push_8x`.
        peak
    }

    /// Ungated full-slice stereo scan (used as reference in tests and for small buffers).
    pub fn scan_stereo_ungated(&self, left: &[f64], right: &[f64]) -> f64 {
        let pl = self.scan_channel_range(left, 0, left.len());
        let pr = self.scan_channel_range(right, 0, right.len());
        pl.max(pr)
    }
}

impl Default for TruePeakMeter8x {
    fn default() -> Self {
        Self::new()
    }
}

/// Candidate block buffered during Pass A awaiting Pass-B 8x oversampling.
struct CandidateBlock {
    block_sample_peak: f64,
    /// Up to `STORED_CTX` samples immediately preceding `body` (may be shorter near track start).
    left_ctx_l: Vec<f64>,
    left_ctx_r: Vec<f64>,
    body_l: Vec<f64>,
    body_r: Vec<f64>,
    /// Up to `STORED_CTX` samples immediately following `body` (filled from subsequent blocks).
    right_ctx_l: Vec<f64>,
    right_ctx_r: Vec<f64>,
    /// True if `left_ctx` starts at track frame 0 (so zero-padding before index 0 is exact).
    at_track_start: bool,
}

impl CandidateBlock {
    fn body_frames(&self) -> usize {
        self.body_l.len().min(self.body_r.len())
    }

    /// Evaluate the 8x true peak over this candidate block plus up to `EDGE_CTX` neighbor samples
    /// on both sides (so any sample whose 32-tap kernel touches this hot block is covered).
    fn eval_true_peak(&self, meter: &TruePeakMeter8x, at_track_end: bool) -> f64 {
        let eval_one = |left_ctx: &[f64], body: &[f64], right_ctx: &[f64]| -> f64 {
            let total = left_ctx.len() + body.len() + right_ctx.len();
            if total == 0 {
                return 0.0;
            }
            let mut joined = Vec::with_capacity(total);
            joined.extend_from_slice(left_ctx);
            joined.extend_from_slice(body);
            joined.extend_from_slice(right_ctx);

            let eval_start = if self.at_track_start {
                0
            } else {
                left_ctx.len().saturating_sub(EDGE_CTX)
            };
            let right_extend = if at_track_end {
                right_ctx.len().min(EDGE_CTX)
            } else {
                right_ctx.len().saturating_sub(EDGE_CTX).min(EDGE_CTX)
            };
            let eval_end = (left_ctx.len() + body.len() + right_extend).min(total);
            meter.scan_channel_range(&joined, eval_start, eval_end)
        };

        let pl = eval_one(&self.left_ctx_l, &self.body_l, &self.right_ctx_l);
        let pr = eval_one(&self.left_ctx_r, &self.body_r, &self.right_ctx_r);
        pl.max(pr)
    }
}

/// Two-pass gated whole-file peak scanner (`Issue #23 §3.1`).
///
/// Feed decoded blocks sequentially via [`Self::push_block`] (or [`Self::push_stereo_buffer`]),
/// then call [`Self::finish`] to run parallel Pass-B oversampling on the surviving hot blocks.
pub struct GatedPeakScanner {
    max_buffered_frames: usize,
    frames_scanned: u64,
    blocks_total: u64,
    blocks_oversampled: u64,
    global_sample_peak: f64,
    running_true_peak: f64,
    /// Rolling last `STORED_CTX` frames from Pass A for seeding the next candidate's `left_ctx`.
    tail_l: VecDeque<f64>,
    tail_r: VecDeque<f64>,
    candidates: Vec<CandidateBlock>,
    buffered_candidate_frames: usize,
}

impl GatedPeakScanner {
    pub fn new() -> Self {
        Self::with_max_buffered_frames(DEFAULT_MAX_BUFFERED_CANDIDATE_FRAMES)
    }

    pub fn with_max_buffered_frames(max_buffered_frames: usize) -> Self {
        Self {
            max_buffered_frames: max_buffered_frames.max(4_096),
            frames_scanned: 0,
            blocks_total: 0,
            blocks_oversampled: 0,
            global_sample_peak: 0.0,
            running_true_peak: 0.0,
            tail_l: VecDeque::with_capacity(STORED_CTX),
            tail_r: VecDeque::with_capacity(STORED_CTX),
            candidates: Vec::new(),
            buffered_candidate_frames: 0,
        }
    }

    /// Push one decoded stereo block into Pass A.
    pub fn push_stereo_buffer(&mut self, buf: &StereoF64Buffer) {
        self.push_block(&buf.left, &buf.right);
    }

    /// Push one decoded stereo block (`left`, `right` equal length) into Pass A.
    pub fn push_block(&mut self, left: &[f64], right: &[f64]) {
        let len = left.len().min(right.len());
        if len == 0 {
            return;
        }
        let left = &left[..len];
        let right = &right[..len];

        // 1. Backfill trailing right context for any recent candidates that still need up to
        //    `STORED_CTX` look-ahead samples across block boundaries.
        for cand in self.candidates.iter_mut().rev() {
            if cand.right_ctx_l.len() >= STORED_CTX {
                break;
            }
            let need = STORED_CTX - cand.right_ctx_l.len();
            let take = need.min(len);
            cand.right_ctx_l.extend_from_slice(&left[..take]);
            cand.right_ctx_r.extend_from_slice(&right[..take]);
        }

        // 2. Compute block sample peak and update global sample peak.
        let mut block_sp = 0.0_f64;
        for i in 0..len {
            let al = left[i].abs();
            let ar = right[i].abs();
            if al.is_finite() && al > block_sp {
                block_sp = al;
            }
            if ar.is_finite() && ar > block_sp {
                block_sp = ar;
            }
        }

        self.blocks_total += 1;
        if block_sp > self.global_sample_peak {
            self.global_sample_peak = block_sp;
            self.prune_below_gate();
        }

        // 3. Keep this block as a Pass-B candidate if its sample peak meets the `-6.02 dB` gate.
        let gate = self.global_sample_peak * GATE_LINEAR_RATIO;
        if block_sp > 0.0 && block_sp >= gate {
            let left_ctx_l: Vec<f64> = self.tail_l.iter().copied().collect();
            let left_ctx_r: Vec<f64> = self.tail_r.iter().copied().collect();
            let at_track_start = self.frames_scanned <= left_ctx_l.len() as u64;
            self.candidates.push(CandidateBlock {
                block_sample_peak: block_sp,
                left_ctx_l,
                left_ctx_r,
                body_l: left.to_vec(),
                body_r: right.to_vec(),
                right_ctx_l: Vec::with_capacity(STORED_CTX),
                right_ctx_r: Vec::with_capacity(STORED_CTX),
                at_track_start,
            });
            self.buffered_candidate_frames += len;
        }

        // 4. Advance rolling `STORED_CTX` tail for subsequent blocks.
        if len >= STORED_CTX {
            self.tail_l.clear();
            self.tail_r.clear();
            self.tail_l.extend(left[len - STORED_CTX..].iter().copied());
            self.tail_r.extend(right[len - STORED_CTX..].iter().copied());
        } else {
            let overflow = (self.tail_l.len() + len).saturating_sub(STORED_CTX);
            for _ in 0..overflow {
                self.tail_l.pop_front();
                self.tail_r.pop_front();
            }
            self.tail_l.extend(left.iter().copied());
            self.tail_r.extend(right.iter().copied());
        }
        self.frames_scanned += len as u64;

        // 5. Bound memory on long constant-loudness tracks by flushing all candidates that
        //    already have full `STORED_CTX` right context whenever RAM exceeds the cap.
        if self.buffered_candidate_frames >= self.max_buffered_frames {
            self.flush_ready_candidates(None);
        }
    }

    fn prune_below_gate(&mut self) {
        let gate = self.global_sample_peak * GATE_LINEAR_RATIO;
        if gate <= 0.0 || self.candidates.is_empty() {
            return;
        }
        let mut kept_frames = 0usize;
        self.candidates.retain(|c| {
            let keep = c.block_sample_peak >= gate;
            if keep {
                kept_frames += c.body_frames();
            }
            keep
        });
        self.buffered_candidate_frames = kept_frames;
    }

    /// Evaluate and drain all candidates that already possess full `STORED_CTX` right context.
    fn flush_ready_candidates(&mut self, cancel: Option<&AtomicBool>) -> bool {
        self.prune_below_gate();
        let ready_count = self
            .candidates
            .iter()
            .take_while(|c| c.right_ctx_l.len() >= STORED_CTX)
            .count();
        if ready_count == 0 {
            return true;
        }
        let mut ready: Vec<CandidateBlock> = self.candidates.drain(..ready_count).collect();
        self.buffered_candidate_frames = self.candidates.iter().map(|c| c.body_frames()).sum();
        self.eval_candidates_parallel(&mut ready, false, cancel)
    }

    fn eval_candidates_parallel(
        &mut self,
        batch: &mut [CandidateBlock],
        at_track_end: bool,
        cancel: Option<&AtomicBool>,
    ) -> bool {
        if batch.is_empty() {
            return true;
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return false;
        }
        let meter = TruePeakMeter8x::shared();
        let mut results: Vec<(usize, f64)> = (0..batch.len()).map(|i| (i, 0.0_f64)).collect();
        let max_slices = worker_pool::mac_slice_count(false);
        let batch_ref: &[CandidateBlock] = batch;
        let cancelled = AtomicBool::new(false);

        worker_pool::for_each_batch(&mut results, max_slices, |_slice_idx, _start, chunk| {
            for (idx, out_peak) in chunk.iter_mut() {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    cancelled.store(true, Ordering::Relaxed);
                    return;
                }
                // Review #10 HP-14: any candidate whose right context was truncated by EOF must
                // evaluate its full remaining right context (not only the final candidate in the
                // batch, e.g. when a hot block is followed by a short quiet final block < 40 frames).
                let is_near_eof = at_track_end && batch_ref[*idx].right_ctx_l.len() < STORED_CTX;
                *out_peak = batch_ref[*idx].eval_true_peak(meter, is_near_eof);
            }
        });

        if cancelled.load(Ordering::Relaxed) || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return false;
        }
        for (_, p) in results {
            if p > self.running_true_peak {
                self.running_true_peak = p;
            }
        }
        self.blocks_oversampled += batch.len() as u64;
        true
    }

    /// Complete Pass B on all surviving hot blocks and return the exact whole-file peak summary.
    /// Returns `None` if `cancel` was signalled before Pass B finished.
    pub fn finish(mut self, cancel: Option<&AtomicBool>) -> Option<PeakScanResult> {
        self.prune_below_gate();
        let mut remaining = std::mem::take(&mut self.candidates);
        self.buffered_candidate_frames = 0;
        if !self.eval_candidates_parallel(&mut remaining, true, cancel) {
            return None;
        }
        let sp = self.global_sample_peak;
        let tp = self.running_true_peak.max(sp);
        Some(PeakScanResult {
            sample_peak_linear: sp,
            sample_peak_dbfs: linear_to_db(sp),
            true_peak_linear: tp,
            true_peak_dbtp: linear_to_db(tp),
            frames_scanned: self.frames_scanned,
            blocks_total: self.blocks_total,
            blocks_oversampled: self.blocks_oversampled,
            meter_id: PEAK_SCAN_METER_ID,
        })
    }
}

impl Default for GatedPeakScanner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #23 T6 (P2a): Sines at `0.05..0.45 * fs` with worst-case inter-sample phase across
    /// 44.1 / 48 / 96 / 192 kHz MUST have under-read `<= 0.1 dB` against the analytic peak (`1.0`).
    #[test]
    fn t6_meter_8x_under_read_within_0_1_db_across_frequencies_phases_and_rates() {
        let meter = TruePeakMeter8x::shared();
        let rates = [44_100_u32, 48_000, 96_000, 192_000];
        let freq_fracs = [0.05_f64, 0.10, 0.15, 0.20, 0.25, 0.30, 0.35, 0.40, 0.43, 0.45];
        let phases = [0.0_f64, PI / 16.0, PI / 8.0, PI / 4.0, 0.37, 1.13, PI / 2.0];
        let amp = 0.75_f64; // analytic peak = -2.4988 dBFS
        let analytic_db = linear_to_db(amp);

        let mut worst_under_read_db = 0.0_f64;
        let mut worst_over_read_db = 0.0_f64;

        for &fs in &rates {
            let len = 1_024usize;
            for &frac in &freq_fracs {
                for &phi in &phases {
                    let sig: Vec<f64> = (0..len)
                        .map(|n| amp * (2.0 * PI * frac * (n as f64) + phi).sin())
                        .collect();
                    // Measure steady interior (skip edge zero-step window of 72 samples).
                    let measured = meter.scan_channel_range(&sig, 72, len - 72);
                    let measured_db = linear_to_db(measured);
                    let under_read = (analytic_db - measured_db).max(0.0);
                    let over_read = (measured_db - analytic_db).max(0.0);
                    if under_read > worst_under_read_db {
                        worst_under_read_db = under_read;
                    }
                    if over_read > worst_over_read_db {
                        worst_over_read_db = over_read;
                    }
                    assert!(
                        under_read <= 0.1,
                        "T6: fs={fs} frac={frac} phi={phi:.3}: under-read {under_read:.4} dB > 0.1 dB (measured={measured_db:.4}, analytic={analytic_db:.4})"
                    );
                    assert!(
                        over_read <= 0.1,
                        "T6: fs={fs} frac={frac} phi={phi:.3}: over-read {over_read:.4} dB > 0.1 dB (measured={measured_db:.4}, analytic={analytic_db:.4})"
                    );
                }
            }
        }
        assert!(worst_under_read_db <= 0.1);
        assert!(worst_over_read_db <= 0.1);

        // Review #10 §1.4 / HP-03: short Gaussian-windowed tone bursts at 0.40..=0.45 * fs
        // centered at worst fractional phases (3/8, 4/8, 5/8) MUST also under-read <= 0.1 dB
        // against the continuous analytic burst peak (`amp`).
        let burst_len = 512usize;
        let center_base = 256.0_f64;
        for &frac in &[0.40_f64, 0.43, 0.45] {
            for &sigma in &[12.0_f64, 20.0] {
                for &frac_shift in &[0.375_f64, 0.500, 0.625] {
                    let t0 = center_base + frac_shift;
                    let burst: Vec<f64> = (0..burst_len)
                        .map(|n| {
                            let dt = (n as f64) - t0;
                            let env = (-0.5 * (dt / sigma).powi(2)).exp();
                            amp * env * (2.0 * PI * frac * dt).cos()
                        })
                        .collect();
                    let measured = meter.scan_channel_range(&burst, 72, burst_len - 72);
                    let measured_db = linear_to_db(measured);
                    let under_read = (analytic_db - measured_db).max(0.0);
                    assert!(
                        under_read <= 0.1,
                        "T6 burst (HP-03): frac={frac} sigma={sigma} shift={frac_shift}: under-read {under_read:.4} dB > 0.1 dB (measured={measured_db:.4}, analytic={analytic_db:.4})"
                    );
                }
            }
        }
    }

    /// Issue #23 T7 (P2a): Pass-B gating soundness on random signals + worst-case `fs/4 @ 45 deg`
    /// inter-sample peaks + arbitrary chunk sizes (including prime 37-frame chunks and mid-stream
    /// memory-cap flush): gated true peak MUST equal ungated true peak exactly, while skipping
    /// quiet blocks!
    #[test]
    fn t7_pass_b_gate_matches_ungated_exactly_including_fs4_45deg_and_boundaries() {
        let meter = TruePeakMeter8x::shared();
        let len = 65_536usize;
        let mut left = vec![0.0_f64; len];
        let mut right = vec![0.0_f64; len];

        // Deterministic PRNG background at -18 dBFS (strictly below the -6.02 dB gate).
        let mut x = 0x9E37_79B9_7F4A_7C15_u64;
        for i in 0..len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let u = ((x >> 11) as f64) / ((1_u64 << 53) as f64);
            let n = (2.0 * u - 1.0) * 0.12;
            left[i] = n;
            right[i] = -n * 0.8;
        }

        // Plant a worst-case `fs/4 @ 45 deg` burst right across a block boundary (frame 16380..16420)
        // where sample peak is `0.50` (`-6.02 dBFS`) and true peak is `0.50 * sqrt(2) = 0.7071`
        // (`-3.01 dBTP`), plus another loud burst at frame 49152 (`0.55` sample peak, low freq)!
        // Notice: `0.50` is > `0.55 * 0.495`, so the fs/4@45deg burst MUST survive the gate and
        // produce the global true peak `0.7071` even though its sample peak (0.50) < 0.55!
        for i in 16_360..16_440 {
            let v = 0.50 * std::f64::consts::SQRT_2 * (2.0 * PI * 0.25 * (i as f64) + PI / 4.0).sin();
            left[i] = v.clamp(-0.50, 0.50);
        }
        for i in 49_140..49_220 {
            right[i] = 0.55 * (2.0 * PI * 0.01 * (i as f64)).sin();
        }

        let ungated = meter.scan_stereo_ungated(&left, &right);
        assert!(
            (ungated - 0.50 * std::f64::consts::SQRT_2).abs() < 0.01,
            "fs/4@45deg burst should yield ~0.7071 true peak, got {ungated:.6}"
        );

        // Test across multiple block sizes including unaligned prime `37` frames and forced mid-stream flush.
        for &(chunk_size, max_buf, expect_skips) in &[
            (4_096usize, DEFAULT_MAX_BUFFERED_CANDIDATE_FRAMES, true),
            (1_024, DEFAULT_MAX_BUFFERED_CANDIDATE_FRAMES, true),
            (37, DEFAULT_MAX_BUFFERED_CANDIDATE_FRAMES, true),
            (2_048, 4_096, false), // exercises bounded-RAM mid-stream flush
        ] {
            let mut scanner = GatedPeakScanner::with_max_buffered_frames(max_buf);
            let mut pos = 0;
            while pos < len {
                let end = (pos + chunk_size).min(len);
                scanner.push_block(&left[pos..end], &right[pos..end]);
                pos = end;
            }
            let res = scanner.finish(None).expect("uncancelled scan must finish");
            assert_eq!(res.frames_scanned, len as u64);
            assert_eq!(res.meter_id, PEAK_SCAN_METER_ID);
            assert!(
                (res.true_peak_linear - ungated).abs() < 1e-12,
                "T7 chunk={chunk_size} max_buf={max_buf}: gated TP ({:.15}) != ungated TP ({:.15})",
                res.true_peak_linear,
                ungated
            );
            if expect_skips {
                assert!(
                    res.blocks_oversampled < res.blocks_total,
                    "T7 chunk={chunk_size}: gate must skip quiet blocks ({}/{})",
                    res.blocks_oversampled,
                    res.blocks_total
                );
            }
        }

        // Verify prompt cancellation returns None.
        let mut cancel_scanner = GatedPeakScanner::new();
        cancel_scanner.push_block(&left, &right);
        let cancel_flag = AtomicBool::new(true);
        assert!(cancel_scanner.finish(Some(&cancel_flag)).is_none());
    }
}
