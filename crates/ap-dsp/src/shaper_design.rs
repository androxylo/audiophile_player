//! Clean-room `8fS` (`352.8 / 384 kHz`) minimum-phase FIR error-feedback noise-shaper designer
//! and frozen runtime coefficient tables (`docs/research/04-noise-shapers.md` §2.3–§6.2).
//!
//! Design algorithm:
//! - Given a target frequency weight `W(ω)` (`1` on `[0, f_band]`, optional log-linear taper to
//!   `f_taper_end`, and floor `ε` above), compute Toeplitz autocorrelation lags
//!   `r[k] = (1/π) ∫₀^π W(ω) cos(kω) dω` on an 8192-point midpoint grid.
//! - Solve `R · c = -r[1..N]` via Levinson–Durbin recursion. Because `W(ω) > 0`, `R` is strictly
//!   positive-definite and `NTF(z) = 1 + Σ_{k=1..N} c_k z^{-k}` is guaranteed minimum-phase
//!   (all zeros strictly inside the unit circle, meeting the Gerzon–Craven bound
//!   `∫₀^π ln|NTF(e^{jω})| dω = 0`).
//! - Bisect `log10(ε) ∈ [-16, 0]` to hit the requested peak `max |NTF|` ceiling.
//!
//! Runtime quantization uses the frozen `const` tables below (zero runtime solver cost,
//! deterministic across platforms) and unit tests audit every frozen table against the
//! clean-room solver to `< 1e-9`.
//!
//! Note on naming (`04-noise-shapers.md` §3): `Ns5Multibit` (`ns5-class`), `Lns15Smooth`
//! (`lns15-class`), and `PggbOptimal` (`wide40-9`) are independent clean-room designs
//! matching the order, passband, and ultrasonic envelope class of those shaper families —
//! they are NOT reverse-engineered or claimed bit-identical to proprietary shapers.

use std::f64::consts::PI;

use crate::selector::NoiseShaperProfile;

/// Maximum supported FIR error-feedback order (used to size fixed ring buffers in `quantize`).
pub const MAX_SHAPER_ORDER: usize = 15;

/// Frequency-domain design specification for a clean-room minimum-phase error-feedback shaper.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShaperDesignSpec {
    /// FIR error-feedback order `N` (`1 <= N <= MAX_SHAPER_ORDER`).
    pub order: usize,
    /// Upper edge of the flat `W = 1` audio passband in Hz (e.g. `20_000.0` or `40_000.0`).
    pub f_band_hz: f64,
    /// Optional upper edge of a log-linear weight taper from `1.0` down to `ε` (`None` = sharp step).
    pub f_taper_end_hz: Option<f64>,
    /// Ceiling on peak out-of-band noise gain `max_ω 20 log10 |NTF(e^{jω})|` in dB.
    pub max_ntf_db: f64,
}

/// Frequency-domain verification metrics of an `NTF(z) = 1 + Σ_{k=1..N} c_k z^{-k}` polynomial.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NtfBandMetrics {
    pub in_band_0_20k_db: f64,
    pub band_20_40k_db: f64,
    pub band_40_100k_db: f64,
    pub band_100k_nyq_db: f64,
    pub total_db: f64,
    pub gerzon_craven_mean_ln: f64,
    pub l1_sum: f64,
    pub peak_ntf_db: f64,
}

/// Return the clean-room design spec for a shaped [`NoiseShaperProfile`], or `None` for
/// `Bypass` / `TpdfDither`.
pub const fn shaper_design_spec(profile: NoiseShaperProfile) -> Option<ShaperDesignSpec> {
    match profile {
        NoiseShaperProfile::Bypass | NoiseShaperProfile::TpdfDither => None,
        NoiseShaperProfile::NsGentle2 => Some(ShaperDesignSpec {
            order: 2,
            f_band_hz: 20_000.0,
            f_taper_end_hz: None,
            max_ntf_db: 6.0,
        }),
        NoiseShaperProfile::Ns5Multibit => Some(ShaperDesignSpec {
            order: 5,
            f_band_hz: 20_000.0,
            f_taper_end_hz: None,
            max_ntf_db: 20.0,
        }),
        NoiseShaperProfile::Lns15Smooth => Some(ShaperDesignSpec {
            order: 15,
            f_band_hz: 20_000.0,
            f_taper_end_hz: Some(60_000.0),
            max_ntf_db: 15.0,
        }),
        NoiseShaperProfile::PggbOptimal => Some(ShaperDesignSpec {
            order: 9,
            f_band_hz: 40_000.0,
            f_taper_end_hz: None,
            max_ntf_db: 18.0,
        }),
    }
}

/// Frozen `ns-gentle-2` (`N = 2`, 20 kHz, `+6 dB` ceiling) taps at `352,800 Hz`.
pub const COEFFS_NS_GENTLE_2_352800: [f64; 2] = [-0.9671215842020704, 0.028140740965355276];

/// Frozen `ns-gentle-2` (`N = 2`, 20 kHz, `+6 dB` ceiling) taps at `384,000 Hz`.
pub const COEFFS_NS_GENTLE_2_384000: [f64; 2] = [-0.9717433371099817, 0.023518987889512587];

/// Frozen `ns5-class` (`N = 5`, 20 kHz, `+20 dB` ceiling) taps at `352,800 Hz`.
pub const COEFFS_NS5_352800: [f64; 5] = [
    -3.5141390833724726,
    4.353793051123896,
    -1.8219329497209977,
    -0.35288985219511504,
    0.3369751975735615,
];

/// Frozen `ns5-class` (`N = 5`, 20 kHz, `+20 dB` ceiling) taps at `384,000 Hz`.
pub const COEFFS_NS5_384000: [f64; 5] = [
    -3.531239327106661,
    4.376059952146921,
    -1.8110181903054046,
    -0.3754128940647836,
    0.3429045538861563,
];

/// Frozen `lns15-class` (`N = 15`, 20 kHz + 40 kHz taper -> 60 kHz, `+15 dB` ceiling) taps at `352,800 Hz`.
pub const COEFFS_LNS15_352800: [f64; 15] = [
    -2.8339997946577324,
    2.031579651056906,
    0.8477682736388443,
    -0.7325274513132967,
    -0.8229923117171236,
    -0.009479869543598404,
    0.5407585820850046,
    0.363313200437863,
    -0.14362151028670062,
    -0.3586434119888982,
    -0.10887621486215876,
    0.21890324644026868,
    0.16227610129487544,
    -0.18982359717963848,
    0.039891848469640605,
];

/// Frozen `lns15-class` (`N = 15`, 20 kHz + 40 kHz taper -> 60 kHz, `+15 dB` ceiling) taps at `384,000 Hz`.
pub const COEFFS_LNS15_384000: [f64; 15] = [
    -2.891286787102481,
    2.046662036048544,
    0.9412594338320859,
    -0.6718228172638918,
    -0.8930799653234239,
    -0.14517608937656348,
    0.4954867815790674,
    0.4661633710177633,
    -0.00626126625169863,
    -0.34273312798968003,
    -0.2275164536954293,
    0.11929922027312465,
    0.21245021265562697,
    -0.08260765957676687,
    -0.018521691962031298,
];

/// Frozen `wide40-9` (`pggb-optimal` class: `N = 9`, 40 kHz, `+18 dB` ceiling) taps at `352,800 Hz`.
pub const COEFFS_WIDE40_9_352800: [f64; 9] = [
    -3.0272202541837214,
    2.801694550878594,
    0.34593817132766264,
    -1.4719845213003817,
    -0.5031438899342742,
    0.980768264055937,
    0.5692886880874015,
    -1.0158890985573668,
    0.3329848683122273,
];

/// Frozen `wide40-9` (`pggb-optimal` class: `N = 9`, 40 kHz, `+18 dB` ceiling) taps at `384,000 Hz`.
pub const COEFFS_WIDE40_9_384000: [f64; 9] = [
    -3.060128124726337,
    2.791093015410925,
    0.4364652774557149,
    -1.4498777150897615,
    -0.6033132113452,
    0.9279307584041289,
    0.6589780154555355,
    -0.9877961328399554,
    0.29468735200422286,
];

/// Look up the frozen error-feedback tap slice `c[1..=N]` for `(profile, sample_rate_hz)`.
///
/// Returns `None` for non-shaped profiles (`Bypass`, `TpdfDither`) and for any sample rate
/// other than `352,800 Hz` or `384,000 Hz` (`8fS` tables must never be reused at other rates).
pub const fn frozen_shaper_coeffs(
    profile: NoiseShaperProfile,
    sample_rate_hz: u32,
) -> Option<&'static [f64]> {
    match (profile, sample_rate_hz) {
        (NoiseShaperProfile::NsGentle2, 352_800) => Some(&COEFFS_NS_GENTLE_2_352800),
        (NoiseShaperProfile::NsGentle2, 384_000) => Some(&COEFFS_NS_GENTLE_2_384000),
        (NoiseShaperProfile::Ns5Multibit, 352_800) => Some(&COEFFS_NS5_352800),
        (NoiseShaperProfile::Ns5Multibit, 384_000) => Some(&COEFFS_NS5_384000),
        (NoiseShaperProfile::Lns15Smooth, 352_800) => Some(&COEFFS_LNS15_352800),
        (NoiseShaperProfile::Lns15Smooth, 384_000) => Some(&COEFFS_LNS15_384000),
        (NoiseShaperProfile::PggbOptimal, 352_800) => Some(&COEFFS_WIDE40_9_352800),
        (NoiseShaperProfile::PggbOptimal, 384_000) => Some(&COEFFS_WIDE40_9_384000),
        _ => None,
    }
}

/// Solve the Toeplitz normal equations `R · c = -r[1..=order]` via Levinson–Durbin recursion.
///
/// Returns `c = [c_1, ..., c_order]` such that `NTF(z) = 1 + Σ_{k=1..=order} c_k z^{-k}` is
/// strictly minimum-phase whenever `r[0..=order]` is a valid positive-definite autocorrelation.
pub fn levinson_durbin(r: &[f64], order: usize) -> Vec<f64> {
    assert!(
        r.len() > order && order > 0 && r[0] > 0.0,
        "levinson_durbin requires order >= 1, r.len() > order, and r[0] > 0"
    );
    let mut a = vec![0.0_f64; order + 1];
    a[0] = 1.0;
    let mut err = r[0];
    let mut a_next = vec![0.0_f64; order + 1];

    for m in 1..=order {
        let mut acc = r[m];
        for j in 1..m {
            acc += a[j] * r[m - j];
        }
        let k = -acc / err;
        a_next[..=order].copy_from_slice(&a);
        a_next[m] = k;
        for j in 1..m {
            a_next[j] = a[j] + k * a[m - j];
        }
        std::mem::swap(&mut a, &mut a_next);
        err *= 1.0 - k * k;
    }

    a[1..=order].to_vec()
}

/// Evaluate `|NTF(e^{jω})|²` at a single normalized radian frequency `w ∈ [0, π]`.
#[inline]
pub fn ntf_mag_sq_at_rad(coeffs: &[f64], w: f64) -> f64 {
    let mut re = 1.0_f64;
    let mut im = 0.0_f64;
    for (idx, &c) in coeffs.iter().enumerate() {
        let angle = ((idx + 1) as f64) * w;
        re += c * angle.cos();
        im -= c * angle.sin();
    }
    re * re + im * im
}

/// Evaluate `|NTF(e^{j 2π f / fs})|²` at frequency `freq_hz`.
#[inline]
pub fn ntf_mag_sq_at_hz(coeffs: &[f64], freq_hz: f64, fs_hz: f64) -> f64 {
    ntf_mag_sq_at_rad(coeffs, 2.0 * PI * freq_hz / fs_hz)
}

/// Clean-room design of FIR error-feedback taps `c[1..=N]` for `spec` at sample rate `fs_hz`.
///
/// Uses an 8192-point midpoint grid on `[0, π]` and 70 bisection iterations on
/// `log10(ε) ∈ [-16, 0]` to hit `spec.max_ntf_db` (exact Rust mirror of `poc/python/audiophile_poc/ns_design.py`).
pub fn design_error_feedback_taps(spec: ShaperDesignSpec, fs_hz: f64) -> Vec<f64> {
    assert!(
        spec.order >= 1 && spec.order <= MAX_SHAPER_ORDER,
        "shaper order must be in 1..={MAX_SHAPER_ORDER}"
    );
    assert!(fs_hz > 0.0 && fs_hz.is_finite(), "fs_hz must be finite and > 0");

    const GRID_N: usize = 8192;
    let dw = PI / (GRID_N as f64);
    let freq_scale = fs_hz / (2.0 * PI);
    let order = spec.order;

    // Precompute frequency grid and trig basis for fast bisection.
    let mut freqs = Vec::with_capacity(GRID_N);
    let mut cos_table = vec![0.0_f64; (order + 1) * GRID_N];
    let mut sin_table = vec![0.0_f64; order * GRID_N];

    for g in 0..GRID_N {
        let w = (g as f64 + 0.5) * dw;
        freqs.push(w * freq_scale);
        cos_table[g] = 1.0;
        for k in 1..=order {
            let (s, c) = ((k as f64) * w).sin_cos();
            cos_table[k * GRID_N + g] = c;
            sin_table[(k - 1) * GRID_N + g] = s;
        }
    }

    let target_lin_sq = 10.0_f64.powf(spec.max_ntf_db / 10.0);
    let mut weights = vec![0.0_f64; GRID_N];
    let mut r = vec![0.0_f64; order + 1];

    let mut solve_for_log_eps = |log10_eps: f64| -> (Vec<f64>, f64) {
        let eps = 10.0_f64.powf(log10_eps);
        let ln_eps = eps.ln();

        // Exploit exact midpoint-grid orthogonality `(1/N) Σ cos(kω) = δ_{k,0}`: decompose
        // `W(ω) = (W(ω) - ε) + ε` and add `ε` directly to `r[0]` so tiny stopband weights
        // (~1e-9) are never lost to floating-point rounding against O(1000) passband sums.
        let mut active_end = 0_usize;
        for (g, &f) in freqs.iter().enumerate() {
            let excess = if f <= spec.f_band_hz {
                1.0 - eps
            } else if let Some(f_end) = spec.f_taper_end_hz {
                if f_end > spec.f_band_hz && f < f_end {
                    let t = (f - spec.f_band_hz) / (f_end - spec.f_band_hz);
                    ((t * ln_eps).exp() - eps).max(0.0)
                } else {
                    0.0
                }
            } else {
                0.0
            };
            weights[g] = excess;
            if excess > 0.0 {
                active_end = g + 1;
            }
        }

        let inv_n = 1.0 / (GRID_N as f64);
        for k in 0..=order {
            let row = &cos_table[k * GRID_N..k * GRID_N + active_end];
            // Kahan compensated summation preserves full f64 precision for ill-conditioned Toeplitz rows.
            let mut sum = 0.0_f64;
            let mut comp = 0.0_f64;
            for g in 0..active_end {
                let y = row[g] * weights[g] - comp;
                let t = sum + y;
                comp = (t - sum) - y;
                sum = t;
            }
            r[k] = sum * inv_n;
        }
        r[0] += eps;

        let c = levinson_durbin(&r, order);

        let mut peak_sq = 0.0_f64;
        for g in 0..GRID_N {
            let mut re = 1.0_f64;
            let mut im = 0.0_f64;
            for k in 1..=order {
                let ck = c[k - 1];
                re += ck * cos_table[k * GRID_N + g];
                im -= ck * sin_table[(k - 1) * GRID_N + g];
            }
            let mag_sq = re * re + im * im;
            if mag_sq > peak_sq {
                peak_sq = mag_sq;
            }
        }
        (c, peak_sq)
    };

    let mut lo = -16.0_f64;
    let mut hi = 0.0_f64;
    for _ in 0..70 {
        let mid = 0.5 * (lo + hi);
        let (_, peak_sq) = solve_for_log_eps(mid);
        if peak_sq > target_lin_sq {
            lo = mid;
        } else {
            hi = mid;
        }
    }

    let (coeffs, _) = solve_for_log_eps(hi);
    coeffs
}

/// Compute band-averaged noise-power gains (vs flat TPDF), Gerzon–Craven integral, L1 norm,
/// and peak `|NTF|` for `coeffs` at sample rate `fs_hz`.
pub fn ntf_band_metrics(coeffs: &[f64], fs_hz: f64) -> NtfBandMetrics {
    const GRID_N: usize = 16_384;
    let dw = PI / (GRID_N as f64);
    let freq_scale = fs_hz / (2.0 * PI);
    let nyq = 0.5 * fs_hz;

    let mut sum_0_20k = 0.0_f64;
    let mut n_0_20k = 0_usize;
    let mut sum_20_40k = 0.0_f64;
    let mut n_20_40k = 0_usize;
    let mut sum_40_100k = 0.0_f64;
    let mut n_40_100k = 0_usize;
    let mut sum_100k_nyq = 0.0_f64;
    let mut n_100k_nyq = 0_usize;
    let mut sum_total = 0.0_f64;
    let mut sum_ln = 0.0_f64;
    let mut peak_sq = 0.0_f64;

    for g in 0..GRID_N {
        let w = (g as f64 + 0.5) * dw;
        let f = w * freq_scale;
        let mag_sq = ntf_mag_sq_at_rad(coeffs, w);

        sum_total += mag_sq;
        sum_ln += 0.5 * mag_sq.ln();
        if mag_sq > peak_sq {
            peak_sq = mag_sq;
        }
        if f <= 20_000.0 {
            sum_0_20k += mag_sq;
            n_0_20k += 1;
        }
        if (20_000.0..=40_000.0).contains(&f) {
            sum_20_40k += mag_sq;
            n_20_40k += 1;
        }
        if (40_000.0..=100_000.0).contains(&f) {
            sum_40_100k += mag_sq;
            n_40_100k += 1;
        }
        if (100_000.0..=nyq).contains(&f) {
            sum_100k_nyq += mag_sq;
            n_100k_nyq += 1;
        }
    }

    let l1_sum: f64 = coeffs.iter().map(|c| c.abs()).sum();

    NtfBandMetrics {
        in_band_0_20k_db: 10.0 * (sum_0_20k / n_0_20k.max(1) as f64).log10(),
        band_20_40k_db: 10.0 * (sum_20_40k / n_20_40k.max(1) as f64).log10(),
        band_40_100k_db: 10.0 * (sum_40_100k / n_40_100k.max(1) as f64).log10(),
        band_100k_nyq_db: 10.0 * (sum_100k_nyq / n_100k_nyq.max(1) as f64).log10(),
        total_db: 10.0 * (sum_total / GRID_N as f64).log10(),
        gerzon_craven_mean_ln: sum_ln / GRID_N as f64,
        l1_sum,
        peak_ntf_db: 10.0 * peak_sq.log10(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHAPED_PROFILES: [NoiseShaperProfile; 4] = [
        NoiseShaperProfile::NsGentle2,
        NoiseShaperProfile::Ns5Multibit,
        NoiseShaperProfile::Lns15Smooth,
        NoiseShaperProfile::PggbOptimal,
    ];

    #[test]
    fn frozen_tables_match_runtime_levinson_durbin_solver_within_1e9() {
        for &rate in &[352_800_u32, 384_000_u32] {
            for &profile in &SHAPED_PROFILES {
                let spec = shaper_design_spec(profile).expect("shaped profile must have spec");
                let frozen = frozen_shaper_coeffs(profile, rate)
                    .expect("8fS rate must return frozen table");
                let designed = design_error_feedback_taps(spec, f64::from(rate));

                assert_eq!(
                    frozen.len(),
                    designed.len(),
                    "tap count mismatch for {profile} @ {rate} Hz"
                );
                let max_diff = frozen
                    .iter()
                    .zip(designed.iter())
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0_f64, f64::max);
                assert!(
                    max_diff < 1e-9,
                    "{profile} @ {rate} Hz frozen table diverged from runtime solver: max_diff={max_diff:.3e}"
                );

                // Minimum-phase Gerzon-Craven check & peak ceiling check.
                let metrics = ntf_band_metrics(frozen, f64::from(rate));
                assert!(
                    metrics.gerzon_craven_mean_ln.abs() < 1e-12,
                    "{profile} @ {rate} Hz failed Gerzon-Craven minimum-phase integral: {:.3e}",
                    metrics.gerzon_craven_mean_ln
                );
                assert!(
                    metrics.peak_ntf_db <= spec.max_ntf_db + 0.02,
                    "{profile} @ {rate} Hz exceeded max |NTF| ceiling: {:.3} dB > {:.2} dB",
                    metrics.peak_ntf_db,
                    spec.max_ntf_db
                );
            }
        }
    }

    #[test]
    fn band_metrics_match_inv04_research_section_3_1() {
        // Expected values from docs/research/04-noise-shapers.md §3.1 at 384 kHz:
        // (profile, 0-20k, 20-40k, 40-100k, 100k-nyq, total, sum|c|)
        let expected_384 = [
            (NoiseShaperProfile::NsGentle2, -14.6, -6.5, 0.5, 5.1, 2.9, 1.00),
            (NoiseShaperProfile::Ns5Multibit, -58.7, -22.3, 6.8, 18.6, 15.6, 10.44),
            (NoiseShaperProfile::Lns15Smooth, -51.5, -27.3, 11.9, 13.8, 12.15, 9.56),
            (NoiseShaperProfile::PggbOptimal, -40.6, -39.1, 11.25, 16.0, 13.65, 11.21),
        ];

        for (profile, b0, b1, b2, b3, tot, l1) in expected_384 {
            let coeffs = frozen_shaper_coeffs(profile, 384_000).unwrap();
            let m = ntf_band_metrics(coeffs, 384_000.0);
            assert!(
                (m.in_band_0_20k_db - b0).abs() < 0.1,
                "{profile} 0-20k: got {:.2}, expected {b0:.2}",
                m.in_band_0_20k_db
            );
            assert!(
                (m.band_20_40k_db - b1).abs() < 0.1,
                "{profile} 20-40k: got {:.2}, expected {b1:.2}",
                m.band_20_40k_db
            );
            assert!(
                (m.band_40_100k_db - b2).abs() < 0.1,
                "{profile} 40-100k: got {:.2}, expected {b2:.2}",
                m.band_40_100k_db
            );
            assert!(
                (m.band_100k_nyq_db - b3).abs() < 0.1,
                "{profile} 100k-nyq: got {:.2}, expected {b3:.2}",
                m.band_100k_nyq_db
            );
            assert!(
                (m.total_db - tot).abs() < 0.1,
                "{profile} total: got {:.2}, expected {tot:.2}",
                m.total_db
            );
            assert!(
                (m.l1_sum - l1).abs() < 0.05,
                "{profile} sum|c|: got {:.2}, expected {l1:.2}",
                m.l1_sum
            );
        }

        // Expected 0-20 kHz band gains at 352.8 kHz from §3.1:
        let expected_352 = [
            (NoiseShaperProfile::NsGentle2, -13.9),
            (NoiseShaperProfile::Ns5Multibit, -55.9),
            (NoiseShaperProfile::Lns15Smooth, -46.3),
            (NoiseShaperProfile::PggbOptimal, -37.9),
        ];
        for (profile, b0) in expected_352 {
            let coeffs = frozen_shaper_coeffs(profile, 352_800).unwrap();
            let m = ntf_band_metrics(coeffs, 352_800.0);
            assert!(
                (m.in_band_0_20k_db - b0).abs() < 0.1,
                "{profile} @ 352.8k 0-20k: got {:.2}, expected {b0:.2}",
                m.in_band_0_20k_db
            );
        }
    }

    #[test]
    fn non_8fs_and_unshaped_profiles_return_none() {
        for &rate in &[44_100_u32, 48_000, 88_200, 96_000, 176_400, 192_000, 768_000] {
            for &profile in &SHAPED_PROFILES {
                assert!(
                    frozen_shaper_coeffs(profile, rate).is_none(),
                    "{profile} must not return 8fS coefficients at {rate} Hz"
                );
            }
        }
        for &rate in &[352_800_u32, 384_000] {
            assert!(frozen_shaper_coeffs(NoiseShaperProfile::Bypass, rate).is_none());
            assert!(frozen_shaper_coeffs(NoiseShaperProfile::TpdfDither, rate).is_none());
        }
    }
}
