//! Issue #5 §6 & Issue #6 §4 mathematical verification suite for the Phase 1 default Gaussian
//! windowed-sinc upsamplers (`gauss-long-lp` and `gauss-hires-lp`) and dummy `DefaultFilterSelector`.
//!
//! Covers:
//! - Test 0: DR-1 mandatory passband floor (`|H(f)|` within `±0.01 dB`, `0..=19 kHz` on dense grid
//!   with `<= 1 Hz` and `>= 64 pts/sigma_f` spacing) across all 6 standard source rates + metadata
//!   consistency + designer rejection of DR-1 violations.
//! - Test 1: Zero-padded `f64` FFT (`>= 16 * N`) shape verification (`-0.01 dB` edge, `-290+ dB`
//!   stopband at `fs_in / 2`, zero passband ripple above `1e-8 dB`).
//! - Test 2: Exact linear-phase prototype symmetry (`h[k] == h[N - 1 - k]` bit-for-bit).
//! - Test 3: DC constant preservation within `1e-15` across all 6 standard rates.
//! - Test 4: Coherent multitone ultrasonic image suppression `<= -290 dBFS` from `fs_in/2..fs_out/2`.
//! - Test 5: Random chunk-size invariance (`< 1e-14`), gapless splice invariance (`< 1e-14`), and
//!   strict `FilterIdentity` mismatch rejection (even for equal-length filters).
//! - Test 7: Diagnostic per-phase `L1` matches brute-force worst case; headroom remains strictly
//!   `G_up ∈ {-3, 0} dB` per `HEADROOM_DESIGN.md`.
//! - Test 8: Default selection without config (`44.1/48k -> gauss-long-lp`, `88.2–192k -> gauss-hires-lp`,
//!   `352.8/384k -> null`) and default `N` strictly below `sinc-steep-4m`.

use std::f64::consts::PI;

use realfft::RealFftPlanner;

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomMode, TrackPeakMetadata};
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::pipeline::{AudioPipeline, GaplessAttachOutcome};
use ap_dsp::selector::{
    FilterArchetype, FilterPhase, TrackSourceInfo, UpsampleRatio, UpsamplerPicker, UpsamplerPolicy,
};
use ap_dsp::upsampler::{
    DR1_MIN_PASSBAND_HZ, GaussianFilterSpec, PolyphaseFirUpsampler, Upsampler, build_upsampler,
    design_gaussian_filter, evaluate_symmetric_prototype_mag,
};

const ALL_SIX_STANDARD_RATES: &[(u32, u32)] = &[
    (44_100, 352_800),
    (48_000, 384_000),
    (88_200, 352_800),
    (96_000, 384_000),
    (176_400, 352_800),
    (192_000, 384_000),
];

/// Compute normalized magnitude spectrum `|H(f)|` (DC = `1.0`) of `prototype` zero-padded to
/// `fft_len` (`>= 16 * N`, power of 2, bin spacing `< 1 Hz` at 384 kHz).
fn zero_padded_response_mag(prototype: &[f64], ratio: usize, fft_len: usize) -> Vec<f64> {
    assert!(fft_len >= prototype.len() * 16);
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(fft_len);
    let mut input = vec![0.0_f64; fft_len];
    input[..prototype.len()].copy_from_slice(prototype);
    let mut spectrum = r2c.make_output_vec();
    r2c.process(&mut input, &mut spectrum).unwrap();
    let inv_ratio = 1.0 / (ratio as f64);
    spectrum.iter().map(|c| c.norm() * inv_ratio).collect()
}

fn mag_to_db(m: f64) -> f64 {
    20.0 * m.max(1e-20).log10()
}

fn run_oneshot_upsampler(up: &mut dyn Upsampler, input: StereoF64Buffer) -> StereoF64Buffer {
    let mut out = up.process_block(input).expect("process_block failed");
    let tail = up.flush().expect("flush failed");
    out.left.extend(tail.left);
    out.right.extend(tail.right);
    out
}

/// Issue #5 §6 Test 0: DR-1 passband floor (`0..=19 kHz` within `±0.01 dB`), metadata match,
/// and strict designer rejection of DR-1 violations.
#[test]
fn test_0_dr1_passband_floor_and_metadata_match_across_all_rates_and_rejects_violations() {
    let fft_len = 524_288_usize; // bin spacing 0.673 Hz @ 352.8k, 0.732 Hz @ 384k (<= 1 Hz & >= 64 pts/sigma_f)

    // Check every valid (archetype, source_rate, target_rate) pair.
    let mut cases = Vec::new();
    for &(src, dst) in ALL_SIX_STANDARD_RATES {
        cases.push((FilterArchetype::GaussLongLp, src, dst));
        if src >= 48_000 {
            cases.push((FilterArchetype::GaussHiresLp, src, dst));
        }
    }

    for (arch, src, dst) in cases {
        let spec = GaussianFilterSpec::for_default_archetype(arch, src, dst)
            .unwrap_or_else(|e| panic!("expected valid spec for {arch} {src}->{dst}: {e}"));
        let designed = design_gaussian_filter(spec);
        let mags = zero_padded_response_mag(&designed.prototype, spec.ratio, fft_len);
        let bin_hz = (dst as f64) / (fft_len as f64);
        assert!(
            bin_hz <= 1.0 && bin_hz <= spec.sigma_f_hz / 64.0,
            "grid spacing {bin_hz:.4} Hz must be <= 1 Hz and <= sigma_f/64 ({:.4} Hz)",
            spec.sigma_f_hz / 64.0
        );

        let max_bin_19k = (DR1_MIN_PASSBAND_HZ / bin_hz).floor() as usize;
        let mut worst_err_db = 0.0_f64;
        for &m in &mags[..=max_bin_19k] {
            let err_db = mag_to_db(m).abs();
            if err_db > worst_err_db {
                worst_err_db = err_db;
            }
        }
        // Also check exact 19,000.0 Hz via DTFT.
        let mag_19k = evaluate_symmetric_prototype_mag(
            &designed.prototype,
            dst as f64,
            spec.ratio,
            DR1_MIN_PASSBAND_HZ,
        );
        worst_err_db = worst_err_db.max(mag_to_db(mag_19k).abs());

        assert!(
            worst_err_db <= 0.01,
            "DR-1 FAILED for {arch} {src}->{dst}: max |20 log10|H|| in 0..19 kHz = {worst_err_db:.6} dB > 0.01 dB"
        );

        // Verify metadata matches exact measured response:
        // 1. -0.01 dB edge
        let mag_at_edge = evaluate_symmetric_prototype_mag(
            &designed.prototype,
            dst as f64,
            spec.ratio,
            designed.metrics.passband_edge_001db_hz,
        );
        let db_at_edge = mag_to_db(mag_at_edge);
        assert!(
            (db_at_edge - (-0.01)).abs() < 0.001,
            "{arch} {src}->{dst}: response at reported -0.01 dB edge ({:.2} Hz) is {db_at_edge:.6} dB",
            designed.metrics.passband_edge_001db_hz
        );

        // 2. Level at 20 kHz
        let reported_20k = designed
            .metrics
            .level_at_20khz_db
            .expect("all standard rates > 40 kHz must report 20 kHz level");
        let bin_20k = (20_000.0 / bin_hz).round() as usize;
        let fft_20k_db = mag_to_db(mags[bin_20k]);
        assert!(
            (reported_20k - fft_20k_db).abs() < 0.005,
            "{arch} {src}->{dst}: reported 20 kHz ({reported_20k:.6} dB) vs FFT ({fft_20k_db:.6} dB)"
        );

        // 3. Stopband edge
        let mag_at_stop = evaluate_symmetric_prototype_mag(
            &designed.prototype,
            dst as f64,
            spec.ratio,
            designed.metrics.stopband_edge_hz,
        );
        let db_at_stop = mag_to_db(mag_at_stop);
        assert!(
            db_at_stop <= -290.0,
            "{arch} {src}->{dst}: response at stopband edge ({:.1} Hz) is {db_at_stop:.2} dB (expected <= -290 dB)",
            designed.metrics.stopband_edge_hz
        );
    }

    // Designer MUST reject DR-1 violations rather than silently clamping!
    // 1. Forcing gauss-hires-lp onto 44.1 kHz (0.42 * 44,100 = 18,522 Hz < 19,000 Hz)
    let err_hires_44 =
        GaussianFilterSpec::for_default_archetype(FilterArchetype::GaussHiresLp, 44_100, 352_800)
            .expect_err("gauss-hires-lp at 44.1 kHz violates DR-1 and must be rejected");
    assert!(
        err_hires_44.contains("DR-1 violation"),
        "expected DR-1 violation error, got: {err_hires_44}"
    );

    // 2. Explicit passband edge below 19 kHz
    let err_low_pass = GaussianFilterSpec::from_pass_stop(
        FilterArchetype::GaussLongLp,
        44_100,
        352_800,
        18_900.0,
        22_050.0,
        None,
    )
    .expect_err("f_pass = 18,900 Hz violates DR-1 and must be rejected");
    assert!(err_low_pass.contains("DR-1 violation"));

    // 3. Explicit (fc, sigma_f) with fc - 3.05 * sigma_f < 19,000 Hz
    let err_fc_sigma = GaussianFilterSpec::from_fc_sigma(
        FilterArchetype::GaussLongLp,
        48_000,
        384_000,
        19_500.0,
        300.0, // 19,500 - 3.05*300 = 18,585 Hz < 19,000 Hz
        None,
    )
    .expect_err("fc - 3.05*sigma_f < 19 kHz must be rejected");
    assert!(err_fc_sigma.contains("DR-1 violation"));
}

/// Issue #5 §6 Test 1: Shape verification on >= 16*N zero-padded f64 FFT (edges, stopband,
/// monotonic zero-ripple passband < 1e-8 dB).
#[test]
fn test_1_shape_edges_stopband_and_ripple_free_passband() {
    let fft_len = 524_288_usize;

    let shape_cases = [
        (FilterArchetype::GaussLongLp, 44_100_u32, 352_800_u32, 20_000.0_f64, 22_050.0_f64),
        (
            FilterArchetype::GaussLongLp,
            48_000,
            384_000,
            48_000.0 * (20_000.0 / 44_100.0),
            24_000.0,
        ),
        (FilterArchetype::GaussHiresLp, 88_200, 352_800, 0.42 * 88_200.0, 44_100.0),
        (FilterArchetype::GaussHiresLp, 96_000, 384_000, 0.42 * 96_000.0, 48_000.0),
        (FilterArchetype::GaussHiresLp, 192_000, 384_000, 0.42 * 192_000.0, 96_000.0),
    ];

    for (arch, src, dst, expected_pass_hz, expected_stop_hz) in shape_cases {
        let spec = GaussianFilterSpec::for_default_archetype(arch, src, dst).unwrap();
        let designed = design_gaussian_filter(spec);
        assert_eq!(designed.metrics.phase, FilterPhase::Linear);
        assert_eq!(designed.prototype.len() % 2, 1, "prototype N must be odd");
        assert!(
            (designed.metrics.passband_edge_001db_hz - expected_pass_hz).abs() < 1e-6,
            "{arch} {src}: passband edge mismatch"
        );
        assert!(
            designed.metrics.stopband_edge_hz <= (src as f64) * 0.5 + 1e-9,
            "{arch} {src}: stopband must be at or below fs_in/2"
        );
        assert!((designed.metrics.stopband_edge_hz - expected_stop_hz).abs() < 1e-6);

        let mags = zero_padded_response_mag(&designed.prototype, spec.ratio, fft_len);
        let bin_hz = (dst as f64) / (fft_len as f64);

        // Passband ripple check: Gaussian error-function roll-off is strictly monotonic (no ripple > 1e-8 dB)
        // and never rises above +1e-8 dB anywhere in [0, expected_pass_hz].
        let pass_end_bin = (expected_pass_hz / bin_hz).floor() as usize;
        let mut prev_db = mag_to_db(mags[0]);
        assert!(prev_db.abs() < 1e-12, "DC must be 0.0 dB, got {prev_db}");
        for (b, &m) in mags[1..=pass_end_bin].iter().enumerate() {
            let db = mag_to_db(m);
            assert!(
                db <= 1e-8,
                "{arch} {src} bin {}: passband overshoot {db:.3e} dB > 1e-8 dB",
                b + 1
            );
            assert!(
                db <= prev_db + 1e-8,
                "{arch} {src} bin {}: non-monotonic passband ripple ({prev_db:.9} -> {db:.9} dB)",
                b + 1
            );
            prev_db = db;
        }

        // Stopband check: every bin from fs_in/2 to fs_out/2 must be <= -290 dB.
        let stop_start_bin = (expected_stop_hz / bin_hz).ceil() as usize;
        let worst_stop_db = mags[stop_start_bin..]
            .iter()
            .map(|&m| mag_to_db(m))
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(
            worst_stop_db <= -290.0,
            "{arch} {src}: worst stopband bin from fs_in/2..fs_out/2 is {worst_stop_db:.2} dB (expected <= -290 dB)"
        );
    }
}

/// Issue #5 §6 Test 2: Linear-phase prototype is bit-for-bit symmetric (`h[k] == h[N - 1 - k]`).
#[test]
fn test_2_linear_phase_prototype_is_bit_exact_symmetric() {
    for &(src, dst) in ALL_SIX_STANDARD_RATES {
        let arch = if src <= 48_000 {
            FilterArchetype::GaussLongLp
        } else {
            FilterArchetype::GaussHiresLp
        };
        let spec = GaussianFilterSpec::for_default_archetype(arch, src, dst).unwrap();
        let designed = design_gaussian_filter(spec);
        let n = designed.prototype.len();
        assert_eq!(n % 2, 1);

        for k in 0..(n / 2) {
            assert_eq!(
                designed.prototype[k],
                designed.prototype[n - 1 - k],
                "{arch} {src}->{dst}: asymmetry at k={k} (n={n})"
            );
        }

        // Also check per-phase exact DC unity sum to machine precision (outside-in sum).
        for (p, coeffs) in designed.bank.iter().enumerate() {
            let m = coeffs.len();
            let half = m / 2;
            let mut s = 0.0_f64;
            for i in 0..half {
                s += coeffs[i] + coeffs[m - 1 - i];
            }
            if (m & 1) == 1 {
                s += coeffs[half];
            }
            assert!(
                (s - 1.0).abs() < 1e-15,
                "{arch} {src}->{dst} phase {p} DC sum = {s} (err={:.3e})",
                (s - 1.0).abs()
            );
        }
    }
}

/// Issue #5 §6 Test 3: Constant DC input equals output to within `1e-15` at every standard rate.
#[test]
fn test_3_dc_preservation_within_1e_15_across_all_standard_rates() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    for &(src, dst) in ALL_SIX_STANDARD_RATES {
        let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: src,
            bits_per_sample: 24,
            channels: 2,
        }));
        let mut up = build_upsampler(&plan).unwrap();
        let metrics = up.design_metrics().unwrap();
        let ratio = (dst / src) as usize;
        let margin_in = metrics.taps_per_phase;
        let n_in = margin_in * 2 + 128;

        let out = run_oneshot_upsampler(
            up.as_mut(),
            StereoF64Buffer {
                sample_rate_hz: src,
                left: vec![0.5_f64; n_in],
                right: vec![-0.75_f64; n_in],
            },
        );
        assert_eq!(out.sample_rate_hz, dst);
        assert_eq!(out.left.len(), n_in * ratio);

        let margin_out = margin_in * ratio;
        for i in margin_out..(out.left.len() - margin_out) {
            assert!(
                (out.left[i] - 0.5).abs() < 1e-15,
                "rate {src} L DC error at {i}: {}",
                (out.left[i] - 0.5).abs()
            );
            assert!(
                (out.right[i] + 0.75).abs() < 1e-15,
                "rate {src} R DC error at {i}: {}",
                (out.right[i] + 0.75).abs()
            );
        }
    }
}

/// Issue #5 §6 Test 4: Coherent multitones (`1, 10, 20, 21.9 kHz` at 44.1k; scaled at 96k)
/// produce ultrasonic bins `<= -290 dBFS` across the entire `fs_in/2 .. fs_out/2` band.
#[test]
fn test_4_coherent_multitone_ultrasonic_images_below_minus_290_dbfs() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    // Case A: 44.1 kHz -> 352.8 kHz (gauss-long-lp), coherent tones at 1, 10, 20, 21.9 kHz.
    // N_sig = 4410 -> bin spacing 10 Hz -> 1000, 10000, 20000, 21900 Hz are exact integer bins.
    {
        let src = 44_100_u32;
        let dst = 352_800_u32;
        let ratio = 8_usize;
        let n_sig = 4_410_usize;
        let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: src,
            bits_per_sample: 16,
            channels: 2,
        }));
        assert_eq!(plan.filter, FilterArchetype::GaussLongLp);
        let mut up = build_upsampler(&plan).unwrap();
        let margin_in = up.design_metrics().unwrap().taps_per_phase;
        let total_in = margin_in + n_sig + margin_in;

        let tones = [1_000.0_f64, 10_000.0, 20_000.0, 21_900.0];
        let amp = 0.25_f64; // sum <= 1.0
        let sig: Vec<f64> = (0..total_in)
            .map(|i| {
                let t = (i as f64) / (src as f64);
                tones.iter().map(|&f| amp * (2.0 * PI * f * t).sin()).sum()
            })
            .collect();

        let out = run_oneshot_upsampler(
            up.as_mut(),
            StereoF64Buffer {
                sample_rate_hz: src,
                left: sig.clone(),
                right: sig,
            },
        );
        let steady = &out.left[(margin_in * ratio)..((margin_in + n_sig) * ratio)];
        let n_out = steady.len();
        assert_eq!(n_out, n_sig * ratio);

        let mut planner = RealFftPlanner::<f64>::new();
        let r2c = planner.plan_fft_forward(n_out);
        let mut buf = steady.to_vec();
        let mut spec = r2c.make_output_vec();
        r2c.process(&mut buf, &mut spec).unwrap();

        let scale = 2.0 / (n_out as f64); // full-scale sine amplitude = 1.0 -> 0 dBFS
        let nyq_bin = (n_sig / 2) + 1; // strictly > fs_in/2 (22,050 Hz is bin 2205)
        // Check from fs_in/2 inclusive!
        let start_bin = n_sig / 2;
        assert!(start_bin < nyq_bin);
        let mut worst_db = f64::NEG_INFINITY;
        let mut worst_bin = start_bin;
        for (b, c) in spec.iter().enumerate().skip(start_bin) {
            let db = mag_to_db(c.norm() * scale);
            if db > worst_db {
                worst_db = db;
                worst_bin = b;
            }
        }
        assert!(
            worst_db <= -290.0,
            "44.1k gauss-long-lp worst ultrasonic bin {worst_bin} ({:.1} Hz) = {worst_db:.2} dBFS (expected <= -290 dBFS)",
            (worst_bin as f64) * (dst as f64) / (n_out as f64)
        );
    }

    // Case B: 96 kHz -> 384 kHz (gauss-hires-lp), coherent tones at 1, 10, 20, 40, 47.5 kHz.
    // N_sig = 4800 -> bin spacing 20 Hz -> all tones are exact integer bins.
    {
        let src = 96_000_u32;
        let dst = 384_000_u32;
        let ratio = 4_usize;
        let n_sig = 4_800_usize;
        let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: src,
            bits_per_sample: 24,
            channels: 2,
        }));
        assert_eq!(plan.filter, FilterArchetype::GaussHiresLp);
        let mut up = build_upsampler(&plan).unwrap();
        let margin_in = up.design_metrics().unwrap().taps_per_phase;
        let total_in = margin_in + n_sig + margin_in;

        let tones = [1_000.0_f64, 10_000.0, 20_000.0, 40_000.0, 47_500.0];
        let amp = 0.2_f64;
        let sig: Vec<f64> = (0..total_in)
            .map(|i| {
                let t = (i as f64) / (src as f64);
                tones.iter().map(|&f| amp * (2.0 * PI * f * t).sin()).sum()
            })
            .collect();

        let out = run_oneshot_upsampler(
            up.as_mut(),
            StereoF64Buffer {
                sample_rate_hz: src,
                left: sig.clone(),
                right: sig,
            },
        );
        let steady = &out.left[(margin_in * ratio)..((margin_in + n_sig) * ratio)];
        let n_out = steady.len();

        let mut planner = RealFftPlanner::<f64>::new();
        let r2c = planner.plan_fft_forward(n_out);
        let mut buf = steady.to_vec();
        let mut spec = r2c.make_output_vec();
        r2c.process(&mut buf, &mut spec).unwrap();

        let scale = 2.0 / (n_out as f64);
        let start_bin = n_sig / 2;
        let mut worst_db = f64::NEG_INFINITY;
        let mut worst_bin = start_bin;
        for (b, c) in spec.iter().enumerate().skip(start_bin) {
            let db = mag_to_db(c.norm() * scale);
            if db > worst_db {
                worst_db = db;
                worst_bin = b;
            }
        }
        assert!(
            worst_db <= -290.0,
            "96k gauss-hires-lp worst ultrasonic bin {worst_bin} ({:.1} Hz) = {worst_db:.2} dBFS (expected <= -290 dBFS)",
            (worst_bin as f64) * (dst as f64) / (n_out as f64)
        );
    }
}

/// Issue #5 §6 Test 5 & Issue #6 §4: Random chunk invariance, gapless splice invariance, and
/// strict `FilterIdentity` mismatch rejection (even for different filters with identical tap length!).
#[test]
fn test_5_chunk_invariance_gapless_splice_and_filter_identity_mismatch_rejection() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    for &(src, expected_filter) in &[
        (44_100_u32, FilterArchetype::GaussLongLp),
        (96_000_u32, FilterArchetype::GaussHiresLp),
    ] {
        let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: src,
            bits_per_sample: 24,
            channels: 2,
        }));
        assert_eq!(plan.filter, expected_filter);

        let n_total = 1_600_usize;
        let left: Vec<f64> = (0..n_total)
            .map(|i| {
                let t = (i as f64) / (src as f64);
                0.4 * (2.0 * PI * 997.0 * t).sin() + 0.3 * (2.0 * PI * 13_331.0 * t).cos()
            })
            .collect();
        let right: Vec<f64> = (0..n_total)
            .map(|i| {
                let t = (i as f64) / (src as f64);
                0.5 * (2.0 * PI * 4_441.0 * t).cos()
            })
            .collect();

        let ref_out = run_oneshot_upsampler(
            build_upsampler(&plan).unwrap().as_mut(),
            StereoF64Buffer {
                sample_rate_hz: src,
                left: left.clone(),
                right: right.clone(),
            },
        );

        // 1. Irregular/pseudo-random block sizes must match one-shot to < 1e-14 (bit-exact).
        let chunk_pattern = [1_usize, 7, 19, 3, 113, 257, 31, 509, 13, 64];
        let mut up = build_upsampler(&plan).unwrap();
        let mut got_l = Vec::new();
        let mut got_r = Vec::new();
        let mut pos = 0;
        let mut pat_idx = 0;
        while pos < n_total {
            let chunk = chunk_pattern[pat_idx % chunk_pattern.len()];
            pat_idx += 1;
            let end = (pos + chunk).min(n_total);
            let blk = up
                .process_block(StereoF64Buffer {
                    sample_rate_hz: src,
                    left: left[pos..end].to_vec(),
                    right: right[pos..end].to_vec(),
                })
                .unwrap();
            got_l.extend(blk.left);
            got_r.extend(blk.right);
            pos = end;
        }
        let tail = up.flush().unwrap();
        got_l.extend(tail.left);
        got_r.extend(tail.right);

        assert_eq!(got_l.len(), ref_out.left.len());
        let max_diff_l = got_l
            .iter()
            .zip(ref_out.left.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        let max_diff_r = got_r
            .iter()
            .zip(ref_out.right.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        assert!(max_diff_l < 1e-14, "chunk invariance L diff={max_diff_l}");
        assert!(max_diff_r < 1e-14, "chunk invariance R diff={max_diff_r}");

        // 2. Gapless split at n_a = 800 must match continuous one-shot to < 1e-14 AND
        //    carry_a.flush_to_f64() must match standalone Track A flush bit-for-bit!
        let n_a = 800_usize;
        let mut up_a = build_upsampler(&plan).unwrap();
        let mut gap_l = Vec::new();
        let mut gap_r = Vec::new();
        let blk_a = up_a
            .process_block(StereoF64Buffer {
                sample_rate_hz: src,
                left: left[..n_a].to_vec(),
                right: right[..n_a].to_vec(),
            })
            .unwrap();
        gap_l.extend(blk_a.left);
        gap_r.extend(blk_a.right);
        let (tail_a, carry_a) = up_a.flush_for_gapless().unwrap();
        gap_l.extend(tail_a.left);
        gap_r.extend(tail_a.right);
        let carry_a = carry_a.expect("expected carryover from Gaussian upsampler");

        // Verify standalone drain of Track A's carryover equals one-shot Track A bit-for-bit.
        let oneshot_a = run_oneshot_upsampler(
            build_upsampler(&plan).unwrap().as_mut(),
            StereoF64Buffer {
                sample_rate_hz: src,
                left: left[..n_a].to_vec(),
                right: right[..n_a].to_vec(),
            },
        );
        let drained_a = carry_a.clone().flush_to_f64();
        assert_eq!(gap_l.len() + drained_a.len(), oneshot_a.len());
        assert_eq!(&drained_a.left[..], &oneshot_a.left[gap_l.len()..]);

        // Hand carryover to equal-filter Track B.
        let mut up_b = build_upsampler(&plan).unwrap();
        up_b.set_incoming_carryover(carry_a)
            .expect("equal-identity upsampler must accept carryover");
        let blk_b = up_b
            .process_block(StereoF64Buffer {
                sample_rate_hz: src,
                left: left[n_a..].to_vec(),
                right: right[n_a..].to_vec(),
            })
            .unwrap();
        gap_l.extend(blk_b.left);
        gap_r.extend(blk_b.right);
        let tail_b = up_b.flush().unwrap();
        gap_l.extend(tail_b.left);
        gap_r.extend(tail_b.right);

        assert_eq!(gap_l.len(), ref_out.left.len());
        let gap_diff_l = gap_l
            .iter()
            .zip(ref_out.left.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        let gap_diff_r = gap_r
            .iter()
            .zip(ref_out.right.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        assert!(gap_diff_l < 1e-14, "gapless L diff={gap_diff_l}");
        assert!(gap_diff_r < 1e-14, "gapless R diff={gap_diff_r}");
    }

    // 3. Issue #6 §4: Two DIFFERENT filters with IDENTICAL (rate, ratio, num_taps, taps_per_phase)
    //    must NEVER splice their FIR histories!
    let spec_a = GaussianFilterSpec::from_fc_sigma(
        FilterArchetype::GaussLongLp,
        44_100,
        352_800,
        20_500.0,
        186.0,
        None,
    )
    .unwrap();
    let spec_b = GaussianFilterSpec::from_fc_sigma(
        FilterArchetype::GaussLongLp,
        44_100,
        352_800,
        20_600.0, // different fc, exact same sigma_f -> IDENTICAL num_taps & taps_per_phase!
        186.0,
        None,
    )
    .unwrap();
    assert_eq!(spec_a.num_taps, spec_b.num_taps);

    let mut up_a = PolyphaseFirUpsampler::from_gaussian(design_gaussian_filter(spec_a));
    let mut up_b = PolyphaseFirUpsampler::from_gaussian(design_gaussian_filter(spec_b));
    assert_eq!(up_a.total_taps(), up_b.total_taps());
    assert_ne!(up_a.identity(), up_b.identity());

    let _ = up_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.5; 512],
            right: vec![0.5; 512],
        })
        .unwrap();
    let (_, carry_a) = up_a.flush_for_gapless().unwrap();
    let carry_a = carry_a.unwrap();

    assert!(
        up_b.set_incoming_carryover(carry_a).is_err(),
        "different FilterIdentity with identical tap length MUST be rejected"
    );

    // 4. Verify AudioPipeline::attach_gapless_carryover cleanly falls back to ConvolverOnlyHandoff
    //    when Track A (gauss-long-lp) and Track B (simple-test) share the exact same 44.1k->352.8k rate!
    let pipe_gauss = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let pipe_simple = AudioPipeline::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SimpleTest),
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let mut sess_a = pipe_gauss
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .unwrap();
    let _ = sess_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.5; 512],
            right: vec![-0.5; 512],
        })
        .unwrap();
    let (_, gap_carry) = sess_a.flush_for_gapless().unwrap();
    let gap_carry = gap_carry.unwrap();

    let mut sess_b = pipe_simple
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .unwrap();
    let outcome = pipe_simple
        .attach_gapless_carryover(&mut sess_b, gap_carry)
        .unwrap();
    match outcome {
        GaplessAttachOutcome::ConvolverOnlyHandoff {
            upsampler_tail_pcm, ..
        } => {
            assert!(
                !upsampler_tail_pcm.is_empty(),
                "different filter at same rate must drain Track A's upsampler tail cleanly"
            );
        }
        other => panic!("expected ConvolverOnlyHandoff on filter switch, got {other:?}"),
    }
}

/// Issue #5 §6 Test 7: Diagnostic per-phase L1 equals brute-force worst-case `±1` input;
/// headroom policy stays strictly `G_up ∈ {-3, 0} dB` per `HEADROOM_DESIGN.md`.
#[test]
fn test_7_diagnostic_l1_matches_brute_force_worst_case_and_headroom_policy_unchanged() {
    for &(arch, src, dst) in &[
        (FilterArchetype::GaussLongLp, 44_100_u32, 352_800_u32),
        (FilterArchetype::GaussHiresLp, 96_000_u32, 384_000_u32),
    ] {
        let spec = GaussianFilterSpec::for_default_archetype(arch, src, dst).unwrap();
        let designed = design_gaussian_filter(spec);

        // Brute-force worst-case output across all phases for input x[i] in {-1, +1}.
        let mut worst_case_out = 0.0_f64;
        for coeffs in designed.bank.iter() {
            let mut acc = 0.0_f64;
            for &c in coeffs {
                let x = if c >= 0.0 { 1.0_f64 } else { -1.0_f64 };
                acc += x * c;
            }
            if acc.abs() > worst_case_out {
                worst_case_out = acc.abs();
            }
        }

        assert!(
            (designed.metrics.l1_norm - worst_case_out).abs() < 1e-14,
            "{arch}: reported L1 ({}) != brute-force worst case ({worst_case_out})",
            designed.metrics.l1_norm
        );
        assert!(
            (designed.metrics.l1_norm_db - 20.0 * worst_case_out.log10()).abs() < 1e-12
        );
        assert!(
            designed.metrics.l1_norm > 1.0,
            "bandlimited sinc filter must have L1 > 1.0 (Gibbs/ISP overshoot)"
        );
    }

    // Verify HEADROOM_DESIGN.md §3: G_up ∈ {-3, 0} dB strictly from source peak (L1 is diagnostic only).
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let quiet = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-3.0 / 20.0)), // -3.0 dBFS -> 0 dB
        ..TrackPeakMetadata::default()
    };
    let loud = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-2.9 / 20.0)), // -2.9 dBFS -> -3 dB
        ..TrackPeakMetadata::default()
    };

    let s_quiet = pipeline.start_track(spec_44, &quiet, false).unwrap();
    assert_eq!(s_quiet.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(s_quiet.report().headroom.mode.gain_db(), 0.0);
    assert!(s_quiet.report().filter_metrics.unwrap().l1_norm_db > 0.0);

    let s_loud = pipeline.start_track(spec_44, &loud, false).unwrap();
    assert_eq!(s_loud.report().headroom.mode, HeadroomMode::Minus3Db);
    assert_eq!(s_loud.report().headroom.mode.gain_db(), -3.0);
}

/// Issue #5 §6 Test 8: Default selection without config and default N strictly below `sinc-steep-4m`.
#[test]
fn test_8_default_selection_and_length_below_sinc_steep_4m() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    // 44.1k & 48k -> gauss-long-lp
    for rate in [44_100_u32, 48_000] {
        let p = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: rate,
            bits_per_sample: 16,
            channels: 2,
        }));
        assert_eq!(p.filter, FilterArchetype::GaussLongLp);
        let up = build_upsampler(&p).unwrap();
        assert_eq!(up.total_taps(), p.tap_budget);
        let sinc_l_taps = FilterArchetype::SincSteep4M.planned_taps(
            rate,
            p.target_rate_hz,
            p.ratio,
            MAX_FILTER_TAPS,
        );
        assert!(
            up.total_taps() < sinc_l_taps,
            "default gauss-long-lp ({}) must be shorter than sinc-steep-4m ({sinc_l_taps})",
            up.total_taps()
        );
    }

    // 88.2k, 96k, 176.4k, 192k -> gauss-hires-lp
    for rate in [88_200_u32, 96_000, 176_400, 192_000] {
        let p = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: rate,
            bits_per_sample: 24,
            channels: 2,
        }));
        assert_eq!(p.filter, FilterArchetype::GaussHiresLp);
        let up = build_upsampler(&p).unwrap();
        assert_eq!(up.total_taps(), p.tap_budget);
        let sinc_l_taps = FilterArchetype::SincSteep4M.planned_taps(
            rate,
            p.target_rate_hz,
            p.ratio,
            MAX_FILTER_TAPS,
        );
        assert!(
            up.total_taps() < sinc_l_taps,
            "default gauss-hires-lp ({}) must be shorter than sinc-steep-4m ({sinc_l_taps})",
            up.total_taps()
        );
    }

    // 352.8k & 384k -> null (1:1 bypass)
    for rate in [352_800_u32, 384_000] {
        let p = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: rate,
            bits_per_sample: 32,
            channels: 2,
        }));
        assert_eq!(p.filter, FilterArchetype::Null);
        assert_eq!(p.ratio, UpsampleRatio::Bypass1x);
        assert_eq!(p.tap_budget, 0);
    }
}

/// Release-only real-time throughput regression guard: ensures default Gaussian polyphase
/// upsamplers sustain a comfortable margin (`>= 2.0x` real-time) on production builds so an
/// accidental inner-loop regression cannot starve the WaveRT audio thread.
#[cfg(not(debug_assertions))]
#[test]
fn test_9_release_realtime_throughput_margin_exceeds_2x() {
    use std::time::Instant;

    for (archetype, src_rate, dst_rate) in [
        (FilterArchetype::GaussLongLp, 44_100_u32, 352_800_u32),
        (FilterArchetype::GaussHiresLp, 88_200_u32, 352_800_u32),
    ] {
        let spec = GaussianFilterSpec::for_default_archetype(archetype, src_rate, dst_rate).unwrap();
        let mut up = PolyphaseFirUpsampler::from_gaussian(design_gaussian_filter(spec));
        let audio_secs = 2.0_f64;
        let total_in = (src_rate as usize) * 2;
        let block_in = 16_384_usize;
        let mut done = 0usize;
        let t0 = Instant::now();
        while done < total_in {
            let n = (total_in - done).min(block_in);
            let buf = StereoF64Buffer {
                sample_rate_hz: src_rate,
                left: vec![0.125; n],
                right: vec![-0.125; n],
            };
            let out = up.process_block(buf).unwrap();
            assert!(!out.is_empty() || done == 0);
            done += n;
        }
        let elapsed_s = t0.elapsed().as_secs_f64().max(1e-6);
        let rt_factor = audio_secs / elapsed_s;
        assert!(
            rt_factor >= 2.0,
            "{archetype} ({src_rate} -> {dst_rate} Hz) fell below 2.0x real-time in release mode: {rt_factor:.2}x ({elapsed_s:.3}s for {audio_secs}s audio)"
        );
    }
}
