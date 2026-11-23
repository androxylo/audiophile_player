//! Single final floating-point -> integer PCM quantization stage & `<= 0 dB` safety headroom.
//!
//! Hard invariants:
//! - Digital gain boosting (`> 0 dB`) is strictly forbidden and rejected with an error.
//! - Integer quantization occurs strictly once here at the end of the floating-point pipeline.
//! - Currently implements flat TPDF dither (and exact `Bypass` pass-through); high-order `8fS`
//!   noise shapers (`NS5`, `LNS15`, `PGGB-class`) will be added in `INV-04`.

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

/// Quantize `f64` stereo audio ONCE into final interleaved MSB-aligned `i32` PCM for the DAC.
///
/// Uses deterministic triangular PDF (TPDF) dither at `dac_bits` precision (clamped to `16..=32`)
/// packed MSB-aligned into 32-bit signed integer frames.
pub fn quantize_to_int32(
    audio: &StereoF64Buffer,
    shaper: NoiseShaperProfile,
    dac_bits: u8,
) -> InterleavedInt32Buffer {
    let bits = dac_bits.clamp(16, 32);
    let levels = (1_u64 << (bits - 1)) as f64;
    let max_int = ((1_i64 << (bits - 1)) - 1) as f64;
    let min_int = -(1_i64 << (bits - 1)) as f64;
    let shift = 32 - u32::from(bits);

    let use_tpdf = shaper != NoiseShaperProfile::Bypass;
    let mut rng_state: u64 = 0x2026_0401_9E37_79B9;

    let mut next_uniform = || -> f64 {
        rng_state = rng_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = rng_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        let bits53 = (z ^ (z >> 31)) >> 11;
        (bits53 as f64) * (1.0 / ((1_u64 << 53) as f64))
    };

    let n = audio.left.len().min(audio.right.len());
    let mut frames = Vec::with_capacity(n);

    for i in 0..n {
        let dl = if use_tpdf {
            next_uniform() - next_uniform()
        } else {
            0.0
        };
        let dr = if use_tpdf {
            next_uniform() - next_uniform()
        } else {
            0.0
        };

        let ql = (audio.left[i] * levels + dl).round().clamp(min_int, max_int) as i32;
        let qr = (audio.right[i] * levels + dr).round().clamp(min_int, max_int) as i32;
        frames.push([ql.wrapping_shl(shift), qr.wrapping_shl(shift)]);
    }

    InterleavedInt32Buffer {
        sample_rate_hz: audio.sample_rate_hz,
        frames,
    }
}
