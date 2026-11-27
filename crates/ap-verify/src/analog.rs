// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! **L12 · Optional Manual Analog Loopback Analyzer** (`PIPELINE_TESTS_REVIEW.md` §3 L12).
//!
//! Reads a DAC -> ADC loopback WAV capture (`--analog <capture.wav>`), aligns it against an
//! expected Layer A reference signal by FFT cross-correlation, fits scalar gain and a short
//! least-squares FIR for the DAC + ADC analog path, and reports residual RMS, THD+N, and
//! twin-tone IMD (`19 + 20 kHz` and `997 + 1,170 Hz`). Diagnostic report only (no pass/fail).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;

use crate::metrics::{linear_to_db, welch_psd_linear};

/// Decoded stereo WAV capture in pure `f64` (`[-1, 1]`).
#[derive(Debug, Clone)]
pub struct AnalogCaptureWav {
    pub sample_rate_hz: u32,
    pub bits_per_sample: u16,
    pub left: Vec<f64>,
    pub right: Vec<f64>,
}

/// Diagnostic report produced by L12 analog loopback analysis (no pass/fail).
#[derive(Debug, Clone)]
pub struct AnalogLoopbackReport {
    pub sample_rate_hz: u32,
    pub aligned_lag_samples: isize,
    pub aligned_lag_ms: f64,
    pub fitted_gain_db: f64,
    pub scalar_residual_rms_db: f64,
    pub fir_residual_rms_db: f64,
    pub thd_plus_n_db: f64,
    pub imd_19_20k_db: Option<f64>,
    pub imd_997_1170_db: Option<f64>,
}

impl AnalogLoopbackReport {
    pub fn to_yaml(&self) -> String {
        let mut s = String::new();
        s.push_str("schema: ap.analog_loopback/1\n");
        s.push_str(&format!("sample_rate_hz: {}\n", self.sample_rate_hz));
        s.push_str(&format!("aligned_lag_samples: {}\n", self.aligned_lag_samples));
        s.push_str(&format!("aligned_lag_ms: {:.4}\n", self.aligned_lag_ms));
        s.push_str(&format!("fitted_gain_db: {:.4}\n", self.fitted_gain_db));
        s.push_str(&format!(
            "scalar_residual_rms_db: {:.2}\n",
            self.scalar_residual_rms_db
        ));
        s.push_str(&format!(
            "fir_residual_rms_db: {:.2}\n",
            self.fir_residual_rms_db
        ));
        s.push_str(&format!("thd_plus_n_db: {:.2}\n", self.thd_plus_n_db));
        match self.imd_19_20k_db {
            Some(v) => s.push_str(&format!("imd_19_20k_db: {:.2}\n", v)),
            None => s.push_str("imd_19_20k_db: null\n"),
        }
        match self.imd_997_1170_db {
            Some(v) => s.push_str(&format!("imd_997_1170_db: {:.2}\n", v)),
            None => s.push_str("imd_997_1170_db: null\n"),
        }
        s
    }
}

/// Minimal RIFF WAV reader supporting 16/24/32-bit PCM and 32/64-bit IEEE float stereo/mono files.
pub fn read_wav_f64(path: &Path) -> Result<AnalogCaptureWav, String> {
    let mut raw = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut raw))
        .map_err(|e| format!("failed to read WAV '{}': {e}", path.display()))?;
    if raw.len() < 44 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WAVE" {
        return Err(format!("'{}' is not a RIFF WAVE file", path.display()));
    }

    let mut fmt_tag = 0_u16;
    let mut channels = 0_u16;
    let mut sample_rate = 0_u32;
    let mut bits_per_sample = 0_u16;
    let mut data_slice: Option<&[u8]> = None;

    let mut off = 12_usize;
    while off + 8 <= raw.len() {
        let id = &raw[off..off + 4];
        let sz = u32::from_le_bytes(raw[off + 4..off + 8].try_into().unwrap()) as usize;
        off += 8;
        if off + sz > raw.len() {
            break;
        }
        let body = &raw[off..off + sz];
        if id == b"fmt " && body.len() >= 16 {
            fmt_tag = u16::from_le_bytes(body[0..2].try_into().unwrap());
            channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
            sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits_per_sample = u16::from_le_bytes(body[14..16].try_into().unwrap());
            if fmt_tag == 0xFFFE && body.len() >= 40 {
                // WAVE_FORMAT_EXTENSIBLE: inspect SubFormat first 2 bytes (1=PCM, 3=IEEE Float).
                fmt_tag = u16::from_le_bytes(body[24..26].try_into().unwrap());
            }
        } else if id == b"data" {
            data_slice = Some(body);
        }
        off += sz + (sz & 1);
    }

    let data = data_slice.ok_or_else(|| format!("WAV '{}' missing data chunk", path.display()))?;
    if channels == 0 || channels > 2 || sample_rate == 0 {
        return Err(format!(
            "unsupported WAV geometry: channels={channels}, rate={sample_rate}"
        ));
    }
    let bps = usize::from(bits_per_sample / 8);
    if bps == 0 {
        return Err("invalid bits_per_sample=0 in WAV".to_string());
    }
    let frame_bytes = bps * usize::from(channels);
    let n_frames = data.len() / frame_bytes;
    let mut left = Vec::with_capacity(n_frames);
    let mut right = Vec::with_capacity(n_frames);

    let decode_sample = |b: &[u8]| -> Result<f64, String> {
        match (fmt_tag, bits_per_sample) {
            (1, 16) => Ok(f64::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0),
            (1, 24) => {
                let s = i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8;
                Ok(f64::from(s) / 8_388_608.0)
            }
            (1, 32) => Ok(f64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])) / 2_147_483_648.0),
            (3, 32) => Ok(f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))),
            (3, 64) => Ok(f64::from_le_bytes(b[0..8].try_into().unwrap())),
            _ => Err(format!(
                "unsupported WAV encoding tag={fmt_tag} bits={bits_per_sample}"
            )),
        }
    };

    for frame in data.chunks_exact(frame_bytes) {
        let l = decode_sample(&frame[0..bps])?;
        let r = if channels == 2 {
            decode_sample(&frame[bps..2 * bps])?
        } else {
            l
        };
        left.push(l);
        right.push(r);
    }

    Ok(AnalogCaptureWav {
        sample_rate_hz: sample_rate,
        bits_per_sample,
        left,
        right,
    })
}

/// Estimate integer lag `lag` such that `capture[i + lag] ≈ g * reference[i]` via FFT cross-correlation.
pub fn find_alignment_lag(reference: &[f64], capture: &[f64]) -> isize {
    let n = reference.len().max(capture.len());
    if n == 0 {
        return 0;
    }
    let fft_len = (n * 2).next_power_of_two().min(1 << 20);
    let take_r = reference.len().min(fft_len / 2);
    let take_c = capture.len().min(fft_len / 2);

    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(fft_len);
    let inv = planner.plan_fft_inverse(fft_len);

    let mut r_buf = vec![Complex::new(0.0_f64, 0.0_f64); fft_len];
    let mut c_buf = vec![Complex::new(0.0_f64, 0.0_f64); fft_len];
    for i in 0..take_r {
        r_buf[i].re = reference[i];
    }
    for i in 0..take_c {
        c_buf[i].re = capture[i];
    }
    fwd.process(&mut r_buf);
    fwd.process(&mut c_buf);
    for i in 0..fft_len {
        c_buf[i] = c_buf[i] * r_buf[i].conj();
    }
    inv.process(&mut c_buf);

    let mut best_idx = 0_usize;
    let mut best_mag = -1.0_f64;
    for (i, c) in c_buf.iter().enumerate() {
        let a = c.re.abs();
        if a > best_mag {
            best_mag = a;
            best_idx = i;
        }
    }
    if best_idx <= fft_len / 2 {
        best_idx as isize
    } else {
        (best_idx as isize) - (fft_len as isize)
    }
}

/// Fit a centered short FIR `h[0..taps]` (`taps` odd) minimizing `||capture - (ref * h)||_2^2`
/// via regularized normal equations solved with pivoted Gaussian elimination.
fn fit_short_fir(reference: &[f64], capture: &[f64], taps: usize) -> Vec<f64> {
    let n = reference.len().min(capture.len());
    let half = taps / 2;
    if n <= taps + 4 {
        let mut h = vec![0.0_f64; taps];
        h[half] = 1.0;
        return h;
    }

    let mut a = vec![vec![0.0_f64; taps]; taps];
    let mut b = vec![0.0_f64; taps];
    for i in half..(n - half) {
        let y = capture[i];
        for r in 0..taps {
            let xr = reference[i + half - r];
            b[r] += xr * y;
            for c in r..taps {
                a[r][c] += xr * reference[i + half - c];
            }
        }
    }
    for r in 0..taps {
        for c in 0..r {
            a[r][c] = a[c][r];
        }
        a[r][r] += 1e-12; // tiny Tikhonov regularization for bandlimited music signals
    }

    // Solve A * h = b via partial-pivot Gaussian elimination.
    for col in 0..taps {
        let mut piv = col;
        let mut max_v = a[col][col].abs();
        for row in (col + 1)..taps {
            if a[row][col].abs() > max_v {
                max_v = a[row][col].abs();
                piv = row;
            }
        }
        if piv != col {
            a.swap(col, piv);
            b.swap(col, piv);
        }
        let diag = a[col][col];
        if diag.abs() < 1e-18 {
            continue;
        }
        for row in (col + 1)..taps {
            let factor = a[row][col] / diag;
            b[row] -= factor * b[col];
            for k in col..taps {
                a[row][k] -= factor * a[col][k];
            }
        }
    }
    let mut h = vec![0.0_f64; taps];
    for i in (0..taps).rev() {
        let mut sum = b[i];
        for k in (i + 1)..taps {
            sum -= a[i][k] * h[k];
        }
        h[i] = if a[i][i].abs() > 1e-18 {
            sum / a[i][i]
        } else {
            0.0
        };
    }
    h
}

/// Measure twin-tone IMD ratio (dB below primary tones) if both primary tones `(f1, f2)` are
/// clearly present (`>= -30 dBFS`) in `signal`.
fn measure_twin_tone_imd_db(signal: &[f64], fs_hz: f64, f1: f64, f2: f64) -> Option<f64> {
    if f2 >= 0.48 * fs_hz || signal.len() < 16_384 {
        return None;
    }
    let nfft = 32_768_usize.min(signal.len().next_power_of_two() / 2).max(4_096);
    let (freqs, psd) = welch_psd_linear(signal, fs_hz, nfft);
    let df = fs_hz / (nfft as f64);
    let band_power = |f_center: f64, bw: f64| -> f64 {
        freqs
            .iter()
            .zip(psd.iter())
            .filter(|&(&f, _)| (f - f_center).abs() <= bw)
            .map(|(_, &p)| p * df)
            .sum()
    };
    let bw = (3.0 * df).max(25.0);
    let p1 = band_power(f1, bw);
    let p2 = band_power(f2, bw);
    let p_prim = p1 + p2;
    if p1 <= 1e-4 || p2 <= 1e-4 || p_prim <= 0.0 {
        return None;
    }
    // Check 2nd-order difference (f2 - f1) and 3rd-order sidebands (2*f1 - f2, 2*f2 - f1).
    let mut p_imd = 0.0_f64;
    for &f_prod in &[(f2 - f1).abs(), (2.0 * f1 - f2).abs(), (2.0 * f2 - f1).abs()] {
        if f_prod > bw && f_prod < 0.48 * fs_hz {
            p_imd += band_power(f_prod, bw);
        }
    }
    Some(if p_imd > 0.0 {
        10.0 * (p_imd / p_prim).log10()
    } else {
        -200.0
    })
}

/// Analyze a DAC->ADC analog loopback capture against the expected Layer A reference signal.
pub fn analyze_analog_loopback(
    reference: &[f64],
    capture: &[f64],
    sample_rate_hz: u32,
) -> AnalogLoopbackReport {
    let lag = find_alignment_lag(reference, capture);
    let (r_slice, c_slice) = if lag >= 0 {
        let u = lag as usize;
        let len = reference.len().min(capture.len().saturating_sub(u));
        (&reference[..len], &capture[u..u + len])
    } else {
        let u = (-lag) as usize;
        let len = reference.len().saturating_sub(u).min(capture.len());
        (&reference[u..u + len], &capture[..len])
    };

    let n = r_slice.len();
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for i in 0..n {
        num += r_slice[i] * c_slice[i];
        den += r_slice[i] * r_slice[i];
    }
    let gain = if den > 1e-24 { num / den } else { 1.0 };

    let mut scalar_sq = 0.0_f64;
    for i in 0..n {
        let d = c_slice[i] - gain * r_slice[i];
        scalar_sq += d * d;
    }
    let scalar_rms = if n > 0 { (scalar_sq / (n as f64)).sqrt() } else { 0.0 };

    // Short 17-tap DAC+ADC path FIR fit.
    let taps = 17_usize;
    let half = taps / 2;
    let h = fit_short_fir(r_slice, c_slice, taps);
    let mut fir_sq = 0.0_f64;
    let mut sig_sq = 0.0_f64;
    let mut cnt = 0_usize;
    for i in half..n.saturating_sub(half) {
        let mut pred = 0.0_f64;
        for k in 0..taps {
            pred += h[k] * r_slice[i + half - k];
        }
        let d = c_slice[i] - pred;
        fir_sq += d * d;
        sig_sq += c_slice[i] * c_slice[i];
        cnt += 1;
    }
    let fir_rms = if cnt > 0 { (fir_sq / (cnt as f64)).sqrt() } else { 0.0 };
    let sig_rms = if cnt > 0 { (sig_sq / (cnt as f64)).sqrt() } else { 1.0 };
    let thd_plus_n_db = if sig_rms > 1e-18 {
        linear_to_db(fir_rms / sig_rms)
    } else {
        -200.0
    };

    let fs = f64::from(sample_rate_hz);
    let imd_19_20k_db = measure_twin_tone_imd_db(c_slice, fs, 19_000.0, 20_000.0);
    let imd_997_1170_db = measure_twin_tone_imd_db(c_slice, fs, 997.0, 1_170.0);

    AnalogLoopbackReport {
        sample_rate_hz,
        aligned_lag_samples: lag,
        aligned_lag_ms: (lag as f64) * 1_000.0 / fs,
        fitted_gain_db: linear_to_db(gain.abs()),
        scalar_residual_rms_db: linear_to_db(scalar_rms),
        fir_residual_rms_db: linear_to_db(fir_rms),
        thd_plus_n_db,
        imd_19_20k_db,
        imd_997_1170_db,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn l12_analog_analyzer_recovers_delay_gain_fir_and_twin_tone_imd() {
        let fs = 96_000_u32;
        let n = 65_536_usize;
        let mut reference = Vec::with_capacity(n);
        for i in 0..n {
            let t = (i as f64) / f64::from(fs);
            reference.push(
                0.35 * (2.0 * PI * 997.0 * t).sin() + 0.35 * (2.0 * PI * 1_170.0 * t).sin(),
            );
        }
        // Simulate DAC+ADC path: 37-sample latency, -1.5 dB gain, tiny 3rd-order nonlinearity (~-90 dB).
        let delay = 37_usize;
        let g = 10.0_f64.powf(-1.5 / 20.0);
        let mut capture = vec![0.0_f64; n + delay];
        for i in 0..n {
            let x = g * reference[i];
            capture[i + delay] = x + 1e-4 * x * x * x;
        }
        let rep = analyze_analog_loopback(&reference, &capture, fs);
        assert_eq!(rep.aligned_lag_samples, delay as isize);
        assert!((rep.fitted_gain_db - (-1.5)).abs() < 0.05);
        assert!(rep.thd_plus_n_db < -80.0, "got {}", rep.thd_plus_n_db);
        let imd = rep.imd_997_1170_db.expect("expected 997+1170 Hz IMD");
        assert!(imd < -70.0 && imd > -115.0, "unexpected IMD {imd:.2} dB");
    }
}
