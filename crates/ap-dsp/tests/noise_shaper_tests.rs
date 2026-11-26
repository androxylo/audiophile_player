// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Comprehensive TDD verification suite for `INV-04` `8fS` error-feedback noise shapers
//! (`docs/research/04-noise-shapers.md` §6.7 — all 6 verification gates).

use std::f64::consts::PI;

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_dsp::quantize::{DEFAULT_QUANTIZER_SEED, StreamingQuantizer, quantize_to_int32};
use ap_dsp::selector::NoiseShaperProfile;
use ap_dsp::shaper_design::{frozen_shaper_coeffs, ntf_mag_sq_at_hz};
use realfft::RealFftPlanner;

const SHAPED_PROFILES: [NoiseShaperProfile; 4] = [
    NoiseShaperProfile::NsGentle2,
    NoiseShaperProfile::Ns5Multibit,
    NoiseShaperProfile::Lns15Smooth,
    NoiseShaperProfile::PggbOptimal,
];

const ALL_DITHERED_PROFILES: [NoiseShaperProfile; 5] = [
    NoiseShaperProfile::TpdfDither,
    NoiseShaperProfile::NsGentle2,
    NoiseShaperProfile::Ns5Multibit,
    NoiseShaperProfile::Lns15Smooth,
    NoiseShaperProfile::PggbOptimal,
];

/// 4-term Blackman-Harris window (`-92 dB` sidelobes) so `+18.6 dB` ultrasonic shaped noise
/// never leaks into `-58.7 dB` in-band `0-20 kHz` Welch PSD measurements.
fn blackman_harris_window(n: usize) -> (Vec<f64>, f64) {
    let mut w = Vec::with_capacity(n);
    let mut u = 0.0_f64;
    let nf = n as f64;
    for i in 0..n {
        let p = 2.0 * PI * (i as f64) / nf;
        let val = 0.35875 - 0.48829 * p.cos() + 0.14128 * (2.0 * p).cos() - 0.01168 * (3.0 * p).cos();
        w.push(val);
        u += val * val;
    }
    (w, u)
}

/// Compute Welch PSD (normalized so white noise of variance `σ²` has `E[P[k]] = σ²` at all
/// interior bins `1 <= k < fft_n / 2`) averaged over both stereo channels with 50% overlap.
fn welch_psd_stereo(left: &[f64], right: &[f64], fft_n: usize) -> Vec<f64> {
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(fft_n);
    let (win, u_power) = blackman_harris_window(fft_n);
    let hop = fft_n / 2;
    let n_bins = fft_n / 2 + 1;
    let mut acc = vec![0.0_f64; n_bins];
    let mut seg_count = 0_usize;

    let mut scratch = r2c.make_scratch_vec();
    let mut buf = r2c.make_input_vec();
    let mut spec = r2c.make_output_vec();

    for chan in [left, right] {
        let mut start = 0;
        while start + fft_n <= chan.len() {
            for i in 0..fft_n {
                buf[i] = chan[start + i] * win[i];
            }
            r2c.process_with_scratch(&mut buf, &mut spec, &mut scratch)
                .unwrap();
            for (k, c) in spec.iter().enumerate() {
                acc[k] += (c.re * c.re + c.im * c.im) / u_power;
            }
            seg_count += 1;
            start += hop;
        }
    }
    assert!(seg_count >= 8, "need enough Welch segments for low variance");
    let inv = 1.0 / (seg_count as f64);
    for v in &mut acc {
        *v *= inv;
    }
    acc
}

/// Gate 1 (§6.7): Empirical Welch PSD of quantization error `y - x` matches analytical
/// `|NTF(f)|² · LSB²/4` within `±0.5 dB` across `0-20k`, `20-40k`, `40-100k`, and `100k-Nyq`
/// for all 5 dithered/shaped profiles at both `352,800` and `384,000 Hz`.
#[test]
fn gate1_welch_psd_matches_analytical_ntf_across_all_bands_and_8fs_rates() {
    let dac_bits: u8 = 16;
    let shift = 32 - u32::from(dac_bits);
    let levels = (1_u64 << (dac_bits - 1)) as f64;
    let n_samples = 131_072_usize;
    let fft_n = 4096_usize;
    let amp = 0.1_f64; // -20 dBFS stimulus well inside full-scale

    for &rate_hz in &[352_800_u32, 384_000_u32] {
        let fs = f64::from(rate_hz);
        let mut left_in = Vec::with_capacity(n_samples);
        let mut right_in = Vec::with_capacity(n_samples);
        for n in 0..n_samples {
            let t = (n as f64) / fs;
            left_in.push(amp * (2.0 * PI * 997.0 * t).sin());
            right_in.push(amp * (2.0 * PI * 1_511.0 * t).cos());
        }
        let audio = StereoF64Buffer {
            sample_rate_hz: rate_hz,
            left: left_in.clone(),
            right: right_in.clone(),
        };

        for &profile in &ALL_DITHERED_PROFILES {
            let mut q = StreamingQuantizer::with_rate(profile, dac_bits, rate_hz).unwrap();
            assert_eq!(q.effective_shaper(), profile);
            let out = q.quantize_block(&audio);

            // Discard initial 256 samples of shaper startup transient, then compute error in LSBs.
            let skip = 256_usize;
            let mut err_l = Vec::with_capacity(n_samples - skip);
            let mut err_r = Vec::with_capacity(n_samples - skip);
            for i in skip..n_samples {
                let ql = (out.frames[i][0] >> shift) as f64;
                let qr = (out.frames[i][1] >> shift) as f64;
                err_l.push(ql - left_in[i] * levels);
                err_r.push(qr - right_in[i] * levels);
            }

            let psd = welch_psd_stereo(&err_l, &err_r, fft_n);
            let coeffs = frozen_shaper_coeffs(profile, rate_hz).unwrap_or(&[]);
            let nyq = 0.5 * fs;
            let bands = [
                ("0-20k", 300.0, 20_000.0),
                ("20-40k", 20_000.0, 40_000.0),
                ("40-100k", 40_000.0, 100_000.0),
                ("100k-nyq", 100_000.0, nyq - 300.0),
            ];

            for (band_name, f_lo, f_hi) in bands {
                let mut emp_sum = 0.0_f64;
                let mut ana_sum = 0.0_f64;
                let mut count = 0_usize;
                for (k, &p_k) in psd.iter().enumerate().take(fft_n / 2).skip(1) {
                    let f = (k as f64) * fs / (fft_n as f64);
                    if f >= f_lo && f <= f_hi {
                        emp_sum += p_k;
                        // Analytical error PSD in LSB^2: (1/4) * |NTF(f)|^2.
                        ana_sum += 0.25 * ntf_mag_sq_at_hz(coeffs, f, fs);
                        count += 1;
                    }
                }
                assert!(count > 10);
                let emp_db = 10.0 * (emp_sum / count as f64).log10();
                let ana_db = 10.0 * (ana_sum / count as f64).log10();
                let diff_db = (emp_db - ana_db).abs();
                assert!(
                    diff_db <= 0.5,
                    "Gate 1 failed for {profile} @ {rate_hz} Hz [{band_name}]: \
                     empirical={emp_db:+.2} dB vs analytical={ana_db:+.2} dB (diff={diff_db:.2} dB > 0.5 dB)"
                );
            }
        }
    }
}

/// Gate 2 (§6.7): Overload / stability — feedback magnitude stays strictly bounded by
/// `<= 1.5 · Σ|c_k|` LSB on both a `-0.5 dBFS` sine AND a synthetic `+3 dB` over burst,
/// with zero post-clip instability or limit cycles.
#[test]
fn gate2_overload_and_feedback_stability_bound_holds_even_on_plus_3db_burst() {
    let amp_m05 = 10.0_f64.powf(-0.5 / 20.0);
    let amp_p3 = 10.0_f64.powf(3.0 / 20.0);

    for &rate_hz in &[352_800_u32, 384_000_u32] {
        let fs = f64::from(rate_hz);
        // 8192 frames of -0.5 dBFS sine, then 512 frames of +3 dB overload burst, then 8192 frames recovery.
        let n_pre = 8_192_usize;
        let n_burst = 512_usize;
        let n_post = 8_192_usize;
        let total = n_pre + n_burst + n_post;

        let mut left = Vec::with_capacity(total);
        let mut right = Vec::with_capacity(total);
        for n in 0..total {
            let t = (n as f64) / fs;
            let env = if n < n_pre {
                amp_m05
            } else if n < n_pre + n_burst {
                amp_p3
            } else {
                0.1
            };
            left.push(env * (2.0 * PI * 997.0 * t).sin());
            right.push(env * (2.0 * PI * 3_137.0 * t).sin());
        }
        let audio = StereoF64Buffer {
            sample_rate_hz: rate_hz,
            left: left.clone(),
            right: right.clone(),
        };

        for &dac_bits in &[16_u8, 24_u8] {
            let shift = 32 - u32::from(dac_bits);
            let levels = (1_u64 << (dac_bits - 1)) as f64;
            for &profile in &SHAPED_PROFILES {
                let coeffs = frozen_shaper_coeffs(profile, rate_hz).unwrap();
                let l1_sum: f64 = coeffs.iter().map(|c| c.abs()).sum();
                let bound = 1.5 * l1_sum;

                let mut q = StreamingQuantizer::with_rate(profile, dac_bits, rate_hz).unwrap();
                let out = q.quantize_block(&audio);

                assert!(
                    q.max_feedback_lsb() <= bound,
                    "Gate 2 failed for {profile} ({dac_bits}b @ {rate_hz}): \
                     max_feedback={:.4} LSB > 1.5*sum|c|={bound:.4} LSB",
                    q.max_feedback_lsb()
                );
                assert!(
                    q.stats().overs_count > 0,
                    "synthetic +3 dB burst must register pre-dither overs"
                );

                // Verify immediate clean recovery after the +3 dB burst (no runaway or limit cycle).
                let mut post_max_err = 0.0_f64;
                for i in (n_pre + n_burst + 64)..total {
                    let ql = (out.frames[i][0] >> shift) as f64;
                    let qr = (out.frames[i][1] >> shift) as f64;
                    let el = (ql - left[i] * levels).abs();
                    let er = (qr - right[i] * levels).abs();
                    post_max_err = post_max_err.max(el.max(er));
                }
                assert!(
                    post_max_err <= bound + 2.0,
                    "post-burst error blew up for {profile} ({dac_bits}b @ {rate_hz}): {post_max_err:.2} LSB"
                );
            }
        }
    }
}

/// Gate 3 (§6.7): `-100 dBFS` ~997 Hz tone at 16-bit is cleanly recovered below the 1 LSB
/// threshold with zero coherent harmonic distortion spurs (2nd-5th harmonics at noise floor).
#[test]
fn gate3_minus_100_dbfs_997hz_tone_at_16bit_is_linear_and_spur_free() {
    let rate_hz = 384_000_u32;
    let fs = f64::from(rate_hz);
    let dac_bits = 16_u8;
    let shift = 32 - u32::from(dac_bits);
    let levels = (1_u64 << (dac_bits - 1)) as f64;

    let fft_n = 65_536_usize;
    let skip = 256_usize;
    let total = fft_n + skip;

    // Bin-aligned tone nearest 997 Hz (`k = 170` -> `996.09375 Hz`) for leak-free coherent FFT.
    let tone_bin = (997.0 * (fft_n as f64) / fs).round() as usize;
    let f_tone = (tone_bin as f64) * fs / (fft_n as f64);
    let amp = 10.0_f64.powf(-100.0 / 20.0); // 1e-5 linear (~0.33 LSB at 16-bit)

    let mut left = Vec::with_capacity(total);
    let mut right = Vec::with_capacity(total);
    for n in 0..total {
        let s = amp * (2.0 * PI * f_tone * (n as f64) / fs).sin();
        left.push(s);
        right.push(s);
    }
    let audio = StereoF64Buffer {
        sample_rate_hz: rate_hz,
        left,
        right,
    };

    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(fft_n);
    let mut scratch = r2c.make_scratch_vec();

    for profile in [
        NoiseShaperProfile::TpdfDither,
        NoiseShaperProfile::NsGentle2,
        NoiseShaperProfile::Ns5Multibit,
    ] {
        let mut q = StreamingQuantizer::with_rate(profile, dac_bits, rate_hz).unwrap();
        let out = q.quantize_block(&audio);

        // Coherent single-shot rectangular FFT (bin-aligned tone) averaged in power over L/R.
        let mut mag2 = vec![0.0_f64; fft_n / 2 + 1];
        for ch in 0..2 {
            let mut input: Vec<f64> = (skip..total)
                .map(|i| ((out.frames[i][ch] >> shift) as f64) / levels)
                .collect();
            let mut spec = r2c.make_output_vec();
            r2c.process_with_scratch(&mut input, &mut spec, &mut scratch)
                .unwrap();
            // Scale so a full-scale sine of amplitude `A` has bin power `A²`.
            let norm = 2.0 / (fft_n as f64);
            for (k, c) in spec.iter().enumerate() {
                let r = c.re * norm;
                let im = c.im * norm;
                mag2[k] += 0.5 * (r * r + im * im);
            }
        }

        let fund_db = 10.0 * mag2[tone_bin].log10();
        assert!(
            (fund_db - (-100.0)).abs() <= 1.5,
            "Gate 3 failed for {profile}: -100 dBFS fundamental measured at {fund_db:+.2} dBFS"
        );

        // Check 2nd through 5th harmonics against local noise floor around each harmonic.
        for h in 2..=5 {
            let h_bin = h * tone_bin;
            let mut local_sum = 0.0_f64;
            let mut local_n = 0_usize;
            for k in (h_bin - 32)..=(h_bin + 32) {
                if k != h_bin && k > 0 && k < fft_n / 2 {
                    local_sum += mag2[k];
                    local_n += 1;
                }
            }
            let local_floor_db = 10.0 * (local_sum / local_n as f64).log10();
            let harm_db = 10.0 * mag2[h_bin].log10();
            assert!(
                harm_db <= local_floor_db + 12.0,
                "Gate 3 harmonic spur H{h} detected for {profile}: \
                 harm={harm_db:+.2} dBFS vs local_floor={local_floor_db:+.2} dBFS"
            );
        }
    }
}

/// Gate 4 (§6.7): Digital silence produces strictly bounded output with zero idle tones
/// (no Welch PSD bin exceeds the smooth analytical `0.25 · |NTF(f)|²` curve by `> 10 dB`).
#[test]
fn gate4_digital_silence_is_bounded_and_free_of_idle_tones() {
    let rate_hz = 384_000_u32;
    let fs = f64::from(rate_hz);
    let n_samples = 65_536_usize;
    let fft_n = 2048_usize;

    let silence = StereoF64Buffer {
        sample_rate_hz: rate_hz,
        left: vec![0.0; n_samples],
        right: vec![0.0; n_samples],
    };

    for &dac_bits in &[16_u8, 24_u8] {
        let shift = 32 - u32::from(dac_bits);
        for &profile in &ALL_DITHERED_PROFILES {
            let coeffs = frozen_shaper_coeffs(profile, rate_hz).unwrap_or(&[]);
            let l1_sum: f64 = coeffs.iter().map(|c| c.abs()).sum();
            let max_allowed_lsb = (1.5 * l1_sum + 2.0).ceil();

            let mut q = StreamingQuantizer::with_rate(profile, dac_bits, rate_hz).unwrap();
            let out = q.quantize_block(&silence);

            let mut l_lsb = Vec::with_capacity(n_samples);
            let mut r_lsb = Vec::with_capacity(n_samples);
            let mut peak_lsb = 0.0_f64;
            for &[fl, fr] in &out.frames {
                let ql = (fl >> shift) as f64;
                let qr = (fr >> shift) as f64;
                peak_lsb = peak_lsb.max(ql.abs().max(qr.abs()));
                l_lsb.push(ql);
                r_lsb.push(qr);
            }
            assert!(
                peak_lsb <= max_allowed_lsb,
                "Gate 4 silence peak out of bounds for {profile} ({dac_bits}b): {peak_lsb} > {max_allowed_lsb}"
            );

            let psd = welch_psd_stereo(&l_lsb[256..], &r_lsb[256..], fft_n);
            for (k, &p_k) in psd.iter().enumerate().take(fft_n / 2).skip(2) {
                let f = (k as f64) * fs / (fft_n as f64);
                let expected = 0.25 * ntf_mag_sq_at_hz(coeffs, f, fs);
                // Ignore ultra-deep near-DC bins where -92 dB Blackman-Harris sidelobe floor
                // sits above a -65 dB theoretical notch (`ns5` below 2 kHz).
                if expected < 1e-6 {
                    continue;
                }
                let excess_db = 10.0 * (p_k / expected).log10();
                assert!(
                    excess_db <= 10.0,
                    "Gate 4 idle tone detected for {profile} ({dac_bits}b) at {f:.0} Hz: \
                     +{excess_db:.2} dB above smooth analytical floor"
                );
            }
        }
    }
}

/// Reference copy of the pre-INV-04 `TpdfDither` (`N = 0`) algorithm for bit-identity regression.
fn reference_tpdf_quantize(
    audio: &StereoF64Buffer,
    dac_bits: u8,
    rng_state: &mut u64,
) -> InterleavedInt32Buffer {
    let next_u = |state: &mut u64| -> f64 {
        *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let bits53 = (z ^ (z >> 31)) >> 11;
        (bits53 as f64) * (1.0 / ((1_u64 << 53) as f64))
    };

    let bits = dac_bits.clamp(16, 32);
    let levels = (1_u64 << (bits - 1)) as f64;
    let max_int = ((1_i64 << (bits - 1)) - 1) as f64;
    let min_int = -(1_i64 << (bits - 1)) as f64;
    let shift = 32 - u32::from(bits);

    let n = audio.left.len().min(audio.right.len());
    let mut frames = Vec::with_capacity(n);
    for i in 0..n {
        let dl = next_u(rng_state) - next_u(rng_state);
        let dr = next_u(rng_state) - next_u(rng_state);
        let ql = (audio.left[i] * levels + dl).round().clamp(min_int, max_int) as i32;
        let qr = (audio.right[i] * levels + dr).round().clamp(min_int, max_int) as i32;
        frames.push([ql.wrapping_shl(shift), qr.wrapping_shl(shift)]);
    }
    InterleavedInt32Buffer {
        sample_rate_hz: audio.sample_rate_hz,
        frames,
    }
}

/// Gate 5 (§6.7): `TpdfDither` (`N = 0`) output is 100% bit-identical to pre-INV-04 `TpdfDither`
/// with the same seed across all supported bit depths and multi-block calls.
#[test]
fn gate5_tpdf_dither_is_bit_identical_to_pre_inv04_baseline() {
    let audio = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: (0..4096).map(|n| 0.45 * (0.017 * n as f64).sin()).collect(),
        right: (0..4096).map(|n| -0.37 * (0.029 * n as f64).cos()).collect(),
    };

    for &dac_bits in &[16_u8, 18, 20, 24] {
        let mut q = StreamingQuantizer::with_rate(NoiseShaperProfile::TpdfDither, dac_bits, 384_000)
            .unwrap();
        let mut ref_seed = DEFAULT_QUANTIZER_SEED;

        // Verify across two blocks to ensure continuous RNG equivalence too.
        let b1 = StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: audio.left[..1500].to_vec(),
            right: audio.right[..1500].to_vec(),
        };
        let b2 = StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: audio.left[1500..].to_vec(),
            right: audio.right[1500..].to_vec(),
        };

        assert_eq!(
            q.quantize_block(&b1).frames,
            reference_tpdf_quantize(&b1, dac_bits, &mut ref_seed).frames
        );
        assert_eq!(
            q.quantize_block(&b2).frames,
            reference_tpdf_quantize(&b2, dac_bits, &mut ref_seed).frames
        );
    }
}

/// Gate 6 (§6.7): Determinism across runs, chunk invariance, state carryover (`export_state` /
/// `restore_state`), and clean non-8fS fallback to `TpdfDither`.
#[test]
fn gate6_determinism_chunk_invariance_state_carryover_and_non_8fs_fallback() {
    let n = 4096_usize;
    let audio = StereoF64Buffer {
        sample_rate_hz: 352_800,
        left: (0..n).map(|i| 0.3 * (0.013 * i as f64).sin()).collect(),
        right: (0..n).map(|i| -0.25 * (0.023 * i as f64).cos()).collect(),
    };

    for profile in [
        NoiseShaperProfile::Bypass,
        NoiseShaperProfile::TpdfDither,
        NoiseShaperProfile::NsGentle2,
        NoiseShaperProfile::Ns5Multibit,
        NoiseShaperProfile::Lns15Smooth,
        NoiseShaperProfile::PggbOptimal,
    ] {
        // Repeatability: two fresh quantizers produce byte-identical output.
        let out_a = quantize_to_int32(&audio, profile, 20).unwrap();
        let out_b = quantize_to_int32(&audio, profile, 20).unwrap();
        assert_eq!(out_a.frames, out_b.frames, "non-deterministic output for {profile}");

        // Chunk invariance + export/restore state handoff matches oneshot bit-for-bit.
        let mut q1 = StreamingQuantizer::with_rate(profile, 20, 352_800).unwrap();
        let mut stitched = Vec::with_capacity(n);
        let splits = [1, 7, 15, 256, 1024];
        let mut cursor = 0;
        for &sz in &splits {
            let end = (cursor + sz).min(n);
            let blk = StereoF64Buffer {
                sample_rate_hz: 352_800,
                left: audio.left[cursor..end].to_vec(),
                right: audio.right[cursor..end].to_vec(),
            };
            stitched.extend(q1.quantize_block(&blk).frames);
            cursor = end;
        }
        // Hand off quantizer state to a fresh instance mid-stream (simulating equal-rate gapless track B).
        let state = q1.export_state();
        let mut q2 = StreamingQuantizer::with_rate(profile, 20, 352_800).unwrap();
        q2.restore_state(state);
        let rest = StereoF64Buffer {
            sample_rate_hz: 352_800,
            left: audio.left[cursor..].to_vec(),
            right: audio.right[cursor..].to_vec(),
        };
        stitched.extend(q2.quantize_block(&rest).frames);

        assert_eq!(
            stitched, out_a.frames,
            "chunk/carryover mismatch for {profile}"
        );
    }

    // Non-8fS rate with shaped profile must fall back cleanly to TpdfDither (never reuse 8fS taps).
    let audio_96k = StereoF64Buffer {
        sample_rate_hz: 96_000,
        left: audio.left[..512].to_vec(),
        right: audio.right[..512].to_vec(),
    };
    let expected_tpdf = quantize_to_int32(&audio_96k, NoiseShaperProfile::TpdfDither, 24).unwrap();
    for &profile in &SHAPED_PROFILES {
        let mut q = StreamingQuantizer::with_rate(profile, 24, 96_000).unwrap();
        assert_eq!(q.requested_shaper(), profile);
        assert_eq!(q.effective_shaper(), NoiseShaperProfile::TpdfDither);
        assert!(q.coeffs().is_empty());
        assert_eq!(q.quantize_block(&audio_96k).frames, expected_tpdf.frames);
    }
}
