//! Pure IEEE 754 `f64` upsampling trait & initial simple windowed-sinc upsampler.
//!
//! Hard architectural invariants:
//! - Every upsampler's target output is ALWAYS floating point (`StereoF64Buffer`).
//! - Integer quantization NEVER occurs inside an upsampler (only once at final DAC output).
//! - Filter tap counts are strictly capped `<= MAX_FILTER_TAPS` (`4,000,000`).

use std::f64::consts::PI;

use ap_core::buffer::StereoF64Buffer;

use crate::selector::{FilterArchetype, SelectedDspPlan, UpsampleRatio};

/// Pluggable floating-point upsampler interface (always `StereoF64Buffer` -> `StereoF64Buffer`).
pub trait Upsampler: Send + Sync {
    fn name(&self) -> &'static str;

    fn upsample(
        &self,
        input: &StereoF64Buffer,
        plan: &SelectedDspPlan,
    ) -> Result<StereoF64Buffer, String>;
}

/// Build the concrete floating-point [`Upsampler`] implementation for a [`SelectedDspPlan`].
pub fn build_upsampler(plan: &SelectedDspPlan) -> Result<Box<dyn Upsampler>, String> {
    match plan.filter {
        FilterArchetype::Bypass => Ok(Box::new(BypassUpsampler)),
        FilterArchetype::SimpleTest => Ok(Box::new(SimpleWindowedSincUpsampler::default())),
        other => Err(format!(
            "upsampler filter '{other}' is not implemented yet (pending INV-03 algorithm research); \
             use 'auto', 'simple-test', or 'bypass' for now"
        )),
    }
}

/// Exact 1:1 floating-point pass-through (used for native 352.8/384 kHz tracks or `--filter bypass`).
#[derive(Debug, Default)]
pub struct BypassUpsampler;

impl Upsampler for BypassUpsampler {
    fn name(&self) -> &'static str {
        "bypass"
    }

    fn upsample(
        &self,
        input: &StereoF64Buffer,
        _plan: &SelectedDspPlan,
    ) -> Result<StereoF64Buffer, String> {
        Ok(input.clone())
    }
}

/// Initial simple polyphase windowed-sinc `f64` upsampler (Blackman-Harris window).
///
/// Serves as the clean bootstrap floating-point upsampler (`2x / 4x / 8x / 12x / 16x` ->
/// `352.8 / 384 kHz`) before deep `INV-03` 4M-tap filter algorithms are added:
/// - Pure IEEE 754 `f64` throughout, zero external crates.
/// - Linear phase, centered polyphase branches (zero fractional group-delay shift).
/// - Exact unity DC gain per polyphase branch (`sum(weights) == 1.0`).
/// - ~92 dB ultrasonic imaging stopband attenuation (4-term Blackman-Harris window).
#[derive(Debug, Clone, Copy)]
pub struct SimpleWindowedSincUpsampler {
    /// Number of source input samples per polyphase branch (must be even; default `64`).
    pub taps_per_phase: usize,
}

impl Default for SimpleWindowedSincUpsampler {
    fn default() -> Self {
        Self { taps_per_phase: 64 }
    }
}

impl SimpleWindowedSincUpsampler {
    /// Build `ratio` normalized `f64` polyphase branches for integer upsampling `1 -> ratio`.
    ///
    /// For output sample `n_out = n_in * ratio + phase` (`phase in 0..ratio`),
    /// `y[n_in * ratio + phase] = sum_{k=0..taps_per_phase} w[phase][k] * x[n_in + k - half]`.
    fn build_polyphase_bank(&self, ratio: usize) -> Vec<Vec<f64>> {
        let m = self.taps_per_phase.max(8) & !1; // enforce even >= 8
        let half = (m / 2) as isize;
        // Cutoff slightly below source Nyquist (0.93 * f_Nyq) so the 64-tap transition band
        // completes cleanly before the first spectral image.
        let cutoff = 0.93_f64;
        let radius = (m as f64) * 0.5;

        let mut bank = Vec::with_capacity(ratio);
        for phase in 0..ratio {
            let frac = (phase as f64) / (ratio as f64);
            let mut weights = Vec::with_capacity(m);
            let mut sum = 0.0_f64;

            for k in 0..m {
                // Distance from desired output time (n_in + frac) to input sample (n_in + k - half)
                let dt = (k as isize - half) as f64 - frac;
                let sinc = if dt.abs() < 1e-12 {
                    cutoff
                } else {
                    let arg = PI * cutoff * dt;
                    arg.sin() / (PI * dt)
                };
                // 4-term Blackman-Harris window over [-radius, +radius]
                let u = (dt + radius) / (2.0 * radius);
                let win = if (0.0..=1.0).contains(&u) {
                    0.35875
                        - 0.48829 * (2.0 * PI * u).cos()
                        + 0.14128 * (4.0 * PI * u).cos()
                        - 0.01168 * (6.0 * PI * u).cos()
                } else {
                    0.0
                };
                let w = sinc * win;
                sum += w;
                weights.push(w);
            }

            // Normalize every polyphase branch strictly to unity DC gain (zero passband ripple at DC)
            if sum.abs() > 1e-15 {
                let inv = 1.0 / sum;
                for w in &mut weights {
                    *w *= inv;
                }
            }
            bank.push(weights);
        }
        bank
    }

    fn upsample_channel(&self, x: &[f64], ratio: usize, bank: &[Vec<f64>]) -> Vec<f64> {
        let n_in = x.len();
        let m = bank[0].len();
        let half = m / 2;
        let mut out = vec![0.0_f64; n_in * ratio];

        // Pad input with `half` zeros on both sides so the inner convolution loop has ZERO
        // bounds checks and auto-vectorizes cleanly in LLVM/rustc.
        let mut padded = vec![0.0_f64; n_in + m];
        padded[half..half + n_in].copy_from_slice(x);

        for n in 0..n_in {
            let win_slice = &padded[n..n + m];
            let base = n * ratio;
            for (phase, coeffs) in bank.iter().enumerate() {
                let mut acc = 0.0_f64;
                for (&s, &c) in win_slice.iter().zip(coeffs.iter()) {
                    acc += s * c;
                }
                out[base + phase] = acc;
            }
        }
        out
    }
}

impl Upsampler for SimpleWindowedSincUpsampler {
    fn name(&self) -> &'static str {
        "simple-test"
    }

    fn upsample(
        &self,
        input: &StereoF64Buffer,
        plan: &SelectedDspPlan,
    ) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        let ratio = match plan.ratio {
            UpsampleRatio::Bypass1x => return Ok(input.clone()),
            UpsampleRatio::Integer(r) if r >= 2 => r as usize,
            UpsampleRatio::Integer(_) => return Ok(input.clone()),
            UpsampleRatio::Arbitrary => {
                return Err(format!(
                    "simple-test upsampler requires an exact integer ratio to 352.8/384 kHz \
                     (source rate {} Hz is non-integer)",
                    input.sample_rate_hz
                ));
            }
        };

        let bank = self.build_polyphase_bank(ratio);
        let left = self.upsample_channel(&input.left, ratio, &bank);
        let right = self.upsample_channel(&input.right, ratio, &bank);

        Ok(StereoF64Buffer {
            sample_rate_hz: input.sample_rate_hz * (ratio as u32),
            left,
            right,
        })
    }
}
