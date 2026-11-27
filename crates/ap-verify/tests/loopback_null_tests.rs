// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Layers A–D loopback null enforcement tests on real audio (`PIPELINE_TESTS_REVIEW.md` §2).

use std::f64::consts::PI;
use std::fs;
use std::io::Write;
use std::path::Path;

use ap_core::config::ConvolutionConfig;
use ap_core::format::{DacWireFormat, StreamSpec};
use ap_core::headroom::TrackPeakMetadata;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile};
use ap_verify::pick::{bundled_fixture_path, track_from_env_or_library_or_fixture};
use ap_verify::report::default_report_dir;
use ap_verify::runner::{NullRunConfig, run_loopback_null_buffers, run_loopback_null_file};

fn write_stereo_f32_wav(path: &Path, sample_rate_hz: u32, left: &[f64], right: &[f64]) {
    let n = left.len().min(right.len());
    let data_bytes = (n * 2 * 4) as u32;
    let riff_size = 4 + (8 + 16) + (8 + data_bytes);
    let mut f = fs::File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&riff_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&3u16.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&sample_rate_hz.to_le_bytes()).unwrap();
    f.write_all(&(sample_rate_hz * 8).to_le_bytes()).unwrap();
    f.write_all(&8u16.to_le_bytes()).unwrap();
    f.write_all(&32u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_bytes.to_le_bytes()).unwrap();
    for i in 0..n {
        f.write_all(&(left[i] as f32).to_le_bytes()).unwrap();
        f.write_all(&(right[i] as f32).to_le_bytes()).unwrap();
    }
}

fn peaking_eq_ir(sample_rate_hz: u32, f0_hz: f64, q: f64, gain_db: f64, len: usize) -> Vec<f64> {
    let a = 10.0_f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * f0_hz / f64::from(sample_rate_hz);
    let alpha = w0.sin() / (2.0 * q);
    let cos_w0 = w0.cos();
    let b0 = 1.0 + alpha * a;
    let b1 = -2.0 * cos_w0;
    let b2 = 1.0 - alpha * a;
    let a0 = 1.0 + alpha / a;
    let a1 = -2.0 * cos_w0;
    let a2 = 1.0 - alpha / a;
    let (b0, b1, b2, a1, a2) = (b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0);

    let mut ir = vec![0.0_f64; len];
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    for (i, slot) in ir.iter_mut().enumerate() {
        let x0 = if i == 0 { 1.0 } else { 0.0 };
        let y0 = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        *slot = y0;
        x2 = x1;
        x1 = x0;
        y2 = y1;
        y1 = y0;
    }
    ir
}

#[test]
fn loopback_null_layers_abc_on_real_fixture_8fs() {
    let fixture = bundled_fixture_path();
    let cfg = NullRunConfig {
        filter_override: Some(FilterArchetype::GaussHiresLp),
        shaper_override: Some(NoiseShaperProfile::NsGentle2),
        dac_bits: Some(24),
        wire_format: DacWireFormat::Int32,
        seed: 0x2026_0001,
        report_dir: Some(default_report_dir()),
        ..NullRunConfig::default()
    };

    let rep = run_loopback_null_file(&fixture, &cfg).expect("loopback null run failed");
    assert!(
        rep.layer_a_passed,
        "Layer A failed: peak={:+.2} dBFS rms={:+.2} dBFS (threshold <= -250 dBFS)",
        rep.layer_a_peak_residual_db, rep.layer_a_rms_residual_db
    );
    assert!(
        rep.layer_b.passed,
        "Layer B failed: {:?}",
        rep.layer_b.failure_reasons
    );
    assert!(
        rep.layer_c_passed,
        "Layer C wire mismatch count: {}",
        rep.layer_c_mismatches
    );
    assert!(rep.passed);
}

#[test]
fn loopback_null_layers_abc_with_deemph_rew_and_downsampler() {
    let fixture = bundled_fixture_path();
    let tmp = std::env::temp_dir().join(format!(
        "ap_verify_full_chain_{}.wav",
        std::process::id()
    ));
    let ir_l = peaking_eq_ir(384_000, 1_200.0, 2.0, 4.0, 2_048);
    let ir_r = peaking_eq_ir(384_000, 800.0, 1.8, 3.0, 2_048);
    write_stereo_f32_wav(&tmp, 384_000, &ir_l, &ir_r);

    // Full chain: 96k -> 384k (ShortLp) -> 50/15us de-emphasis -> stereo REW -> 192k downsampler
    // -> gain -> ns-wide-9 (24-bit) -> Int24Packed wire bytes -> decoded i32.
    let cfg = NullRunConfig {
        filter_override: Some(FilterArchetype::ShortLp),
        shaper_override: Some(NoiseShaperProfile::NsWide9),
        dac_bits: Some(24),
        wire_format: DacWireFormat::Int24Packed,
        max_rate_hz: Some(192_000),
        deemphasis: true,
        conv_config: ConvolutionConfig {
            enabled: true,
            file: Some(tmp.clone()),
            ..ConvolutionConfig::default()
        },
        seed: 0x2026_0002,
        ..NullRunConfig::default()
    };

    let rep = run_loopback_null_file(&fixture, &cfg).expect("full-chain loopback null failed");
    let _ = fs::remove_file(&tmp);

    assert_eq!(rep.internal_rate_hz, 384_000);
    assert_eq!(rep.output_rate_hz, 192_000);
    assert!(rep.deemphasis);
    assert!(
        rep.layer_a_passed,
        "Full-chain Layer A failed: peak={:+.2} dBFS rms={:+.2} dBFS",
        rep.layer_a_peak_residual_db, rep.layer_a_rms_residual_db
    );
    assert!(
        rep.layer_b.passed,
        "Full-chain Layer B failed: {:?}",
        rep.layer_b.failure_reasons
    );
    assert!(rep.layer_c_passed);
    assert!(rep.passed);
}

#[test]
fn loopback_null_layer_d_bit_perfect_identity() {
    // Native 384 kHz / 24-bit synthetic track, null filter, no conv, 0 dB headroom -> Bypass.
    let spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let levels = (1_u64 << 23) as f64;
    let n = 38_400; // 0.1 s
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let il = (((2.0 * PI * 997.0 * (i as f64) / 384_000.0).sin() * 0.5) * levels).round();
        let ir = (((2.0 * PI * 1_170.0 * (i as f64) / 384_000.0).sin() * 0.4) * levels).round();
        left.push(il / levels);
        right.push(ir / levels);
    }

    let cfg = NullRunConfig {
        filter_override: Some(FilterArchetype::Null),
        wire_format: DacWireFormat::Int24In32,
        fixed_gain_db: Some(0.0),
        dac_margin_db: 0.0,
        seed: 0x2026_0003,
        ..NullRunConfig::default()
    };

    let rep = run_loopback_null_buffers(
        "synthetic_384k_24b_identity",
        "synthetic",
        spec,
        TrackPeakMetadata {
            true_peak_linear: Some(0.5),
            ..TrackPeakMetadata::default()
        },
        &left,
        &right,
        false,
        &cfg,
    )
    .expect("Layer D run failed");

    assert!(rep.layer_d_applicable, "expected bit-perfect Layer D path");
    assert!(
        rep.layer_d_passed && rep.layer_d_mismatches == 0,
        "Layer D bit-perfect identity had {} mismatches",
        rep.layer_d_mismatches
    );
    assert!(rep.passed);
}

#[test]
fn loopback_null_env_or_fixture_track_smoke() {
    // Respects AP_NULL_FILE / AP_NULL_SEED if explicitly set; otherwise runs cleanly on fixture
    // (deterministic and fast even on headless CI without a music library).
    let picked = if std::env::var("AP_NULL_FILE").is_ok() || std::env::var("AP_NULL_LIBRARY").is_ok()
    {
        track_from_env_or_library_or_fixture(None).expect("pick track")
    } else {
        ap_verify::pick::PickedTrack {
            path: bundled_fixture_path(),
            seed: 0x2026_0004,
            origin: ap_verify::pick::TrackOrigin::FixtureFallback,
        }
    };

    let cfg = NullRunConfig {
        filter_override: Some(FilterArchetype::ShortLp),
        seed: picked.seed,
        ..NullRunConfig::default()
    };
    let rep = run_loopback_null_file(&picked.path, &cfg).expect("loopback null failed");
    assert!(
        rep.passed,
        "Loopback null failed on '{}' (seed={}): A={:+.2} dB, B={:?}, C={}",
        picked.path.display(),
        picked.seed,
        rep.layer_a_peak_residual_db,
        rep.layer_b.failure_reasons,
        rep.layer_c_mismatches
    );
}
