// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Building Block C: Cepstral minimum-phase and intermediate-phase conversion
//! (`UPSAMPLING_RESEARCH.md` §4.3 — `#6 minphase-long`, `#7 intermediate-long`, `#8b short-mp`).
//!
//! Algorithm:
//! 1. Take an odd-length linear-phase prototype `h_lin[0..N]` (normalized to `sum == ratio`).
//! 2. Zero-pad to `K >= 16 * N` (power of 2) and compute its real FFT magnitude `H = |FFT_K(h_lin)|`,
//!    flooring `H` at `1e-15` (`-300 dB`, matching our Gaussian stopband floor).
//! 3. Compute the real cepstrum `c = IFFT_K(log H)`, fold into causal minimum phase
//!    (`c[0]` kept, `c[1..K/2]` doubled, `c[K/2]` kept, `c[K/2+1..K]` zeroed), and transform
//!    `G_min = FFT_K(c_min)`.
//! 4. **Minimum phase (`#6`, `#8b`):** `h_min = first N samples of IFFT_K(exp(G_min))`.
//! 5. **Intermediate phase (`#7`, `alpha = 0.5`):** blend unwrapped phase
//!    `phi_ip[k] = alpha * G_min[k].im + (1 - alpha) * phi_lin[k]` where
//!    `phi_lin[k] = -2 * PI * k * ((N - 1) / 2) / K`, and compute
//!    `h_ip = first N samples of IFFT_K(exp(G_min[k].re) * exp(j * phi_ip[k]))`.

use std::f64::consts::PI;

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// Default minimum-phase magnitude floor (`1e-15` = `-300 dB`, `UPSAMPLING_RESEARCH.md` §4.3).
pub const CEPSTRUM_MAG_FLOOR: f64 = 1e-15;

/// Safe upper bound on cepstrum FFT size (`2^20 = 1,048,576` points).
pub const MAX_CEPSTRUM_K: usize = 1 << 20;

/// Default phase blend factor `alpha` for `#7 intermediate-long` (`0.5` = halfway between min & lin).
pub const INTERMEDIATE_PHASE_ALPHA: f64 = 0.5;

/// Compute the causal minimum-phase complex log-spectrum `G_min[k] = ln|H[k]| + j * phi_min[k]`
/// from a one-sided real log-magnitude spectrum `log_mag[0..=K/2]` where `K` is a power of two
/// (`4 <= K <= MAX_CEPSTRUM_K`).
///
/// Uses the exact real-cepstrum Hilbert-transform fold shared with [`convert_prototype_phase`]:
/// keep `c[0]`, double `c[1..K/2]`, keep `c[K/2]`, zero `c[K/2+1..K]`.
pub fn min_phase_log_spectrum_from_log_mag(log_mag: &[f64]) -> Result<Vec<Complex<f64>>, String> {
    let spec_len = log_mag.len();
    if spec_len < 3 {
        return Err(format!(
            "cepstral min-phase requires at least 3 spectrum bins (K >= 4), got {spec_len}"
        ));
    }
    let half_k = spec_len - 1;
    let k_len = half_k
        .checked_mul(2)
        .ok_or_else(|| "cepstrum FFT length overflow".to_string())?;
    if !k_len.is_power_of_two() {
        return Err(format!(
            "cepstral min-phase requires log_mag.len() == K/2 + 1 for power-of-two K, got {} (K={k_len})",
            spec_len
        ));
    }
    if k_len > MAX_CEPSTRUM_K {
        return Err(format!(
            "cepstral min-phase FFT length {k_len} exceeds safe cap ({MAX_CEPSTRUM_K})"
        ));
    }
    if log_mag.iter().any(|v| !v.is_finite()) {
        return Err("cepstral min-phase log_mag contains non-finite values".to_string());
    }

    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(k_len);
    let c2r = planner.plan_fft_inverse(k_len);
    let mut r2c_scratch = r2c.make_scratch_vec();
    let mut c2r_scratch = c2r.make_scratch_vec();

    let mut log_mag_spec = Vec::with_capacity(spec_len);
    for &lm in log_mag {
        log_mag_spec.push(Complex::new(lm, 0.0));
    }

    let mut cepstrum = vec![0.0_f64; k_len];
    c2r.process_with_scratch(&mut log_mag_spec, &mut cepstrum, &mut c2r_scratch)
        .map_err(|e| format!("cepstrum inverse FFT failed: {e}"))?;
    let inv_k = 1.0 / (k_len as f64);
    for v in &mut cepstrum {
        *v *= inv_k;
    }

    for v in &mut cepstrum[1..half_k] {
        *v *= 2.0;
    }
    for v in &mut cepstrum[(half_k + 1)..k_len] {
        *v = 0.0;
    }

    let mut g_min = r2c.make_output_vec();
    r2c.process_with_scratch(&mut cepstrum, &mut g_min, &mut r2c_scratch)
        .map_err(|e| format!("cepstrum min-phase FFT failed: {e}"))?;
    g_min[0].im = 0.0;
    g_min[half_k].im = 0.0;

    Ok(g_min)
}

/// Compute the causal minimum-phase complex spectrum `H_min[k] = exp(G_min[k])` directly from a
/// one-sided linear magnitude spectrum `mag[0..=K/2]` (`K` power of two, `K <= MAX_CEPSTRUM_K`).
pub fn min_phase_spectrum_from_mag(mag: &[f64]) -> Result<Vec<Complex<f64>>, String> {
    if mag.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return Err("cepstral min-phase magnitude spectrum must be finite and >= 0".to_string());
    }
    let log_mag: Vec<f64> = mag
        .iter()
        .map(|&m| m.max(CEPSTRUM_MAG_FLOOR).ln())
        .collect();
    let g_min = min_phase_log_spectrum_from_log_mag(&log_mag)?;
    let last = g_min.len() - 1;
    let mut out: Vec<Complex<f64>> = g_min
        .into_iter()
        .map(|g| {
            let r = g.re.exp();
            let (s, c) = g.im.sin_cos();
            Complex::new(r * c, r * s)
        })
        .collect();
    out[0].im = 0.0;
    out[last].im = 0.0;
    Ok(out)
}

/// Convert a symmetric odd-length linear-phase prototype `h_lin[0..N]` into minimum phase (`alpha = 1.0`)
/// or intermediate phase (`0.0 < alpha < 1.0`, typically `0.5`) of identical length `N`.
///
/// Returns `(prototype_out, estimated_group_delay_samples)` where `estimated_group_delay_samples`
/// is the impulse-peak/energy delay in output-rate samples (`~0` for minimum phase, `~(N-1)/4` for
/// `alpha = 0.5` intermediate phase).
pub fn convert_prototype_phase(
    h_lin: &[f64],
    alpha: f64,
) -> Result<(Vec<f64>, usize), String> {
    let n = h_lin.len();
    if n < 3 || (n & 1) == 0 {
        return Err(format!(
            "cepstral phase conversion requires an odd-length linear-phase prototype >= 3, got {n}"
        ));
    }
    if !alpha.is_finite() || !(0.0..=1.0).contains(&alpha) {
        return Err(format!(
            "invalid intermediate-phase blend alpha={alpha} (expected 0.0..=1.0)"
        ));
    }

    // K >= 16 * N power of 2 (UPS §4.3), capped safely at MAX_CEPSTRUM_K for memory hygiene.
    if n > MAX_CEPSTRUM_K / 2 {
        return Err(format!(
            "cepstral phase conversion prototype length {n} exceeds safe cap ({})",
            MAX_CEPSTRUM_K / 2
        ));
    }
    let k_len = n
        .saturating_mul(16)
        .next_power_of_two()
        .clamp(4_096, MAX_CEPSTRUM_K);
    let half_k = k_len / 2;
    let spec_len = half_k + 1;

    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(k_len);
    let c2r = planner.plan_fft_inverse(k_len);

    // 1. Forward RFFT of zero-padded linear prototype -> |H[k]|.
    let mut time_buf = vec![0.0_f64; k_len];
    time_buf[..n].copy_from_slice(h_lin);
    let mut spec = r2c.make_output_vec();
    let mut r2c_scratch = r2c.make_scratch_vec();
    let mut c2r_scratch = c2r.make_scratch_vec();
    r2c.process_with_scratch(&mut time_buf, &mut spec, &mut r2c_scratch)
        .map_err(|e| format!("cepstrum forward FFT failed: {e}"))?;

    // 2-5. Real log-magnitude -> causal minimum-phase log-spectrum G_min = log|H| + j * phi_min.
    let log_mag: Vec<f64> = spec
        .iter()
        .map(|s| s.norm().max(CEPSTRUM_MAG_FLOOR).ln())
        .collect();
    let g_min = min_phase_log_spectrum_from_log_mag(&log_mag)?;
    let inv_k = 1.0 / (k_len as f64);

    // 6. Construct target complex spectrum H_out[k]:
    //    - alpha == 1.0: pure minimum phase exp(G_min[k])
    //    - alpha <  1.0: blend unwrapped minimum phase with causal linear phase
    //                    phi_lin[k] = -2 * pi * k * ((N - 1) / 2) / K
    let lin_delay = ((n - 1) / 2) as f64;
    let mut out_spec = vec![Complex::new(0.0, 0.0); spec_len];
    for (k, (dst, &g)) in out_spec.iter_mut().zip(g_min.iter()).enumerate() {
        let mag = g.re.exp();
        let phase = if (alpha - 1.0).abs() < 1e-12 {
            g.im
        } else {
            let phi_lin = -2.0 * PI * (k as f64) * lin_delay / (k_len as f64);
            alpha * g.im + (1.0 - alpha) * phi_lin
        };
        let (s, c) = phase.sin_cos();
        dst.re = mag * c;
        dst.im = mag * s;
    }
    // Enforce realfft DC & Nyquist zero-imaginary invariant.
    out_spec[0].im = 0.0;
    out_spec[half_k].im = 0.0;

    // 7. Inverse FFT -> take first N samples and scale by 1/K.
    let mut out_time = vec![0.0_f64; k_len];
    c2r.process_with_scratch(&mut out_spec, &mut out_time, &mut c2r_scratch)
        .map_err(|e| format!("phase-converted inverse FFT failed: {e}"))?;

    let mut proto = Vec::with_capacity(n);
    for &v in &out_time[..n] {
        proto.push(v * inv_k);
    }

    // Locate main impulse peak (group-delay proxy in output samples).
    let peak_idx = proto
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
        .map(|(idx, _)| idx)
        .unwrap_or(0);

    Ok((proto, peak_idx))
}
