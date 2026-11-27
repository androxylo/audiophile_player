// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use ap_dsp::convolution::{
    PreparedStereoIr, WavOutputFormat, WavSampleEncoding, load_mono_wav, load_stereo_wav,
    measure_stereo_ir_gains, probe_wav_header, write_stereo_wav,
};
use ap_dsp::phase::{min_phase_log_spectrum_from_log_mag, min_phase_spectrum_from_mag};
use realfft::RealFftPlanner;

fn temp_wav_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("ap_dsp_roomfir_helper_{}_{name}", std::process::id()));
    p
}

#[test]
fn stereo_wav_roundtrip_f64_is_bit_exact() {
    let path = temp_wav_path("roundtrip_f64.wav");
    let left = vec![1.0, -0.5, 0.25, -0.125, 1e-9, -1e-12];
    let right = vec![0.0, 0.75, -0.3333333333333333, 0.5, -1e-7, 0.0];

    write_stereo_wav(&path, 384_000, &left, &right, WavOutputFormat::Float64).unwrap();
    let hdr = probe_wav_header(&path).unwrap();
    assert_eq!(hdr.encoding, WavSampleEncoding::IeeeFloat);
    assert_eq!(hdr.channels, 2);
    assert_eq!(hdr.sample_rate_hz, 384_000);
    assert_eq!(hdr.bits_per_sample, 64);
    assert_eq!(hdr.num_frames, left.len());

    let loaded = load_stereo_wav(&path).unwrap();
    assert_eq!(loaded.sample_rate_hz, 384_000);
    assert_eq!(loaded.left, left);
    assert_eq!(loaded.right, right);
    let _ = fs::remove_file(&path);
}

#[test]
fn stereo_wav_roundtrip_f32_matches_f32_precision() {
    let path = temp_wav_path("roundtrip_f32.wav");
    let left = vec![1.0, -0.5, 0.123456789];
    let right = vec![-0.25, 0.875, -0.987654321];

    write_stereo_wav(&path, 352_800, &left, &right, WavOutputFormat::Float32).unwrap();
    let hdr = probe_wav_header(&path).unwrap();
    assert_eq!(hdr.bits_per_sample, 32);
    assert_eq!(hdr.sample_rate_hz, 352_800);

    let loaded = load_stereo_wav(&path).unwrap();
    for (&a, &b) in loaded.left.iter().zip(left.iter()) {
        assert_eq!(a, (b as f32) as f64);
    }
    for (&a, &b) in loaded.right.iter().zip(right.iter()) {
        assert_eq!(a, (b as f32) as f64);
    }
    let _ = fs::remove_file(&path);
}

#[test]
fn stereo_wav_write_rejects_invalid_inputs() {
    let path = temp_wav_path("invalid.wav");
    assert!(write_stereo_wav(&path, 384_000, &[1.0], &[1.0, 2.0], WavOutputFormat::Float64).is_err());
    assert!(write_stereo_wav(&path, 384_000, &[], &[], WavOutputFormat::Float64).is_err());
    assert!(write_stereo_wav(&path, 0, &[1.0], &[1.0], WavOutputFormat::Float64).is_err());
    assert!(write_stereo_wav(&path, 384_000, &[f64::NAN], &[0.0], WavOutputFormat::Float64).is_err());
    assert!(write_stereo_wav(&path, 384_000, &[0.0], &[f64::INFINITY], WavOutputFormat::Float32).is_err());
}

#[test]
fn mono_wav_loader_reads_float32_and_rejects_stereo() {
    let mono_path = temp_wav_path("mono_f32.wav");
    let samples = [0.25_f32, -0.5_f32, 1.0_f32, -0.125_f32];
    let data_bytes = (samples.len() * 4) as u32;
    let riff_size = 4 + 24 + 8 + data_bytes;
    {
        let mut f = File::create(&mono_path).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&48_000u32.to_le_bytes()).unwrap();
        f.write_all(&(48_000u32 * 4).to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_bytes.to_le_bytes()).unwrap();
        for s in samples {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
    }

    let loaded = load_mono_wav(&mono_path).unwrap();
    assert_eq!(loaded.sample_rate_hz, 48_000);
    assert_eq!(loaded.samples.len(), samples.len());
    for (&a, &b) in loaded.samples.iter().zip(samples.iter()) {
        assert_eq!(a, b as f64);
    }
    let _ = fs::remove_file(&mono_path);

    // Reject stereo WAV
    let stereo_path = temp_wav_path("reject_stereo.wav");
    write_stereo_wav(&stereo_path, 48_000, &[1.0], &[1.0], WavOutputFormat::Float32).unwrap();
    assert!(load_mono_wav(&stereo_path).is_err());
    let _ = fs::remove_file(&stereo_path);
}

#[test]
fn measure_stereo_ir_gains_matches_prepared_ir_identically() {
    // Low-pass + slight boost IR on L, unity delta on R.
    let mut left = vec![0.0_f64; 1024];
    left[0] = 1.2;
    left[1] = 0.4;
    left[2] = -0.1;
    let mut right = vec![0.0_f64; 1024];
    right[0] = 1.0;

    let metrics = measure_stereo_ir_gains(&left, &right).unwrap();
    let prepared = PreparedStereoIr::from_stereo_ir("test", 384_000, &left, &right).unwrap();

    assert_eq!(metrics.active_taps, 3);
    assert!((metrics.max_boost_db - prepared.max_boost_db).abs() < 1e-12);
    assert!((metrics.l1_norm - prepared.l1_norm).abs() < 1e-12);
    assert!((metrics.l1_norm_db - prepared.l1_norm_db).abs() < 1e-12);
    assert!(metrics.max_boost_db > 3.0);
}

#[test]
fn min_phase_spectrum_preserves_magnitude_and_concentrates_energy_causally() {
    let k = 4096usize;
    let spec_len = k / 2 + 1;
    // Smooth shelf-like target magnitude: +3 dB at low frequencies, 0 dB at high frequencies.
    let mag: Vec<f64> = (0..spec_len)
        .map(|i| {
            let x = (i as f64) / (spec_len as f64);
            let boost_db = 3.0 / (1.0 + (x / 0.08).powi(4));
            10.0_f64.powf(boost_db / 20.0)
        })
        .collect();

    let h_min = min_phase_spectrum_from_mag(&mag).unwrap();
    assert_eq!(h_min.len(), spec_len);
    for (i, (&c, &m)) in h_min.iter().zip(mag.iter()).enumerate() {
        assert!(
            (c.norm() - m).abs() < 1e-9,
            "bin {i}: expected mag {m}, got {}",
            c.norm()
        );
    }

    // IFFT to time domain: minimum-phase impulse must have practically zero anti-causal energy
    // (second half of circular buffer).
    let mut planner = RealFftPlanner::<f64>::new();
    let c2r = planner.plan_fft_inverse(k);
    let mut spec = h_min;
    let mut time = vec![0.0_f64; k];
    c2r.process(&mut spec, &mut time).unwrap();
    let inv_k = 1.0 / (k as f64);
    for v in &mut time {
        *v *= inv_k;
    }

    let causal_energy: f64 = time[..k / 2].iter().map(|v| v * v).sum();
    let anticausal_energy: f64 = time[k / 2..].iter().map(|v| v * v).sum();
    assert!(
        anticausal_energy < causal_energy * 1e-10,
        "expected negligible anti-causal energy, got {anticausal_energy} vs {causal_energy}"
    );
    // Reject non-power-of-two K and non-finite values.
    assert!(min_phase_log_spectrum_from_log_mag(&[0.0; 10]).is_err());
    assert!(min_phase_spectrum_from_mag(&[-1.0, 1.0, 1.0]).is_err());
}
