// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Single final floating-point -> integer PCM quantization stage, `8fS` minimum-phase
//! error-feedback noise shaping (`docs/research/04-noise-shapers.md`), and `<= 0 dB` safety headroom.
//!
//! Hard invariants:
//! - Digital gain boosting (`> 0 dB`) is strictly forbidden and rejected with an error.
//! - Integer quantization occurs strictly once here at the end of the floating-point pipeline.
//! - [`StreamingQuantizer`] maintains continuous deterministic SplitMix64 TPDF PRNG and
//!   per-channel FIR error-feedback state across streaming blocks and equal-rate gapless tracks.
//! - Quantization error `e[n] = q[n] - v[n]` is ALWAYS computed BEFORE output clamping so
//!   clipping excursions can never inject unbounded error into the feedback loop (§6.1).

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::headroom::MAX_ALLOWED_GAIN_DB;

use crate::selector::NoiseShaperProfile;
use crate::shaper_design::{MAX_SHAPER_ORDER, frozen_shaper_coeffs};

/// Default deterministic SplitMix64 seed for TPDF dither.
pub const DEFAULT_QUANTIZER_SEED: u64 = 0x2026_0401_9E37_79B9;

/// Apply a strictly non-boosting gain (`gain_db <= 0.0`) in pure IEEE 754 `f64`.
pub fn apply_safe_gain_f64(audio: &mut StereoF64Buffer, gain_db: f64) -> Result<(), String> {
    if !gain_db.is_finite() || gain_db > MAX_ALLOWED_GAIN_DB {
        return Err(format!(
            "gain_db ({gain_db}) violates strict <= 0 dB audiophile headroom rule"
        ));
    }
    if (gain_db - 0.0).abs() < 1e-12 {
        return Ok(());
    }
    let linear = 10.0_f64.powf(gain_db / 20.0);
    for s in &mut audio.left {
        *s *= linear;
    }
    for s in &mut audio.right {
        *s *= linear;
    }
    Ok(())
}

/// Per-track pre-quantizer peak and clipping backstop statistics (`HEADROOM_DESIGN.md` §6,
/// Review #5 Finding #15).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct QuantizerStats {
    /// Number of individual channel samples exceeding `[min_int, max_int]` before dither/shaping.
    pub overs_count: u64,
    /// Maximum pre-quantizer linear amplitude observed across both channels.
    pub max_pre_quant_peak: f64,
}

impl QuantizerStats {
    pub fn max_pre_quant_peak_dbfs(&self) -> f64 {
        if self.max_pre_quant_peak > 0.0 {
            20.0 * self.max_pre_quant_peak.log10()
        } else {
            f64::NEG_INFINITY
        }
    }

    /// Merge statistics from a secondary quantizer (e.g. gapless tail drain / handoff)
    /// into this report (Review #6 Finding #18).
    pub fn merge(&mut self, other: QuantizerStats) {
        self.overs_count = self.overs_count.saturating_add(other.overs_count);
        if other.max_pre_quant_peak > self.max_pre_quant_peak {
            self.max_pre_quant_peak = other.max_pre_quant_peak;
        }
    }
}

/// Fixed-size per-channel FIR error-feedback ring buffer (`<= MAX_SHAPER_ORDER` taps).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ShaperChannelState {
    hist: [f64; MAX_SHAPER_ORDER],
    /// Index of the most recently pushed quantization error `e[n - 1]`.
    idx: usize,
}

impl ShaperChannelState {
    #[inline]
    pub fn reset(&mut self) {
        self.hist = [0.0; MAX_SHAPER_ORDER];
        self.idx = 0;
    }

    #[inline]
    fn feedback(&self, coeffs: &[f64]) -> f64 {
        let mut fb = 0.0_f64;
        let mut pos = self.idx;
        for &c in coeffs {
            fb += c * self.hist[pos];
            pos = if pos == 0 {
                MAX_SHAPER_ORDER - 1
            } else {
                pos - 1
            };
        }
        fb
    }

    #[inline]
    fn push(&mut self, err: f64) {
        self.idx = if self.idx + 1 == MAX_SHAPER_ORDER {
            0
        } else {
            self.idx + 1
        };
        self.hist[self.idx] = err;
    }
}

/// Portable quantizer PRNG + per-channel shaper error-feedback history for equal-rate gapless
/// continuity (`docs/research/04-noise-shapers.md` §6.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuantizerCarryoverState {
    pub rng_state: u64,
    pub shaper_l: ShaperChannelState,
    pub shaper_r: ShaperChannelState,
}

/// Stateful streaming `f64 -> i32` quantizer preserving continuous TPDF PRNG and error-feedback
/// noise-shaper state across blocks (and across equal-rate gapless boundaries).
#[derive(Debug, Clone)]
pub struct StreamingQuantizer {
    requested_shaper: NoiseShaperProfile,
    effective_shaper: NoiseShaperProfile,
    dac_bits: u8,
    sample_rate_hz: u32,
    coeffs: &'static [f64],
    rng_state: u64,
    shaper_l: ShaperChannelState,
    shaper_r: ShaperChannelState,
    stats: QuantizerStats,
    max_feedback_lsb: f64,
}

impl StreamingQuantizer {
    /// Construct a quantizer assuming `384,000 Hz` when a shaped profile is requested.
    ///
    /// Prefer [`Self::with_rate`] whenever the actual target rate (`352,800` vs `384,000 Hz`,
    /// or a non-`8fS` pass-through rate) is known.
    pub fn new(shaper: NoiseShaperProfile, dac_bits: u8) -> Result<Self, String> {
        Self::with_rate(shaper, dac_bits, 384_000)
    }

    /// Construct a quantizer bound to `sample_rate_hz`.
    ///
    /// Shaped profiles (`ns-gentle-2`, `ns5`, `lns15`, `pggb-optimal`) load their frozen `4fS`/`8fS`
    /// coefficient table for `176,400`, `192,000`, `352,800`, or `384,000 Hz`. At any other sample
    /// rate (`<= 96 kHz`), shaped profiles fall back cleanly to `TpdfDither` (`04-noise-shapers.md` §6.2).
    pub fn with_rate(
        shaper: NoiseShaperProfile,
        dac_bits: u8,
        sample_rate_hz: u32,
    ) -> Result<Self, String> {
        Self::with_rate_options(shaper, dac_bits, sample_rate_hz, true)
    }

    /// Construct a quantizer bound to `sample_rate_hz`, optionally suppressing stderr warnings when
    /// an automatic shaper profile cleanly degrades to flat `TpdfDither` on a `<= 96 kHz` DAC.
    pub fn with_rate_options(
        shaper: NoiseShaperProfile,
        dac_bits: u8,
        sample_rate_hz: u32,
        warn_on_fallback: bool,
    ) -> Result<Self, String> {
        let (effective_shaper, coeffs): (NoiseShaperProfile, &'static [f64]) = match shaper {
            NoiseShaperProfile::Bypass => (NoiseShaperProfile::Bypass, &[]),
            NoiseShaperProfile::TpdfDither => (NoiseShaperProfile::TpdfDither, &[]),
            NoiseShaperProfile::NsGentle2
            | NoiseShaperProfile::Ns5Multibit
            | NoiseShaperProfile::Lns15Smooth
            | NoiseShaperProfile::PggbOptimal => {
                if let Some(table) = frozen_shaper_coeffs(shaper, sample_rate_hz) {
                    (shaper, table)
                } else {
                    if warn_on_fallback {
                        eprintln!(
                            "[shaper warn] noise shaper '{shaper}' requires 4fS/8fS (176.4/192/352.8/384 kHz), \
                             got {sample_rate_hz} Hz; falling back to tpdf"
                        );
                    }
                    (NoiseShaperProfile::TpdfDither, &[])
                }
            }
        };

        Ok(Self {
            requested_shaper: shaper,
            effective_shaper,
            dac_bits: dac_bits.clamp(16, 32),
            sample_rate_hz,
            coeffs,
            rng_state: DEFAULT_QUANTIZER_SEED,
            shaper_l: ShaperChannelState::default(),
            shaper_r: ShaperChannelState::default(),
            stats: QuantizerStats::default(),
            max_feedback_lsb: 0.0,
        })
    }

    /// The profile originally requested by the caller/policy.
    pub fn requested_shaper(&self) -> NoiseShaperProfile {
        self.requested_shaper
    }

    /// The active profile executing in this quantizer (equals `TpdfDither` if a shaped profile
    /// fell back at a non-`8fS` rate).
    pub fn effective_shaper(&self) -> NoiseShaperProfile {
        self.effective_shaper
    }

    /// Effective quantizer bit depth (`16..=32`).
    pub fn dac_bits(&self) -> u8 {
        self.dac_bits
    }

    /// Sample rate this quantizer's shaper coefficients are bound to.
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Active FIR error-feedback taps (`&[]` for `Bypass` and `TpdfDither`).
    pub fn coeffs(&self) -> &'static [f64] {
        self.coeffs
    }

    /// Return cumulative pre-quantizer peak and over-clip count for this track session.
    pub fn stats(&self) -> QuantizerStats {
        self.stats
    }

    /// Maximum absolute error-feedback term observed across both channels in LSB units.
    pub fn max_feedback_lsb(&self) -> f64 {
        self.max_feedback_lsb
    }

    /// Snapshot PRNG and per-channel shaper history for equal-rate gapless handoff (§6.4).
    pub fn export_state(&self) -> QuantizerCarryoverState {
        QuantizerCarryoverState {
            rng_state: self.rng_state,
            shaper_l: self.shaper_l,
            shaper_r: self.shaper_r,
        }
    }

    /// Restore PRNG and per-channel shaper history from a preceding equal-rate track (§6.4).
    pub fn restore_state(&mut self, state: QuantizerCarryoverState) {
        self.rng_state = state.rng_state;
        self.shaper_l = state.shaper_l;
        self.shaper_r = state.shaper_r;
    }

    /// Zero the per-channel error-feedback history (e.g. on rate change, seek, or stop).
    pub fn reset_shaper_state(&mut self) {
        self.shaper_l.reset();
        self.shaper_r.reset();
    }

    #[inline]
    fn next_uniform(&mut self) -> f64 {
        self.rng_state = self.rng_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let bits53 = (z ^ (z >> 31)) >> 11;
        (bits53 as f64) * (1.0 / ((1_u64 << 53) as f64))
    }

    pub fn quantize_block(&mut self, audio: &StereoF64Buffer) -> InterleavedInt32Buffer {
        let bits = self.dac_bits;
        let levels = (1_u64 << (bits - 1)) as f64;
        let max_int = ((1_i64 << (bits - 1)) - 1) as f64;
        let min_int = -(1_i64 << (bits - 1)) as f64;
        let shift = 32 - u32::from(bits);
        let use_tpdf = !matches!(self.effective_shaper, NoiseShaperProfile::Bypass);
        let coeffs = self.coeffs;
        let shaped = !coeffs.is_empty();

        let n = audio.left.len().min(audio.right.len());
        let mut frames = Vec::with_capacity(n);

        for i in 0..n {
            // Keep RNG draw order identical to baseline TPDF so N=0 is bit-identical.
            let (dl, dr) = if use_tpdf {
                let l = self.next_uniform() - self.next_uniform();
                let r = self.next_uniform() - self.next_uniform();
                (l, r)
            } else {
                (0.0, 0.0)
            };

            let sl = audio.left[i];
            let sr = audio.right[i];
            let peak = sl.abs().max(sr.abs());
            if peak > self.stats.max_pre_quant_peak {
                self.stats.max_pre_quant_peak = peak;
            }

            // Review #6 Finding #18 & INV-04 §6.3: count overs strictly BEFORE adding feedback
            // or TPDF dither so representable full-scale samples never trigger spurious clip warns.
            let scaled_l = sl * levels;
            if scaled_l > max_int || scaled_l < min_int {
                self.stats.overs_count += 1;
            }
            let scaled_r = sr * levels;
            if scaled_r > max_int || scaled_r < min_int {
                self.stats.overs_count += 1;
            }

            let (raw_l, raw_r) = if shaped {
                let fb_l = self.shaper_l.feedback(coeffs);
                let fb_r = self.shaper_r.feedback(coeffs);
                let fb_abs = fb_l.abs().max(fb_r.abs());
                if fb_abs > self.max_feedback_lsb {
                    self.max_feedback_lsb = fb_abs;
                }

                let v_l = scaled_l + fb_l;
                let v_r = scaled_r + fb_r;

                let q_l = (v_l + dl).round();
                let q_r = (v_r + dr).round();

                // §6.1 / §6.3 critical stability rule: compute quantization error e = q - v
                // strictly BEFORE clamping so clipping can never destabilize the feedback loop.
                let e_l = if q_l.is_finite() && v_l.is_finite() {
                    q_l - v_l
                } else {
                    0.0
                };
                let e_r = if q_r.is_finite() && v_r.is_finite() {
                    q_r - v_r
                } else {
                    0.0
                };
                self.shaper_l.push(e_l);
                self.shaper_r.push(e_r);
                (q_l, q_r)
            } else {
                ((scaled_l + dl).round(), (scaled_r + dr).round())
            };

            let ql = raw_l.clamp(min_int, max_int) as i32;
            let qr = raw_r.clamp(min_int, max_int) as i32;
            frames.push([ql.wrapping_shl(shift), qr.wrapping_shl(shift)]);
        }

        InterleavedInt32Buffer {
            sample_rate_hz: audio.sample_rate_hz,
            frames,
        }
    }
}

/// One-shot convenience wrapper around [`StreamingQuantizer::with_rate`].
pub fn quantize_to_int32(
    audio: &StereoF64Buffer,
    shaper: NoiseShaperProfile,
    dac_bits: u8,
) -> Result<InterleavedInt32Buffer, String> {
    let mut q = StreamingQuantizer::with_rate(shaper, dac_bits, audio.sample_rate_hz)?;
    Ok(q.quantize_block(audio))
}
