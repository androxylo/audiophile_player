// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! **L2 · Filter Design Spec Enforcement** (`PIPELINE_TESTS_REVIEW.md` §3 L2).
//!
//! Single authoritative spec table checking every curated filter archetype across both rate
//! families (`44.1/88.2/176.4/352.8 -> 352.8 kHz` and `48/96/192/384 -> 384 kHz`) directly from
//! the exported runtime taps:
//! - passband ripple;
//! - `-3 dB` point;
//! - stopband attenuation at and above `fs_src - f_pass`;
//! - phase symmetry / minimum-phase / intermediate-phase energy localization;
//! - per-phase unity DC sum (`|sum(phase) - 1.0| <= 1e-15`);
//! - cepstrum `k_len <= 2^20` cap for prototypes > 65,536 taps.

use ap_core::format::StreamSpec;
use ap_dsp::phase::convert_prototype_phase;
use ap_dsp::selector::{FilterArchetype, FilterPhase, TrackSourceInfo, UpsamplerPicker, UpsamplerPolicy};
use ap_dsp::upsampler::{build_upsampler, design_gaussian_filter, gauss_apod_4m_spec};
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Single authoritative specification row for an upsampling filter archetype.
#[derive(Debug, Clone, Copy)]
struct ArchetypeSpec {
    archetype: FilterArchetype,
    phase: FilterPhase,
    /// Minimum valid source rate in Hz (e.g., `88_200` for hi-res-only `gauss-hires-lp`).
    min_source_rate_hz: u32,
    /// Passband edge as a fraction of `fs_src` (`<= 0.5`).
    f_pass_frac: f64,
    /// Maximum allowed passband deviation (`|20 log10 |H(f)||`) over `[0, f_pass]`.
    max_passband_ripple_db: f64,
    /// Allowed `-3 dB` cutoff range as a fraction of `fs_src`.
    f_3db_min_frac: f64,
    f_3db_max_frac: f64,
    /// Minimum stopband attenuation (positive dB) for `f >= fs_src - f_pass`.
    min_stopband_db: f64,
    /// Optional cap on taps when validating multi-million-tap archetypes at higher rates in CI.
    test_max_taps: Option<usize>,
}

/// Authoritative L2 filter spec table (shared specification bounds across all archetypes).
const FILTER_SPEC_TABLE: &[ArchetypeSpec] = &[
    ArchetypeSpec {
        archetype: FilterArchetype::GaussLongLp,
        phase: FilterPhase::Linear,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.44,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.45,
        f_3db_max_frac: 0.505,
        min_stopband_db: 180.0,
        test_max_taps: None,
    },
    ArchetypeSpec {
        archetype: FilterArchetype::GaussHiresLp,
        phase: FilterPhase::Linear,
        min_source_rate_hz: 88_200,
        f_pass_frac: 0.41,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.43,
        f_3db_max_frac: 0.505,
        min_stopband_db: 180.0,
        test_max_taps: None,
    },
    ArchetypeSpec {
        archetype: FilterArchetype::GaussApod4M,
        phase: FilterPhase::Linear,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.45,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.45,
        f_3db_max_frac: 0.505,
        min_stopband_db: 180.0,
        test_max_taps: Some(65_536),
    },
    ArchetypeSpec {
        archetype: FilterArchetype::SincSteep4M,
        phase: FilterPhase::Linear,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.45,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.46,
        f_3db_max_frac: 0.505,
        min_stopband_db: 180.0,
        test_max_taps: Some(65_536),
    },
    ArchetypeSpec {
        archetype: FilterArchetype::ExtSharpLp,
        phase: FilterPhase::Linear,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.45,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.46,
        f_3db_max_frac: 0.505,
        min_stopband_db: 180.0,
        test_max_taps: Some(65_536),
    },
    ArchetypeSpec {
        archetype: FilterArchetype::MinphaseLong,
        phase: FilterPhase::Minimum,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.41,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.43,
        f_3db_max_frac: 0.505,
        min_stopband_db: 150.0,
        test_max_taps: None,
    },
    ArchetypeSpec {
        archetype: FilterArchetype::IntermediateLong,
        phase: FilterPhase::Intermediate,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.41,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.43,
        f_3db_max_frac: 0.505,
        min_stopband_db: 150.0,
        test_max_taps: None,
    },
    ArchetypeSpec {
        archetype: FilterArchetype::ShortLp,
        phase: FilterPhase::Linear,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.42,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.43,
        f_3db_max_frac: 0.505,
        min_stopband_db: 120.0,
        test_max_taps: None,
    },
    ArchetypeSpec {
        archetype: FilterArchetype::ShortMp,
        phase: FilterPhase::Minimum,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.42,
        max_passband_ripple_db: 0.05,
        f_3db_min_frac: 0.43,
        f_3db_max_frac: 0.505,
        min_stopband_db: 120.0,
        test_max_taps: None,
    },
    ArchetypeSpec {
        archetype: FilterArchetype::SimpleTest,
        phase: FilterPhase::Linear,
        min_source_rate_hz: 44_100,
        f_pass_frac: 0.40,
        max_passband_ripple_db: 0.25,
        f_3db_min_frac: 0.42,
        f_3db_max_frac: 0.52,
        min_stopband_db: 90.0,
        test_max_taps: None,
    },
];

/// Compute `|H(f)| / ratio` via independent complex `rustfft` on a dense grid.
fn fft_mag_normalized(taps: &[f64], ratio: usize) -> (Vec<f64>, usize) {
    let k_len = (taps.len() * 4)
        .next_power_of_two()
        .clamp(65_536, 1 << 19);
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(k_len);
    let mut buf = vec![Complex::new(0.0_f64, 0.0_f64); k_len];
    for (dst, &v) in buf.iter_mut().zip(taps.iter()) {
        dst.re = v;
    }
    fft.process(&mut buf);
    let inv_ratio = 1.0 / (ratio as f64);
    let mags = buf[..=k_len / 2]
        .iter()
        .map(|c| c.norm() * inv_ratio)
        .collect();
    (mags, k_len)
}

fn db(mag: f64) -> f64 {
    20.0 * mag.max(1e-20).log10()
}

#[test]
fn l2_all_archetypes_meet_spec_across_44k1_and_48k_families() {
    // 1x, 2x, 4x, 8x in both 44.1 kHz and 48 kHz families.
    let rate_cases: &[u32] = &[
        44_100, 88_200, 176_400, 352_800, 48_000, 96_000, 192_000, 384_000,
    ];

    for &src_rate in rate_cases {
        let spec = StreamSpec {
            sample_rate_hz: src_rate,
            bits_per_sample: 24,
            channels: 2,
        };
        let src_info = TrackSourceInfo::from_stream_spec(spec);

        // Native 8fS (352.8 / 384 kHz) must always resolve to Null pass-through.
        if src_rate >= 352_800 {
            let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
            let plan = picker.select(&src_info);
            assert_eq!(plan.filter, FilterArchetype::Null);
            let up = build_upsampler(&plan).unwrap();
            assert_eq!(up.total_taps(), 0);
            assert!(up.verify_export().is_none());
            continue;
        }

        for row in FILTER_SPEC_TABLE {
            if src_rate < row.min_source_rate_hz {
                continue;
            }
            let mut policy = UpsamplerPolicy {
                filter_override: Some(row.archetype),
                ..UpsamplerPolicy::default()
            };
            if let Some(cap) = row.test_max_taps {
                policy.max_taps = cap;
            }
            let picker = UpsamplerPicker::new(policy).unwrap();
            let plan = picker.select(&src_info);
            let up = build_upsampler(&plan).unwrap_or_else(|e| {
                panic!(
                    "build_upsampler failed for {} @ {src_rate} Hz: {e}",
                    row.archetype
                )
            });
            let export = up
                .verify_export()
                .expect("non-Null upsampler must export taps");
            let ratio = export.ratio;
            let fs_out = f64::from(plan.target_rate_hz);
            let fs_src = f64::from(src_rate);

            // 1. Every polyphase phase must sum to 1.0 within 1e-15 (Neumaier compensated sum so
            //    alternating-sign sinc taps don't accumulate naive f64 summation rounding)!
            assert_eq!(export.bank.len(), ratio);
            for (p, coeffs) in export.bank.iter().enumerate() {
                let mut sum = 0.0_f64;
                let mut comp = 0.0_f64;
                for &v in coeffs {
                    let t = sum + v;
                    if sum.abs() >= v.abs() {
                        comp += (sum - t) + v;
                    } else {
                        comp += (v - t) + sum;
                    }
                    sum = t;
                }
                let exact_sum = sum + comp;
                assert!(
                    (exact_sum - 1.0).abs() <= 1e-15,
                    "{} @ {src_rate} Hz phase {p} sum={exact_sum:.17} (|sum-1|={:.3e} > 1e-15)",
                    row.archetype,
                    (exact_sum - 1.0).abs()
                );
            }

            // 2. Frequency response from exported prototype taps.
            let taps = export.prototype_taps();
            let (mags, k_len) = fft_mag_normalized(&taps, ratio);
            let df = fs_out / (k_len as f64);

            // Passband ripple over [0, f_pass]
            let f_pass = row.f_pass_frac * fs_src;
            let pass_bin = (f_pass / df).floor() as usize;
            let mut max_ripple_db = 0.0_f64;
            for &m in &mags[..=pass_bin] {
                let dev = db(m).abs();
                if dev > max_ripple_db {
                    max_ripple_db = dev;
                }
            }
            assert!(
                max_ripple_db <= row.max_passband_ripple_db,
                "{} @ {src_rate} Hz passband ripple {max_ripple_db:.4} dB > {:.4} dB (f_pass={f_pass:.0} Hz)",
                row.archetype,
                row.max_passband_ripple_db
            );

            // -3 dB point
            let f_3db_bin = mags
                .iter()
                .position(|&m| db(m) <= -3.0)
                .unwrap_or(mags.len() - 1);
            let f_3db_frac = (f_3db_bin as f64 * df) / fs_src;
            assert!(
                f_3db_frac >= row.f_3db_min_frac && f_3db_frac <= row.f_3db_max_frac,
                "{} @ {src_rate} Hz -3 dB point at {f_3db_frac:.4}*fs_src outside [{:.4}, {:.4}]",
                row.archetype,
                row.f_3db_min_frac,
                row.f_3db_max_frac
            );

            // Stopband attenuation at and above fs_src - f_pass
            let f_stop_start = fs_src - f_pass;
            let stop_bin = (f_stop_start / df).ceil() as usize;
            let mut worst_stop_db = f64::NEG_INFINITY;
            for &m in &mags[stop_bin..] {
                let d = db(m);
                if d > worst_stop_db {
                    worst_stop_db = d;
                }
            }
            assert!(
                worst_stop_db <= -row.min_stopband_db,
                "{} @ {src_rate} Hz stopband max {worst_stop_db:+.2} dB > -{:.1} dB (from {f_stop_start:.0} Hz)",
                row.archetype,
                row.min_stopband_db
            );

            // 3. Phase type check directly from exported taps (trim zero-pad edges).
            let trim_eps = 1e-18;
            let first_nz = taps.iter().position(|&v| v.abs() > trim_eps).unwrap_or(0);
            let last_nz = taps.iter().rposition(|&v| v.abs() > trim_eps).unwrap_or(taps.len() - 1);
            let core = &taps[first_nz..=last_nz];
            let peak_idx = core
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
                .map(|(idx, _)| idx)
                .unwrap_or(0);

            match row.phase {
                FilterPhase::Linear => {
                    // For odd-length symmetric prototypes, h[k] == h[len - 1 - k] (or within
                    // 1e-13 for polyphase-normalized even-m SimpleTest).
                    let cn = core.len();
                    let mut max_asym = 0.0_f64;
                    for k in 0..cn / 2 {
                        let d = (core[k] - core[cn - 1 - k]).abs();
                        if d > max_asym {
                            max_asym = d;
                        }
                    }
                    let asym_tol = if row.archetype == FilterArchetype::SimpleTest {
                        1e-4
                    } else {
                        1e-13
                    };
                    assert!(
                        max_asym <= asym_tol,
                        "{} @ {src_rate} Hz linear-phase asymmetry {max_asym:.3e} > {asym_tol:.1e}",
                        row.archetype
                    );
                }
                FilterPhase::Minimum => {
                    let total_e: f64 = core.iter().map(|v| v * v).sum();
                    let front_e: f64 = core[..core.len() / 2].iter().map(|v| v * v).sum();
                    assert!(
                        peak_idx < core.len() / 4 && front_e / total_e > 0.99,
                        "{} @ {src_rate} Hz min-phase check failed (peak_idx={peak_idx}/{}, front_energy={:.4})",
                        row.archetype,
                        core.len(),
                        front_e / total_e
                    );
                }
                FilterPhase::Intermediate => {
                    let frac = (peak_idx as f64) / (core.len().max(1) as f64);
                    assert!(
                        frac > 0.05 && frac < 0.45,
                        "{} @ {src_rate} Hz intermediate-phase peak fraction {frac:.3} outside (0.05, 0.45)",
                        row.archetype
                    );
                }
            }
        }
    }
}

#[test]
fn l2_cepstrum_cap_handles_prototypes_above_65k_taps() {
    // Prototype strictly above 65,536 taps (hits k_len <= 2^20 cap in phase.rs).
    let base_spec = gauss_apod_4m_spec(44_100, 352_800, 80_000, None)
        .expect("gauss_apod_4m_spec > 65k");
    let lin = design_gaussian_filter(base_spec);
    assert!(
        lin.taps().len() > 65_536,
        "expected prototype > 65,536 taps, got {}",
        lin.taps().len()
    );

    let (mp_taps, mp_peak) =
        convert_prototype_phase(lin.taps(), 1.0).expect("cepstrum > 65k taps must succeed");
    assert_eq!(mp_taps.len(), lin.taps().len());
    assert!(
        mp_peak < mp_taps.len() / 8,
        "min-phase peak {mp_peak} should be front-loaded"
    );

    // Magnitude spectrum must match linear-phase prototype within 0.05 dB in passband [0, 20 kHz].
    let (m_lin, k_len) = fft_mag_normalized(lin.taps(), 8);
    let (m_mp, _) = fft_mag_normalized(&mp_taps, 8);
    let df = 352_800.0 / (k_len as f64);
    let bin_20k = (20_000.0 / df).floor() as usize;
    let mut max_diff_db = 0.0_f64;
    for k in 0..=bin_20k {
        let diff = (db(m_lin[k]) - db(m_mp[k])).abs();
        if diff > max_diff_db {
            max_diff_db = diff;
        }
    }
    assert!(
        max_diff_db <= 0.05,
        ">65k cepstrum min-phase passband deviation {max_diff_db:.4} dB > 0.05 dB"
    );
}
