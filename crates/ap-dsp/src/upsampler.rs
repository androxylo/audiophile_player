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

use std::f64::consts::PI;
use std::sync::Arc;

use ap_core::buffer::StereoF64Buffer;

use crate::MAX_FILTER_TAPS;
use crate::selector::{
    FilterArchetype, FilterDesignMetrics, FilterIdentity, FilterPhase,
    MAX_SUPPORTED_SOURCE_RATE_HZ, MAX_UPSAMPLE_RATIO, MIN_SUPPORTED_SOURCE_RATE_HZ,
    SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE, SIMPLE_TEST_MIN_TAPS_PER_PHASE, SelectedDspPlan,
    UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsampleRatio, simple_test_taps_per_phase,
};

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
    bank: Arc<Vec<Vec<f64>>>,
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

    /// Flush the carried unpadded FIR history against trailing zeros into `f64` samples using the
    /// EXACT polyphase bank that generated this carryover (used when a gapless sequence ends,
    /// switches sample rate/filter identity, or encounters an error).
    pub fn flush_to_f64(mut self) -> StereoF64Buffer {
        if self.hist_l.is_empty()
            || self.ratio == 0
            || self.taps_per_phase == 0
            || self.bank.len() != self.ratio
        {
            return StereoF64Buffer::empty(self.target_rate_hz);
        }
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
        StereoF64Buffer {
            sample_rate_hz: self.target_rate_hz,
            left,
            right,
        }
    }
}

/// Pluggable stateful floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`).
pub trait Upsampler: Send {
    fn name(&self) -> &'static str;

    /// Prototype FIR kernel tap count (`0` for [`NullUpsampler`]).
    fn total_taps(&self) -> usize;

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

/// Design Building Block A (Gaussian-windowed sinc prototype) in pure `f64`:
/// - Exact `sinc(0) = 1`, Gaussian window evaluated in `f64` (no lookup tables)
/// - Exact mirrored symmetry `h[center - k] == h[center + k]`
/// - Exact per-phase DC normalization (`sum(bank[p]) == 1.0`) while preserving bit-exact symmetry
/// - Measured/analytical [`FilterDesignMetrics`] including diagnostic per-phase `L1` norm
pub fn design_gaussian_filter(spec: GaussianFilterSpec) -> DesignedGaussianFilter {
    let n = spec.num_taps;
    let center = spec.half_span;
    let fs_out = spec.target_rate_hz as f64;
    let ratio = spec.ratio;
    let sigma_samples = spec.sigma_t_seconds * fs_out;

    // 1. Raw symmetric prototype h_raw[0..N] scaled by `ratio` (so each phase sums to ~1.0).
    let mut h_raw = vec![0.0_f64; n];
    let ratio_f = ratio as f64;
    h_raw[center] = ratio_f * (2.0 * spec.fc_hz / fs_out);
    for k in 1..=center {
        let t = k as f64;
        let sinc_val = (2.0 * PI * spec.fc_hz * t / fs_out).sin() / (PI * t);
        let u = t / sigma_samples;
        let win = (-0.5 * u * u).exp();
        let val = ratio_f * sinc_val * win;
        h_raw[center - k] = val;
        h_raw[center + k] = val;
    }

    // 2. Polyphase decomposition:
    //    y[n * L + p] = sum_{s = -m_half..=m_half} bank[p][m_half + s] * x[n + s]
    //    where bank[p][m_half + s] = h[center + p - s * L].
    let m_half = center.div_ceil(ratio);
    let taps_per_phase = 2 * m_half + 1;
    let mut bank = vec![vec![0.0_f64; taps_per_phase]; ratio];
    let l_isize = ratio as isize;
    let c_isize = center as isize;
    let n_isize = n as isize;

    for p in 0..ratio {
        for i in 0..taps_per_phase {
            let s = (i as isize) - (m_half as isize);
            let proto_idx = c_isize + (p as isize) - s * l_isize;
            if proto_idx >= 0 && proto_idx < n_isize {
                bank[p][i] = h_raw[proto_idx as usize];
            }
        }
    }

    // 3. Per-phase exact DC normalization (`sum(bank[p]) == 1.0`), preserving bit-exact symmetry:
    //    - Phase 0 is self-symmetric around `m_half` (`bank[0][m_half - s] == bank[0][m_half + s]`).
    //    - Complementary phases `p` and `q = L - p` satisfy `bank[q][m_half + 1 - s] == bank[p][m_half + s]`.
    //    - If `L` is even, phase `L / 2` is self-symmetric under `s <-> 1 - s`.
    //    We use Neumaier compensated summation + symmetric residual folding so exact DC sum
    //    equals 1.0 to < 2e-16 while preserving bit-for-bit prototype symmetry.
    {
        // Phase 0: exact symmetric normalization around center tap `m_half`.
        let sum0 = neumaier_sum(&bank[0]);
        let inv0 = 1.0 / sum0;
        bank[0][m_half] *= inv0;
        for s in 1..=m_half {
            let v = bank[0][m_half + s] * inv0;
            bank[0][m_half + s] = v;
            bank[0][m_half - s] = v;
        }
        let rem0 = 1.0 - neumaier_sum(&bank[0]);
        bank[0][m_half] += rem0;

        // Phases 1..=L/2
        for p in 1..=(ratio / 2) {
            let q = ratio - p;
            let sum_p = neumaier_sum(&bank[p]);
            let inv_p = 1.0 / sum_p;
            if p == q {
                // Even-L midpoint phase: self-symmetric under i <-> taps_per_phase - i (bank[p][0] == 0).
                bank[p][0] = 0.0;
                for i in 1..=m_half {
                    let mirror = taps_per_phase - i;
                    let v = bank[p][i] * inv_p;
                    bank[p][i] = v;
                    bank[p][mirror] = v;
                }
                let rem_p = 1.0 - neumaier_sum(&bank[p]);
                bank[p][m_half] += 0.5 * rem_p;
                bank[p][m_half + 1] += 0.5 * rem_p;
            } else {
                bank[p][0] = 0.0;
                bank[q][0] = 0.0;
                for i in 1..taps_per_phase {
                    let v = bank[p][i] * inv_p;
                    bank[p][i] = v;
                    let mirror = taps_per_phase - i;
                    bank[q][mirror] = v;
                }
                let rem_p = 1.0 - neumaier_sum(&bank[p]);
                bank[p][m_half] += rem_p;
                bank[q][taps_per_phase - m_half] = bank[p][m_half];
            }
        }
    }

    // 4. Reconstruct the exact normalized prototype `h[0..N]` from the normalized polyphase bank
    //    so `prototype` and `bank` agree bit-for-bit on every tap.
    let mut prototype = vec![0.0_f64; n];
    for p in 0..ratio {
        for i in 0..taps_per_phase {
            let s = (i as isize) - (m_half as isize);
            let proto_idx = c_isize + (p as isize) - s * l_isize;
            if proto_idx >= 0 && proto_idx < n_isize {
                prototype[proto_idx as usize] = bank[p][i];
            }
        }
    }

    // 5. Compute diagnostic worst-case per-phase L1 norm and exact 20 kHz magnitude.
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
        stopband_attenuation_db: GAUSS_STOPBAND_ATTENUATION_DB,
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

    match plan.filter {
        FilterArchetype::Null => Ok(Box::new(NullUpsampler::new(plan.target_rate_hz))),
        FilterArchetype::GaussLongLp | FilterArchetype::GaussHiresLp => {
            let ratio_u32 = match plan.ratio {
                UpsampleRatio::Bypass1x => {
                    return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
                }
                UpsampleRatio::Integer(r) if r >= 2 => r,
                UpsampleRatio::Integer(_) => {
                    return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
                }
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
            let spec = GaussianFilterSpec::for_default_archetype(
                plan.filter,
                plan.source_rate_hz,
                plan.target_rate_hz,
            )?;
            let cap = plan.max_taps.min(MAX_FILTER_TAPS);
            if spec.num_taps > cap {
                return Err(format!(
                    "upsampler '{}' requires {} taps which exceeds --max-taps ({cap})",
                    plan.filter, spec.num_taps
                ));
            }
            let designed = design_gaussian_filter(spec);
            Ok(Box::new(PolyphaseFirUpsampler::from_gaussian(designed)))
        }
        FilterArchetype::SimpleTest => {
            let ratio_u32 = match plan.ratio {
                UpsampleRatio::Bypass1x => {
                    return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
                }
                UpsampleRatio::Integer(r) if r >= 2 => r,
                UpsampleRatio::Integer(_) => {
                    return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
                }
                UpsampleRatio::Arbitrary => {
                    return Err(format!(
                        "{UNSUPPORTED_SAMPLE_RATE_PREFIX} simple-test upsampler requires an exact \
                         integer ratio to 352.8/384 kHz (source rate {} Hz is non-integer)",
                        plan.source_rate_hz
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
        other => Err(format!(
            "upsampler filter '{other}' is not implemented yet (scheduled for Phases 2–3); \
             use 'auto', 'gauss-long-lp', 'gauss-hires-lp', 'simple-test', or 'null' for now"
        )),
    }
}

/// Neumaier compensated summation for full-precision `f64` totals on alternating-sign sinc taps.
fn neumaier_sum(values: &[f64]) -> f64 {
    let mut sum = 0.0_f64;
    let mut c = 0.0_f64;
    for &x in values {
        let t = sum + x;
        if sum.abs() >= x.abs() {
            c += (sum - t) + x;
        } else {
            c += (x - t) + sum;
        }
        sum = t;
    }
    sum + c
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
    m_half: usize,
    bank: Arc<Vec<Vec<f64>>>,
    metrics: FilterDesignMetrics,
    identity: FilterIdentity,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    total_in_frames: usize,
    has_incoming_carryover: bool,
    flushed: bool,
}

impl PolyphaseFirUpsampler {
    pub fn from_gaussian(designed: DesignedGaussianFilter) -> Self {
        let lead = designed.m_half;
        Self {
            name: designed.spec.archetype.cli_name(),
            source_rate_hz: designed.spec.source_rate_hz,
            target_rate_hz: designed.spec.target_rate_hz,
            ratio: designed.spec.ratio,
            taps_per_phase: designed.taps_per_phase,
            m_half: designed.m_half,
            bank: designed.bank,
            metrics: designed.metrics,
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
        self.metrics.num_taps
    }

    fn design_metrics(&self) -> Option<FilterDesignMetrics> {
        Some(self.metrics)
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

        // Append `m_half` trailing zeros (`m_half` lead + `m_half` tail = `2 * m_half = m - 1`
        // -> exact `total_in_frames` windows -> exact `total_in_frames * ratio` output samples).
        if self.m_half > 0 {
            self.hist_l.resize(self.hist_l.len() + self.m_half, 0.0_f64);
            self.hist_r.resize(self.hist_r.len() + self.m_half, 0.0_f64);
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
                pad_tail: self.m_half,
                bank: Arc::clone(&self.bank),
                hist_l,
                hist_r,
            }),
        ))
    }
}

/// Legacy test-only stateful polyphase Kaiser-windowed sinc `f64` upsampler (`--filter simple-test`).
///
/// Retained strictly for regression/comparison testing now that `gauss-long-lp` and
/// `gauss-hires-lp` are the active default upsamplers.
#[derive(Debug)]
pub struct SimpleWindowedSincUpsampler {
    source_rate_hz: u32,
    ratio: usize,
    taps_per_phase: usize,
    bank: Arc<Vec<Vec<f64>>>,
    identity: FilterIdentity,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    total_in_frames: usize,
    has_incoming_carryover: bool,
    flushed: bool,
}

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
        // Pre-seed history with `half - 1` leading zeros so tap `j = 0` (`k = half - 1`) aligns
        // strictly with `x[0]`, and taps `j in -half + 1 ..= half` are centered symmetrically.
        let lead = half - 1;
        Self {
            source_rate_hz,
            ratio,
            taps_per_phase: m,
            bank,
            identity,
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

impl Upsampler for SimpleWindowedSincUpsampler {
    fn name(&self) -> &'static str {
        "simple-test"
    }

    fn total_taps(&self) -> usize {
        self.ratio * self.taps_per_phase
    }

    fn identity(&self) -> FilterIdentity {
        self.identity
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if self.flushed {
            return Err("simple-test upsampler cannot accept blocks after flush()".to_string());
        }
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        if input.sample_rate_hz != self.source_rate_hz {
            return Err(format!(
                "simple-test upsampler sample rate mismatch: expected {} Hz, got {} Hz",
                self.source_rate_hz, input.sample_rate_hz
            ));
        }
        let out_rate = self.source_rate_hz * (self.ratio as u32);
        if input.is_empty() {
            return Ok(StereoF64Buffer::empty(out_rate));
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
            sample_rate_hz: out_rate,
            left,
            right,
        })
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        let out_rate = self.source_rate_hz * (self.ratio as u32);
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok(StereoF64Buffer::empty(out_rate));
        }
        self.flushed = true;

        // Append `half` trailing zeros (`half - 1` leading + `half` trailing = `m - 1`
        // -> exact `total_in_frames` windows -> exact `total_in_frames * ratio` output samples).
        let pad_tail = self.taps_per_phase / 2;
        if pad_tail > 0 {
            self.hist_l.resize(self.hist_l.len() + pad_tail, 0.0_f64);
            self.hist_r.resize(self.hist_r.len() + pad_tail, 0.0_f64);
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
            sample_rate_hz: out_rate,
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
        let out_rate = self.source_rate_hz * (self.ratio as u32);
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok((StereoF64Buffer::empty(out_rate), None));
        }
        self.flushed = true;
        let hist_l = core::mem::take(&mut self.hist_l);
        let hist_r = core::mem::take(&mut self.hist_r);
        if hist_l.is_empty() {
            return Ok((StereoF64Buffer::empty(out_rate), None));
        }
        Ok((
            StereoF64Buffer::empty(out_rate),
            Some(UpsamplerCarryover {
                identity: self.identity,
                source_rate_hz: self.source_rate_hz,
                target_rate_hz: out_rate,
                ratio: self.ratio,
                taps_per_phase: self.taps_per_phase,
                pad_tail: self.taps_per_phase / 2,
                bank: Arc::clone(&self.bank),
                hist_l,
                hist_r,
            }),
        ))
    }
}

/// Zeroth-order modified Bessel function of the first kind `I_0(x)` computed in pure `f64`
/// to full machine precision (`~1e-16` relative accuracy).
fn bessel_i0(x: f64) -> f64 {
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
    let inv_i0_beta = 1.0 / bessel_i0(beta);
    let radius = (m as f64) * 0.5;

    let mut bank = Vec::with_capacity(ratio);
    for phase in 0..ratio {
        let frac = (phase as f64) / (ratio as f64);
        let mut weights = Vec::with_capacity(m);
        let mut sum = 0.0_f64;

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
                bessel_i0(arg) * inv_i0_beta
            } else {
                0.0
            };
            let w = sinc * win;
            sum += w;
            weights.push(w);
        }

        if sum.abs() > 1e-15 {
            let inv = 1.0 / sum;
            for w in &mut weights {
                *w *= inv;
            }
        }
        bank.push(weights);
    }
    bank
}
