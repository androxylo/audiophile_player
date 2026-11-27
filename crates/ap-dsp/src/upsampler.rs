// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Stateful, bounded-memory floating-point upsamplers: Gaussian windowed-sinc defaults
//! (`gauss-long-lp` / `gauss-hires-lp`), zero-copy `NullUpsampler`, and legacy `SimpleTest`.
//!
//! Hard architectural invariants (`UPSAMPLING_RESEARCH.md` §§4, 6, 9; `FILTER_SELECTION_RESEARCH.md` §4):
//! - Every upsampler consumes bounded [`StereoF64Buffer`] blocks (`process_block` + `flush`)
//!   and ALWAYS outputs [`StereoF64Buffer`] (pure IEEE 754 `f64`).
//! - **DR-1 (mandatory floor):** Every filter at `fs_in >= 44.1 kHz` keeps `|H(f)|` within
//!   `±0.01 dB` of unity from `0 Hz` to `19 kHz` absolute; violations are rejected with `Err`.
//! - **Exact odd prototype length `N`**, mirrored symmetry, per-phase unity DC normalization,
//!   and exact zero group-delay shift.
//! - Streaming chunked processing and equal-filter gapless splits produce **bit-identical**
//!   output to whole-track processing across arbitrary chunk boundaries.
//! - Gapless carryover validates full [`FilterIdentity`] (not just tap count) so different
//!   filters of equal length never splice.
//! - Filter tap counts are strictly capped `<= MAX_FILTER_TAPS` (`4,000,000`).

use std::collections::{HashMap, VecDeque};
use std::f64::consts::PI;
use std::sync::{Arc, Mutex, OnceLock};

use ap_core::buffer::StereoF64Buffer;

use crate::MAX_FILTER_TAPS;
use crate::dd::{DD_ONE, DD_PI, DD_TWO_PI, Dd, dd_bessel_i0, dd_sum_f64};
use crate::phase::{INTERMEDIATE_PHASE_ALPHA, convert_prototype_phase};
use crate::selector::{
    FilterArchetype, FilterDesignMetrics, FilterIdentity, FilterPhase,
    MAX_SUPPORTED_SOURCE_RATE_HZ, MAX_UPSAMPLE_RATIO, MIN_SUPPORTED_SOURCE_RATE_HZ,
    SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE, SIMPLE_TEST_MIN_TAPS_PER_PHASE, SelectedDspPlan,
    UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsampleRatio, simple_test_taps_per_phase,
};
use crate::upsampler_fft::{FftUpolsUpsampler, PreparedUpsamplerKernel};

/// Mandatory DR-1 passband floor in Hz (`UPSAMPLING_RESEARCH.md` §4.1, §9).
pub const DR1_MIN_PASSBAND_HZ: f64 = 19_000.0;

/// Minimum source rate at which absolute 19 kHz DR-1 applies (sub-44.1k legacy sources have
/// Nyquist below 19 kHz).
pub const DR1_MIN_SOURCE_RATE_HZ: u32 = 44_100;

/// Gaussian `-0.01 dB` passband landmark (`fc - 3.05 * sigma_f` -> `-0.00994 dB`, inside `±0.01 dB`).
pub const GAUSS_PASSBAND_SIGMAS: f64 = 3.05;

/// Gaussian `-300 dB` stopband landmark (`fc + 7.95 * sigma_f` -> `-300.6 dB`).
pub const GAUSS_STOPBAND_SIGMAS: f64 = 7.95;

/// Gaussian time-domain window truncation radius (`±8.4 * sigma_t` -> window `<= -306 dB`).
pub const GAUSS_TRUNCATION_SIGMAS: f64 = 8.4;

/// Target stopband attenuation in dB for the default Gaussian prototypes.
pub const GAUSS_STOPBAND_ATTENUATION_DB: f64 = -300.0;

/// Short-Gaussian (`#8a/#8b`) `-150 dB` stopband landmark (`fc + 5.55 * sigma_f` -> `< -150 dB`).
pub const SHORT_GAUSS_STOPBAND_SIGMAS: f64 = 5.55;

/// Short-Gaussian (`#8a/#8b`) window truncation radius (`±6.2 * sigma_t` -> window `<= -167 dB`).
pub const SHORT_GAUSS_TRUNCATION_SIGMAS: f64 = 6.2;

/// Target stopband attenuation in dB for `#8a short-lp` / `#8b short-mp`.
pub const SHORT_GAUSS_STOPBAND_ATTENUATION_DB: f64 = -150.0;

/// Kaiser stopband attenuation target (`A = 200 dB`) for `#4 sinc-steep-4m` and `#5 ext-sharp-lp`.
pub const KAISER_STEEP_ATTENUATION_DB: f64 = 200.0;

/// Kaiser beta for `A = 200 dB`: `0.1102 * (200.0 - 8.7) = 21.08126`.
pub const KAISER_STEEP_BETA: f64 = 0.1102 * (KAISER_STEEP_ATTENUATION_DB - 8.7);

/// Empirical Kaiser normalized transition width `delta_f / fs` for stopband attenuation `a_db`
/// and odd length `num_taps` (`UPSAMPLING_RESEARCH.md` §4.1, Review #9 D7).
#[inline]
pub(crate) fn kaiser_transition_width_norm(attenuation_db: f64, num_taps: usize) -> f64 {
    (attenuation_db - 7.95) / (14.36 * (num_taps.max(2) as f64))
}

/// Evaluate a double-double (`Dd`) Kaiser-windowed sinc sample `sinc(2*fc*tau) * I0(beta*sqrt(1-(tau/half)^2))/I0(beta)`
/// at normalized offset `tau` (shared by steep upsamplers and anti-aliasing decimators, D7).
#[inline]
pub(crate) fn kaiser_sinc_dd(
    tau: Dd,
    fc_dd: Dd,
    half_dd: Dd,
    beta_dd: Dd,
    inv_i0_beta: Dd,
) -> Dd {
    let sinc_dd = if tau.to_f64().abs() < 1e-15 {
        fc_dd.add(fc_dd)
    } else {
        let x = DD_TWO_PI.mul(fc_dd).mul(tau);
        let (sin_x, _) = x.sin_cos();
        sin_x.div(DD_PI.mul(tau))
    };
    let r = tau.div(half_dd);
    if r.to_f64().abs() >= 1.0 {
        return Dd::from_f64(0.0);
    }
    let rad = DD_ONE.sub(r.sqr());
    let win = if rad.hi > 0.0 {
        dd_bessel_i0(beta_dd.mul(rad.sqrt())).mul(inv_i0_beta)
    } else {
        inv_i0_beta
    };
    sinc_dd.mul(win)
}

/// Direct polyphase vs FFT/UPOLS crossover threshold in taps per phase (`UPSAMPLING_RESEARCH.md` §4.2).
pub const DIRECT_POLYPHASE_MAX_TAPS_PER_PHASE: usize = 2_048;

/// Maximum cached prepared FFT kernels and polyphase filter banks in memory.
const MAX_CACHED_KERNELS: usize = 16;

/// Unpadded trailing FIR input history carried between consecutive equal-filter gapless tracks
/// so interpolation across track boundaries matches a single continuous one-shot upsample
/// with zero boundary dip or click.
#[derive(Debug, Clone)]
pub struct UpsamplerCarryover {
    pub identity: FilterIdentity,
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    pub taps_per_phase: usize,
    pub pad_tail: usize,
    pub(crate) bank: Arc<Vec<Vec<f64>>>,
    pub hist_l: Vec<f64>,
    pub hist_r: Vec<f64>,
}

impl UpsamplerCarryover {
    /// Maximum absolute sample amplitude present in the carried unpadded FIR history (`0.0` if empty).
    pub fn peak_linear(&self) -> f64 {
        let max_l = self
            .hist_l
            .iter()
            .copied()
            .map(f64::abs)
            .fold(0.0_f64, f64::max);
        let max_r = self
            .hist_r
            .iter()
            .copied()
            .map(f64::abs)
            .fold(0.0_f64, f64::max);
        max_l.max(max_r)
    }

    /// Output length corresponding to [`Upsampler::flush`] (i.e. exact `total_in_frames * ratio`
    /// duration without the post-ringing zero-input FIR decay tail).
    pub fn exact_flush_len(&self) -> usize {
        self.pad_tail * self.ratio
    }

    /// Flush the carried unpadded FIR history against trailing zeros into `f64` samples using the
    /// EXACT polyphase bank that generated this carryover (used when a gapless sequence ends,
    /// switches sample rate/filter identity, or encounters an error).
    ///
    /// Pads by the full `taps_per_phase - 1` zeros (Review #6 Finding #13) so the entire FIR
    /// zero-input decay tail rings down cleanly to zero — matching a continuous zero-input render
    /// bit-for-bit across the full impulse support, with the first [`Self::exact_flush_len`]
    /// samples matching [`Upsampler::flush`] bit-for-bit.
    pub fn flush_to_f64(mut self) -> StereoF64Buffer {
        if self.hist_l.is_empty()
            || self.ratio == 0
            || self.taps_per_phase == 0
            || self.bank.len() != self.ratio
        {
            return StereoF64Buffer::empty(self.target_rate_hz);
        }
        let full_pad = self.taps_per_phase.saturating_sub(1);
        if self.taps_per_phase > 2_048 {
            let bank = Arc::clone(&self.bank);
            if let Ok(buf) = crate::upsampler_fft::flush_carryover_via_fft(&self, bank) {
                return buf;
            }
        }
        if full_pad > 0 {
            self.hist_l.resize(self.hist_l.len() + full_pad, 0.0_f64);
            self.hist_r.resize(self.hist_r.len() + full_pad, 0.0_f64);
        }
        let (left, right) = convolve_polyphase_bank(
            &self.bank,
            self.ratio,
            self.taps_per_phase,
            &mut self.hist_l,
            &mut self.hist_r,
        );
        StereoF64Buffer {
            sample_rate_hz: self.target_rate_hz,
            left,
            right,
        }
    }
}

/// Read-only verification snapshot of an active upsampler's polyphase bank and alignment
/// (`PIPELINE_TESTS_REVIEW.md` §2.3, exposed only under `test` or `feature = "verify"`).
#[cfg(any(test, feature = "verify"))]
#[derive(Debug, Clone)]
pub struct UpsamplerExport {
    pub ratio: usize,
    pub taps_per_phase: usize,
    pub pad_lead: usize,
    pub pad_tail: usize,
    pub bank: Arc<Vec<Vec<f64>>>,
}

#[cfg(any(test, feature = "verify"))]
impl UpsamplerExport {
    /// Causal prototype FIR taps `h[0..taps_per_phase * ratio]` corresponding to zero-stuffed
    /// linear convolution `h[j * ratio + p] = bank[p][taps_per_phase - 1 - j]`.
    pub fn prototype_taps(&self) -> Vec<f64> {
        let mut h = vec![0.0_f64; self.taps_per_phase * self.ratio];
        for (p, coeffs) in self.bank.iter().enumerate() {
            for j in 0..self.taps_per_phase {
                h[j * self.ratio + p] = coeffs[self.taps_per_phase - 1 - j];
            }
        }
        h
    }

    /// Exact leading discard / latency in output-rate samples (`pad_tail * ratio`).
    pub fn latency_samples(&self) -> usize {
        self.pad_tail * self.ratio
    }
}

/// Pluggable stateful floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`).
pub trait Upsampler: Send {
    fn name(&self) -> &'static str;

    /// Prototype FIR kernel tap count (`0` for [`NullUpsampler`]).
    fn total_taps(&self) -> usize;

    /// Exact alignment latency in output-rate samples (`pad_tail * ratio`; `0` for `NullUpsampler`).
    fn latency_samples(&self) -> usize {
        0
    }

    /// Read-only polyphase bank & alignment export for independent loopback null verification.
    #[cfg(any(test, feature = "verify"))]
    fn verify_export(&self) -> Option<UpsamplerExport> {
        None
    }

    /// Measured/analytical filter metadata (`None` for [`NullUpsampler`]).
    fn design_metrics(&self) -> Option<FilterDesignMetrics> {
        None
    }

    /// Unique kernel identity for strict gapless carryover validation.
    fn identity(&self) -> FilterIdentity;

    /// Process one bounded source block, emitting all ready upsampled `f64` samples.
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;

    /// Flush trailing FIR overlap tail at end-of-track (emits remaining exact samples).
    fn flush(&mut self) -> Result<StereoF64Buffer, String>;

    /// Attach unpadded FIR history from the preceding equal-filter gapless track instead of
    /// leading zeros. Returns `Err(carryover)` if the carryover's [`FilterIdentity`] or geometry
    /// does not match this upsampler.
    fn set_incoming_carryover(
        &mut self,
        carryover: UpsamplerCarryover,
    ) -> Result<(), UpsamplerCarryover> {
        Err(carryover)
    }

    /// End a track for gapless continuity: skip trailing zero-padding and hand the unpadded
    /// FIR history to the next equal-filter session so there is zero boundary discontinuity.
    fn flush_for_gapless(&mut self) -> Result<(StereoF64Buffer, Option<UpsamplerCarryover>), String> {
        self.flush().map(|buf| (buf, None))
    }
}

/// Specification for Building Block A: Gaussian-windowed sinc prototype (`UPSAMPLING_RESEARCH.md` §4.1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GaussianFilterSpec {
    pub archetype: FilterArchetype,
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    /// Cutoff (`-6 dB` point) in Hz.
    pub fc_hz: f64,
    /// Gaussian frequency-domain standard deviation `sigma_f` in Hz.
    pub sigma_f_hz: f64,
    /// Gaussian time-domain standard deviation `sigma_t = 1 / (2 * PI * sigma_f)` in seconds.
    pub sigma_t_seconds: f64,
    /// Passband `-0.01 dB` edge (`fc - 3.05 * sigma_f`) in Hz.
    pub passband_edge_001db_hz: f64,
    /// Stopband edge (`fc + 7.95 * sigma_f`) in Hz.
    pub stopband_edge_hz: f64,
    /// Half-span of the odd prototype (`N = 2 * half_span + 1`).
    pub half_span: usize,
    /// Odd prototype length `N = 2 * half_span + 1`.
    pub num_taps: usize,
    /// Optional content-derived effective Nyquist (reserved for Phase 6 Detector H).
    pub effective_nyquist_hz: Option<u32>,
}

impl GaussianFilterSpec {
    /// Construct and validate a Gaussian prototype from explicit `(fc_hz, sigma_f_hz)`.
    ///
    /// Strictly enforces **DR-1** (`fc - 3.05 * sigma_f >= 19_000 Hz` for all `source_rate_hz >= 44_100`)
    /// and rejects violations with `Err(...)` rather than silently clamping.
    pub fn from_fc_sigma(
        archetype: FilterArchetype,
        source_rate_hz: u32,
        target_rate_hz: u32,
        fc_hz: f64,
        sigma_f_hz: f64,
        effective_nyquist_hz: Option<u32>,
    ) -> Result<Self, String> {
        if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ)
            .contains(&source_rate_hz)
        {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {source_rate_hz} Hz is outside the supported \
                 {MIN_SUPPORTED_SOURCE_RATE_HZ}..={MAX_SUPPORTED_SOURCE_RATE_HZ} Hz range"
            ));
        }
        if target_rate_hz <= source_rate_hz || target_rate_hz % source_rate_hz != 0 {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} Gaussian upsampler requires an exact integer \
                 ratio >= 2x (got {source_rate_hz} -> {target_rate_hz} Hz)"
            ));
        }
        let ratio_u32 = target_rate_hz / source_rate_hz;
        if ratio_u32 > MAX_UPSAMPLE_RATIO {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} integer upsampling ratio {ratio_u32}x exceeds \
                 hard cap {MAX_UPSAMPLE_RATIO}x (source rate {source_rate_hz} Hz)"
            ));
        }
        if !fc_hz.is_finite() || !sigma_f_hz.is_finite() || fc_hz <= 0.0 || sigma_f_hz <= 0.0 {
            return Err(format!(
                "invalid Gaussian filter parameters: fc={fc_hz} Hz, sigma_f={sigma_f_hz} Hz"
            ));
        }

        let nyquist = 0.5 * (source_rate_hz as f64);
        let passband_edge_001db_hz = fc_hz - GAUSS_PASSBAND_SIGMAS * sigma_f_hz;
        let stopband_edge_hz = fc_hz + GAUSS_STOPBAND_SIGMAS * sigma_f_hz;

        if passband_edge_001db_hz <= 0.0 || fc_hz >= nyquist {
            return Err(format!(
                "invalid Gaussian filter shape for {source_rate_hz} Hz: \
                 passband={passband_edge_001db_hz:.2} Hz, fc={fc_hz:.2} Hz, nyquist={nyquist:.2} Hz"
            ));
        }

        // Mandatory DR-1 floor: reject any parameter set below 19 kHz for fs_in >= 44.1 kHz.
        if source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ
            && passband_edge_001db_hz + 1e-6 < DR1_MIN_PASSBAND_HZ
        {
            return Err(format!(
                "DR-1 violation for '{archetype}' at {source_rate_hz} Hz: -0.01 dB passband edge \
                 ({passband_edge_001db_hz:.2} Hz) is below mandatory {DR1_MIN_PASSBAND_HZ:.0} Hz floor"
            ));
        }

        let sigma_t_seconds = 1.0 / (2.0 * PI * sigma_f_hz);
        let half_span_f = (GAUSS_TRUNCATION_SIGMAS * sigma_t_seconds * (target_rate_hz as f64)).ceil();
        if !half_span_f.is_finite() || half_span_f < 1.0 || half_span_f > (MAX_FILTER_TAPS as f64) {
            return Err(format!(
                "Gaussian filter span ({half_span_f}) exceeds hard {MAX_FILTER_TAPS}-tap cap"
            ));
        }
        let half_span = half_span_f as usize;
        let num_taps = 2 * half_span + 1;
        let ratio = ratio_u32 as usize;
        let m_half = half_span.div_ceil(ratio);
        let taps_per_phase = 2 * m_half + 1;
        let poly_total = ratio.saturating_mul(taps_per_phase);
        if num_taps > MAX_FILTER_TAPS || poly_total > MAX_FILTER_TAPS {
            return Err(format!(
                "Gaussian filter length ({num_taps} taps, polyphase {poly_total}) exceeds hard \
                 {MAX_FILTER_TAPS}-tap ceiling"
            ));
        }

        Ok(Self {
            archetype,
            source_rate_hz,
            target_rate_hz,
            ratio,
            fc_hz,
            sigma_f_hz,
            sigma_t_seconds,
            passband_edge_001db_hz,
            stopband_edge_hz,
            half_span,
            num_taps,
            effective_nyquist_hz,
        })
    }

    /// Construct and validate a Gaussian prototype from passband `-0.01 dB` edge `f_pass_hz` and
    /// `-300 dB` stopband edge `f_stop_hz` (`UPSAMPLING_RESEARCH.md` §4.1 rule: shape -> `sigma_f` -> `N`).
    pub fn from_pass_stop(
        archetype: FilterArchetype,
        source_rate_hz: u32,
        target_rate_hz: u32,
        f_pass_hz: f64,
        f_stop_hz: f64,
        effective_nyquist_hz: Option<u32>,
    ) -> Result<Self, String> {
        if !f_pass_hz.is_finite()
            || !f_stop_hz.is_finite()
            || f_pass_hz <= 0.0
            || f_stop_hz <= f_pass_hz
        {
            return Err(format!(
                "invalid Gaussian pass/stop edges: f_pass={f_pass_hz} Hz, f_stop={f_stop_hz} Hz"
            ));
        }
        let nyquist = 0.5 * (source_rate_hz as f64);
        if f_stop_hz > nyquist + 1e-6 {
            return Err(format!(
                "Gaussian stop edge ({f_stop_hz:.2} Hz) exceeds source Nyquist ({nyquist:.2} Hz)"
            ));
        }
        let sigma_f_hz = (f_stop_hz - f_pass_hz) / (GAUSS_PASSBAND_SIGMAS + GAUSS_STOPBAND_SIGMAS);
        let fc_hz = f_pass_hz + GAUSS_PASSBAND_SIGMAS * sigma_f_hz;
        Self::from_fc_sigma(
            archetype,
            source_rate_hz,
            target_rate_hz,
            fc_hz,
            sigma_f_hz,
            effective_nyquist_hz,
        )
    }

    /// Derive the default [`GaussianFilterSpec`] for [`FilterArchetype::GaussLongLp`] or
    /// [`FilterArchetype::GaussHiresLp`] (`UPSAMPLING_RESEARCH.md` §4.4).
    pub fn for_default_archetype(
        archetype: FilterArchetype,
        source_rate_hz: u32,
        target_rate_hz: u32,
    ) -> Result<Self, String> {
        let fs_in = source_rate_hz as f64;
        let f_stop = 0.5 * fs_in;
        let f_pass = match archetype {
            FilterArchetype::GaussLongLp => {
                // 20,000 Hz at 44.1 kHz, scaled proportionally with fs_in (21,768.7 Hz at 48 kHz).
                fs_in * (20_000.0 / 44_100.0)
            }
            FilterArchetype::GaussHiresLp => {
                // Hi-res shape: flat to 0.42 * fs_in (40.32 kHz at 96 kHz), -300 dB at fs_in / 2.
                // Note: at 44.1 kHz, 0.42 * 44,100 = 18,522 Hz < 19,000 Hz, so DR-1 cleanly
                // rejects forcing gauss-hires-lp onto 44.1 kHz sources rather than silently clamping.
                0.42 * fs_in
            }
            other => {
                return Err(format!(
                    "filter '{other}' is not a Phase 1 default Gaussian archetype"
                ));
            }
        };
        Self::from_pass_stop(archetype, source_rate_hz, target_rate_hz, f_pass, f_stop, None)
    }
}

/// Compute the shape-derived odd tap count `N` for a default Gaussian archetype (used by
/// `FilterArchetype::planned_taps` so `SelectedDspPlan::tap_budget` matches `upsampler.total_taps()`).
pub fn default_gaussian_num_taps(
    archetype: FilterArchetype,
    source_rate_hz: u32,
    target_rate_hz: u32,
) -> Option<usize> {
    GaussianFilterSpec::for_default_archetype(archetype, source_rate_hz, target_rate_hz)
        .ok()
        .map(|s| s.num_taps)
}

/// Derive exact odd prototype length `N = 2 * m_half * ratio + 1` bounded so `poly_total <= cap`.
fn constant_time_m_half(desired_half_in: usize, ratio: usize, cap: usize) -> Option<usize> {
    let cap = cap.min(MAX_FILTER_TAPS);
    let max_m = cap / ratio.max(1);
    let max_m_half = max_m.saturating_sub(1) / 2;
    let m_half = desired_half_in.min(max_m_half);
    if m_half < 4 { None } else { Some(m_half) }
}

/// Build a `#8a short-lp` / `#8b short-mp` Gaussian prototype spec (`-150 dB` at Nyquist,
/// flat `-0.01 dB` to `19 kHz` DR-1 floor at 44.1k, `0.42 * fs_in` at hi-res).
pub fn short_gaussian_spec(
    archetype: FilterArchetype,
    source_rate_hz: u32,
    target_rate_hz: u32,
    effective_nyquist_hz: Option<u32>,
) -> Result<(GaussianFilterSpec, f64, f64), String> {
    if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ).contains(&source_rate_hz) {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {source_rate_hz} Hz is outside the supported \
             {MIN_SUPPORTED_SOURCE_RATE_HZ}..={MAX_SUPPORTED_SOURCE_RATE_HZ} Hz range"
        ));
    }
    if target_rate_hz <= source_rate_hz || target_rate_hz % source_rate_hz != 0 {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} short Gaussian requires an exact integer ratio >= 2x \
             (got {source_rate_hz} -> {target_rate_hz} Hz)"
        ));
    }
    let ratio_u32 = target_rate_hz / source_rate_hz;
    if ratio_u32 > MAX_UPSAMPLE_RATIO {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} integer upsampling ratio {ratio_u32}x exceeds cap"
        ));
    }
    let fs_in = source_rate_hz as f64;
    // Review #7 F11: If an untrusted/YAML `effective_nyquist_hz` would force `f_pass` below the
    // mandatory 19 kHz DR-1 floor, degrade gracefully to native Nyquist (`None`) instead of failing.
    let mut effective_nyquist_hz = effective_nyquist_hz.filter(|&f| {
        !(source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ && (f as f64) <= DR1_MIN_PASSBAND_HZ * 1.02)
    });
    let compute_edges = |feff: Option<u32>| -> (f64, f64) {
        let f_stop = feff
            .map(|f| (f as f64).min(0.5 * fs_in))
            .unwrap_or(0.5 * fs_in);
        let f_pass = if source_rate_hz <= 48_000 {
            (fs_in * (19_000.0 / 44_100.0)).min(f_stop * 0.90)
        } else {
            (0.42 * fs_in).min(f_stop * 0.88)
        };
        (f_pass, f_stop)
    };
    let (mut f_pass, mut f_stop) = compute_edges(effective_nyquist_hz);
    if effective_nyquist_hz.is_some()
        && source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ
        && (f_pass + 1e-6 < DR1_MIN_PASSBAND_HZ || f_stop <= f_pass)
    {
        effective_nyquist_hz = None;
        let (p, s) = compute_edges(None);
        f_pass = p;
        f_stop = s;
    }
    if source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ && f_pass + 1e-6 < DR1_MIN_PASSBAND_HZ {
        return Err(format!(
            "DR-1 violation for '{archetype}' at {source_rate_hz} Hz: f_pass={f_pass:.2} Hz < 19000 Hz"
        ));
    }
    let sigma_f_hz = (f_stop - f_pass) / (GAUSS_PASSBAND_SIGMAS + SHORT_GAUSS_STOPBAND_SIGMAS);
    let fc_hz = f_pass + GAUSS_PASSBAND_SIGMAS * sigma_f_hz;
    let sigma_t_seconds = 1.0 / (2.0 * PI * sigma_f_hz);
    let half_span = (SHORT_GAUSS_TRUNCATION_SIGMAS * sigma_t_seconds * (target_rate_hz as f64)).ceil() as usize;
    let num_taps = 2 * half_span + 1;
    let ratio = ratio_u32 as usize;
    let m_half = half_span.div_ceil(ratio);
    let poly_total = ratio.saturating_mul(2 * m_half + 1);
    if num_taps > MAX_FILTER_TAPS || poly_total > MAX_FILTER_TAPS {
        return Err(format!(
            "short Gaussian length ({num_taps}) exceeds hard {MAX_FILTER_TAPS}-tap cap"
        ));
    }
    let spec = GaussianFilterSpec {
        archetype,
        source_rate_hz,
        target_rate_hz,
        ratio,
        fc_hz,
        sigma_f_hz,
        sigma_t_seconds,
        passband_edge_001db_hz: f_pass,
        stopband_edge_hz: f_stop,
        half_span,
        num_taps,
        effective_nyquist_hz,
    };
    Ok((spec, SHORT_GAUSS_STOPBAND_ATTENUATION_DB, SHORT_GAUSS_TRUNCATION_SIGMAS))
}

/// Build a `#3 gauss-apod-4m` (`sinc-MGa` class) Gaussian spec whose `±8.4 σ_t` span fits
/// `65,536 * ratio + 1` (bounded by `cap`) with ZERO dead padding and `-300 dB` at `f_stop <= fs_in/2`.
pub fn gauss_apod_4m_spec(
    source_rate_hz: u32,
    target_rate_hz: u32,
    cap: usize,
    effective_nyquist_hz: Option<u32>,
) -> Result<GaussianFilterSpec, String> {
    if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ).contains(&source_rate_hz) {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {source_rate_hz} Hz is outside supported range"
        ));
    }
    if target_rate_hz <= source_rate_hz || target_rate_hz % source_rate_hz != 0 {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} gauss-apod-4m requires an exact integer ratio >= 2x"
        ));
    }
    let ratio_u32 = target_rate_hz / source_rate_hz;
    if ratio_u32 > MAX_UPSAMPLE_RATIO {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} integer upsampling ratio {ratio_u32}x exceeds cap"
        ));
    }
    let ratio = ratio_u32 as usize;
    let m_half = constant_time_m_half(32_768, ratio, cap).ok_or_else(|| {
        format!("--max-taps ({cap}) is too small for {ratio}x gauss-apod-4m")
    })?;
    let half_span = m_half * ratio;
    let num_taps = 2 * half_span + 1;
    let fs_in = source_rate_hz as f64;
    let fs_out = target_rate_hz as f64;

    // Derive sigma_t directly from half_span so every coefficient inside ±half_span is above
    // -306 dB (zero dead padding; ultra-steep ~20 Hz Gaussian transition at 65,536 * ratio).
    let sigma_t_seconds = (half_span as f64) / (GAUSS_TRUNCATION_SIGMAS * fs_out);
    let sigma_f_hz = 1.0 / (2.0 * PI * sigma_t_seconds);
    let mut effective_nyquist_hz = effective_nyquist_hz.filter(|&f| {
        !(source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ && (f as f64) <= DR1_MIN_PASSBAND_HZ * 1.02)
    });
    let mut f_stop = effective_nyquist_hz
        .map(|f| (f as f64).min(0.5 * fs_in))
        .unwrap_or(0.5 * fs_in);
    let mut fc_hz = f_stop - GAUSS_STOPBAND_SIGMAS * sigma_f_hz;
    let mut f_pass = fc_hz - GAUSS_PASSBAND_SIGMAS * sigma_f_hz;

    if effective_nyquist_hz.is_some()
        && (f_pass <= 0.0
            || fc_hz <= 0.0
            || (source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ && f_pass + 1e-6 < DR1_MIN_PASSBAND_HZ))
    {
        effective_nyquist_hz = None;
        f_stop = 0.5 * fs_in;
        fc_hz = f_stop - GAUSS_STOPBAND_SIGMAS * sigma_f_hz;
        f_pass = fc_hz - GAUSS_PASSBAND_SIGMAS * sigma_f_hz;
    }

    if f_pass <= 0.0 || fc_hz <= 0.0 || fc_hz >= 0.5 * fs_in {
        return Err(format!(
            "invalid gauss-apod-4m parameters for {source_rate_hz} Hz (cap={cap}): fc={fc_hz:.2}"
        ));
    }
    if source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ && f_pass + 1e-6 < DR1_MIN_PASSBAND_HZ {
        return Err(format!(
            "DR-1 violation for 'gauss-apod-4m' at {source_rate_hz} Hz (cap={cap}): \
             -0.01 dB passband ({f_pass:.2} Hz) < 19000 Hz"
        ));
    }

    Ok(GaussianFilterSpec {
        archetype: FilterArchetype::GaussApod4M,
        source_rate_hz,
        target_rate_hz,
        ratio,
        fc_hz,
        sigma_f_hz,
        sigma_t_seconds,
        passband_edge_001db_hz: f_pass,
        stopband_edge_hz: f_stop,
        half_span,
        num_taps,
        effective_nyquist_hz,
    })
}

/// Build an `OQ-37` HF ultrasonic noise-removal Gaussian spec (`#2 gauss-hires-lp` with `-300 dB`
/// stopband at 48 kHz for 4fS `>= 176.4 kHz` or 44.1 kHz for 2fS `88.2/96 kHz`).
pub fn hires_hf_noise_filter_spec(
    source_rate_hz: u32,
    target_rate_hz: u32,
) -> Result<GaussianFilterSpec, String> {
    let (f_pass, f_stop) = if source_rate_hz >= 176_400 {
        (40_000.0_f64, 48_000.0_f64)
    } else if source_rate_hz >= 88_200 {
        (34_000.0_f64, 44_100.0_f64)
    } else {
        return GaussianFilterSpec::for_default_archetype(
            FilterArchetype::GaussLongLp,
            source_rate_hz,
            target_rate_hz,
        );
    };
    GaussianFilterSpec::from_pass_stop(
        FilterArchetype::GaussHiresLp,
        source_rate_hz,
        target_rate_hz,
        f_pass,
        f_stop,
        Some(f_stop as u32),
    )
}

/// Specification for Kaiser-windowed steep linear-phase prototypes (`#4 sinc-steep-4m`, `#5 ext-sharp-lp`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KaiserFilterSpec {
    pub archetype: FilterArchetype,
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    pub fc_hz: f64,
    pub beta: f64,
    pub attenuation_db: f64,
    pub passband_edge_001db_hz: f64,
    pub stopband_edge_hz: f64,
    pub half_span: usize,
    pub num_taps: usize,
    pub m_half: usize,
    pub effective_nyquist_hz: Option<u32>,
}

impl KaiserFilterSpec {
    pub fn for_archetype(
        archetype: FilterArchetype,
        source_rate_hz: u32,
        target_rate_hz: u32,
        cap: usize,
        effective_nyquist_hz: Option<u32>,
    ) -> Result<Self, String> {
        if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ).contains(&source_rate_hz) {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {source_rate_hz} Hz is outside supported range"
            ));
        }
        if target_rate_hz <= source_rate_hz || target_rate_hz % source_rate_hz != 0 {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} '{archetype}' requires an exact integer ratio >= 2x"
            ));
        }
        let ratio_u32 = target_rate_hz / source_rate_hz;
        if ratio_u32 > MAX_UPSAMPLE_RATIO {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} integer upsampling ratio {ratio_u32}x exceeds cap"
            ));
        }
        let desired_half_in = match archetype {
            FilterArchetype::SincSteep4M => 65_535, // 131,070 * ratio + 1 (`sinc-L` class)
            FilterArchetype::ExtSharpLp => 32_768,  // 65,536 * ratio + 1 (`sinc-Mx` / `ext3` class)
            other => {
                return Err(format!("'{other}' is not a Kaiser steep archetype"));
            }
        };
        let ratio = ratio_u32 as usize;
        let m_half = constant_time_m_half(desired_half_in, ratio, cap).ok_or_else(|| {
            format!("--max-taps ({cap}) is too small for {ratio}x {archetype}")
        })?;
        let half_span = m_half * ratio;
        let num_taps = 2 * half_span + 1;
        let fs_in = source_rate_hz as f64;
        let fs_out = target_rate_hz as f64;
        let a_db = KAISER_STEEP_ATTENUATION_DB;
        let beta = KAISER_STEEP_BETA;

        // Empirical Kaiser transition width (UPS §4.1) with slight margin so -200 dB hits <= f_stop.
        let delta_f = kaiser_transition_width_norm(a_db, num_taps) * fs_out;
        let mut effective_nyquist_hz = effective_nyquist_hz.filter(|&f| {
            !(source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ && (f as f64) <= DR1_MIN_PASSBAND_HZ * 1.02)
        });
        let mut f_stop = effective_nyquist_hz
            .map(|f| (f as f64).min(0.5 * fs_in))
            .unwrap_or(0.5 * fs_in);
        let mut fc_hz = f_stop - 0.55 * delta_f;
        let mut f_pass = f_stop - 1.15 * delta_f;

        if effective_nyquist_hz.is_some()
            && (f_pass <= 0.0
                || fc_hz <= 0.0
                || (source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ
                    && f_pass + 1e-6 < DR1_MIN_PASSBAND_HZ))
        {
            effective_nyquist_hz = None;
            f_stop = 0.5 * fs_in;
            fc_hz = f_stop - 0.55 * delta_f;
            f_pass = f_stop - 1.15 * delta_f;
        }

        if f_pass <= 0.0 || fc_hz <= 0.0 || fc_hz >= 0.5 * fs_in {
            return Err(format!(
                "invalid Kaiser filter '{archetype}' at {source_rate_hz} Hz (cap={cap}): fc={fc_hz:.2} Hz"
            ));
        }
        if source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ && f_pass + 1e-6 < DR1_MIN_PASSBAND_HZ {
            return Err(format!(
                "DR-1 violation for '{archetype}' at {source_rate_hz} Hz (cap={cap}): \
                 -0.01 dB passband ({f_pass:.2} Hz) < 19000 Hz"
            ));
        }

        Ok(Self {
            archetype,
            source_rate_hz,
            target_rate_hz,
            ratio,
            fc_hz,
            beta,
            attenuation_db: -a_db,
            passband_edge_001db_hz: f_pass,
            stopband_edge_hz: f_stop,
            half_span,
            num_taps,
            m_half,
            effective_nyquist_hz,
        })
    }
}

/// Unified tap-count helper used by `FilterArchetype::planned_taps` so `SelectedDspPlan::tap_budget`
/// matches `upsampler.total_taps()` for every archetype.
pub fn planned_taps_for_archetype(
    archetype: FilterArchetype,
    source_rate_hz: u32,
    target_rate_hz: u32,
    max_taps: usize,
) -> Option<usize> {
    let cap = max_taps.min(MAX_FILTER_TAPS);
    match archetype {
        FilterArchetype::GaussLongLp | FilterArchetype::GaussHiresLp => {
            default_gaussian_num_taps(archetype, source_rate_hz, target_rate_hz)
        }
        FilterArchetype::GaussApod4M => {
            gauss_apod_4m_spec(source_rate_hz, target_rate_hz, cap, None)
                .ok()
                .map(|s| s.num_taps)
        }
        FilterArchetype::SincSteep4M | FilterArchetype::ExtSharpLp => {
            KaiserFilterSpec::for_archetype(archetype, source_rate_hz, target_rate_hz, cap, None)
                .ok()
                .map(|s| s.num_taps)
        }
        FilterArchetype::MinphaseLong | FilterArchetype::IntermediateLong => {
            let ref_arch = if source_rate_hz <= 48_000 {
                FilterArchetype::GaussLongLp
            } else {
                FilterArchetype::GaussHiresLp
            };
            default_gaussian_num_taps(ref_arch, source_rate_hz, target_rate_hz)
        }
        FilterArchetype::ShortLp | FilterArchetype::ShortMp => {
            short_gaussian_spec(archetype, source_rate_hz, target_rate_hz, None)
                .ok()
                .map(|(s, _, _)| s.num_taps)
        }
        FilterArchetype::Null | FilterArchetype::SimpleTest => None,
    }
}

/// Designed odd-length linear-phase Gaussian prototype + DC-normalized polyphase bank + metrics.
#[derive(Debug, Clone)]
pub struct DesignedGaussianFilter {
    pub spec: GaussianFilterSpec,
    /// Normalized odd-length prototype `h[0..N]` (`sum(h) == L`, `h[k] == h[N - 1 - k]` bit-exact).
    pub prototype: Vec<f64>,
    /// Polyphase coefficient bank (`ratio` phases x `taps_per_phase`, each summing to `1.0`).
    pub bank: Arc<Vec<Vec<f64>>>,
    pub taps_per_phase: usize,
    pub m_half: usize,
    pub metrics: FilterDesignMetrics,
    pub identity: FilterIdentity,
}

#[cfg(any(test, feature = "verify"))]
impl DesignedGaussianFilter {
    /// Read-only slice of the designed prototype FIR taps (`sum(taps) == ratio`).
    pub fn taps(&self) -> &[f64] {
        &self.prototype
    }
}

/// Evaluate the exact zero-phase DTFT magnitude `|H(f)|` of a symmetric odd-length prototype
/// `h[0..N]` (normalized to `sum(h) == ratio`) at frequency `freq_hz` (`0..=fs_out/2`).
pub fn evaluate_symmetric_prototype_mag(
    prototype: &[f64],
    fs_out_hz: f64,
    ratio: usize,
    freq_hz: f64,
) -> f64 {
    let n = prototype.len();
    debug_assert!(n % 2 == 1);
    let center = n / 2;
    let omega = 2.0 * PI * freq_hz / fs_out_hz;
    let mut acc = prototype[center];
    for k in 1..=center {
        acc += 2.0 * prototype[center + k] * (omega * (k as f64)).cos();
    }
    (acc / (ratio as f64)).abs()
}

/// Generic designed upsampling filter (linear, minimum, or intermediate phase) ready for either
/// direct polyphase (`PolyphaseFirUpsampler`) or FFT/UPOLS (`FftUpolsUpsampler`) execution.
#[derive(Debug, Clone)]
pub struct DesignedFilter {
    pub name: &'static str,
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    pub prototype: Vec<f64>,
    pub bank: Arc<Vec<Vec<f64>>>,
    pub taps_per_phase: usize,
    pub pad_lead: usize,
    pub pad_tail: usize,
    pub metrics: FilterDesignMetrics,
    pub identity: FilterIdentity,
}

#[cfg(any(test, feature = "verify"))]
impl DesignedFilter {
    /// Read-only slice of the designed prototype FIR taps (`sum(taps) == ratio`).
    pub fn taps(&self) -> &[f64] {
        &self.prototype
    }
}

/// Evaluate the exact DTFT magnitude `|H(f)|` of an arbitrary causal prototype `h[0..N]`
/// (normalized to `sum(h) == ratio`) at frequency `freq_hz`.
pub fn evaluate_prototype_mag(
    prototype: &[f64],
    fs_out_hz: f64,
    ratio: usize,
    freq_hz: f64,
) -> f64 {
    let omega = 2.0 * PI * freq_hz / fs_out_hz;
    let mut re = 0.0_f64;
    let mut im = 0.0_f64;
    for (k, &h) in prototype.iter().enumerate() {
        let ang = omega * (k as f64);
        let (s, c) = ang.sin_cos();
        re += h * c;
        im -= h * s;
    }
    (re * re + im * im).sqrt() / (ratio as f64)
}

/// Build an exact per-phase DC-normalized symmetric polyphase bank from raw symmetric odd prototype
/// `h_raw[0..N]` using ~106-bit double-double (`dd_sum_f64`) normalization, preserving bit-exact
/// symmetry and reconstructing the exact matching `prototype[0..N]`.
fn normalize_symmetric_prototype_to_bank(
    h_raw: &[f64],
    ratio: usize,
    half_span: usize,
) -> (Vec<f64>, Vec<Vec<f64>>, usize, usize) {
    let n = h_raw.len();
    let m_half = half_span.div_ceil(ratio);
    let taps_per_phase = 2 * m_half + 1;
    let mut bank = vec![vec![0.0_f64; taps_per_phase]; ratio];
    let l_isize = ratio as isize;
    let c_isize = half_span as isize;
    let n_isize = n as isize;

    for (p, branch) in bank.iter_mut().enumerate() {
        for (i, slot) in branch.iter_mut().enumerate() {
            let s = (i as isize) - (m_half as isize);
            let proto_idx = c_isize + (p as isize) - s * l_isize;
            if proto_idx >= 0 && proto_idx < n_isize {
                *slot = h_raw[proto_idx as usize];
            }
        }
    }

    // Phase 0: exact symmetric double-double normalization around center tap `m_half`.
    let sum0 = dd_sum_f64(&bank[0]);
    let inv0 = DD_ONE.div(sum0);
    bank[0][m_half] = Dd::from_f64(bank[0][m_half]).mul(inv0).to_f64();
    for s in 1..=m_half {
        let v = Dd::from_f64(bank[0][m_half + s]).mul(inv0).to_f64();
        bank[0][m_half + s] = v;
        bank[0][m_half - s] = v;
    }
    let rem0 = DD_ONE.sub(dd_sum_f64(&bank[0])).to_f64();
    bank[0][m_half] += rem0;

    // Phases 1..=L/2
    for p in 1..=(ratio / 2) {
        let q = ratio - p;
        let sum_p = dd_sum_f64(&bank[p]);
        let inv_p = DD_ONE.div(sum_p);
        if p == q {
            bank[p][0] = 0.0;
            for i in 1..=m_half {
                let mirror = taps_per_phase - i;
                let v = Dd::from_f64(bank[p][i]).mul(inv_p).to_f64();
                bank[p][i] = v;
                bank[p][mirror] = v;
            }
            let rem_p = DD_ONE.sub(dd_sum_f64(&bank[p])).to_f64();
            bank[p][m_half] += 0.5 * rem_p;
            bank[p][m_half + 1] += 0.5 * rem_p;
        } else {
            bank[p][0] = 0.0;
            bank[q][0] = 0.0;
            for i in 1..taps_per_phase {
                let v = Dd::from_f64(bank[p][i]).mul(inv_p).to_f64();
                bank[p][i] = v;
                let mirror = taps_per_phase - i;
                bank[q][mirror] = v;
            }
            let rem_p = DD_ONE.sub(dd_sum_f64(&bank[p])).to_f64();
            bank[p][m_half] += rem_p;
            bank[q][taps_per_phase - m_half] = bank[p][m_half];
        }
    }

    let mut prototype = vec![0.0_f64; n];
    for (p, branch) in bank.iter().enumerate() {
        for (i, &coeff) in branch.iter().enumerate() {
            let s = (i as isize) - (m_half as isize);
            let proto_idx = c_isize + (p as isize) - s * l_isize;
            if proto_idx >= 0 && proto_idx < n_isize {
                prototype[proto_idx as usize] = coeff;
            }
        }
    }

    (prototype, bank, taps_per_phase, m_half)
}

/// Design Building Block A (Gaussian-windowed sinc prototype) in ~106-bit double-double (`U8`),
/// rounded once to `f64`, with bit-exact symmetry and double-double per-phase unity DC normalization.
pub fn design_gaussian_filter(spec: GaussianFilterSpec) -> DesignedGaussianFilter {
    design_gaussian_filter_with_attenuation(spec, GAUSS_STOPBAND_ATTENUATION_DB)
}

/// Design a Gaussian prototype with explicit target stopband attenuation metadata (e.g. `-150 dB`
/// for `#8a short-lp` vs `-300 dB` for `#1/#2/#3`).
pub fn design_gaussian_filter_with_attenuation(
    spec: GaussianFilterSpec,
    stopband_attenuation_db: f64,
) -> DesignedGaussianFilter {
    let n = spec.num_taps;
    let center = spec.half_span;
    let fs_out = spec.target_rate_hz as f64;
    let ratio = spec.ratio;

    let ratio_dd = Dd::from_f64(ratio as f64);
    let fc_dd = Dd::from_f64(spec.fc_hz);
    let fs_out_dd = Dd::from_f64(fs_out);
    let omega_c = DD_TWO_PI.mul(fc_dd).div(fs_out_dd);
    let sigma_samples_dd = Dd::from_f64(spec.sigma_t_seconds).mul(fs_out_dd);
    let inv_sigma_dd = DD_ONE.div(sigma_samples_dd);
    let neg_half = Dd::from_f64(-0.5);

    // 1. Synthesize half-prototype h_half[0..=center] in ~106-bit double-double (parallel when large).
    let mut half_vals = vec![0.0_f64; center + 1];
    half_vals[0] = ratio_dd.mul(fc_dd.mul_f64(2.0).div(fs_out_dd)).to_f64();

    let eval_slice = |out: &mut [f64], k_start: usize| {
        for (idx, slot) in out.iter_mut().enumerate() {
            let k = (k_start + idx) as f64;
            let k_dd = Dd::from_f64(k);
            let sinc_val = omega_c.mul_f64(k).sin().div(DD_PI.mul(k_dd));
            let u = k_dd.mul(inv_sigma_dd);
            let win = neg_half.mul(u.sqr()).exp();
            *slot = ratio_dd.mul(sinc_val).mul(win).to_f64();
        }
    };

    if center >= 16_384 {
        let workers = std::thread::available_parallelism()
            .map(|p| p.get().clamp(2, 8))
            .unwrap_or(4);
        let chunk_len = center.div_ceil(workers);
        std::thread::scope(|s| {
            for (ci, chunk) in half_vals[1..].chunks_mut(chunk_len).enumerate() {
                let k_start = 1 + ci * chunk_len;
                s.spawn(move || eval_slice(chunk, k_start));
            }
        });
    } else {
        eval_slice(&mut half_vals[1..], 1);
    }

    let mut h_raw = vec![0.0_f64; n];
    h_raw[center] = half_vals[0];
    for k in 1..=center {
        let v = half_vals[k];
        h_raw[center - k] = v;
        h_raw[center + k] = v;
    }

    // 2. Per-phase double-double DC normalization + symmetric reconstruction.
    let (prototype, bank, taps_per_phase, m_half) =
        normalize_symmetric_prototype_to_bank(&h_raw, ratio, center);

    // 3. Diagnostic L1 norm and exact 20 kHz magnitude.
    let l1_norm = bank
        .iter()
        .map(|phase| phase.iter().map(|c| c.abs()).sum::<f64>())
        .fold(0.0_f64, f64::max);
    let l1_norm_db = 20.0 * l1_norm.max(1e-30).log10();

    let level_at_20khz_db = if spec.source_rate_hz > 40_000 {
        let mag = evaluate_symmetric_prototype_mag(&prototype, fs_out, ratio, 20_000.0);
        Some(20.0 * mag.max(1e-30).log10())
    } else {
        None
    };

    let length_seconds = ((n - 1) as f64) / fs_out;
    let metrics = FilterDesignMetrics {
        phase: FilterPhase::Linear,
        num_taps: n,
        taps_per_phase,
        length_seconds,
        group_delay_seconds: 0.5 * length_seconds,
        passband_edge_001db_hz: spec.passband_edge_001db_hz,
        level_at_20khz_db,
        stopband_edge_hz: spec.stopband_edge_hz,
        stopband_attenuation_db,
        l1_norm,
        l1_norm_db,
    };

    let identity = FilterIdentity {
        archetype: spec.archetype,
        source_rate_hz: spec.source_rate_hz,
        target_rate_hz: spec.target_rate_hz,
        ratio,
        num_taps: n,
        taps_per_phase,
        fc_hz_bits: spec.fc_hz.to_bits(),
        sigma_f_hz_bits: spec.sigma_f_hz.to_bits(),
        effective_nyquist_hz: spec.effective_nyquist_hz,
    };

    DesignedGaussianFilter {
        spec,
        prototype,
        bank: Arc::new(bank),
        taps_per_phase,
        m_half,
        metrics,
        identity,
    }
}

/// Design a steep linear-phase Kaiser-windowed sinc prototype (`#4 sinc-steep-4m`, `#5 ext-sharp-lp`)
/// in ~106-bit double-double (`U8`), rounded once to `f64`, with deterministic parallel synthesis.
pub fn design_kaiser_filter(spec: KaiserFilterSpec) -> DesignedFilter {
    let n = spec.num_taps;
    let center = spec.half_span;
    let fs_out = spec.target_rate_hz as f64;
    let ratio = spec.ratio;

    let ratio_dd = Dd::from_f64(ratio as f64);
    let fc_norm_dd = Dd::from_f64(spec.fc_hz).div(Dd::from_f64(fs_out));
    let beta_dd = Dd::from_f64(spec.beta);
    let inv_i0_beta = DD_ONE.div(dd_bessel_i0(beta_dd));
    let center_dd = Dd::from_f64(center as f64);

    let mut half_vals = vec![0.0_f64; center + 1];
    half_vals[0] = ratio_dd.mul(fc_norm_dd.mul_f64(2.0)).to_f64();

    let eval_slice = |out: &mut [f64], k_start: usize| {
        for (idx, slot) in out.iter_mut().enumerate() {
            let k_dd = Dd::from_f64((k_start + idx) as f64);
            let sample = kaiser_sinc_dd(k_dd, fc_norm_dd, center_dd, beta_dd, inv_i0_beta);
            *slot = ratio_dd.mul(sample).to_f64();
        }
    };

    if center >= 16_384 {
        let workers = std::thread::available_parallelism()
            .map(|p| p.get().clamp(2, 8))
            .unwrap_or(4);
        let chunk_len = center.div_ceil(workers);
        std::thread::scope(|s| {
            for (ci, chunk) in half_vals[1..].chunks_mut(chunk_len).enumerate() {
                let k_start = 1 + ci * chunk_len;
                s.spawn(move || eval_slice(chunk, k_start));
            }
        });
    } else {
        eval_slice(&mut half_vals[1..], 1);
    }

    let mut h_raw = vec![0.0_f64; n];
    h_raw[center] = half_vals[0];
    for k in 1..=center {
        let v = half_vals[k];
        h_raw[center - k] = v;
        h_raw[center + k] = v;
    }

    let (prototype, bank, taps_per_phase, m_half) =
        normalize_symmetric_prototype_to_bank(&h_raw, ratio, center);

    let l1_norm = bank
        .iter()
        .map(|phase| phase.iter().map(|c| c.abs()).sum::<f64>())
        .fold(0.0_f64, f64::max);
    let l1_norm_db = 20.0 * l1_norm.max(1e-30).log10();

    let level_at_20khz_db = if spec.source_rate_hz > 40_000 {
        let mag = evaluate_symmetric_prototype_mag(&prototype, fs_out, ratio, 20_000.0);
        Some(20.0 * mag.max(1e-30).log10())
    } else {
        None
    };

    let length_seconds = ((n - 1) as f64) / fs_out;
    let metrics = FilterDesignMetrics {
        phase: FilterPhase::Linear,
        num_taps: n,
        taps_per_phase,
        length_seconds,
        group_delay_seconds: 0.5 * length_seconds,
        passband_edge_001db_hz: spec.passband_edge_001db_hz,
        level_at_20khz_db,
        stopband_edge_hz: spec.stopband_edge_hz,
        stopband_attenuation_db: spec.attenuation_db,
        l1_norm,
        l1_norm_db,
    };

    let identity = FilterIdentity {
        archetype: spec.archetype,
        source_rate_hz: spec.source_rate_hz,
        target_rate_hz: spec.target_rate_hz,
        ratio,
        num_taps: n,
        taps_per_phase,
        fc_hz_bits: spec.fc_hz.to_bits(),
        sigma_f_hz_bits: spec.beta.to_bits(),
        effective_nyquist_hz: spec.effective_nyquist_hz,
    };

    DesignedFilter {
        name: spec.archetype.cli_name(),
        source_rate_hz: spec.source_rate_hz,
        target_rate_hz: spec.target_rate_hz,
        ratio,
        prototype,
        bank: Arc::new(bank),
        taps_per_phase,
        pad_lead: m_half,
        pad_tail: m_half,
        metrics,
        identity,
    }
}

/// Design a cepstrally phase-converted filter (`#6 minphase-long`, `#7 intermediate-long`, `#8b short-mp`)
/// from a linear-phase Gaussian base prototype with exact double-double per-phase DC normalization.
pub fn design_phase_converted_filter(
    archetype: FilterArchetype,
    base: &DesignedGaussianFilter,
    phase: FilterPhase,
    alpha: f64,
) -> Result<DesignedFilter, String> {
    let ratio = base.spec.ratio;
    let n = base.prototype.len();
    let (proto_raw, peak_idx) = convert_prototype_phase(&base.prototype, alpha)?;

    // Decompose causal prototype h[0..N] into polyphase bank:
    //   bank[p][i] = h[(m - 1 - i) * ratio + p]
    // so an impulse at x[0] produces exact causal impulse response h[0..N].
    let m = n.div_ceil(ratio);
    let mut bank = vec![vec![0.0_f64; m]; ratio];
    for (p, branch) in bank.iter_mut().enumerate() {
        for (i, slot) in branch.iter_mut().enumerate() {
            let u = m - 1 - i;
            let idx = u * ratio + p;
            if idx < n {
                *slot = proto_raw[idx];
            }
        }
        // Per-phase exact double-double DC normalization (`sum(bank[p]) == 1.0`).
        let sum_p = dd_sum_f64(branch);
        if sum_p.hi.abs() > 1e-15 {
            let inv_p = DD_ONE.div(sum_p);
            for c in branch.iter_mut() {
                *c = Dd::from_f64(*c).mul(inv_p).to_f64();
            }
            // Fold remaining <1 ULP residual onto the largest-magnitude tap in this phase.
            let rem = DD_ONE.sub(dd_sum_f64(branch)).to_f64();
            if let Some((max_i, _)) = branch
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            {
                branch[max_i] += rem;
            }
        }
    }

    // Reconstruct normalized causal prototype `prototype[0..N]` matching `bank` bit-for-bit.
    let mut prototype = vec![0.0_f64; n];
    for (p, branch) in bank.iter().enumerate() {
        for (i, &c) in branch.iter().enumerate() {
            let u = m - 1 - i;
            let idx = u * ratio + p;
            if idx < n {
                prototype[idx] = c;
            }
        }
    }

    let (pad_lead, pad_tail, gd_samples) = match phase {
        FilterPhase::Minimum => (m.saturating_sub(1), 0, peak_idx),
        FilterPhase::Intermediate => {
            let gd_in = (peak_idx / ratio).min(m.saturating_sub(1));
            (m.saturating_sub(1).saturating_sub(gd_in), gd_in, peak_idx)
        }
        FilterPhase::Linear => {
            let half = m.saturating_sub(1) / 2;
            (half, m.saturating_sub(1) - half, (n - 1) / 2)
        }
    };

    let l1_norm = bank
        .iter()
        .map(|br| br.iter().map(|c| c.abs()).sum::<f64>())
        .fold(0.0_f64, f64::max);
    let l1_norm_db = 20.0 * l1_norm.max(1e-30).log10();
    let fs_out = base.spec.target_rate_hz as f64;
    let level_at_20khz_db = if base.spec.source_rate_hz > 40_000 {
        let mag = evaluate_prototype_mag(&prototype, fs_out, ratio, 20_000.0);
        Some(20.0 * mag.max(1e-30).log10())
    } else {
        None
    };

    let metrics = FilterDesignMetrics {
        phase,
        num_taps: n,
        taps_per_phase: m,
        length_seconds: ((n - 1) as f64) / fs_out,
        group_delay_seconds: (gd_samples as f64) / fs_out,
        passband_edge_001db_hz: base.metrics.passband_edge_001db_hz,
        level_at_20khz_db,
        stopband_edge_hz: base.metrics.stopband_edge_hz,
        stopband_attenuation_db: base.metrics.stopband_attenuation_db,
        l1_norm,
        l1_norm_db,
    };

    let identity = FilterIdentity {
        archetype,
        source_rate_hz: base.spec.source_rate_hz,
        target_rate_hz: base.spec.target_rate_hz,
        ratio,
        num_taps: n,
        taps_per_phase: m,
        fc_hz_bits: base.spec.fc_hz.to_bits(),
        sigma_f_hz_bits: alpha.to_bits(),
        effective_nyquist_hz: base.spec.effective_nyquist_hz,
    };

    Ok(DesignedFilter {
        name: archetype.cli_name(),
        source_rate_hz: base.spec.source_rate_hz,
        target_rate_hz: base.spec.target_rate_hz,
        ratio,
        prototype,
        bank: Arc::new(bank),
        taps_per_phase: m,
        pad_lead,
        pad_tail,
        metrics,
        identity,
    })
}

/// Concurrent-deduplicating LRU cache (`Review #7 F15`):
/// - Multiple threads requesting the same uncached `K` share one `OnceLock` slot, so the
///   expensive filter synthesis runs strictly ONCE.
/// - Failed builds remove their slot so a subsequent retry can succeed.
/// - Eviction uses true LRU order (evicting the oldest entry not actively shared externally, or
///   the oldest entry when all are shared) rather than wiping all hot cached kernels.
struct LruOnceCache<K, V> {
    capacity: usize,
    entries: HashMap<K, Arc<OnceLock<Result<Arc<V>, String>>>>,
    order: VecDeque<K>,
}

impl<K: Eq + std::hash::Hash + Copy, V> LruOnceCache<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::with_capacity(capacity.max(1)),
            order: VecDeque::with_capacity(capacity.max(1)),
        }
    }

    fn touch(&mut self, key: K) {
        if let Some(pos) = self.order.iter().position(|k| *k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key);
    }

    fn evict_if_needed(&mut self, keep_key: K) {
        while self.entries.len() > self.capacity {
            // Prefer evicting the oldest entry whose cached `Arc<V>` has no external borrowers.
            let victim_pos = self
                .order
                .iter()
                .position(|k| {
                    *k != keep_key
                        && self.entries.get(k).is_some_and(|slot| match slot.get() {
                            Some(Ok(arc)) => Arc::strong_count(arc) == 1,
                            _ => false,
                        })
                })
                .or_else(|| self.order.iter().position(|k| *k != keep_key));
            let Some(pos) = victim_pos else {
                break;
            };
            if let Some(victim) = self.order.remove(pos) {
                self.entries.remove(&victim);
            }
        }
    }
}

fn lru_cache_get_or_init<K, V>(
    cache: &Mutex<LruOnceCache<K, V>>,
    key: K,
    build_fn: impl FnOnce() -> Result<V, String>,
) -> Result<Arc<V>, String>
where
    K: Eq + std::hash::Hash + Copy,
{
    let slot = {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.entries.get(&key).cloned() {
            guard.touch(key);
            existing
        } else {
            let fresh = Arc::new(OnceLock::new());
            guard.entries.insert(key, Arc::clone(&fresh));
            guard.touch(key);
            guard.evict_if_needed(key);
            fresh
        }
    };

    let res = slot.get_or_init(|| build_fn().map(Arc::new));
    match res {
        Ok(arc) => Ok(Arc::clone(arc)),
        Err(err) => {
            let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
            if guard
                .entries
                .get(&key)
                .is_some_and(|cur| Arc::ptr_eq(cur, &slot))
            {
                guard.entries.remove(&key);
                if let Some(pos) = guard.order.iter().position(|k| *k == key) {
                    guard.order.remove(pos);
                }
            }
            Err(err.clone())
        }
    }
}

fn fft_kernel_cache() -> &'static Mutex<LruOnceCache<FilterIdentity, PreparedUpsamplerKernel>> {
    static CACHE: OnceLock<Mutex<LruOnceCache<FilterIdentity, PreparedUpsamplerKernel>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LruOnceCache::new(MAX_CACHED_KERNELS)))
}

fn polyphase_filter_cache() -> &'static Mutex<LruOnceCache<FilterIdentity, DesignedFilter>> {
    static CACHE: OnceLock<Mutex<LruOnceCache<FilterIdentity, DesignedFilter>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LruOnceCache::new(MAX_CACHED_KERNELS)))
}

fn get_or_prepare_fft_kernel(
    identity: FilterIdentity,
    build_fn: impl FnOnce() -> Result<DesignedFilter, String>,
) -> Result<Arc<PreparedUpsamplerKernel>, String> {
    lru_cache_get_or_init(fft_kernel_cache(), identity, || {
        let designed = build_fn()?;
        PreparedUpsamplerKernel::from_polyphase_bank(
            designed.name,
            designed.source_rate_hz,
            designed.target_rate_hz,
            designed.ratio,
            designed.taps_per_phase,
            designed.pad_lead,
            designed.pad_tail,
            designed.bank,
            designed.metrics,
            designed.identity,
        )
    })
}

fn get_or_design_polyphase(
    identity: FilterIdentity,
    build_fn: impl FnOnce() -> Result<DesignedFilter, String>,
) -> Result<Arc<DesignedFilter>, String> {
    lru_cache_get_or_init(polyphase_filter_cache(), identity, || {
        let mut designed = build_fn()?;
        // Discard unused flat prototype vector before caching (`PolyphaseFirUpsampler` uses `bank`).
        designed.prototype = Vec::new();
        Ok(designed)
    })
}

fn build_from_designed_cached(
    identity: FilterIdentity,
    taps_per_phase: usize,
    build_fn: impl FnOnce() -> Result<DesignedFilter, String>,
) -> Result<Box<dyn Upsampler>, String> {
    if taps_per_phase > DIRECT_POLYPHASE_MAX_TAPS_PER_PHASE {
        let kernel = get_or_prepare_fft_kernel(identity, build_fn)?;
        Ok(Box::new(FftUpolsUpsampler::new(kernel)))
    } else {
        let designed = get_or_design_polyphase(identity, build_fn)?;
        Ok(Box::new(PolyphaseFirUpsampler::from_designed(&designed)))
    }
}

/// Build the Gaussian prototype spec for a plan, honoring `hf_noise_filter` and gracefully
/// ignoring `effective_nyquist_hz` values that fall below the DR-1 floor (Review #7 F11).
/// Shared by `GaussLongLp`, `GaussHiresLp`, `MinphaseLong`, and `IntermediateLong`.
fn gauss_spec_for_plan(
    archetype: FilterArchetype,
    plan: &SelectedDspPlan,
) -> Result<GaussianFilterSpec, String> {
    if archetype == FilterArchetype::GaussHiresLp && plan.hf_noise_filter {
        return hires_hf_noise_filter_spec(plan.source_rate_hz, plan.target_rate_hz);
    }
    if let Some(f_eff) = plan.effective_nyquist_hz {
        let fs_in = plan.source_rate_hz as f64;
        let f_stop = (f_eff as f64).min(0.5 * fs_in);
        let min_ok = plan.source_rate_hz < DR1_MIN_SOURCE_RATE_HZ
            || (f_eff as f64) > DR1_MIN_PASSBAND_HZ * 1.02;
        if min_ok {
            let f_pass = (f_stop * (20_000.0 / 22_050.0)).max(
                if plan.source_rate_hz >= DR1_MIN_SOURCE_RATE_HZ {
                    DR1_MIN_PASSBAND_HZ
                } else {
                    f_stop * 0.85
                },
            );
            if f_stop > f_pass + 1.0 {
                if let Ok(spec) = GaussianFilterSpec::from_pass_stop(
                    archetype,
                    plan.source_rate_hz,
                    plan.target_rate_hz,
                    f_pass,
                    f_stop,
                    Some(f_eff),
                ) {
                    return Ok(spec);
                }
            }
        }
    }
    GaussianFilterSpec::for_default_archetype(archetype, plan.source_rate_hz, plan.target_rate_hz)
}

fn require_integer_ratio(plan: &SelectedDspPlan) -> Result<Option<u32>, String> {
    let ratio_u32 = match plan.ratio {
        UpsampleRatio::Bypass1x => return Ok(None),
        UpsampleRatio::Integer(r) if r >= 2 => r,
        UpsampleRatio::Integer(_) => return Ok(None),
        UpsampleRatio::Arbitrary => {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {} upsampler requires an exact integer \
                 ratio to 352.8/384 kHz (source rate {} Hz is non-integer)",
                plan.filter, plan.source_rate_hz
            ));
        }
    };
    if ratio_u32 > MAX_UPSAMPLE_RATIO {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} integer upsampling ratio {}x exceeds \
             hard cap {}x (source rate {} Hz)",
            ratio_u32, MAX_UPSAMPLE_RATIO, plan.source_rate_hz
        ));
    }
    Ok(Some(ratio_u32))
}

fn designed_from_gaussian(g: DesignedGaussianFilter) -> DesignedFilter {
    DesignedFilter {
        name: g.spec.archetype.cli_name(),
        source_rate_hz: g.spec.source_rate_hz,
        target_rate_hz: g.spec.target_rate_hz,
        ratio: g.spec.ratio,
        prototype: g.prototype,
        bank: g.bank,
        taps_per_phase: g.taps_per_phase,
        pad_lead: g.m_half,
        pad_tail: g.m_half,
        metrics: g.metrics,
        identity: g.identity,
    }
}

/// Build the concrete stateful floating-point [`Upsampler`] for a [`SelectedDspPlan`].
pub fn build_upsampler(plan: &SelectedDspPlan) -> Result<Box<dyn Upsampler>, String> {
    if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ)
        .contains(&plan.source_rate_hz)
    {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {} Hz is outside the supported {}..={} Hz range",
            plan.source_rate_hz, MIN_SUPPORTED_SOURCE_RATE_HZ, MAX_SUPPORTED_SOURCE_RATE_HZ
        ));
    }

    let cap = plan.max_taps.min(MAX_FILTER_TAPS);

    match plan.filter {
        FilterArchetype::Null => Ok(Box::new(NullUpsampler::new(plan.target_rate_hz))),
        FilterArchetype::GaussLongLp | FilterArchetype::GaussHiresLp => {
            if require_integer_ratio(plan)?.is_none() {
                return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
            }
            let spec = gauss_spec_for_plan(plan.filter, plan)?;
            if spec.num_taps > cap {
                return Err(format!(
                    "upsampler '{}' requires {} taps which exceeds --max-taps ({cap})",
                    plan.filter, spec.num_taps
                ));
            }
            let m_half = spec.half_span.div_ceil(spec.ratio);
            let m = 2 * m_half + 1;
            let id = FilterIdentity {
                archetype: spec.archetype,
                source_rate_hz: spec.source_rate_hz,
                target_rate_hz: spec.target_rate_hz,
                ratio: spec.ratio,
                num_taps: spec.num_taps,
                taps_per_phase: m,
                fc_hz_bits: spec.fc_hz.to_bits(),
                sigma_f_hz_bits: spec.sigma_f_hz.to_bits(),
                effective_nyquist_hz: spec.effective_nyquist_hz,
            };
            build_from_designed_cached(id, m, || Ok(designed_from_gaussian(design_gaussian_filter(spec))))
        }
        FilterArchetype::GaussApod4M => {
            if require_integer_ratio(plan)?.is_none() {
                return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
            }
            let spec = gauss_apod_4m_spec(
                plan.source_rate_hz,
                plan.target_rate_hz,
                cap,
                plan.effective_nyquist_hz,
            )?;
            let m_half = spec.half_span.div_ceil(spec.ratio);
            let m = 2 * m_half + 1;
            let id = FilterIdentity {
                archetype: spec.archetype,
                source_rate_hz: spec.source_rate_hz,
                target_rate_hz: spec.target_rate_hz,
                ratio: spec.ratio,
                num_taps: spec.num_taps,
                taps_per_phase: m,
                fc_hz_bits: spec.fc_hz.to_bits(),
                sigma_f_hz_bits: spec.sigma_f_hz.to_bits(),
                effective_nyquist_hz: spec.effective_nyquist_hz,
            };
            build_from_designed_cached(id, m, || Ok(designed_from_gaussian(design_gaussian_filter(spec))))
        }
        FilterArchetype::SincSteep4M | FilterArchetype::ExtSharpLp => {
            if require_integer_ratio(plan)?.is_none() {
                return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
            }
            let spec = KaiserFilterSpec::for_archetype(
                plan.filter,
                plan.source_rate_hz,
                plan.target_rate_hz,
                cap,
                plan.effective_nyquist_hz,
            )?;
            let m = 2 * spec.m_half + 1;
            let id = FilterIdentity {
                archetype: spec.archetype,
                source_rate_hz: spec.source_rate_hz,
                target_rate_hz: spec.target_rate_hz,
                ratio: spec.ratio,
                num_taps: spec.num_taps,
                taps_per_phase: m,
                fc_hz_bits: spec.fc_hz.to_bits(),
                sigma_f_hz_bits: spec.beta.to_bits(),
                effective_nyquist_hz: spec.effective_nyquist_hz,
            };
            build_from_designed_cached(id, m, || Ok(design_kaiser_filter(spec)))
        }
        FilterArchetype::ShortLp => {
            if require_integer_ratio(plan)?.is_none() {
                return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
            }
            let (spec, atten_db, _) = short_gaussian_spec(
                FilterArchetype::ShortLp,
                plan.source_rate_hz,
                plan.target_rate_hz,
                plan.effective_nyquist_hz,
            )?;
            if spec.num_taps > cap {
                return Err(format!(
                    "upsampler 'short-lp' requires {} taps which exceeds --max-taps ({cap})",
                    spec.num_taps
                ));
            }
            let m_half = spec.half_span.div_ceil(spec.ratio);
            let m = 2 * m_half + 1;
            let id = FilterIdentity {
                archetype: spec.archetype,
                source_rate_hz: spec.source_rate_hz,
                target_rate_hz: spec.target_rate_hz,
                ratio: spec.ratio,
                num_taps: spec.num_taps,
                taps_per_phase: m,
                fc_hz_bits: spec.fc_hz.to_bits(),
                sigma_f_hz_bits: spec.sigma_f_hz.to_bits(),
                effective_nyquist_hz: spec.effective_nyquist_hz,
            };
            build_from_designed_cached(id, m, || {
                Ok(designed_from_gaussian(
                    design_gaussian_filter_with_attenuation(spec, atten_db),
                ))
            })
        }
        FilterArchetype::MinphaseLong | FilterArchetype::IntermediateLong | FilterArchetype::ShortMp => {
            if require_integer_ratio(plan)?.is_none() {
                return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
            }
            let (base_spec, atten_db) = if plan.filter == FilterArchetype::ShortMp {
                let (s, a, _) = short_gaussian_spec(
                    FilterArchetype::ShortLp,
                    plan.source_rate_hz,
                    plan.target_rate_hz,
                    plan.effective_nyquist_hz,
                )?;
                (s, a)
            } else {
                let ref_arch = if plan.source_rate_hz <= 48_000 {
                    FilterArchetype::GaussLongLp
                } else {
                    FilterArchetype::GaussHiresLp
                };
                let s = gauss_spec_for_plan(ref_arch, plan)?;
                (s, GAUSS_STOPBAND_ATTENUATION_DB)
            };
            if base_spec.num_taps > cap {
                return Err(format!(
                    "upsampler '{}' requires {} taps which exceeds --max-taps ({cap})",
                    plan.filter, base_spec.num_taps
                ));
            }
            let (phase, alpha) = match plan.filter {
                FilterArchetype::IntermediateLong => (FilterPhase::Intermediate, INTERMEDIATE_PHASE_ALPHA),
                _ => (FilterPhase::Minimum, 1.0),
            };
            let m = base_spec.num_taps.div_ceil(base_spec.ratio);
            let id = FilterIdentity {
                archetype: plan.filter,
                source_rate_hz: base_spec.source_rate_hz,
                target_rate_hz: base_spec.target_rate_hz,
                ratio: base_spec.ratio,
                num_taps: base_spec.num_taps,
                taps_per_phase: m,
                fc_hz_bits: base_spec.fc_hz.to_bits(),
                sigma_f_hz_bits: alpha.to_bits(),
                effective_nyquist_hz: base_spec.effective_nyquist_hz,
            };
            build_from_designed_cached(id, m, || {
                let base = design_gaussian_filter_with_attenuation(base_spec, atten_db);
                design_phase_converted_filter(plan.filter, &base, phase, alpha)
            })
        }
        FilterArchetype::SimpleTest => {
            let Some(ratio_u32) = require_integer_ratio(plan)? else {
                return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
            };
            let ratio = ratio_u32 as usize;
            let taps_per_phase = simple_test_taps_per_phase(ratio, plan.tap_budget).ok_or_else(|| {
                format!(
                    "--max-taps budget is too small for {ratio}x simple-test upsampling \
                     (minimum is {} taps = {SIMPLE_TEST_MIN_TAPS_PER_PHASE} taps/phase)",
                    ratio * SIMPLE_TEST_MIN_TAPS_PER_PHASE
                )
            })?;
            let total_taps = ratio.checked_mul(taps_per_phase).ok_or_else(|| {
                format!(
                    "upsampler tap count overflow (ratio={ratio}, taps_per_phase={taps_per_phase})"
                )
            })?;
            if total_taps > MAX_FILTER_TAPS {
                return Err(format!(
                    "upsampler total taps ({total_taps}) exceeds hard {MAX_FILTER_TAPS}-tap ceiling"
                ));
            }
            Ok(Box::new(SimpleWindowedSincUpsampler::new(
                plan.source_rate_hz,
                ratio,
                taps_per_phase,
            )))
        }
    }
}

/// Core polyphase direct-form convolution helper shared by [`PolyphaseFirUpsampler`],
/// [`SimpleWindowedSincUpsampler`], and [`UpsamplerCarryover::flush_to_f64`].
///
/// Accumulates taps from the outside in (smallest tail taps first, largest center taps last)
/// so small tail contributions are never drowned out by the main lobe — achieving `< 5e-16`
/// DC accuracy across long alternating-sign sinc branches.
fn convolve_polyphase_bank(
    bank: &[Vec<f64>],
    ratio: usize,
    m: usize,
    hist_l: &mut Vec<f64>,
    hist_r: &mut Vec<f64>,
) -> (Vec<f64>, Vec<f64>) {
    let available = hist_l.len();
    if available < m {
        return (Vec::new(), Vec::new());
    }
    let n_windows = available - m + 1;
    let out_len = n_windows * ratio;
    let mut out_l = vec![0.0_f64; out_len];
    let mut out_r = vec![0.0_f64; out_len];
    let half = m / 2;
    let is_odd = (m & 1) == 1;

    for n in 0..n_windows {
        let wl = &hist_l[n..n + m];
        let wr = &hist_r[n..n + m];
        let base = n * ratio;
        for (phase, coeffs) in bank.iter().enumerate() {
            let coeffs = &coeffs[..m];
            let mut acc_l = 0.0_f64;
            let mut acc_r = 0.0_f64;
            for i in 0..half {
                let j = m - 1 - i;
                let ci = coeffs[i];
                let cj = coeffs[j];
                acc_l += wl[i] * ci + wl[j] * cj;
                acc_r += wr[i] * ci + wr[j] * cj;
            }
            if is_odd {
                let cm = coeffs[half];
                acc_l += wl[half] * cm;
                acc_r += wr[half] * cm;
            }
            out_l[base + phase] = acc_l;
            out_r[base + phase] = acc_r;
        }
    }

    hist_l.drain(0..n_windows);
    hist_r.drain(0..n_windows);
    (out_l, out_r)
}

/// Null 1:1 stateful floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`, zero copy).
///
/// Native `352.8 / 384 kHz` FLAC blocks (or live 8fS streams) pass through in `O(1)` with
/// zero reallocation so downstream `f64` stages (REW convolution, delay, quantization) always
/// operate uniformly on pure floating-point blocks.
#[derive(Debug)]
pub struct NullUpsampler {
    sample_rate_hz: u32,
    flushed: bool,
}

impl NullUpsampler {
    pub fn new(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz,
            flushed: false,
        }
    }
}

impl Upsampler for NullUpsampler {
    fn name(&self) -> &'static str {
        "null"
    }

    fn total_taps(&self) -> usize {
        0
    }

    fn identity(&self) -> FilterIdentity {
        FilterIdentity {
            archetype: FilterArchetype::Null,
            source_rate_hz: self.sample_rate_hz,
            target_rate_hz: self.sample_rate_hz,
            ratio: 1,
            num_taps: 0,
            taps_per_phase: 0,
            fc_hz_bits: 0,
            sigma_f_hz_bits: 0,
            effective_nyquist_hz: None,
        }
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if self.flushed {
            return Err("null upsampler cannot accept blocks after flush()".to_string());
        }
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        if input.sample_rate_hz != self.sample_rate_hz {
            return Err(format!(
                "null upsampler sample rate mismatch: expected {} Hz, got {} Hz",
                self.sample_rate_hz, input.sample_rate_hz
            ));
        }
        Ok(input)
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        self.flushed = true;
        Ok(StereoF64Buffer::empty(self.sample_rate_hz))
    }
}

/// Stateful odd-length direct-form polyphase FIR `f64` upsampler (Phase 1 default Gaussian
/// executor for `gauss-long-lp` and `gauss-hires-lp`).
///
/// Properties:
/// - Exact odd prototype `N = 2 * half_span + 1` -> exact integer group delay and zero shift.
/// - Leading pad `m_half` + trailing pad `m_half` -> exact `total_in_frames * ratio` output samples.
/// - Streaming chunk invariance and gapless splice invariance bit-for-bit identical to one-shot.
/// - Strict [`FilterIdentity`] validation on [`Upsampler::set_incoming_carryover`].
#[derive(Debug)]
pub struct PolyphaseFirUpsampler {
    name: &'static str,
    source_rate_hz: u32,
    target_rate_hz: u32,
    ratio: usize,
    taps_per_phase: usize,
    total_taps: usize,
    pad_tail: usize,
    bank: Arc<Vec<Vec<f64>>>,
    metrics: Option<FilterDesignMetrics>,
    identity: FilterIdentity,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    total_in_frames: usize,
    has_incoming_carryover: bool,
    flushed: bool,
}

impl PolyphaseFirUpsampler {
    pub fn from_gaussian(designed: DesignedGaussianFilter) -> Self {
        Self::from_designed(&designed_from_gaussian(designed))
    }

    pub fn from_designed(designed: &DesignedFilter) -> Self {
        let lead = designed.pad_lead;
        Self {
            name: designed.name,
            source_rate_hz: designed.source_rate_hz,
            target_rate_hz: designed.target_rate_hz,
            ratio: designed.ratio,
            taps_per_phase: designed.taps_per_phase,
            total_taps: designed.metrics.num_taps,
            pad_tail: designed.pad_tail,
            bank: Arc::clone(&designed.bank),
            metrics: Some(designed.metrics),
            identity: designed.identity,
            hist_l: vec![0.0_f64; lead],
            hist_r: vec![0.0_f64; lead],
            total_in_frames: 0,
            has_incoming_carryover: false,
            flushed: false,
        }
    }

    /// Expose polyphase coefficient bank for mathematical symmetry & response inspection.
    pub fn polyphase_bank(&self) -> &[Vec<f64>] {
        &self.bank
    }
}

impl Upsampler for PolyphaseFirUpsampler {
    fn name(&self) -> &'static str {
        self.name
    }

    fn total_taps(&self) -> usize {
        self.total_taps
    }

    fn latency_samples(&self) -> usize {
        self.pad_tail * self.ratio
    }

    #[cfg(any(test, feature = "verify"))]
    fn verify_export(&self) -> Option<UpsamplerExport> {
        let pad_lead = self
            .taps_per_phase
            .saturating_sub(1)
            .saturating_sub(self.pad_tail);
        Some(UpsamplerExport {
            ratio: self.ratio,
            taps_per_phase: self.taps_per_phase,
            pad_lead,
            pad_tail: self.pad_tail,
            bank: Arc::clone(&self.bank),
        })
    }

    fn design_metrics(&self) -> Option<FilterDesignMetrics> {
        self.metrics
    }

    fn identity(&self) -> FilterIdentity {
        self.identity
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if self.flushed {
            return Err(format!(
                "{} upsampler cannot accept blocks after flush()",
                self.name
            ));
        }
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        if input.sample_rate_hz != self.source_rate_hz {
            return Err(format!(
                "{} upsampler sample rate mismatch: expected {} Hz, got {} Hz",
                self.name, self.source_rate_hz, input.sample_rate_hz
            ));
        }
        if input.is_empty() {
            return Ok(StereoF64Buffer::empty(self.target_rate_hz));
        }

        self.total_in_frames += input.left.len();
        self.hist_l.extend_from_slice(&input.left);
        self.hist_r.extend_from_slice(&input.right);

        let (left, right) = convolve_polyphase_bank(
            &self.bank,
            self.ratio,
            self.taps_per_phase,
            &mut self.hist_l,
            &mut self.hist_r,
        );
        Ok(StereoF64Buffer {
            sample_rate_hz: self.target_rate_hz,
            left,
            right,
        })
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok(StereoF64Buffer::empty(self.target_rate_hz));
        }
        self.flushed = true;

        // Append `pad_tail` trailing zeros (`pad_lead + pad_tail = m - 1`
        // -> exact `total_in_frames` windows -> exact `total_in_frames * ratio` output samples).
        if self.pad_tail > 0 {
            self.hist_l.resize(self.hist_l.len() + self.pad_tail, 0.0_f64);
            self.hist_r.resize(self.hist_r.len() + self.pad_tail, 0.0_f64);
        }

        let (left, right) = convolve_polyphase_bank(
            &self.bank,
            self.ratio,
            self.taps_per_phase,
            &mut self.hist_l,
            &mut self.hist_r,
        );
        self.hist_l.clear();
        self.hist_r.clear();
        Ok(StereoF64Buffer {
            sample_rate_hz: self.target_rate_hz,
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
            || carryover.identity != self.identity
            || carryover.source_rate_hz != self.source_rate_hz
            || carryover.ratio != self.ratio
            || carryover.taps_per_phase != self.taps_per_phase
            || carryover.hist_l.len() != carryover.hist_r.len()
            || carryover.hist_l.len() >= self.taps_per_phase
        {
            return Err(carryover);
        }
        self.hist_l = carryover.hist_l;
        self.hist_r = carryover.hist_r;
        self.has_incoming_carryover = true;
        Ok(())
    }

    fn flush_for_gapless(&mut self) -> Result<(StereoF64Buffer, Option<UpsamplerCarryover>), String> {
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok((StereoF64Buffer::empty(self.target_rate_hz), None));
        }
        self.flushed = true;
        let hist_l = core::mem::take(&mut self.hist_l);
        let hist_r = core::mem::take(&mut self.hist_r);
        if hist_l.is_empty() {
            return Ok((StereoF64Buffer::empty(self.target_rate_hz), None));
        }
        Ok((
            StereoF64Buffer::empty(self.target_rate_hz),
            Some(UpsamplerCarryover {
                identity: self.identity,
                source_rate_hz: self.source_rate_hz,
                target_rate_hz: self.target_rate_hz,
                ratio: self.ratio,
                taps_per_phase: self.taps_per_phase,
                pad_tail: self.pad_tail,
                bank: Arc::clone(&self.bank),
                hist_l,
                hist_r,
            }),
        ))
    }
}

/// Legacy test-only stateful polyphase Kaiser-windowed sinc `f64` upsampler (`--filter simple-test`).
///
/// Review #9 D8: delegates directly to [`PolyphaseFirUpsampler`] rather than duplicating its
/// streaming/gapless state machine.
#[derive(Debug)]
pub struct SimpleWindowedSincUpsampler(PolyphaseFirUpsampler);

impl SimpleWindowedSincUpsampler {
    pub fn new(source_rate_hz: u32, ratio: usize, taps_per_phase: usize) -> Self {
        let m = taps_per_phase.max(8) & !1; // enforce even >= 8
        let half = m / 2;
        let bank = Arc::new(build_polyphase_bank(ratio, m));
        let (cutoff, beta) = kaiser_params_for_taps_per_phase(m);
        let target_rate_hz = source_rate_hz.saturating_mul(ratio as u32);
        let identity = FilterIdentity {
            archetype: FilterArchetype::SimpleTest,
            source_rate_hz,
            target_rate_hz,
            ratio,
            num_taps: ratio * m,
            taps_per_phase: m,
            fc_hz_bits: cutoff.to_bits(),
            sigma_f_hz_bits: beta.to_bits(),
            effective_nyquist_hz: None,
        };
        // Pre-seed history with `half - 1` leading zeros (`pad_tail = half`, so
        // `(half - 1) + half = m - 1` -> exact `total_in_frames * ratio` output samples).
        let lead = half - 1;
        Self(PolyphaseFirUpsampler {
            name: "simple-test",
            source_rate_hz,
            target_rate_hz,
            ratio,
            taps_per_phase: m,
            total_taps: ratio * m,
            pad_tail: half,
            bank,
            metrics: None,
            identity,
            hist_l: vec![0.0_f64; lead],
            hist_r: vec![0.0_f64; lead],
            total_in_frames: 0,
            has_incoming_carryover: false,
            flushed: false,
        })
    }

    /// Expose polyphase coefficient bank for mathematical symmetry & response inspection.
    pub fn polyphase_bank(&self) -> &[Vec<f64>] {
        self.0.polyphase_bank()
    }
}

impl Upsampler for SimpleWindowedSincUpsampler {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn total_taps(&self) -> usize {
        self.0.total_taps()
    }
    fn latency_samples(&self) -> usize {
        self.0.latency_samples()
    }
    #[cfg(any(test, feature = "verify"))]
    fn verify_export(&self) -> Option<UpsamplerExport> {
        self.0.verify_export()
    }
    fn design_metrics(&self) -> Option<FilterDesignMetrics> {
        self.0.design_metrics()
    }
    fn identity(&self) -> FilterIdentity {
        self.0.identity()
    }
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        self.0.process_block(input)
    }
    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        self.0.flush()
    }
    fn set_incoming_carryover(
        &mut self,
        carryover: UpsamplerCarryover,
    ) -> Result<(), UpsamplerCarryover> {
        self.0.set_incoming_carryover(carryover)
    }
    fn flush_for_gapless(&mut self) -> Result<(StereoF64Buffer, Option<UpsamplerCarryover>), String> {
        self.0.flush_for_gapless()
    }
}

/// Zeroth-order modified Bessel function of the first kind `I_0(x)` computed in pure `f64`
/// to full machine precision (`~1e-16` relative accuracy; shared with `analysis.rs`, D7).
pub(crate) fn bessel_i0_f64(x: f64) -> f64 {
    let half_x = 0.5 * x;
    let y = half_x * half_x;
    let mut sum = 1.0_f64;
    let mut term = 1.0_f64;
    let mut k = 1.0_f64;
    while term > sum * 1e-16 {
        term *= y / (k * k);
        sum += term;
        k += 1.0;
        if k > 200.0 {
            break;
        }
    }
    sum
}

/// Choose Kaiser `(cutoff, beta)` for a given taps-per-phase length `m` (legacy `SimpleTest`).
fn kaiser_params_for_taps_per_phase(m: usize) -> (f64, f64) {
    if m >= SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE {
        return (0.955_f64, 12.0_f64);
    }
    let m_f = (m.max(SIMPLE_TEST_MIN_TAPS_PER_PHASE)) as f64;
    let a_db = (18.0 + 0.85 * m_f).clamp(28.0, 118.0);
    let beta = if a_db > 50.0 {
        0.1102 * (a_db - 8.7)
    } else if a_db >= 21.0 {
        0.5842 * (a_db - 21.0).powf(0.4) + 0.07886 * (a_db - 21.0)
    } else {
        0.0
    };
    let half_tw_nyq = ((a_db - 8.0) / (7.1785 * m_f)) * 0.5;
    let cutoff = (0.91 + half_tw_nyq).clamp(0.955, 0.995);
    (cutoff, beta)
}

/// Build an exact linear-phase Kaiser-windowed sinc polyphase bank (`ratio` phases x `m` taps)
/// for legacy `SimpleTest`.
fn build_polyphase_bank(ratio: usize, m: usize) -> Vec<Vec<f64>> {
    let half = (m / 2) as isize;
    let (cutoff, beta) = kaiser_params_for_taps_per_phase(m);
    let inv_i0_beta = 1.0 / bessel_i0_f64(beta);
    let radius = (m as f64) * 0.5;

    let mut bank = Vec::with_capacity(ratio);
    for phase in 0..ratio {
        let frac = (phase as f64) / (ratio as f64);
        let mut weights = Vec::with_capacity(m);

        for k in 0..m {
            let j = (k as isize) - half + 1;
            let dt = (j as f64) - frac;
            let sinc = if dt.abs() < 1e-12 {
                cutoff
            } else {
                let arg = PI * cutoff * dt;
                arg.sin() / (PI * dt)
            };
            let win = if dt.abs() < radius {
                let x = dt / radius;
                let arg = beta * (1.0 - x * x).max(0.0).sqrt();
                bessel_i0_f64(arg) * inv_i0_beta
            } else {
                0.0
            };
            weights.push(sinc * win);
        }

        let exact_sum = dd_sum_f64(&weights).to_f64();
        if exact_sum.abs() > 1e-15 {
            let inv = 1.0 / exact_sum;
            for w in &mut weights {
                *w *= inv;
            }
            let residual = 1.0 - dd_sum_f64(&weights).to_f64();
            if let Some(slot) = weights.get_mut(m / 2) {
                *slot += residual;
            }
        }
        bank.push(weights);
    }
    bank
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn kernel_cache_dedups_concurrent_builds_and_removes_failed_slots() {
        let cache: Arc<Mutex<LruOnceCache<u32, usize>>> = Arc::new(Mutex::new(LruOnceCache::new(4)));
        let builds = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&cache);
            let b = Arc::clone(&builds);
            let bar = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                bar.wait();
                lru_cache_get_or_init(&c, 42, || {
                    b.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    Ok(12345)
                })
            }));
        }
        let results: Vec<Arc<usize>> = handles.into_iter().map(|h| h.join().unwrap().unwrap()).collect();
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "F15: 8 concurrent callers for the same key must build strictly ONCE"
        );
        for r in &results[1..] {
            assert!(Arc::ptr_eq(&results[0], r));
        }

        // Failed build removes slot so a subsequent retry succeeds!
        let err = lru_cache_get_or_init(&cache, 99, || Err("transient failure".to_string()));
        assert!(err.is_err());
        let ok = lru_cache_get_or_init(&cache, 99, || Ok(777)).unwrap();
        assert_eq!(*ok, 777);
    }

    #[test]
    fn kernel_cache_lru_keeps_hot_entry_and_polyphase_shares_bank() {
        // LRU capacity = 2: insert 1, insert 2, touch 1 (hot), insert 3 -> evicts 2, keeps 1!
        let cache: Mutex<LruOnceCache<u32, u32>> = Mutex::new(LruOnceCache::new(2));
        let builds = AtomicUsize::new(0);
        let a1 = lru_cache_get_or_init(&cache, 1, || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(10)
        })
        .unwrap();
        let _ = lru_cache_get_or_init(&cache, 2, || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(20)
        })
        .unwrap();
        // Touch 1 so 1 is MRU and 2 is LRU.
        let a1_again = lru_cache_get_or_init(&cache, 1, || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(999)
        })
        .unwrap();
        assert!(Arc::ptr_eq(&a1, &a1_again));
        // Insert 3 -> evicts 2, NOT hot entry 1!
        let _ = lru_cache_get_or_init(&cache, 3, || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(30)
        })
        .unwrap();
        let a1_still_hot = lru_cache_get_or_init(&cache, 1, || {
            builds.fetch_add(1, Ordering::SeqCst);
            Ok(888)
        })
        .unwrap();
        assert!(
            Arc::ptr_eq(&a1, &a1_still_hot),
            "F15: LRU eviction must keep hot entry 1 cached"
        );
        assert_eq!(builds.load(Ordering::SeqCst), 3);

        // Polyphase cache hit shares the exact same `Arc<Vec<Vec<f64>>>` bank without cloning!
        let spec = GaussianFilterSpec::for_default_archetype(
            FilterArchetype::GaussHiresLp,
            176_400,
            352_800,
        )
        .unwrap();
        let m_half = spec.half_span.div_ceil(spec.ratio);
        let m = 2 * m_half + 1;
        let id = FilterIdentity {
            archetype: spec.archetype,
            source_rate_hz: spec.source_rate_hz,
            target_rate_hz: spec.target_rate_hz,
            ratio: spec.ratio,
            num_taps: spec.num_taps,
            taps_per_phase: m,
            fc_hz_bits: spec.fc_hz.to_bits(),
            sigma_f_hz_bits: spec.sigma_f_hz.to_bits(),
            effective_nyquist_hz: spec.effective_nyquist_hz,
        };
        let d1 = get_or_design_polyphase(id, || Ok(designed_from_gaussian(design_gaussian_filter(spec)))).unwrap();
        let d2 = get_or_design_polyphase(id, || Ok(designed_from_gaussian(design_gaussian_filter(spec)))).unwrap();
        assert!(Arc::ptr_eq(&d1.bank, &d2.bank), "F15: polyphase cache hit must share Arc bank");
        assert!(d1.prototype.is_empty(), "F15: cached polyphase entry must not retain unused prototype");
    }
}
