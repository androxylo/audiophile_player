// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Independent loopback null, harshness-detector, coherence, and NTF PSD metrics
//! (`PIPELINE_TESTS_REVIEW.md` §§2.3–2.4).

use std::f64::consts::PI;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

/// Convert a linear amplitude to dBFS (clamped at `-350.0 dB` for exact zero).
pub fn linear_to_db(val: f64) -> f64 {
    if val > 0.0 && val.is_finite() {
        20.0 * val.log10()
    } else {
        -350.0
    }
}

/// Peak sample-by-sample residual `20 * log10(max_i |a[i] - b[i]|)` in dBFS.
pub fn peak_residual_db(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(
        a.len(),
        b.len(),
        "peak_residual_db length mismatch: {} vs {}",
        a.len(),
        b.len()
    );
    let mut peak = 0.0_f64;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let d = (x - y).abs();
        if d > peak {
            peak = d;
        }
    }
    linear_to_db(peak)
}

/// RMS sample-by-sample residual `20 * log10(sqrt(mean((a - b)^2)))` in dBFS.
pub fn rms_residual_db(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    if a.is_empty() {
        return -350.0;
    }
    let sum_sq: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| {
            let d = x - y;
            d * d
        })
        .sum();
    linear_to_db((sum_sq / (a.len() as f64)).sqrt())
}

/// Band-limited RMS residual (in dBFS) between `a` and `b` over `[f_lo_hz, f_hi_hz]` using
/// Hann-windowed Welch spectral integration.
pub fn band_residual_db(a: &[f64], b: &[f64], fs_hz: f64, f_lo_hz: f64, f_hi_hz: f64) -> f64 {
    assert_eq!(a.len(), b.len());
    let diff: Vec<f64> = a.iter().zip(b.iter()).map(|(&x, &y)| x - y).collect();
    let nfft = 16_384_usize.min(diff.len().next_power_of_two() / 2).max(256);
    let (freqs, psd_lin) = welch_psd_linear(&diff, fs_hz, nfft);
    let df = fs_hz / (nfft as f64);
    let power: f64 = freqs
        .iter()
        .zip(psd_lin.iter())
        .filter(|&(&f, _)| f >= f_lo_hz && f <= f_hi_hz)
        .map(|(_, &p)| p * df)
        .sum();
    if power > 0.0 {
        10.0 * power.log10()
    } else {
        -350.0
    }
}

/// Normalized zero-lag Pearson cross-correlation `ρ(e, s) ∈ [-1, 1]`.
pub fn xcorr(e: &[f64], s: &[f64]) -> f64 {
    let n = e.len().min(s.len());
    if n == 0 {
        return 0.0;
    }
    let inv_n = 1.0 / (n as f64);
    let mean_e: f64 = e[..n].iter().sum::<f64>() * inv_n;
    let mean_s: f64 = s[..n].iter().sum::<f64>() * inv_n;
    let mut num = 0.0_f64;
    let mut den_e = 0.0_f64;
    let mut den_s = 0.0_f64;
    for i in 0..n {
        let de = e[i] - mean_e;
        let ds = s[i] - mean_s;
        num += de * ds;
        den_e += de * de;
        den_s += ds * ds;
    }
    let denom = (den_e * den_s).sqrt();
    if denom > 0.0 { num / denom } else { 0.0 }
}

/// Count samples in `unity * 10^(gain_db/20)` that exceed the representable `dac_bits` integer
/// grid `[-2^(b-1), 2^(b-1) - 1]` prior to dither/shaping.
pub fn overs(unity: &[f64], gain_db: f64, dac_bits: u8) -> u64 {
    let b = dac_bits.clamp(16, 32);
    let levels = (1_u64 << (b - 1)) as f64;
    let max_int = ((1_i64 << (b - 1)) - 1) as f64;
    let min_int = -(1_i64 << (b - 1)) as f64;
    let g_lin = if (gain_db - 0.0).abs() < 1e-12 {
        1.0
    } else {
        10.0_f64.powf(gain_db / 20.0)
    };
    let mut count = 0_u64;
    for &u in unity {
        let scaled = u * g_lin * levels;
        if scaled > max_int || scaled < min_int {
            count += 1;
        }
    }
    count
}

/// One-sided Hann-windowed 50%-overlap Welch PSD (linear power per Hz) for real signal `x`.
pub fn welch_psd_linear(x: &[f64], fs_hz: f64, nfft: usize) -> (Vec<f64>, Vec<f64>) {
    let nfft = nfft.max(64).next_power_of_two();
    let hop = nfft / 2;
    let bins = nfft / 2 + 1;
    let freqs: Vec<f64> = (0..bins)
        .map(|k| (k as f64) * fs_hz / (nfft as f64))
        .collect();
    if x.len() < nfft {
        return (freqs, vec![0.0_f64; bins]);
    }

    let window: Vec<f64> = (0..nfft)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * (i as f64) / (nfft as f64)).cos()))
        .collect();
    let win_energy: f64 = window.iter().map(|&w| w * w).sum();

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(nfft);
    let mut buf = vec![Complex::new(0.0_f64, 0.0_f64); nfft];
    let mut acc = vec![0.0_f64; bins];
    let mut segs = 0_usize;

    let mut start = 0_usize;
    while start + nfft <= x.len() {
        for i in 0..nfft {
            buf[i] = Complex::new(x[start + i] * window[i], 0.0);
        }
        fft.process(&mut buf);
        for k in 0..bins {
            let mag2 = buf[k].norm_sqr();
            let one_sided = if k == 0 || k == nfft / 2 { 1.0 } else { 2.0 };
            acc[k] += one_sided * mag2;
        }
        segs += 1;
        start += hop;
    }

    let scale = 1.0 / ((segs as f64) * fs_hz * win_energy);
    for v in &mut acc {
        *v *= scale;
    }
    (freqs, acc)
}

/// One-sided Hann-windowed Welch PSD in dB/Hz.
pub fn welch_psd(x: &[f64], fs_hz: f64, nfft: usize) -> (Vec<f64>, Vec<f64>) {
    let (freqs, lin) = welch_psd_linear(x, fs_hz, nfft);
    let db = lin
        .into_iter()
        .map(|p| if p > 0.0 { 10.0 * p.log10() } else { -350.0 })
        .collect();
    (freqs, db)
}

/// 1/3-octave band coherence measurement between quantization error `e` and signal `s`.
#[derive(Debug, Clone, Copy)]
pub struct CoherenceBand {
    pub center_hz: f64,
    pub f_lo_hz: f64,
    pub f_hi_hz: f64,
    pub coherence: f64,
    pub coherence_db: f64,
}

/// Compute magnitude-squared coherence `|S_es(f)|^2 / (S_ee(f) * S_ss(f))` averaged over standard
/// 1/3-octave bands from 100 Hz to 8 kHz (Hann-windowed 50%-overlap Welch cross-spectral estimator).
pub fn coherence_bands(e: &[f64], s: &[f64], fs_hz: f64, nfft: usize) -> Vec<CoherenceBand> {
    let n = e.len().min(s.len());
    let nfft = nfft.max(256).next_power_of_two();
    let hop = nfft / 2;
    let bins = nfft / 2 + 1;

    // Standard ISO 1/3-octave centers from 100 Hz to 8000 Hz.
    const CENTERS_HZ: &[f64] = &[
        100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0,
        1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0,
    ];
    let half_band_ratio = 2.0_f64.powf(1.0 / 6.0);

    if n < nfft * 2 {
        return CENTERS_HZ
            .iter()
            .map(|&fc| CoherenceBand {
                center_hz: fc,
                f_lo_hz: fc / half_band_ratio,
                f_hi_hz: fc * half_band_ratio,
                coherence: 0.0,
                coherence_db: -350.0,
            })
            .collect();
    }

    let window: Vec<f64> = (0..nfft)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * (i as f64) / (nfft as f64)).cos()))
        .collect();

    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(nfft);
    let mut e_buf = vec![Complex::new(0.0_f64, 0.0_f64); nfft];
    let mut s_buf = vec![Complex::new(0.0_f64, 0.0_f64); nfft];

    let mut see = vec![0.0_f64; bins];
    let mut sss = vec![0.0_f64; bins];
    let mut ses = vec![Complex::new(0.0_f64, 0.0_f64); bins];

    let mut start = 0_usize;
    while start + nfft <= n {
        for i in 0..nfft {
            let w = window[i];
            e_buf[i] = Complex::new(e[start + i] * w, 0.0);
            s_buf[i] = Complex::new(s[start + i] * w, 0.0);
        }
        fft.process(&mut e_buf);
        fft.process(&mut s_buf);
        for k in 0..bins {
            see[k] += e_buf[k].norm_sqr();
            sss[k] += s_buf[k].norm_sqr();
            ses[k] += e_buf[k] * s_buf[k].conj();
        }
        start += hop;
    }

    let df = fs_hz / (nfft as f64);
    CENTERS_HZ
        .iter()
        .map(|&fc| {
            let f_lo = fc / half_band_ratio;
            let f_hi = fc * half_band_ratio;
            let mut sum_coh = 0.0_f64;
            let mut cnt = 0_usize;
            for k in 1..bins {
                let f = (k as f64) * df;
                if f >= f_lo && f <= f_hi {
                    let denom = see[k] * sss[k];
                    if denom > 1e-30 {
                        sum_coh += (ses[k].norm_sqr() / denom).clamp(0.0, 1.0);
                        cnt += 1;
                    }
                }
            }
            let coh = if cnt > 0 {
                sum_coh / (cnt as f64)
            } else {
                0.0
            };
            let coh_db = if coh > 0.0 {
                10.0 * coh.log10()
            } else {
                -350.0
            };
            CoherenceBand {
                center_hz: fc,
                f_lo_hz: f_lo,
                f_hi_hz: f_hi,
                coherence: coh,
                coherence_db: coh_db,
            }
        })
        .collect()
}

/// Evaluate analytical noise-transfer-function PSD (in LSB^2 / Hz) at frequency `freq_hz` for
/// error-feedback coefficients `coeffs` (`H_ntf(z) = 1 + sum_{k=1}^N c_k z^{-k}` or `-` per
/// `quantize.rs` feedback sign: `v = x + sum c_k e[n-k]`, `q = round(v + d)` =>
/// `q - x = e[n] + sum c_k e[n-k]`, where TPDF dither + quantizer gives white `e[n]` with variance
/// `Var(e) = 1/12 + 1/6 = 1/4` LSB^2!).
///
/// Wait: check `quantize.rs`:
/// - `dl = u1 - u2` (`u1, u2 ~ U[0, 1)`), so `Var(dl) = 1/6` LSB^2.
/// - `q = round(v + dl)`, `e = q - v`, so `e = dl + eps` where `Var(e) = 1/6 + 1/12 = 1/4` LSB^2!
/// - Output error `q - x = (q - v) + (v - x) = e[n] + sum_{k=1}^N c_k e[n-k]`.
/// - Therefore `H_ntf(e^{jω}) = 1 + sum_{k=1}^N c_k e^{-j k ω}`, and one-sided PSD is
///   `S_q(f) = (2 / fs) * (1/4) * |H_ntf(e^{j 2π f / fs})|^2 = |H_ntf|^2 / (2 * fs)`.
pub fn analytical_ntf_psd_lsb2_per_hz(coeffs: &[f64], fs_hz: f64, freq_hz: f64) -> f64 {
    let omega = 2.0 * PI * freq_hz / fs_hz;
    let mut re = 1.0_f64;
    let mut im = 0.0_f64;
    for (idx, &c) in coeffs.iter().enumerate() {
        let k = (idx + 1) as f64;
        let ang = omega * k;
        re += c * ang.cos();
        im -= c * ang.sin();
    }
    let mag2 = re * re + im * im;
    // One-sided PSD for white error of variance 0.25 LSB^2 spread across [0, fs/2]:
    // integral_0^{fs/2} S(f) df = 0.25 => S_flat = 0.25 / (fs/2) = 0.5 / fs.
    (0.5 / fs_hz) * mag2
}

/// Measure the maximum in-band (`[200 Hz, f_hi_hz]`) deviation in dB between the Welch PSD of
/// quantization error `e_lsb` and the analytical shaper NTF PSD, averaged in 1/3-octave bins so
/// Welch variance stays well below 0.2 dB.
pub fn ntf_max_inband_deviation_db(
    e_lsb: &[f64],
    coeffs: &[f64],
    fs_hz: f64,
    f_hi_hz: f64,
    nfft: usize,
) -> f64 {
    let (freqs, psd_lin) = welch_psd_linear(e_lsb, fs_hz, nfft);
    // Group into 1/3-octave bands from 500 Hz up to `f_hi_hz` (at 384 kHz, nfft=32768 gives
    // df=11.7 Hz so 500 Hz..20 kHz has plenty of bins per 1/3-octave band for tight variance).
    let mut fc = 500.0_f64;
    let step = 2.0_f64.powf(1.0 / 3.0);
    let half_band = 2.0_f64.powf(1.0 / 6.0);
    let mut max_dev_db = 0.0_f64;
    while fc * half_band <= f_hi_hz {
        let lo = fc / half_band;
        let hi = fc * half_band;
        let mut sum_meas = 0.0_f64;
        let mut sum_theo = 0.0_f64;
        let mut count = 0_usize;
        for (&f, &p_meas) in freqs.iter().zip(psd_lin.iter()) {
            if f >= lo && f <= hi {
                sum_meas += p_meas;
                sum_theo += analytical_ntf_psd_lsb2_per_hz(coeffs, fs_hz, f);
                count += 1;
            }
        }
        if count >= 4 && sum_meas > 0.0 && sum_theo > 0.0 {
            let dev_db = (10.0 * (sum_meas / sum_theo).log10()).abs();
            if dev_db > max_dev_db {
                max_dev_db = dev_db;
            }
        }
        fc *= step;
    }
    max_dev_db
}

/// Summary of Layer B (Gain + Quantizer harshness detector, `PIPELINE_TESTS_REVIEW.md` §2.4).
#[derive(Debug, Clone)]
pub struct LayerBMetrics {
    pub overs_count: u64,
    pub max_abs_err_lsb: f64,
    pub err_bound_lsb: f64,
    pub abs_mean_err_lsb: f64,
    pub max_abs_xcorr: f64,
    pub max_coherence_excess_db: f64,
    pub ntf_max_dev_db: f64,
    pub passed: bool,
    pub failure_reasons: Vec<String>,
}
