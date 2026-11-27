// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Deterministic, GPL-compatible self-generated test audio fixtures & REW IRs
//! (`PIPELINE_TESTS_REVIEW.md` §4.5).
//!
//! Avoids bloating git history with multi-megabyte binary fixtures while deterministically
//! generating all six verification signals in pure IEEE 754 `f64`:
//! - 44.1/16 and 48/24 decaying piano-like multi-tone + noise bursts
//! - 0 dBFS hot master with `+3 dB` (`fs/4 @ 45°`) intersample overs
//! - 2.5 s ultra-short track
//! - 45 s look-ahead seam track
//! - 64k-tap `+6 dB` mid-band peaking REW impulse response

use std::f64::consts::{PI, SQRT_2};

use ap_core::format::StreamSpec;

/// Simple deterministic SplitMix64 PRNG for noise bursts inside synthetic fixtures.
struct Prng(u64);
impl Prng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let bits53 = (z ^ (z >> 31)) >> 11;
        (bits53 as f64) * (2.0 / ((1_u64 << 53) as f64)) - 1.0
    }
}

/// Quantize normalized `[-1, 1)` samples to a clean `bits`-bit integer PCM grid.
fn quantize_to_grid(samples: &mut [f64], bits: u16) {
    let levels = (1_u64 << (bits - 1)) as f64;
    let min_i = -(1_i64 << (bits - 1)) as f64;
    let max_i = ((1_i64 << (bits - 1)) - 1) as f64;
    for s in samples {
        *s = (*s * levels).round().clamp(min_i, max_i) / levels;
    }
}

/// Synthesize a piano-like decaying multi-tone + transient noise bursts quantized cleanly to `bits`.
pub fn piano_multitone(
    sample_rate_hz: u32,
    bits_per_sample: u16,
    duration_s: f64,
    seed: u64,
) -> (StreamSpec, Vec<f64>, Vec<f64>) {
    let n = (f64::from(sample_rate_hz) * duration_s).round() as usize;
    let fs = f64::from(sample_rate_hz);
    let mut rng = Prng(seed ^ 0xA1B2_C3D4_E5F6_0708);

    // Rich harmonic chord notes with exponential attack/decay envelopes every 1.25 s + short noise bursts.
    let chord_l = [261.63_f64, 329.63, 392.00, 523.25, 1_046.50, 2_093.00];
    let chord_r = [293.66_f64, 369.99, 440.00, 587.33, 1_174.66, 2_349.32];
    let note_period_s = 1.25_f64;

    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f64) / fs;
        let local_t = t % note_period_s;
        let attack = (local_t / 0.004).min(1.0);
        let decay = (-local_t * 2.8).exp();
        let env = attack * decay;

        let mut sl = 0.0_f64;
        let mut sr = 0.0_f64;
        for (h, (&fl, &fr)) in chord_l.iter().zip(chord_r.iter()).enumerate() {
            let w = 0.12 / ((h + 1) as f64).sqrt();
            sl += w * (2.0 * PI * fl * t).sin();
            sr += w * (2.0 * PI * fr * t).sin();
        }
        // Short percussive noise burst during first 20 ms of each note.
        let burst = if local_t < 0.02 {
            0.05 * (1.0 - local_t / 0.02)
        } else {
            0.0
        };
        left.push(sl * env + burst * rng.next_f64());
        right.push(sr * env + burst * rng.next_f64());
    }

    quantize_to_grid(&mut left, bits_per_sample);
    quantize_to_grid(&mut right, bits_per_sample);

    (
        StreamSpec {
            sample_rate_hz,
            bits_per_sample,
            channels: 2,
        },
        left,
        right,
    )
}

/// Synthesize a 0 dBFS hot master with `+3.01 dB` intersample overs (`fs/4` at `45°` phase).
pub fn hot_isp_master(
    sample_rate_hz: u32,
    bits_per_sample: u16,
    duration_s: f64,
) -> (StreamSpec, Vec<f64>, Vec<f64>) {
    let n = (f64::from(sample_rate_hz) * duration_s).round() as usize;
    let fs = f64::from(sample_rate_hz);
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        let t = (i as f64) / fs;
        let in_isp_burst = ((t * 2.0) as usize) % 2 == 0;
        let v = if in_isp_burst {
            // Discrete samples are exact +-1.0, reconstructed continuous peak is sqrt(2) (+3.01 dBTP).
            ((2.0 * PI * 0.25 * (i as f64) + PI / 4.0).sin() * SQRT_2).clamp(-1.0, 1.0)
        } else {
            0.99 * (2.0 * PI * 1_000.0 * t).sin()
        };
        left.push(v);
        right.push(-v);
    }
    quantize_to_grid(&mut left, bits_per_sample);
    quantize_to_grid(&mut right, bits_per_sample);
    (
        StreamSpec {
            sample_rate_hz,
            bits_per_sample,
            channels: 2,
        },
        left,
        right,
    )
}

/// Synthesize an ultra-short (`2.5 s`) stereo fixture.
pub fn ultra_short_track(sample_rate_hz: u32, bits_per_sample: u16) -> (StreamSpec, Vec<f64>, Vec<f64>) {
    piano_multitone(sample_rate_hz, bits_per_sample, 2.5, 0x5407)
}

/// Synthesize a 45 s stereo fixture for look-ahead seam tests (`L6`).
pub fn long_seam_track(sample_rate_hz: u32, bits_per_sample: u16) -> (StreamSpec, Vec<f64>, Vec<f64>) {
    piano_multitone(sample_rate_hz, bits_per_sample, 45.0, 0x455E)
}

/// Synthesize a REW-style stereo impulse response with a `+6 dB` mid-band peaking EQ (`1 kHz`, Q=2.0).
pub fn rew_mid_peak_ir(sample_rate_hz: u32, len: usize) -> Vec<f64> {
    let f0_hz = 1_000.0_f64;
    let q = 2.0_f64;
    let gain_db = 6.0_f64;
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
