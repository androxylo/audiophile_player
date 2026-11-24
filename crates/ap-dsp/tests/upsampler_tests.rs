//! TDD mathematical verification tests for the stateful streaming `f64` windowed-sinc
//! upsampler (`SimpleWindowedSincUpsampler`), zero-copy `NullUpsampler`, chunk-boundary
//! bit-exact equivalence, and final `f64 -> i32` TPDF quantizer.

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_dsp::quantize::{StreamingQuantizer, apply_safe_gain_f64, quantize_to_int32};
use ap_dsp::selector::{
    FilterArchetype, NoiseShaperProfile, SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE, TrackSourceInfo,
    UpsamplerPicker, UpsamplerPolicy,
};
use ap_dsp::upsampler::{SimpleWindowedSincUpsampler, build_upsampler};

fn run_oneshot(
    plan: &ap_dsp::selector::SelectedDspPlan,
    input: StereoF64Buffer,
) -> StereoF64Buffer {
    let mut up = build_upsampler(plan).expect("upsampler should build");
    let mut out = up.process_block(input).expect("process_block failed");
    let tail = up.flush().expect("flush failed");
    out.left.extend(tail.left);
    out.right.extend(tail.right);
    out
}

#[test]
fn simple_upsampler_outputs_float64_at_352800_and_384000_preserving_dc_exactly() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    for &(src_rate, expected_out_rate) in &[
        (44_100_u32, 352_800_u32),
        (48_000, 384_000),
        (88_200, 352_800),
        (96_000, 384_000),
        (176_400, 352_800),
        (192_000, 384_000),
        (384_000, 384_000),
    ] {
        let info = TrackSourceInfo::from_stream_spec(StreamSpec {
            sample_rate_hz: src_rate,
            bits_per_sample: 24,
            channels: 2,
        });
        let plan = picker.select(&info);

        let n_in = 512;
        let input = StereoF64Buffer {
            sample_rate_hz: src_rate,
            left: vec![0.5_f64; n_in],
            right: vec![-0.25_f64; n_in],
        };

        let out = run_oneshot(&plan, input);
        assert_eq!(out.sample_rate_hz, expected_out_rate);
        let ratio = (expected_out_rate / src_rate) as usize;
        assert_eq!(out.left.len(), n_in * ratio);
        assert_eq!(out.right.len(), n_in * ratio);

        // Interior samples (away from 256-tap/phase FIR edge transient) must preserve DC to < 1e-12
        let margin = 136 * ratio;
        for i in margin..(out.left.len() - margin) {
            assert!(
                (out.left[i] - 0.5).abs() < 1e-12,
                "DC drift L at rate {src_rate} idx {i}: {}",
                out.left[i]
            );
            assert!(
                (out.right[i] + 0.25).abs() < 1e-12,
                "DC drift R at rate {src_rate} idx {i}: {}",
                out.right[i]
            );
        }
    }
}

#[test]
fn streaming_chunked_upsampling_is_bit_exact_with_oneshot_across_arbitrary_chunk_sizes() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    }));

    let n_in = 1500;
    let left: Vec<f64> = (0..n_in)
        .map(|i| {
            let t = i as f64 / 44_100.0;
            0.35 * (2.0 * std::f64::consts::PI * 997.0 * t).sin()
                + 0.25 * (2.0 * std::f64::consts::PI * 6311.0 * t).cos()
        })
        .collect();
    let right: Vec<f64> = (0..n_in)
        .map(|i| {
            let t = i as f64 / 44_100.0;
            0.4 * (2.0 * std::f64::consts::PI * 1500.0 * t).cos()
        })
        .collect();

    let ref_out = run_oneshot(
        &plan,
        StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: left.clone(),
            right: right.clone(),
        },
    );
    assert_eq!(ref_out.left.len(), n_in * 8);

    // Stream in tiny uneven chunk sizes (17, 137, 512) and assert literally 0.0 difference!
    for chunk_size in [17_usize, 137, 512] {
        let mut up = build_upsampler(&plan).unwrap();
        let mut got_l = Vec::new();
        let mut got_r = Vec::new();

        let mut pos = 0;
        while pos < n_in {
            let end = (pos + chunk_size).min(n_in);
            let b = up
                .process_block(StereoF64Buffer {
                    sample_rate_hz: 44_100,
                    left: left[pos..end].to_vec(),
                    right: right[pos..end].to_vec(),
                })
                .unwrap();
            got_l.extend(b.left);
            got_r.extend(b.right);
            pos = end;
        }
        let tail = up.flush().unwrap();
        got_l.extend(tail.left);
        got_r.extend(tail.right);

        assert_eq!(got_l.len(), ref_out.left.len(), "chunk_size={chunk_size}");
        assert_eq!(got_l, ref_out.left, "L mismatch for chunk_size={chunk_size}");
        assert_eq!(got_r, ref_out.right, "R mismatch for chunk_size={chunk_size}");
    }
}

#[test]
fn simple_upsampler_reconstructs_1khz_sine_at_8x_352800_with_high_accuracy() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let info = TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    });
    let plan = picker.select(&info);
    assert_eq!(plan.filter, FilterArchetype::SimpleTest);

    let n_in = 1024;
    let freq = 1000.0_f64;
    let left: Vec<f64> = (0..n_in)
        .map(|i| 0.5 * (2.0 * std::f64::consts::PI * freq * (i as f64) / 44_100.0).sin())
        .collect();
    let right: Vec<f64> = (0..n_in)
        .map(|i| 0.5 * (2.0 * std::f64::consts::PI * freq * (i as f64) / 44_100.0).cos())
        .collect();

    let out = run_oneshot(
        &plan,
        StereoF64Buffer {
            sample_rate_hz: 44_100,
            left,
            right,
        },
    );
    assert_eq!(out.sample_rate_hz, 352_800);

    let margin = 136 * 8;
    let mut err_sq = 0.0_f64;
    let mut sig_sq = 0.0_f64;
    for i in margin..(out.left.len() - margin) {
        let t = (i as f64) / 352_800.0;
        let expected_l = 0.5 * (2.0 * std::f64::consts::PI * freq * t).sin();
        let diff = out.left[i] - expected_l;
        err_sq += diff * diff;
        sig_sq += expected_l * expected_l;
    }
    let snr_db = 10.0 * (sig_sq / err_sq.max(1e-30)).log10();
    assert!(
        snr_db > 105.0,
        "expected > 105 dB SNR on 8x 1 kHz sine reconstruction, got {snr_db:.2} dB"
    );
}

#[test]
fn simple_upsampler_suppresses_ultrasonic_images_at_8x() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    }));
    let n_in = 1024;
    let f_sig = 10_000.0_f64;
    let sig: Vec<f64> = (0..n_in)
        .map(|i| 0.5 * (2.0 * std::f64::consts::PI * f_sig * (i as f64) / 44_100.0).sin())
        .collect();

    let out = run_oneshot(
        &plan,
        StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: sig.clone(),
            right: sig,
        },
    );

    let seg = &out.left[(136 * 8)..(out.left.len() - 136 * 8)];
    let tone_power = |freq_hz: f64| -> f64 {
        let n = seg.len();
        let (mut re, mut im) = (0.0_f64, 0.0_f64);
        for (i, &x) in seg.iter().enumerate() {
            let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos());
            let ph = 2.0 * std::f64::consts::PI * freq_hz * (i as f64) / 352_800.0;
            re += x * w * ph.cos();
            im += x * w * ph.sin();
        }
        re * re + im * im
    };

    let p_sig = tone_power(10_000.0);
    let p_img = tone_power(34_100.0);
    let rejection_db = 10.0 * (p_sig / p_img.max(1e-30)).log10();
    assert!(
        rejection_db > 105.0,
        "expected > 105 dB ultrasonic image rejection at 34.1 kHz, got {rejection_db:.2} dB"
    );
}

#[test]
fn simple_upsampler_is_flat_through_20khz_for_44100_to_352800() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    }));
    assert_eq!(plan.filter, FilterArchetype::SimpleTest);

    let n_in = 2048;
    let f_sig = 20_000.0_f64;
    let amp = 0.5_f64;
    let sig: Vec<f64> = (0..n_in)
        .map(|i| amp * (2.0 * std::f64::consts::PI * f_sig * (i as f64) / 44_100.0).sin())
        .collect();

    let out = run_oneshot(
        &plan,
        StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: sig.clone(),
            right: sig,
        },
    );

    // Measure interior 20 kHz amplitude directly via exact RMS & phase-coherent projection.
    let margin = 160 * 8;
    let seg = &out.left[margin..(out.left.len() - margin)];
    let rms_out = (seg.iter().map(|x| x * x).sum::<f64>() / (seg.len() as f64)).sqrt();
    let expected_rms = amp / std::f64::consts::SQRT_2;
    let gain_db = 20.0 * (rms_out / expected_rms).log10();
    assert!(
        gain_db.abs() < 0.05,
        "expected flat 20 kHz passband (|gain| < 0.05 dB) for 44.1->352.8 kHz, got {gain_db:+.4} dB"
    );

    // Also verify the 24.1 kHz ultrasonic image of 20 kHz is deeply suppressed (> 100 dB).
    let tone_power = |freq_hz: f64| -> f64 {
        let n = seg.len();
        let (mut re, mut im) = (0.0_f64, 0.0_f64);
        for (i, &x) in seg.iter().enumerate() {
            // 4-term Blackman-Harris analysis window for >92 dB spectral sidelobe isolation
            let u = (i as f64) / (n as f64);
            let w = 0.35875
                - 0.48829 * (2.0 * std::f64::consts::PI * u).cos()
                + 0.14128 * (4.0 * std::f64::consts::PI * u).cos()
                - 0.01168 * (6.0 * std::f64::consts::PI * u).cos();
            let ph = 2.0 * std::f64::consts::PI * freq_hz * (i as f64) / 352_800.0;
            re += x * w * ph.cos();
            im += x * w * ph.sin();
        }
        re * re + im * im
    };
    let p_20k = tone_power(20_000.0);
    let p_img_24k1 = tone_power(24_100.0);
    let img_rej_db = 10.0 * (p_20k / p_img_24k1.max(1e-30)).log10();
    assert!(
        img_rej_db > 100.0,
        "expected > 100 dB rejection of 24.1 kHz image, got {img_rej_db:.2} dB"
    );
}

#[test]
fn polyphase_bank_has_exact_linear_phase_symmetry_across_all_branches() {
    let m = SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE;
    let ratio = 8;
    let half = m / 2;
    let up = SimpleWindowedSincUpsampler::new(44_100, ratio, m);
    let bank = up.polyphase_bank();
    assert_eq!(bank.len(), ratio);

    // Every polyphase branch must have exact unity DC gain.
    for (p, coeffs) in bank.iter().enumerate() {
        assert_eq!(coeffs.len(), m);
        let sum: f64 = coeffs.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-14,
            "branch {p} DC gain drifted: {sum}"
        );
    }

    // Phase 0: trailing tap j = +half is strictly 0.0, and remaining taps are exactly symmetric
    // around center index (half - 1).
    assert_eq!(bank[0][m - 1], 0.0);
    for d in 1..half {
        let diff = (bank[0][half - 1 - d] - bank[0][half - 1 + d]).abs();
        assert!(
            diff < 1e-15,
            "phase 0 asymmetry at offset {d}: diff={diff}"
        );
    }

    // Complementary phases p and (ratio - p) must be exact time-reversals of each other.
    for p in 1..ratio {
        let q = ratio - p;
        for k in 0..m {
            let diff = (bank[p][k] - bank[q][m - 1 - k]).abs();
            assert!(
                diff < 1e-15,
                "phase {p}/{q} mirror asymmetry at tap {k}: diff={diff}"
            );
        }
    }
}

#[test]
fn upsampler_honors_max_taps_budget_and_matches_plan_tap_budget() {
    let info = TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    });

    // Default cap -> 256 taps/phase * 8 = 2048 taps
    let default_picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let default_plan = default_picker.select(&info);
    let default_up = build_upsampler(&default_plan).unwrap();
    assert_eq!(default_plan.tap_budget, 256 * 8);
    assert_eq!(default_up.total_taps(), default_plan.tap_budget);

    // Explicit --max-taps 512 -> 64 taps/phase * 8 = 512 taps
    let cap512 = UpsamplerPicker::new(UpsamplerPolicy {
        max_taps: 512,
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let p512 = cap512.select(&info);
    let u512 = build_upsampler(&p512).unwrap();
    assert_eq!(p512.tap_budget, 512);
    assert_eq!(u512.total_taps(), 512);

    // Non-multiple --max-taps 100 -> floor-even 12 taps/phase * 8 = 96 taps (reported == actual)
    let cap100 = UpsamplerPicker::new(UpsamplerPolicy {
        max_taps: 100,
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let p100 = cap100.select(&info);
    let u100 = build_upsampler(&p100).unwrap();
    assert_eq!(p100.tap_budget, 96);
    assert_eq!(u100.total_taps(), 96);
}

#[test]
fn upsamplers_reject_sample_rate_mismatch_and_post_flush_blocks() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    // 1. SimpleWindowedSincUpsampler
    let plan_44 = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    }));
    let mut up = build_upsampler(&plan_44).unwrap();
    let wrong_rate = StereoF64Buffer {
        sample_rate_hz: 48_000,
        left: vec![0.1; 16],
        right: vec![0.1; 16],
    };
    let err = up.process_block(wrong_rate).unwrap_err();
    assert!(err.contains("sample rate mismatch"), "unexpected err: {err}");

    let _ = up
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.1; 16],
            right: vec![0.1; 16],
        })
        .unwrap();
    let _ = up.flush().unwrap();
    let err_after = up
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.1; 16],
            right: vec![0.1; 16],
        })
        .unwrap_err();
    assert!(err_after.contains("after flush"), "unexpected err: {err_after}");

    // 2. NullUpsampler
    let plan_384 = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    }));
    let mut null_up = build_upsampler(&plan_384).unwrap();
    assert_eq!(null_up.total_taps(), 0);
    let err_null_rate = null_up
        .process_block(StereoF64Buffer {
            sample_rate_hz: 352_800,
            left: vec![0.1; 8],
            right: vec![0.1; 8],
        })
        .unwrap_err();
    assert!(err_null_rate.contains("sample rate mismatch"));
    let _ = null_up.flush().unwrap();
    let err_null_after = null_up
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: vec![0.1; 8],
            right: vec![0.1; 8],
        })
        .unwrap_err();
    assert!(err_null_after.contains("after flush"));
}

#[test]
fn future_unimplemented_noise_shapers_report_clear_error_until_inv04() {
    let dummy = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: vec![0.1; 4],
        right: vec![-0.1; 4],
    };
    for unimpl in [
        NoiseShaperProfile::Ns5Multibit,
        NoiseShaperProfile::Lns15Smooth,
        NoiseShaperProfile::PggbOptimal,
    ] {
        assert!(!unimpl.is_implemented());
        let err = StreamingQuantizer::new(unimpl, 24).unwrap_err();
        assert!(
            err.contains("not implemented yet"),
            "unexpected err for {unimpl}: {err}"
        );
        assert!(quantize_to_int32(&dummy, unimpl, 24).is_err());
    }
    assert!(NoiseShaperProfile::Bypass.is_implemented());
    assert!(NoiseShaperProfile::TpdfDither.is_implemented());
    assert!(StreamingQuantizer::new(NoiseShaperProfile::Bypass, 24).is_ok());
    assert!(StreamingQuantizer::new(NoiseShaperProfile::TpdfDither, 24).is_ok());
}

#[test]
fn safe_gain_and_tpdf_quantizer_never_boost_and_clamp_cleanly() {
    let mut buf = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: vec![0.0, 0.5, 1.0, -1.0],
        right: vec![0.0, -0.5, 1.2, -1.2],
    };
    apply_safe_gain_f64(&mut buf, -3.0).unwrap();
    let expected_half = 0.5 * 10.0_f64.powf(-3.0 / 20.0);
    assert!((buf.left[1] - expected_half).abs() < 1e-12);
    assert!(apply_safe_gain_f64(&mut buf, 0.5).is_err());

    let pcm = quantize_to_int32(&buf, NoiseShaperProfile::TpdfDither, 24).unwrap();
    assert_eq!(pcm.sample_rate_hz, 384_000);
    assert_eq!(pcm.frames.len(), 4);
    assert!(pcm.frames[2][1] > 0);
    assert!(pcm.frames[3][1] < 0);
}

#[test]
fn null_upsampler_moves_native_384khz_f64_buffer_zero_copy_and_bit_exact() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 32,
        channels: 2,
    }));
    assert_eq!(plan.filter, FilterArchetype::Null);

    let input = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: vec![0.123456789_f64, -0.987654321, 0.0, 0.5],
        right: vec![-0.111111111_f64, 0.222222222, -0.5, 0.0],
    };
    let orig_ptr_l = input.left.as_ptr();
    let orig_ptr_r = input.right.as_ptr();

    let mut upsampler = build_upsampler(&plan).unwrap();
    assert_eq!(upsampler.name(), "null");
    let out = upsampler.process_block(input).unwrap();
    let tail = upsampler.flush().unwrap();

    assert!(tail.is_empty());
    assert_eq!(out.left.as_ptr(), orig_ptr_l);
    assert_eq!(out.right.as_ptr(), orig_ptr_r);
    assert_eq!(out.sample_rate_hz, 384_000);
    assert_eq!(out.left[0], 0.123456789_f64);
    assert_eq!(out.right[1], 0.222222222_f64);
}

#[test]
fn future_unimplemented_filter_archetypes_report_clear_error_until_inv03() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::GaussApod4M),
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let plan = picker.select(&TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    }));
    let err = match build_upsampler(&plan) {
        Ok(_) => panic!("expected unimplemented error for GaussApod4M before INV-03"),
        Err(e) => e,
    };
    assert!(err.contains("not implemented yet"));
}
