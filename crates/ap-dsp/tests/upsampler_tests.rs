//! TDD mathematical verification tests for the initial `f64` windowed-sinc upsampler
//! (`SimpleWindowedSincUpsampler`) and final `f64 -> i32` TPDF quantizer.

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_dsp::quantize::{apply_safe_gain_f64, quantize_to_int32};
use ap_dsp::selector::{
    FilterArchetype, NoiseShaperProfile, TrackSourceInfo, UpsamplerPicker, UpsamplerPolicy,
};
use ap_dsp::upsampler::build_upsampler;

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
        let upsampler = build_upsampler(&plan).expect("upsampler should build");

        let n_in = 512;
        let input = StereoF64Buffer {
            sample_rate_hz: src_rate,
            left: vec![0.5_f64; n_in],
            right: vec![-0.25_f64; n_in],
        };

        let out = upsampler.upsample(&input, &plan).expect("upsample failed");
        assert_eq!(out.sample_rate_hz, expected_out_rate);
        let ratio = (expected_out_rate / src_rate) as usize;
        assert_eq!(out.left.len(), n_in * ratio);
        assert_eq!(out.right.len(), n_in * ratio);

        // Interior samples (away from FIR edge transient) must preserve DC to < 1e-12
        let margin = 40 * ratio;
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
    let input = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left,
        right,
    };

    let upsampler = build_upsampler(&plan).unwrap();
    let out = upsampler.upsample(&input, &plan).unwrap();
    assert_eq!(out.sample_rate_hz, 352_800);

    // Compare interior upsampled sine against analytical 1 kHz sine at 352,800 Hz.
    // Linear-phase zero-group-delay centered polyphase must match phase & amplitude cleanly.
    let margin = 48 * 8;
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
        snr_db > 85.0,
        "expected > 85 dB SNR on 8x 1 kHz sine reconstruction, got {snr_db:.2} dB"
    );
}

#[test]
fn simple_upsampler_suppresses_ultrasonic_images_at_8x() {
    // 10 kHz tone at 44.1 kHz creates first zero-stuffing image at 44.1 - 10 = 34.1 kHz.
    // Measure DFT energy at 10 kHz (signal) vs 34.1 kHz (image) after 8x upsampling to 352.8 kHz.
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let info = TrackSourceInfo::from_stream_spec(StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    });
    let plan = picker.select(&info);
    let n_in = 1024;
    let f_sig = 10_000.0_f64;
    let sig: Vec<f64> = (0..n_in)
        .map(|i| 0.5 * (2.0 * std::f64::consts::PI * f_sig * (i as f64) / 44_100.0).sin())
        .collect();
    let input = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: sig.clone(),
        right: sig,
    };

    let upsampler = build_upsampler(&plan).unwrap();
    let out = upsampler.upsample(&input, &plan).unwrap();

    // Windowed Goertzel / DFT energy check on interior segment
    let seg = &out.left[(48 * 8)..(out.left.len() - 48 * 8)];
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
        rejection_db > 80.0,
        "expected > 80 dB ultrasonic image rejection at 34.1 kHz, got {rejection_db:.2} dB"
    );
}

#[test]
fn safe_gain_and_tpdf_quantizer_never_boost_and_clamp_cleanly() {
    let mut buf = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: vec![0.0, 0.5, 1.0, -1.0],
        right: vec![0.0, -0.5, 1.2, -1.2], // > 1.0 over-scale test
    };
    // -3 dB safety attenuation
    apply_safe_gain_f64(&mut buf, -3.0).unwrap();
    let expected_half = 0.5 * 10.0_f64.powf(-3.0 / 20.0);
    assert!((buf.left[1] - expected_half).abs() < 1e-12);

    // Boosting (> 0 dB) must be strictly rejected
    assert!(apply_safe_gain_f64(&mut buf, 0.5).is_err());

    let pcm = quantize_to_int32(&buf, NoiseShaperProfile::TpdfDither, 24);
    assert_eq!(pcm.sample_rate_hz, 384_000);
    assert_eq!(pcm.frames.len(), 4);
    // Clamped cleanly inside i32 bounds with zero wraparound
    assert!(pcm.frames[2][1] > 0);
    assert!(pcm.frames[3][1] < 0);
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
