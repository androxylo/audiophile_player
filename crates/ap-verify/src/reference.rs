// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Independent naive pure-`f64` reference DSP operators sharing zero code with `ap-dsp`.
//!
//! Uses full complex `rustfft` (`f64`) one-shot linear convolution (with large-block `2^24`
//! overlap-add only when `x.len() + h.len() > 2^23` so multi-minute full-track renders fit in RAM
//! while remaining completely independent of `ap-dsp`'s `realfft` UPOLS implementation).

use std::f64::consts::PI;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Maximum one-shot complex FFT size before switching to large-block overlap-add (`2^23` = 8M
/// points ~ 128 MiB complex `f64` scratch). Still independent of `ap-dsp`'s UPOLS partition sizes.
const MAX_ONESHOT_FFT: usize = 1 << 23;

/// Zero-stuff `x` by integer factor `factor >= 1` (`out[n * factor] = x[n]`, zeros between).
pub fn zero_stuff(x: &[f64], factor: usize) -> Vec<f64> {
    assert!(factor >= 1, "zero_stuff factor must be >= 1");
    if factor == 1 {
        return x.to_vec();
    }
    let mut out = vec![0.0_f64; x.len() * factor];
    for (i, &v) in x.iter().enumerate() {
        out[i * factor] = v;
    }
    out
}

/// Independent one-shot complex `rustfft` `f64` linear convolution `y = x * h` of length
/// `x.len() + h.len() - 1` (empty if either slice is empty).
///
/// Automatically falls back to large-block (`MAX_ONESHOT_FFT`) overlap-add if `x.len() + h.len()`
/// exceeds `MAX_ONESHOT_FFT`, keeping RAM bounded for `AP_NULL_FULL=1` multi-minute renders.
pub fn fft_convolve_oneshot(x: &[f64], h: &[f64]) -> Vec<f64> {
    if x.is_empty() || h.is_empty() {
        return Vec::new();
    }
    let out_len = x.len() + h.len() - 1;
    let fft_len = out_len.next_power_of_two();

    if fft_len <= MAX_ONESHOT_FFT {
        let mut planner = FftPlanner::<f64>::new();
        let fwd = planner.plan_fft_forward(fft_len);
        let inv = planner.plan_fft_inverse(fft_len);

        let mut x_buf = vec![Complex::new(0.0_f64, 0.0_f64); fft_len];
        for (dst, &v) in x_buf.iter_mut().zip(x.iter()) {
            dst.re = v;
        }
        let mut h_buf = vec![Complex::new(0.0_f64, 0.0_f64); fft_len];
        for (dst, &v) in h_buf.iter_mut().zip(h.iter()) {
            dst.re = v;
        }

        fwd.process(&mut x_buf);
        fwd.process(&mut h_buf);

        let inv_n = 1.0_f64 / (fft_len as f64);
        for (xb, hb) in x_buf.iter_mut().zip(h_buf.iter()) {
            *xb = (*xb * *hb) * inv_n;
        }
        inv.process(&mut x_buf);

        let mut out = Vec::with_capacity(out_len);
        for c in x_buf.into_iter().take(out_len) {
            out.push(c.re);
        }
        return out;
    }

    // Large-block overlap-add for full multi-minute tracks (still complex rustfft, block size
    // >= 2^23, completely independent of ap-dsp's realfft UPOLS).
    let fft_len = (h.len() * 2).next_power_of_two().max(MAX_ONESHOT_FFT);
    let block_len = fft_len - h.len() + 1;
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(fft_len);
    let inv = planner.plan_fft_inverse(fft_len);

    let mut h_buf = vec![Complex::new(0.0_f64, 0.0_f64); fft_len];
    for (dst, &v) in h_buf.iter_mut().zip(h.iter()) {
        dst.re = v;
    }
    fwd.process(&mut h_buf);

    let mut out = vec![0.0_f64; out_len];
    let mut x_buf = vec![Complex::new(0.0_f64, 0.0_f64); fft_len];
    let inv_n = 1.0_f64 / (fft_len as f64);

    let mut pos = 0_usize;
    while pos < x.len() {
        let take = block_len.min(x.len() - pos);
        for c in &mut x_buf {
            *c = Complex::new(0.0, 0.0);
        }
        for (dst, &v) in x_buf.iter_mut().zip(&x[pos..pos + take]) {
            dst.re = v;
        }
        fwd.process(&mut x_buf);
        for (xb, hb) in x_buf.iter_mut().zip(h_buf.iter()) {
            *xb = (*xb * *hb) * inv_n;
        }
        inv.process(&mut x_buf);
        let write_len = (take + h.len() - 1).min(out_len - pos);
        for i in 0..write_len {
            out[pos + i] += x_buf[i].re;
        }
        pos += take;
    }
    out
}

/// Independent IEC 60908 CD-DA 50/15 µs de-emphasis reference filter in pure `f64`
/// (`tau1 = 50 µs`, `tau2 = 15 µs`, bilinear transform pre-warped uniformly at `min(10 kHz, 0.45*fs)`).
pub fn biquad_deemph(x: &[f64], sample_rate_hz: u32) -> Vec<f64> {
    const TAU1: f64 = 50.0e-6;
    const TAU2: f64 = 15.0e-6;
    let fs = f64::from(sample_rate_hz);
    let f_match = 10_000.0_f64.min(0.45 * fs);
    let w = PI * f_match / fs;
    let warp = w.tan() / w;
    let t1 = (2.0 * fs * TAU1) / warp;
    let t2 = (2.0 * fs * TAU2) / warp;
    let a0 = 1.0 + t1;
    let b0 = (1.0 + t2) / a0;
    let b1 = (1.0 - t2) / a0;
    let a1 = (1.0 - t1) / a0;

    let mut y = Vec::with_capacity(x.len());
    let mut x1 = 0.0_f64;
    let mut y1 = 0.0_f64;
    for &x0 in x {
        let y0 = b0 * x0 + b1 * x1 - a1 * y1;
        y.push(y0);
        x1 = x0;
        y1 = y0;
    }
    y
}

/// Apply integer sample delay (`delay_samples` leading zeros, preserving total length `x.len() + delay_samples`).
pub fn apply_delay(x: &[f64], delay_samples: usize) -> Vec<f64> {
    if delay_samples == 0 {
        return x.to_vec();
    }
    let mut out = vec![0.0_f64; delay_samples + x.len()];
    out[delay_samples..].copy_from_slice(x);
    out
}

/// Direct naive polyphase decimation with exported kernel bank (`up_l` phases × `taps_per_phase`)
/// and exact leading/trailing zero padding (`pad_lead`, `pad_tail`).
pub fn polyphase_decimate_naive(
    x: &[f64],
    up_l: usize,
    down_m: usize,
    pad_lead: usize,
    pad_tail: usize,
    bank: &[Vec<f64>],
) -> Vec<f64> {
    if x.is_empty() || bank.is_empty() {
        return Vec::new();
    }
    let tpp = bank[0].len();
    let mut padded = Vec::with_capacity(pad_lead + x.len() + pad_tail);
    padded.resize(pad_lead, 0.0_f64);
    padded.extend_from_slice(x);
    padded.resize(padded.len() + pad_tail, 0.0_f64);

    let mut out = Vec::new();
    let mut base = 0_usize;
    let mut p = 0_usize;
    while base + tpp <= padded.len() {
        let coeffs = &bank[p];
        let mut acc = 0.0_f64;
        for j in 0..tpp {
            acc += padded[base + j] * coeffs[j];
        }
        out.push(acc);
        let next_p = p + down_m;
        base += next_p / up_l;
        p = next_p % up_l;
    }
    out
}

/// Ideal frequency-domain brick-wall downsampler via complex `rustfft` (for L8 anti-aliasing checks).
///
/// Resamples `x` from `input_rate_hz` down to `output_rate_hz` (`output_rate_hz < input_rate_hz`)
/// by keeping only bins strictly inside `[-f_out/2, +f_out/2)` of the input DFT and inverse-FFTing
/// at length `n_out = round(x.len() * output_rate_hz / input_rate_hz)`.
pub fn brickwall_fft(x: &[f64], input_rate_hz: u32, output_rate_hz: u32) -> Vec<f64> {
    assert!(output_rate_hz > 0 && output_rate_hz < input_rate_hz);
    let n_in = x.len();
    if n_in == 0 {
        return Vec::new();
    }
    let n_out = ((n_in as u128 * u128::from(output_rate_hz)) / u128::from(input_rate_hz)) as usize;
    if n_out == 0 {
        return Vec::new();
    }

    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(n_in);
    let inv = planner.plan_fft_inverse(n_out);

    let mut spec_in: Vec<Complex<f64>> = x.iter().map(|&v| Complex::new(v, 0.0)).collect();
    fwd.process(&mut spec_in);

    let mut spec_out = vec![Complex::new(0.0_f64, 0.0_f64); n_out];
    let half = n_out / 2;
    let scale = 1.0_f64 / (n_in as f64);

    // DC + positive frequencies
    for k in 0..half {
        spec_out[k] = spec_in[k] * scale;
    }
    // Nyquist bin if even
    if n_out % 2 == 0 {
        spec_out[half] = Complex::new(0.5 * (spec_in[half].re + spec_in[n_in - half].re) * scale, 0.0);
    } else {
        spec_out[half] = spec_in[half] * scale;
    }
    // Negative frequencies
    for k in 1..half {
        spec_out[n_out - k] = spec_in[n_in - k] * scale;
    }
    if n_out % 2 == 1 {
        spec_out[n_out - half] = spec_in[n_in - half] * scale;
    }

    inv.process(&mut spec_out);
    spec_out.into_iter().map(|c| c.re).collect()
}
