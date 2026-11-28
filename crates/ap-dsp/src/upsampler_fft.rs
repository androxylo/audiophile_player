// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Lossless zero-stuffed tiled-FFT Uniform Partitioned Overlap-Save (UPOLS) `f64` upsampler.
//!
//! Architectural properties (`docs/research/07-ultimate-mode.md` §1, `UPSAMPLING_RESEARCH.md` §4.2):
//! - **Mathematically lossless:** Matches direct-form polyphase FIR convolution to `<= -280 dBFS`
//!   RMS and `<= -260 dBFS` peak (typically ~`-295 dBFS`, pure IEEE 754 `f64` rounding floor)
//!   across arbitrary prototype lengths up to `4,000,000` taps, arbitrary chunk sizes, and gapless
//!   track boundaries.
//! - **Zero-stuffed tiled-FFT trick:** Transforms real input blocks at the **source rate** (`N_in`)
//!   and tiles/conjugate-mirrors each `L×`-smaller spectrum inside the frequency delay line (FDL),
//!   cutting forward-FFT work and FDL RAM by `L×` while keeping the exact output-rate linear
//!   convolution.
//! - **Bit-exact deterministic parallelism:** Parallelizes stereo L/R channels and disjoint
//!   frequency-bin slices via [`crate::worker_pool`] (persistent zero-dependency worker pools) with
//!   an unchanged per-bin partition summation order — 100% bit-identical to single-threaded execution.
//! - **General phase support:** Works identically for linear-phase, minimum-phase, and
//!   intermediate-phase polyphase banks via explicit `(pad_lead, pad_tail)` alignment.

use std::sync::Arc;

use ap_core::buffer::StereoF64Buffer;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::selector::{FilterDesignMetrics, FilterIdentity};
use crate::upsampler::{DesignedGaussianFilter, Upsampler, UpsamplerCarryover};

/// Minimum input-rate UPOLS partition block size (`B_in`).
const MIN_INPUT_BLOCK: usize = 256;

/// Target output-rate UPOLS partition block size (`B_out = B_in * L` ~ 65,536 frames).
const TARGET_OUTPUT_BLOCK: usize = 65_536;

/// Minimum non-zero partitions before spawning bin-slice MAC worker threads per channel.
const PARALLEL_MAC_MIN_PARTITIONS: usize = 8;

/// Number of frequency-bin worker slices per channel when parallel MAC is active.
const PARALLEL_MAC_SLICES: usize = 4;

/// Immutable, pre-transformed FFT/UPOLS upsampler kernel ready to share across tracks (`Arc`).
pub struct PreparedUpsamplerKernel {
    pub name: &'static str,
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    pub taps_per_phase: usize,
    /// Leading zero-pad count in input samples (`m_half` for odd linear-phase; `m - 1` for min-phase).
    pub pad_lead: usize,
    /// Trailing zero-pad count in input samples (`taps_per_phase - 1 - pad_lead`).
    pub pad_tail: usize,
    pub metrics: FilterDesignMetrics,
    pub identity: FilterIdentity,
    /// Polyphase coefficient bank retained for [`UpsamplerCarryover`] interoperability.
    pub bank: Arc<Vec<Vec<f64>>>,
    /// Input-rate UPOLS block size `B_in` (strict power of 2).
    pub b_in: usize,
    /// Output-rate UPOLS block size `B_out = B_in * ratio`.
    pub b_out: usize,
    pub num_partitions: usize,
    pub nonzero_partitions: usize,
    r2c_in: Arc<dyn RealToComplex<f64>>,
    c2r_out: Arc<dyn ComplexToReal<f64>>,
    ir_spectra: Vec<Vec<Complex<f64>>>,
    nonzero: Vec<bool>,
}

impl std::fmt::Debug for PreparedUpsamplerKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedUpsamplerKernel")
            .field("name", &self.name)
            .field("source_rate_hz", &self.source_rate_hz)
            .field("target_rate_hz", &self.target_rate_hz)
            .field("ratio", &self.ratio)
            .field("taps_per_phase", &self.taps_per_phase)
            .field("pad_lead", &self.pad_lead)
            .field("pad_tail", &self.pad_tail)
            .field("b_in", &self.b_in)
            .field("b_out", &self.b_out)
            .field("num_partitions", &self.num_partitions)
            .field("nonzero_partitions", &self.nonzero_partitions)
            .finish()
    }
}

impl PreparedUpsamplerKernel {
    /// Prepare an immutable FFT/UPOLS upsampler kernel from a polyphase bank (`ratio` phases × `m`
    /// taps) and explicit `(pad_lead, pad_tail)` where `pad_lead + pad_tail == m - 1`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_polyphase_bank(
        name: &'static str,
        source_rate_hz: u32,
        target_rate_hz: u32,
        ratio: usize,
        taps_per_phase: usize,
        pad_lead: usize,
        pad_tail: usize,
        bank: Arc<Vec<Vec<f64>>>,
        metrics: FilterDesignMetrics,
        identity: FilterIdentity,
    ) -> Result<Self, String> {
        if ratio < 2 {
            return Err(format!("FFT upsampler requires ratio >= 2, got {ratio}"));
        }
        if taps_per_phase == 0 || bank.len() != ratio {
            return Err(format!(
                "invalid polyphase bank dimensions (ratio={ratio}, phases={}, m={taps_per_phase})",
                bank.len()
            ));
        }
        if bank.iter().any(|p| p.len() != taps_per_phase) {
            return Err("polyphase bank phase length mismatch".to_string());
        }
        if pad_lead + pad_tail != taps_per_phase.saturating_sub(1) {
            return Err(format!(
                "invalid pad_lead ({pad_lead}) + pad_tail ({pad_tail}) != taps_per_phase - 1 ({})",
                taps_per_phase.saturating_sub(1)
            ));
        }

        // Choose input-rate block size B_in (power of 2) so B_out = B_in * ratio targets ~65,536
        // frames (1 UPOLS partition) for mega-tap filters and stays compact for shorter kernels.
        let max_b_in = (TARGET_OUTPUT_BLOCK / ratio).max(MIN_INPUT_BLOCK);
        let max_b_in_pow2 = if max_b_in.is_power_of_two() {
            max_b_in
        } else {
            (max_b_in.next_power_of_two() >> 1).max(MIN_INPUT_BLOCK)
        };
        let b_in = taps_per_phase
            .next_power_of_two()
            .clamp(MIN_INPUT_BLOCK, max_b_in_pow2);
        let b_out = b_in * ratio;
        let n_in = b_in * 2;
        let n_out = b_out * 2;
        let spec_out_len = b_out + 1;

        // Build causal zero-stuffed impulse response g[u * ratio + p] = bank[p][m - 1 - u].
        // Convolving g with x_up and evaluating at (n + pad_tail) * ratio + p reproduces
        // sum_{i=0..m} bank[p][i] * x[n + i - pad_lead] identically for any phase class.
        let m_out = taps_per_phase * ratio;
        let num_partitions = m_out.div_ceil(b_out);

        let mut planner = RealFftPlanner::<f64>::new();
        let r2c_in = planner.plan_fft_forward(n_in);
        let r2c_out = planner.plan_fft_forward(n_out);
        let c2r_out = planner.plan_fft_inverse(n_out);

        let spec_scale = 1.0 / (n_out as f64);
        let mut r2c_scratch = r2c_out.make_scratch_vec();
        let mut time_buf = vec![0.0_f64; n_out];

        let mut ir_spectra = Vec::with_capacity(num_partitions);
        let mut nonzero = Vec::with_capacity(num_partitions);
        let mut nonzero_partitions = 0usize;

        for p_idx in 0..num_partitions {
            let start = p_idx * b_out;
            let end = (start + b_out).min(m_out);
            time_buf.fill(0.0);
            let mut has_nz = false;

            // Fill time_buf[0..(end - start)] directly from g[start..end].
            for out_idx in start..end {
                let u = out_idx / ratio;
                let phase = out_idx % ratio;
                let val = bank[phase][taps_per_phase - 1 - u];
                if val != 0.0 {
                    has_nz = true;
                }
                time_buf[out_idx - start] = val;
            }

            nonzero.push(has_nz);
            let mut spec = vec![Complex::new(0.0, 0.0); spec_out_len];
            if has_nz {
                nonzero_partitions += 1;
                r2c_out
                    .process_with_scratch(&mut time_buf, &mut spec, &mut r2c_scratch)
                    .map_err(|e| format!("FFT upsampler IR partition FFT failed: {e}"))?;
                for bin in &mut spec {
                    bin.re *= spec_scale;
                    bin.im *= spec_scale;
                }
            }
            ir_spectra.push(spec);
        }

        Ok(Self {
            name,
            source_rate_hz,
            target_rate_hz,
            ratio,
            taps_per_phase,
            pad_lead,
            pad_tail,
            metrics,
            identity,
            bank,
            b_in,
            b_out,
            num_partitions,
            nonzero_partitions,
            r2c_in,
            c2r_out,
            ir_spectra,
            nonzero,
        })
    }

    /// Convenience constructor directly from a [`DesignedGaussianFilter`].
    pub fn from_gaussian(designed: DesignedGaussianFilter) -> Result<Self, String> {
        let m_half = designed.m_half;
        Self::from_polyphase_bank(
            designed.spec.archetype.cli_name(),
            designed.spec.source_rate_hz,
            designed.spec.target_rate_hz,
            designed.spec.ratio,
            designed.taps_per_phase,
            m_half,
            m_half,
            designed.bank,
            designed.metrics,
            designed.identity,
        )
    }
}

/// Accumulate one input-rate spectrum `u_spec` (tiled `L` times with conjugate symmetry) multiplied
/// by partition spectrum `h_spec` into `acc_chunk` covering global output bins `[start..end)`.
///
/// Exact per-bin operation matches a full `N_out`-point FFT of the zero-stuffed input frame
/// bit-for-bit, with zero heap allocation and contiguous inner loops for AVX2/FMA vectorization.
#[inline]
fn accumulate_tiled_partition_slice(
    acc_chunk: &mut [Complex<f64>],
    h_spec: &[Complex<f64>],
    u_spec: &[Complex<f64>],
    n_in: usize,
    h_in: usize,
    start: usize,
) {
    let end = start + acc_chunk.len();
    let mask = n_in - 1; // n_in is strictly a power of 2
    let mut k = start;

    while k < end {
        let q = k & mask;
        if q < h_in {
            let run = (h_in - q).min(end - k);
            let acc_s = &mut acc_chunk[(k - start)..(k - start + run)];
            let h_s = &h_spec[k..(k + run)];
            let u_s = &u_spec[q..(q + run)];
            for ((a, &u), &h) in acc_s.iter_mut().zip(u_s.iter()).zip(h_s.iter()) {
                a.re += u.re * h.re - u.im * h.im;
                a.im += u.re * h.im + u.im * h.re;
            }
            k += run;
        } else if q == h_in {
            let u = u_spec[h_in];
            let h = h_spec[k];
            let a = &mut acc_chunk[k - start];
            a.re += u.re * h.re - u.im * h.im;
            a.im += u.re * h.im + u.im * h.re;
            k += 1;
        } else {
            let run = (n_in - q).min(end - k);
            let acc_s = &mut acc_chunk[(k - start)..(k - start + run)];
            let h_s = &h_spec[k..(k + run)];
            let u_hi = n_in - q;
            for i in 0..run {
                let u = u_spec[u_hi - i];
                let h = h_s[i];
                // conj(u) * h = (u.re - j u.im) * (h.re + j h.im)
                acc_s[i].re += u.re * h.re + u.im * h.im;
                acc_s[i].im += u.re * h.im - u.im * h.re;
            }
            k += run;
        }
    }
}

/// Per-channel tiled-FFT UPOLS upsampler state (zero allocation in steady-state inner loop).
struct ChannelTiledUpolsState {
    b_in: usize,
    b_out: usize,
    num_partitions: usize,
    /// Input-rate FDL (`num_partitions` × `(B_in + 1)` complex bins — `L×` smaller than output-rate).
    fdl: Vec<Vec<Complex<f64>>>,
    fdl_head: usize,
    prev_in: Vec<f64>,
    pending_in: Vec<f64>,
    in_time_buf: Vec<f64>,
    out_time_buf: Vec<f64>,
    acc_spec: Vec<Complex<f64>>,
    r2c_scratch: Vec<Complex<f64>>,
    c2r_scratch: Vec<Complex<f64>>,
    /// Remaining leading causal output samples to discard for exact group-delay / carryover alignment.
    discard_out_remaining: usize,
}

impl ChannelTiledUpolsState {
    fn new(kernel: &PreparedUpsamplerKernel, discard_out_samples: usize) -> Self {
        let b_in = kernel.b_in;
        let b_out = kernel.b_out;
        let spec_in_len = b_in + 1;
        let spec_out_len = b_out + 1;
        let fdl = (0..kernel.num_partitions)
            .map(|_| vec![Complex::new(0.0, 0.0); spec_in_len])
            .collect();

        Self {
            b_in,
            b_out,
            num_partitions: kernel.num_partitions,
            fdl,
            fdl_head: 0,
            prev_in: vec![0.0_f64; b_in],
            pending_in: Vec::with_capacity(b_in),
            in_time_buf: vec![0.0_f64; b_in * 2],
            out_time_buf: vec![0.0_f64; b_out * 2],
            acc_spec: vec![Complex::new(0.0, 0.0); spec_out_len],
            r2c_scratch: kernel.r2c_in.make_scratch_vec(),
            c2r_scratch: kernel.c2r_out.make_scratch_vec(),
            discard_out_remaining: discard_out_samples,
        }
    }

    /// Execute one exact `B_in -> B_out` tiled-FFT UPOLS block, respecting `discard_out_remaining`.
    fn process_one_block(
        &mut self,
        block_in: &[f64],
        kernel: &PreparedUpsamplerKernel,
        out: &mut Vec<f64>,
    ) -> Result<(), String> {
        let b_in = self.b_in;
        let b_out = self.b_out;
        let n_in = b_in * 2;
        debug_assert_eq!(block_in.len(), b_in);

        // 1. Assemble 2*B_in input-rate overlap-save frame: [prev_in, block_in].
        self.in_time_buf[..b_in].copy_from_slice(&self.prev_in);
        self.in_time_buf[b_in..n_in].copy_from_slice(block_in);
        self.prev_in.copy_from_slice(block_in);

        // 2. Forward input-rate RFFT directly into FDL slot.
        let head = self.fdl_head;
        kernel
            .r2c_in
            .process_with_scratch(
                &mut self.in_time_buf,
                &mut self.fdl[head],
                &mut self.r2c_scratch,
            )
            .map_err(|e| format!("FFT upsampler forward RFFT failed: {e}"))?;

        // 3. Tiled complex MAC across all non-zero IR partitions via persistent worker pool (F12).
        // Review #9 D9: `for_each_mac_slice` runs inline on the caller thread when `slices == 1`.
        let p_count = self.num_partitions;
        let spec_out_len = b_out + 1;
        let ir_spectra = &kernel.ir_spectra;
        let nonzero = &kernel.nonzero;
        let fdl = &self.fdl;
        let mac_slices = crate::worker_pool::mac_slice_count(true);
        let slices = if kernel.nonzero_partitions >= PARALLEL_MAC_MIN_PARTITIONS
            && kernel.nonzero_partitions.saturating_mul(spec_out_len)
                >= crate::worker_pool::MAC_PARALLEL_MIN_WORK
            && mac_slices > 1
            && spec_out_len >= mac_slices * 256
        {
            mac_slices.min(PARALLEL_MAC_SLICES)
        } else {
            1
        };
        crate::worker_pool::for_each_mac_slice(
            &mut self.acc_spec,
            slices,
            |_slice_idx, bin_start, acc_chunk| {
                for c in acc_chunk.iter_mut() {
                    *c = Complex::new(0.0, 0.0);
                }
                for p in 0..p_count {
                    if !nonzero[p] {
                        continue;
                    }
                    let fdl_idx = if head >= p {
                        head - p
                    } else {
                        head + p_count - p
                    };
                    accumulate_tiled_partition_slice(
                        acc_chunk,
                        &ir_spectra[p],
                        &fdl[fdl_idx],
                        n_in,
                        b_in,
                        bin_start,
                    );
                }
            },
        );

        // Guard realfft DC & Nyquist real-only invariant.
        self.acc_spec[0].im = 0.0;
        self.acc_spec[b_out].im = 0.0;

        // 4. Inverse output-rate C2R FFT (IR spectra already carry 1 / N_out normalization).
        kernel
            .c2r_out
            .process_with_scratch(
                &mut self.acc_spec,
                &mut self.out_time_buf,
                &mut self.c2r_scratch,
            )
            .map_err(|e| format!("FFT upsampler inverse C2R FFT failed: {e}"))?;

        // 5. Valid linear-convolution output is [B_out..2*B_out]; apply alignment discard.
        let valid = &self.out_time_buf[b_out..2 * b_out];
        if self.discard_out_remaining >= b_out {
            self.discard_out_remaining -= b_out;
        } else if self.discard_out_remaining > 0 {
            let skip = self.discard_out_remaining;
            self.discard_out_remaining = 0;
            out.extend_from_slice(&valid[skip..]);
        } else {
            out.extend_from_slice(valid);
        }

        self.fdl_head = (head + 1) % p_count;
        Ok(())
    }

    fn push_samples(
        &mut self,
        input: &[f64],
        kernel: &PreparedUpsamplerKernel,
    ) -> Result<Vec<f64>, String> {
        let b_in = self.b_in;
        let total = self.pending_in.len() + input.len();
        let full_blocks = total / b_in;
        let mut out = Vec::with_capacity(full_blocks * self.b_out);
        let mut offset = 0usize;

        if !self.pending_in.is_empty() {
            let need = b_in - self.pending_in.len();
            if input.len() < need {
                self.pending_in.extend_from_slice(input);
                return Ok(out);
            }
            self.pending_in.extend_from_slice(&input[..need]);
            offset = need;
            let staged = std::mem::take(&mut self.pending_in);
            self.process_one_block(&staged, kernel, &mut out)?;
            self.pending_in = staged;
            self.pending_in.clear();
        }

        while offset + b_in <= input.len() {
            self.process_one_block(&input[offset..offset + b_in], kernel, &mut out)?;
            offset += b_in;
        }

        if offset < input.len() {
            self.pending_in.extend_from_slice(&input[offset..]);
        }

        Ok(out)
    }

    /// Feed zero-padded blocks until exactly `remaining_output` aligned output samples are emitted.
    fn flush_exact(
        &mut self,
        remaining_output: usize,
        kernel: &PreparedUpsamplerKernel,
    ) -> Result<Vec<f64>, String> {
        if remaining_output == 0 {
            return Ok(Vec::new());
        }
        let b_in = self.b_in;
        let mut out = Vec::with_capacity(remaining_output.next_multiple_of(self.b_out));

        if !self.pending_in.is_empty() {
            let mut block = std::mem::take(&mut self.pending_in);
            block.resize(b_in, 0.0);
            self.process_one_block(&block, kernel, &mut out)?;
        }

        let zero_block = vec![0.0_f64; b_in];
        while out.len() < remaining_output {
            self.process_one_block(&zero_block, kernel, &mut out)?;
        }

        out.truncate(remaining_output);
        Ok(out)
    }
}

/// Stateful pure-`f64` zero-stuffed tiled-FFT UPOLS upsampler implementing [`Upsampler`].
pub struct FftUpolsUpsampler {
    kernel: Arc<PreparedUpsamplerKernel>,
    left: ChannelTiledUpolsState,
    right: ChannelTiledUpolsState,
    /// Trailing `<= m - 1` raw input history for exact gapless [`UpsamplerCarryover`] handoff.
    tail_hist_l: Vec<f64>,
    tail_hist_r: Vec<f64>,
    /// Number of logical pre-history input frames (either `pad_lead` zeros or incoming carryover len).
    initial_hist_len: usize,
    total_in_frames: usize,
    total_emitted_out_frames: usize,
    has_incoming_carryover: bool,
    flushed: bool,
}

impl FftUpolsUpsampler {
    pub fn new(kernel: Arc<PreparedUpsamplerKernel>) -> Self {
        let discard = kernel.pad_tail * kernel.ratio;
        let left = ChannelTiledUpolsState::new(&kernel, discard);
        let right = ChannelTiledUpolsState::new(&kernel, discard);
        let pad_lead = kernel.pad_lead;
        Self {
            kernel,
            left,
            right,
            tail_hist_l: vec![0.0_f64; pad_lead],
            tail_hist_r: vec![0.0_f64; pad_lead],
            initial_hist_len: pad_lead,
            total_in_frames: 0,
            total_emitted_out_frames: 0,
            has_incoming_carryover: false,
            flushed: false,
        }
    }

    pub fn from_gaussian(designed: DesignedGaussianFilter) -> Result<Self, String> {
        let kernel = Arc::new(PreparedUpsamplerKernel::from_gaussian(designed)?);
        Ok(Self::new(kernel))
    }

    pub fn prepared_kernel(&self) -> &PreparedUpsamplerKernel {
        &self.kernel
    }

    fn append_tail_history(&mut self, in_l: &[f64], in_r: &[f64]) {
        let max_keep = self.kernel.taps_per_phase.saturating_sub(1);
        if max_keep == 0 {
            return;
        }
        if in_l.len() >= max_keep {
            let start = in_l.len() - max_keep;
            self.tail_hist_l.clear();
            self.tail_hist_l.extend_from_slice(&in_l[start..]);
            self.tail_hist_r.clear();
            self.tail_hist_r.extend_from_slice(&in_r[start..]);
        } else {
            self.tail_hist_l.extend_from_slice(in_l);
            self.tail_hist_r.extend_from_slice(in_r);
            if self.tail_hist_l.len() > max_keep {
                let drop_n = self.tail_hist_l.len() - max_keep;
                self.tail_hist_l.drain(0..drop_n);
                self.tail_hist_r.drain(0..drop_n);
            }
        }
    }

    /// Process both channels in parallel via persistent worker pool (bit-identical to sequential).
    fn push_stereo(&mut self, in_l: &[f64], in_r: &[f64]) -> Result<(Vec<f64>, Vec<f64>), String> {
        let kernel = &self.kernel;
        let left = &mut self.left;
        let right = &mut self.right;
        if kernel.num_partitions >= 2 || in_l.len() >= kernel.b_in {
            let (l_res, r_res) = crate::worker_pool::join_stereo(
                || left.push_samples(in_l, kernel),
                || right.push_samples(in_r, kernel),
            );
            Ok((l_res?, r_res?))
        } else {
            let out_l = left.push_samples(in_l, kernel)?;
            let out_r = right.push_samples(in_r, kernel)?;
            Ok((out_l, out_r))
        }
    }

    fn flush_stereo_exact(&mut self, remaining: usize) -> Result<(Vec<f64>, Vec<f64>), String> {
        if remaining == 0 {
            return Ok((Vec::new(), Vec::new()));
        }
        let kernel = &self.kernel;
        let left = &mut self.left;
        let right = &mut self.right;
        if kernel.num_partitions >= 2 || remaining >= kernel.b_out {
            let (l_res, r_res) = crate::worker_pool::join_stereo(
                || left.flush_exact(remaining, kernel),
                || right.flush_exact(remaining, kernel),
            );
            Ok((l_res?, r_res?))
        } else {
            let out_l = left.flush_exact(remaining, kernel)?;
            let out_r = right.flush_exact(remaining, kernel)?;
            Ok((out_l, out_r))
        }
    }
}

/// Fast lossless FFT flush for a large [`UpsamplerCarryover`] (avoids `O(m^2 * L)` direct-form
/// flushing when a mega-tap filter like `#4 sinc-L` drains at end-of-queue or filter change).
pub(crate) fn flush_carryover_via_fft(
    carryover: &UpsamplerCarryover,
    bank: Arc<Vec<Vec<f64>>>,
) -> Result<StereoF64Buffer, String> {
    let m = carryover.taps_per_phase;
    let ratio = carryover.ratio;
    let pad_tail = carryover.pad_tail.min(m.saturating_sub(1));
    let pad_lead = m.saturating_sub(1).saturating_sub(pad_tail);

    let dummy_metrics = FilterDesignMetrics {
        phase: crate::selector::FilterPhase::Linear,
        num_taps: carryover.identity.num_taps,
        taps_per_phase: m,
        length_seconds: 0.0,
        group_delay_seconds: 0.0,
        passband_edge_001db_hz: 0.0,
        level_at_20khz_db: None,
        stopband_edge_hz: 0.0,
        stopband_attenuation_db: 0.0,
        l1_norm: 0.0,
        l1_norm_db: 0.0,
    };

    let kernel = Arc::new(PreparedUpsamplerKernel::from_polyphase_bank(
        carryover.identity.archetype.cli_name(),
        carryover.source_rate_hz,
        carryover.target_rate_hz,
        ratio,
        m,
        pad_lead,
        pad_tail,
        bank,
        dummy_metrics,
        carryover.identity,
    )?);

    // Match direct `UpsamplerCarryover::flush_to_f64`: pad `hist` with `m - 1` trailing zeros
    // and evaluate `n_windows = (hist.len() + m - 1) - m + 1 = hist.len()` windows -> `hist.len() * L`
    // aligned output samples starting after discarding `(m - 1) * L` causal lead samples.
    let total_out = carryover.hist_l.len() * ratio;
    let discard = m.saturating_sub(1) * ratio;
    let mut left_st = ChannelTiledUpolsState::new(&kernel, discard);
    let mut right_st = ChannelTiledUpolsState::new(&kernel, discard);

    let (l_res, r_res) = crate::worker_pool::join_stereo(
        || -> Result<Vec<f64>, String> {
            let mut v = left_st.push_samples(&carryover.hist_l, &kernel)?;
            let rem = total_out.saturating_sub(v.len());
            let tail = left_st.flush_exact(rem, &kernel)?;
            v.extend_from_slice(&tail);
            v.truncate(total_out);
            Ok(v)
        },
        || -> Result<Vec<f64>, String> {
            let mut r = right_st.push_samples(&carryover.hist_r, &kernel)?;
            let rem_r = total_out.saturating_sub(r.len());
            let tail_r = right_st.flush_exact(rem_r, &kernel)?;
            r.extend_from_slice(&tail_r);
            r.truncate(total_out);
            Ok(r)
        },
    );
    let (mut out_l, mut out_r) = (l_res?, r_res?);

    debug_assert_eq!(out_l.len(), total_out);
    debug_assert_eq!(out_r.len(), total_out);
    out_l.truncate(total_out);
    out_r.truncate(total_out);
    Ok(StereoF64Buffer {
        sample_rate_hz: carryover.target_rate_hz,
        left: out_l,
        right: out_r,
    })
}

impl Upsampler for FftUpolsUpsampler {
    fn name(&self) -> &'static str {
        self.kernel.name
    }

    fn total_taps(&self) -> usize {
        self.kernel.metrics.num_taps
    }

    fn latency_samples(&self) -> usize {
        self.kernel.pad_tail * self.kernel.ratio
    }

    #[cfg(any(test, feature = "verify"))]
    fn verify_export(&self) -> Option<crate::upsampler::UpsamplerExport> {
        Some(crate::upsampler::UpsamplerExport {
            ratio: self.kernel.ratio,
            taps_per_phase: self.kernel.taps_per_phase,
            pad_lead: self.kernel.pad_lead,
            pad_tail: self.kernel.pad_tail,
            bank: Arc::clone(&self.kernel.bank),
        })
    }

    fn design_metrics(&self) -> Option<FilterDesignMetrics> {
        Some(self.kernel.metrics)
    }

    fn identity(&self) -> FilterIdentity {
        self.kernel.identity
    }

    fn enable_gapless_prering(&mut self) -> usize {
        if self.total_in_frames == 0 && !self.has_incoming_carryover && !self.flushed {
            let pre_internal = self.kernel.pad_tail * self.kernel.ratio;
            if pre_internal > 0 {
                self.left = ChannelTiledUpolsState::new(&self.kernel, 0);
                self.right = ChannelTiledUpolsState::new(&self.kernel, 0);
                let m_minus_1 = self.kernel.taps_per_phase.saturating_sub(1);
                self.tail_hist_l = vec![0.0_f64; m_minus_1];
                self.tail_hist_r = vec![0.0_f64; m_minus_1];
                self.initial_hist_len = m_minus_1;
                return pre_internal;
            }
        }
        0
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if self.flushed {
            return Err(format!(
                "{} FFT upsampler cannot accept blocks after flush()",
                self.kernel.name
            ));
        }
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        if input.sample_rate_hz != self.kernel.source_rate_hz {
            return Err(format!(
                "{} FFT upsampler sample rate mismatch: expected {} Hz, got {} Hz",
                self.kernel.name, self.kernel.source_rate_hz, input.sample_rate_hz
            ));
        }
        if input.is_empty() {
            return Ok(StereoF64Buffer::empty(self.kernel.target_rate_hz));
        }

        self.total_in_frames += input.left.len();
        self.append_tail_history(&input.left, &input.right);

        let (left, right) = self.push_stereo(&input.left, &input.right)?;
        debug_assert_eq!(left.len(), right.len());
        self.total_emitted_out_frames += left.len();

        Ok(StereoF64Buffer {
            sample_rate_hz: self.kernel.target_rate_hz,
            left,
            right,
        })
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok(StereoF64Buffer::empty(self.kernel.target_rate_hz));
        }
        self.flushed = true;

        // Total aligned windows matching PolyphaseFirUpsampler::flush:
        // (initial_hist_len + total_in_frames + pad_tail) - m + 1
        let m_minus_1 = self.kernel.taps_per_phase.saturating_sub(1);
        let total_windows = (self.initial_hist_len + self.total_in_frames + self.kernel.pad_tail)
            .saturating_sub(m_minus_1);
        let target_total_out = total_windows * self.kernel.ratio;
        let remaining = target_total_out.saturating_sub(self.total_emitted_out_frames);

        let (left, right) = self.flush_stereo_exact(remaining)?;
        self.total_emitted_out_frames += left.len();
        self.tail_hist_l.clear();
        self.tail_hist_r.clear();

        Ok(StereoF64Buffer {
            sample_rate_hz: self.kernel.target_rate_hz,
            left,
            right,
        })
    }

    fn set_incoming_carryover(
        &mut self,
        carryover: UpsamplerCarryover,
    ) -> Result<(), UpsamplerCarryover> {
        if self.flushed
            || self.total_in_frames != 0
            || carryover.identity != self.kernel.identity
            || carryover.source_rate_hz != self.kernel.source_rate_hz
            || carryover.ratio != self.kernel.ratio
            || carryover.taps_per_phase != self.kernel.taps_per_phase
            || carryover.hist_l.len() != carryover.hist_r.len()
            || carryover.hist_l.len() >= self.kernel.taps_per_phase
        {
            return Err(carryover);
        }

        // Re-initialize UPOLS state with discard = (m - 1) * ratio and feed `carryover.hist`
        // directly so the first emitted output uses exact window `s[0..m]`.
        let discard = self.kernel.taps_per_phase.saturating_sub(1) * self.kernel.ratio;
        let mut new_left = ChannelTiledUpolsState::new(&self.kernel, discard);
        let mut new_right = ChannelTiledUpolsState::new(&self.kernel, discard);

        let pre_l = match new_left.push_samples(&carryover.hist_l, &self.kernel) {
            Ok(v) => v,
            Err(_) => return Err(carryover),
        };
        let pre_r = match new_right.push_samples(&carryover.hist_r, &self.kernel) {
            Ok(v) => v,
            Err(_) => return Err(carryover),
        };
        // Since carryover.hist.len() <= m - 1, all causal samples produced from hist alone are
        // strictly inside the `(m - 1) * ratio` discard window.
        debug_assert!(pre_l.is_empty() && pre_r.is_empty());

        self.left = new_left;
        self.right = new_right;
        self.initial_hist_len = carryover.hist_l.len();
        self.tail_hist_l = carryover.hist_l;
        self.tail_hist_r = carryover.hist_r;
        self.has_incoming_carryover = true;
        Ok(())
    }

    fn flush_for_gapless(&mut self) -> Result<(StereoF64Buffer, Option<UpsamplerCarryover>), String> {
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok((StereoF64Buffer::empty(self.kernel.target_rate_hz), None));
        }
        self.flushed = true;

        // Emit all exact windows that depend strictly on Track A's real samples (zero trailing
        // padding ever enters a window), then hand the trailing `<= m - 1` input samples to Track B.
        let m_minus_1 = self.kernel.taps_per_phase.saturating_sub(1);
        let gapless_windows = (self.initial_hist_len + self.total_in_frames).saturating_sub(m_minus_1);
        let target_gapless_out = gapless_windows * self.kernel.ratio;
        let remaining = target_gapless_out.saturating_sub(self.total_emitted_out_frames);

        let (left, right) = self.flush_stereo_exact(remaining)?;
        self.total_emitted_out_frames += left.len();

        let hist_l = std::mem::take(&mut self.tail_hist_l);
        let hist_r = std::mem::take(&mut self.tail_hist_r);
        let carry = if hist_l.is_empty() {
            None
        } else {
            Some(UpsamplerCarryover {
                identity: self.kernel.identity,
                source_rate_hz: self.kernel.source_rate_hz,
                target_rate_hz: self.kernel.target_rate_hz,
                ratio: self.kernel.ratio,
                taps_per_phase: self.kernel.taps_per_phase,
                pad_tail: self.kernel.pad_tail,
                bank: Arc::clone(&self.kernel.bank),
                hist_l,
                hist_r,
            })
        };

        Ok((
            StereoF64Buffer {
                sample_rate_hz: self.kernel.target_rate_hz,
                left,
                right,
            },
            carry,
        ))
    }
}
