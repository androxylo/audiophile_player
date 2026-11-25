//! Single final floating-point -> integer PCM quantization stage & `<= 0 dB` safety headroom.
//!
//! Hard invariants:
//! - Digital gain boosting (`> 0 dB`) is strictly forbidden and rejected with an error.
//! - Integer quantization occurs strictly once here at the end of the floating-point pipeline.
//! - [`StreamingQuantizer`] maintains continuous deterministic PRNG (and future noise-shaper
//!   error-feedback) state seamlessly across streaming block boundaries.

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::headroom::MAX_ALLOWED_GAIN_DB;

use crate::selector::NoiseShaperProfile;

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
    /// Number of individual channel samples clamped against `[min_int, max_int]`.
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

/// Stateful streaming `f64 -> i32` quantizer preserving continuous dither/shaper state across blocks.
#[derive(Debug, Clone)]
pub struct StreamingQuantizer {
    shaper: NoiseShaperProfile,
    dac_bits: u8,
    rng_state: u64,
    stats: QuantizerStats,
}

impl StreamingQuantizer {
    pub fn new(shaper: NoiseShaperProfile, dac_bits: u8) -> Result<Self, String> {
        if !shaper.is_implemented() {
            return Err(format!(
                "noise shaper '{shaper}' is not implemented yet (pending INV-04 algorithm research); \
                 use 'auto', 'tpdf', or 'bypass' for now"
            ));
        }
        Ok(Self {
            shaper,
            dac_bits: dac_bits.clamp(16, 32),
            rng_state: 0x2026_0401_9E37_79B9,
            stats: QuantizerStats::default(),
        })
    }

    /// Return cumulative pre-quantizer peak and over-clip count for this track session.
    pub fn stats(&self) -> QuantizerStats {
        self.stats
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
        let use_tpdf = match self.shaper {
            NoiseShaperProfile::Bypass => false,
            NoiseShaperProfile::TpdfDither => true,
            NoiseShaperProfile::Ns5Multibit
            | NoiseShaperProfile::Lns15Smooth
            | NoiseShaperProfile::PggbOptimal => {
                unreachable!("unimplemented noise shapers are rejected in StreamingQuantizer::new")
            }
        };

        let n = audio.left.len().min(audio.right.len());
        let mut frames = Vec::with_capacity(n);

        for i in 0..n {
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

            // Review #6 Finding #18: count overs strictly BEFORE adding TPDF dither so a
            // representable full-scale sample (`max_int / levels` = 0 dBFS) never triggers
            // spurious `[clip warn]` or false `M` write-backs from +1 LSB dither excursions.
            let scaled_l = sl * levels;
            if scaled_l > max_int || scaled_l < min_int {
                self.stats.overs_count += 1;
            }
            let scaled_r = sr * levels;
            if scaled_r > max_int || scaled_r < min_int {
                self.stats.overs_count += 1;
            }

            let raw_l = (scaled_l + dl).round();
            let raw_r = (scaled_r + dr).round();

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

/// One-shot convenience wrapper around [`StreamingQuantizer`].
pub fn quantize_to_int32(
    audio: &StereoF64Buffer,
    shaper: NoiseShaperProfile,
    dac_bits: u8,
) -> Result<InterleavedInt32Buffer, String> {
    let mut q = StreamingQuantizer::new(shaper, dac_bits)?;
    Ok(q.quantize_block(audio))
}
