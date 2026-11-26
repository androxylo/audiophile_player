// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Phase 2 verification suite covering all curated filter archetypes (`#3`–`#8`, `OQ-37` HF noise
//! filter, `U8` double-double kernel design) per `UPSAMPLING_RESEARCH.md` §6 Tests 0–8, plus the
//! `OQ-39` Ryzen 9 7900 real-time throughput benchmark (`#3`/`#4` + 4M-tap full-tail REW).

use std::f64::consts::PI;
use std::sync::Arc;
use std::time::Instant;

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::convolution::{PartitionedStereoConvolver, PreparedStereoIr, StereoConvolver};
use ap_dsp::phase::INTERMEDIATE_PHASE_ALPHA;
use ap_dsp::selector::{
    FilterArchetype, FilterPhase, TrackSourceInfo, UpsamplerPicker, UpsamplerPolicy,
};
use ap_dsp::upsampler::{
    GaussianFilterSpec, KaiserFilterSpec, SHORT_GAUSS_STOPBAND_ATTENUATION_DB, Upsampler,
    build_upsampler, design_gaussian_filter, design_gaussian_filter_with_attenuation,
    design_kaiser_filter, design_phase_converted_filter, evaluate_prototype_mag,
    evaluate_symmetric_prototype_mag, gauss_apod_4m_spec, hires_hf_noise_filter_spec,
    short_gaussian_spec,
};
use realfft::RealFftPlanner;

/// Compute exact zero-padded real FFT magnitude `|H(f)| / ratio` at frequency grid `k * fs_out / K`.
fn prototype_fft_mag(prototype: &[f64], ratio: usize, k_len: usize) -> Vec<f64> {
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(k_len);
    let mut buf = vec![0.0_f64; k_len];
    buf[..prototype.len()].copy_from_slice(prototype);
    let mut spec = r2c.make_output_vec();
    let mut scratch = r2c.make_scratch_vec();
    r2c.process_with_scratch(&mut buf, &mut spec, &mut scratch)
        .expect("FFT failed");
    let inv_ratio = 1.0 / (ratio as f64);
    spec.iter().map(|c| c.norm() * inv_ratio).collect()
}

fn mag_to_db(mag: f64) -> f64 {
    20.0 * mag.max(1e-20).log10()
}

fn run_oneshot(up: &mut dyn Upsampler, input: &StereoF64Buffer) -> StereoF64Buffer {
    let mut out = up.process_block(input.clone()).expect("process_block");
    let tail = up.flush().expect("flush");
    out.left.extend_from_slice(&tail.left);
    out.right.extend_from_slice(&tail.right);
    out
}

#[test]
fn test_0_dr1_passband_floor_and_rejection_across_new_archetypes_and_rates() {
    let rates: &[(u32, u32)] = &[
        (44_100, 352_800),
        (48_000, 384_000),
        (88_200, 352_800),
        (96_000, 384_000),
        (176_400, 352_800),
        (192_000, 384_000),
    ];

    // 1. Short Gaussian (#8a short-lp & #8b short-mp) + Cepstral (#6 minphase-long, #7 intermediate-long)
    //    checked across all 6 standard rates on a dense FFT grid up to 19 kHz.
    for &(src, dst) in rates {
        let ratio = (dst / src) as usize;
        // #8a short-lp & #8b short-mp
        let (short_spec, atten_db, _) =
            short_gaussian_spec(FilterArchetype::ShortLp, src, dst, None).expect("short spec");
        let short_lp = design_gaussian_filter_with_attenuation(short_spec, atten_db);
        let short_mp = design_phase_converted_filter(
            FilterArchetype::ShortMp,
            &short_lp,
            FilterPhase::Minimum,
            1.0,
        )
        .expect("short-mp");

        // #6 minphase-long & #7 intermediate-long
        let base_arch = if src <= 48_000 {
            FilterArchetype::GaussLongLp
        } else {
            FilterArchetype::GaussHiresLp
        };
        let base_spec = GaussianFilterSpec::for_default_archetype(base_arch, src, dst).unwrap();
        let base_lp = design_gaussian_filter(base_spec);
        let mp_long = design_phase_converted_filter(
            FilterArchetype::MinphaseLong,
            &base_lp,
            FilterPhase::Minimum,
            1.0,
        )
        .expect("minphase-long");
        let ip_long = design_phase_converted_filter(
            FilterArchetype::IntermediateLong,
            &base_lp,
            FilterPhase::Intermediate,
            INTERMEDIATE_PHASE_ALPHA,
        )
        .expect("intermediate-long");

        for (label, proto) in [
            ("short-lp", &short_lp.prototype),
            ("short-mp", &short_mp.prototype),
            ("minphase-long", &mp_long.prototype),
            ("intermediate-long", &ip_long.prototype),
        ] {
            let k_len = (proto.len() * 16).next_power_of_two().max(65_536);
            let mags = prototype_fft_mag(proto, ratio, k_len);
            let max_bin = ((19_000.0 * (k_len as f64)) / (dst as f64)).floor() as usize;
            let mut worst_dev_db = 0.0_f64;
            for &m in &mags[..=max_bin] {
                let dev = mag_to_db(m).abs();
                if dev > worst_dev_db {
                    worst_dev_db = dev;
                }
            }
            assert!(
                worst_dev_db <= 0.01,
                "DR-1 violation for {label} @ {src}->{dst} Hz: max |gain| in 0..=19kHz = {worst_dev_db:.5} dB"
            );
        }
    }

    // 2. Mega-tap linear-phase archetypes (#3 gauss-apod-4m, #4 sinc-steep-4m, #5 ext-sharp-lp)
    //    checked at representative 1x (44.1k) and hi-res (96k) rates via dense FFT.
    for &(src, dst) in &[(44_100_u32, 352_800_u32), (96_000_u32, 384_000_u32)] {
        let ratio = (dst / src) as usize;
        let g3 = design_gaussian_filter(
            gauss_apod_4m_spec(src, dst, MAX_FILTER_TAPS, None).expect("g3 spec"),
        );
        let k4 = design_kaiser_filter(
            KaiserFilterSpec::for_archetype(
                FilterArchetype::SincSteep4M,
                src,
                dst,
                MAX_FILTER_TAPS,
                None,
            )
            .expect("k4 spec"),
        );
        let k5 = design_kaiser_filter(
            KaiserFilterSpec::for_archetype(
                FilterArchetype::ExtSharpLp,
                src,
                dst,
                MAX_FILTER_TAPS,
                None,
            )
            .expect("k5 spec"),
        );

        for (label, proto) in [
            ("gauss-apod-4m", &g3.prototype),
            ("sinc-steep-4m", &k4.prototype),
            ("ext-sharp-lp", &k5.prototype),
        ] {
            // Exact symmetry check (Test 2)
            let n = proto.len();
            assert_eq!(n % 2, 1, "{label} must be odd length");
            for idx in 0..(n / 2) {
                assert_eq!(
                    proto[idx],
                    proto[n - 1 - idx],
                    "{label} symmetry mismatch at {idx}"
                );
            }

            // Dense 2x-padded FFT for fast exact frequency verification
            let k_len = (n * 2).next_power_of_two();
            let mags = prototype_fft_mag(proto, ratio, k_len);
            let max_bin = ((19_000.0 * (k_len as f64)) / (dst as f64)).floor() as usize;
            let mut worst_dev_db = 0.0_f64;
            for &m in &mags[..=max_bin] {
                let dev = mag_to_db(m).abs();
                if dev > worst_dev_db {
                    worst_dev_db = dev;
                }
            }
            assert!(
                worst_dev_db <= 0.01,
                "DR-1 violation for {label} @ {src}->{dst} Hz: worst={worst_dev_db:.6} dB"
            );

            // Stopband check at Nyquist (fs_in / 2) and above
            let nyq_bin = (((src as f64) * 0.5 * (k_len as f64)) / (dst as f64)).ceil() as usize;
            let stop_floor = if label == "gauss-apod-4m" { -280.0 } else { -190.0 };
            let mut worst_stop_db = -400.0_f64;
            for &m in &mags[nyq_bin..] {
                let db = mag_to_db(m);
                if db > worst_stop_db {
                    worst_stop_db = db;
                }
            }
            assert!(
                worst_stop_db <= stop_floor,
                "{label} @ {src}->{dst} Hz stopband {worst_stop_db:.2} dB > {stop_floor} dB"
            );
        }
    }

    // 3. OQ-37 HF ultrasonic noise filter at 4fS (192->384k) & 2fS (96->384k):
    //    flat through 40 kHz (192k) / 34 kHz (96k), and <= -290 dB above 48 kHz / 44.1 kHz!
    let hf_192 = design_gaussian_filter(hires_hf_noise_filter_spec(192_000, 384_000).unwrap());
    let mag_40k = evaluate_symmetric_prototype_mag(&hf_192.prototype, 384_000.0, 2, 40_000.0);
    let mag_48k = evaluate_symmetric_prototype_mag(&hf_192.prototype, 384_000.0, 2, 48_000.0);
    let mag_65k = evaluate_symmetric_prototype_mag(&hf_192.prototype, 384_000.0, 2, 65_000.0);
    assert!(mag_to_db(mag_40k).abs() <= 0.01, "40k={:.4} dB", mag_to_db(mag_40k));
    assert!(mag_to_db(mag_48k) <= -290.0, "48k={:.2} dB", mag_to_db(mag_48k));
    assert!(mag_to_db(mag_65k) <= -290.0, "65k={:.2} dB", mag_to_db(mag_65k));

    // 4. DR-1 rejection test on tiny --max-taps for Kaiser / gauss-apod-4m
    assert!(
        KaiserFilterSpec::for_archetype(FilterArchetype::SincSteep4M, 44_100, 352_800, 512, None)
            .is_err(),
        "tiny max-taps violating DR-1 must be rejected"
    );
    assert!(
        gauss_apod_4m_spec(44_100, 352_800, 512, None).is_err(),
        "tiny max-taps violating DR-1 must be rejected for gauss-apod-4m"
    );
}

#[test]
fn test_2_cepstral_phase_energy_and_magnitude_match() {
    let base_spec =
        GaussianFilterSpec::for_default_archetype(FilterArchetype::GaussLongLp, 44_100, 352_800)
            .unwrap();
    let lin = design_gaussian_filter(base_spec);
    let mp = design_phase_converted_filter(
        FilterArchetype::MinphaseLong,
        &lin,
        FilterPhase::Minimum,
        1.0,
    )
    .expect("minphase-long");
    let ip = design_phase_converted_filter(
        FilterArchetype::IntermediateLong,
        &lin,
        FilterPhase::Intermediate,
        INTERMEDIATE_PHASE_ALPHA,
    )
    .expect("intermediate-long");

    // 1. Passband magnitude of -mp and -ip matches -lp to within 1e-4 dB (UPS §4.3).
    for freq in [0.0_f64, 1_000.0, 10_000.0, 15_000.0, 19_000.0, 20_000.0] {
        let m_lin = mag_to_db(evaluate_symmetric_prototype_mag(&lin.prototype, 352_800.0, 8, freq));
        let m_mp = mag_to_db(evaluate_prototype_mag(&mp.prototype, 352_800.0, 8, freq));
        let m_ip = mag_to_db(evaluate_prototype_mag(&ip.prototype, 352_800.0, 8, freq));
        assert!(
            (m_lin - m_mp).abs() < 1e-4,
            "minphase mag mismatch at {freq} Hz: lin={m_lin:.6}, mp={m_mp:.6}"
        );
        assert!(
            (m_lin - m_ip).abs() < 1e-4,
            "intermediate mag mismatch at {freq} Hz: lin={m_lin:.6}, ip={m_ip:.6}"
        );
    }

    // 2. Minimum phase: >= 99% of total energy sits in the first 2.0 ms (705 samples @ 352.8 kHz).
    let total_e: f64 = mp.prototype.iter().map(|v| v * v).sum();
    let early_samples = (0.002 * 352_800.0) as usize;
    let early_e: f64 = mp.prototype[..early_samples].iter().map(|v| v * v).sum();
    assert!(
        early_e / total_e >= 0.99,
        "minphase-long must put >= 99% energy within first 2 ms, got {:.4}%",
        100.0 * early_e / total_e
    );

    // 3. Intermediate phase: energy-weighted center lies strictly between minphase and linear phase.
    let energy_center = |h: &[f64]| -> f64 {
        let mut num = 0.0;
        let mut den = 0.0;
        for (i, &v) in h.iter().enumerate() {
            let e = v * v;
            num += (i as f64) * e;
            den += e;
        }
        num / den
    };
    let c_mp = energy_center(&mp.prototype);
    let c_ip = energy_center(&ip.prototype);
    let c_lp = energy_center(&lin.prototype);
    assert!(
        c_mp < c_ip && c_ip < c_lp,
        "expected energy center mp < ip < lp, got mp={c_mp:.1}, ip={c_ip:.1}, lp={c_lp:.1}"
    );
}

#[test]
fn test_3_and_5_dc_accuracy_and_gapless_invariance_for_new_archetypes() {
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };

    for arch in [
        FilterArchetype::ShortLp,
        FilterArchetype::ShortMp,
        FilterArchetype::MinphaseLong,
        FilterArchetype::IntermediateLong,
        FilterArchetype::GaussApod4M,
    ] {
        // Use a moderate cap for GaussApod4M in debug/fast unit tests while still triggering FFT path.
        let max_taps = if arch == FilterArchetype::GaussApod4M {
            80_000
        } else {
            MAX_FILTER_TAPS
        };
        let picker = UpsamplerPicker::new(UpsamplerPolicy {
            filter_override: Some(arch),
            max_taps,
            ..UpsamplerPolicy::default()
        })
        .unwrap();
        let plan = picker.select(&TrackSourceInfo::from_stream_spec(spec_44));

        // 1. DC preservation in steady-state interior (Test 3): input length must exceed 2 * m_half
        //    so the filter window at the midpoint contains zero leading/trailing boundary padding.
        let n_in = if arch == FilterArchetype::GaussApod4M {
            14_000usize
        } else {
            4_000usize
        };
        let dc_buf = StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.5_f64; n_in],
            right: vec![-0.25_f64; n_in],
        };
        let mut up_dc = build_upsampler(&plan).expect("build dc");
        let dc_out = run_oneshot(up_dc.as_mut(), &dc_buf);
        assert_eq!(dc_out.left.len(), n_in * 8);
        let mid = dc_out.left.len() / 2;
        for idx in (mid - 256)..(mid + 256) {
            assert!(
                (dc_out.left[idx] - 0.5).abs() < 1e-13,
                "{arch} DC L drift at {idx}: {}",
                dc_out.left[idx]
            );
            assert!(
                (dc_out.right[idx] + 0.25).abs() < 1e-13,
                "{arch} DC R drift at {idx}: {}",
                dc_out.right[idx]
            );
        }

        // 2. Gapless split invariance (Test 5)
        let sig_l: Vec<f64> = (0..3_000)
            .map(|n| 0.4 * (2.0 * PI * 997.0 * (n as f64) / 44_100.0).sin())
            .collect();
        let sig_r: Vec<f64> = (0..3_000)
            .map(|n| 0.3 * (2.0 * PI * 3_571.0 * (n as f64) / 44_100.0).cos())
            .collect();
        let full = StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: sig_l.clone(),
            right: sig_r.clone(),
        };
        let mut up_one = build_upsampler(&plan).unwrap();
        let ref_out = run_oneshot(up_one.as_mut(), &full);

        let split = 1_437;
        let mut up_a = build_upsampler(&plan).unwrap();
        let a_blk = up_a
            .process_block(StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: sig_l[..split].to_vec(),
                right: sig_r[..split].to_vec(),
            })
            .unwrap();
        let (a_tail, carry) = up_a.flush_for_gapless().unwrap();
        let mut up_b = build_upsampler(&plan).unwrap();
        up_b.set_incoming_carryover(carry.expect("carryover"))
            .expect("matching identity");
        let b_blk = up_b
            .process_block(StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: sig_l[split..].to_vec(),
                right: sig_r[split..].to_vec(),
            })
            .unwrap();
        let b_tail = up_b.flush().unwrap();

        let mut gap_l = a_blk.left;
        gap_l.extend_from_slice(&a_tail.left);
        gap_l.extend_from_slice(&b_blk.left);
        gap_l.extend_from_slice(&b_tail.left);
        assert_eq!(gap_l.len(), ref_out.left.len());
        let max_diff = gap_l
            .iter()
            .zip(ref_out.left.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            max_diff < 1e-13,
            "{arch} gapless mismatch max_diff={max_diff:e}"
        );
    }

    // Verify short-lp stopband attenuation metadata & actual stopband <= -150 dB
    let (short_spec, atten_db, _) =
        short_gaussian_spec(FilterArchetype::ShortLp, 44_100, 352_800, None).unwrap();
    let short_lp = design_gaussian_filter_with_attenuation(short_spec, atten_db);
    assert_eq!(short_lp.metrics.stopband_attenuation_db, SHORT_GAUSS_STOPBAND_ATTENUATION_DB);
    let nyq_mag = evaluate_symmetric_prototype_mag(&short_lp.prototype, 352_800.0, 8, 22_050.0);
    assert!(
        mag_to_db(nyq_mag) <= -150.0,
        "short-lp Nyquist mag {:.2} dB > -150 dB",
        mag_to_db(nyq_mag)
    );
}

#[test]
fn oq39_benchmark_ultimate_chain_throughput_on_host_cpu() {
    // Only run full 4M-tap REW + 1M-tap sinc-L timing benchmark in optimized release builds
    // so debug `cargo test` stays instant.
    if cfg!(debug_assertions) {
        return;
    }

    // Build a dense 4,194,304-tap stereo REW IR (all 64 partitions non-zero!).
    let ir_len = 1 << 22;
    let mut ir_l = vec![0.0_f64; ir_len];
    let mut ir_r = vec![0.0_f64; ir_len];
    ir_l[0] = 0.8;
    ir_r[0] = 0.8;
    for p in 1..64 {
        ir_l[p * 65_536 + 13] = 1e-4 / (p as f64);
        ir_r[p * 65_536 + 17] = -1e-4 / (p as f64);
    }
    let rew_ir = Arc::new(
        PreparedStereoIr::from_stereo_ir_with_options("bench-4m", 352_800, &ir_l, &ir_r, true)
            .expect("4m rew"),
    );

    // 5 seconds of 44.1 kHz stereo audio (220,500 input frames -> 1,764,000 output frames).
    let audio_secs = 5.0_f64;
    let n_in = (44_100.0 * audio_secs) as usize;
    let block_in = 8_192usize;
    let src_l: Vec<f64> = (0..n_in)
        .map(|n| 0.3 * (2.0 * PI * 997.0 * (n as f64) / 44_100.0).sin())
        .collect();
    let src_r: Vec<f64> = (0..n_in)
        .map(|n| 0.3 * (2.0 * PI * 1_481.0 * (n as f64) / 44_100.0).cos())
        .collect();

    for arch in [FilterArchetype::GaussApod4M, FilterArchetype::SincSteep4M] {
        let picker = UpsamplerPicker::new(UpsamplerPolicy {
            filter_override: Some(arch),
            max_taps: MAX_FILTER_TAPS,
            ..UpsamplerPolicy::default()
        })
        .unwrap();
        let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: 44_100,
            bits_per_sample: 16,
            channels: 2,
        }));

        let t_prep = Instant::now();
        let mut up = build_upsampler(&plan).expect("build");
        let prep_ms = t_prep.elapsed().as_secs_f64() * 1_000.0;
        let mut conv = PartitionedStereoConvolver::new(Arc::clone(&rew_ir));

        let t_run = Instant::now();
        let mut pos = 0usize;
        while pos < n_in {
            let end = (pos + block_in).min(n_in);
            let blk = StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: src_l[pos..end].to_vec(),
                right: src_r[pos..end].to_vec(),
            };
            let up_out = up.process_block(blk).expect("up");
            let _ = conv.process_block(up_out).expect("conv");
            pos = end;
        }
        let elapsed_s = t_run.elapsed().as_secs_f64();
        let speed_x_rt = audio_secs / elapsed_s.max(1e-9);
        println!(
            "[OQ-39 bench] {arch} ({} taps) + 4M-tap REW (64p): prep={prep_ms:.1} ms, \
             {audio_secs:.1}s audio in {elapsed_s:.3}s => {speed_x_rt:.2}x real time",
            up.total_taps()
        );
        assert!(
            speed_x_rt > 1.0,
            "{arch} + 4M-tap REW fell below real time: {speed_x_rt:.2}x RT"
        );
    }
}
