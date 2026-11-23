//! Stateful, bounded-memory floating-point upsampler trait & initial windowed-sinc kernel.
//!
//! Hard architectural invariants:
//! - Every upsampler consumes bounded [`StereoF64Buffer`] blocks (`process_block` + `flush`)
//!   and ALWAYS outputs [`StereoF64Buffer`] (pure IEEE 754 `f64`).
//! - Streaming chunked processing produces **bit-identical** output to whole-track processing
//!   across arbitrary chunk boundaries (verified by TDD).
//! - Filter tap counts are strictly capped `<= MAX_FILTER_TAPS` (`4,000,000`).

use std::f64::consts::PI;

use ap_core::buffer::StereoF64Buffer;

use crate::selector::{FilterArchetype, SelectedDspPlan, UpsampleRatio};

/// Pluggable stateful floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`).
pub trait Upsampler: Send {
    fn name(&self) -> &'static str;

    /// Process one bounded source block, emitting all ready upsampled `f64` samples.
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;

    /// Flush trailing FIR overlap tail at end-of-track (emits remaining exact samples).
    fn flush(&mut self) -> Result<StereoF64Buffer, String>;
}

/// Build the concrete stateful floating-point [`Upsampler`] for a [`SelectedDspPlan`].
pub fn build_upsampler(plan: &SelectedDspPlan) -> Result<Box<dyn Upsampler>, String> {
    match plan.filter {
        FilterArchetype::Null => Ok(Box::new(NullUpsampler::new(plan.target_rate_hz))),
        FilterArchetype::SimpleTest => {
            let ratio = match plan.ratio {
                UpsampleRatio::Bypass1x => return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz))),
                UpsampleRatio::Integer(r) if r >= 2 => r as usize,
                UpsampleRatio::Integer(_) => {
                    return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
                }
                UpsampleRatio::Arbitrary => {
                    return Err(format!(
                        "simple-test upsampler requires an exact integer ratio to 352.8/384 kHz \
                         (source rate {} Hz is non-integer)",
                        plan.source_rate_hz
                    ));
                }
            };
            Ok(Box::new(SimpleWindowedSincUpsampler::new(
                plan.source_rate_hz,
                ratio,
                64,
            )))
        }
        other => Err(format!(
            "upsampler filter '{other}' is not implemented yet (pending INV-03 algorithm research); \
             use 'auto', 'simple-test', or 'null' for now"
        )),
    }
}

/// Null 1:1 stateful floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`, zero copy).
///
/// Native `352.8 / 384 kHz` FLAC blocks (or live 8fS streams) pass through in `O(1)` with
/// zero reallocation so downstream `f64` stages (REW convolution, delay, quantization) always
/// operate uniformly on pure floating-point blocks.
#[derive(Debug)]
pub struct NullUpsampler {
    sample_rate_hz: u32,
}

impl NullUpsampler {
    pub fn new(sample_rate_hz: u32) -> Self {
        Self { sample_rate_hz }
    }
}

impl Upsampler for NullUpsampler {
    fn name(&self) -> &'static str {
        "null"
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        self.sample_rate_hz = input.sample_rate_hz;
        Ok(input)
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        Ok(StereoF64Buffer::empty(self.sample_rate_hz))
    }
}

/// Initial simple stateful polyphase windowed-sinc `f64` upsampler (Blackman-Harris window).
///
/// Retains strictly `taps_per_phase - 1` overlap samples across streaming blocks so arbitrary
/// chunk sizes produce **bit-for-bit identical** output to whole-track processing:
/// - Pure IEEE 754 `f64` throughout, zero external crates.
/// - Linear phase, centered polyphase branches (exact zero group-delay shift).
/// - Exact unity DC gain per polyphase branch (`sum(weights) == 1.0`).
/// - ~92 dB ultrasonic imaging stopband attenuation (4-term Blackman-Harris window).
#[derive(Debug)]
pub struct SimpleWindowedSincUpsampler {
    source_rate_hz: u32,
    ratio: usize,
    taps_per_phase: usize,
    bank: Vec<Vec<f64>>,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    total_in_frames: usize,
    flushed: bool,
}

impl SimpleWindowedSincUpsampler {
    pub fn new(source_rate_hz: u32, ratio: usize, taps_per_phase: usize) -> Self {
        let m = taps_per_phase.max(8) & !1; // enforce even >= 8
        let half = m / 2;
        let bank = build_polyphase_bank(ratio, m);
        // Pre-seed history with `half` leading zeros so the first output sample is centered on x[0].
        Self {
            source_rate_hz,
            ratio,
            taps_per_phase: m,
            bank,
            hist_l: vec![0.0_f64; half],
            hist_r: vec![0.0_f64; half],
            total_in_frames: 0,
            flushed: false,
        }
    }

    fn convolve_available(&mut self) -> (Vec<f64>, Vec<f64>) {
        let m = self.taps_per_phase;
        let available = self.hist_l.len();
        if available < m {
            return (Vec::new(), Vec::new());
        }
        // Number of complete m-sample windows available; each produces `ratio` output samples.
        let n_windows = available - m + 1;
        let out_len = n_windows * self.ratio;
        let mut out_l = vec![0.0_f64; out_len];
        let mut out_r = vec![0.0_f64; out_len];

        for n in 0..n_windows {
            let wl = &self.hist_l[n..n + m];
            let wr = &self.hist_r[n..n + m];
            let base = n * self.ratio;
            for (phase, coeffs) in self.bank.iter().enumerate() {
                let mut acc_l = 0.0_f64;
                let mut acc_r = 0.0_f64;
                for i in 0..m {
                    let c = coeffs[i];
                    acc_l += wl[i] * c;
                    acc_r += wr[i] * c;
                }
                out_l[base + phase] = acc_l;
                out_r[base + phase] = acc_r;
            }
        }

        // Retain strictly the trailing `m - 1` samples for overlap into the next streaming block.
        self.hist_l.drain(0..n_windows);
        self.hist_r.drain(0..n_windows);
        (out_l, out_r)
    }
}

impl Upsampler for SimpleWindowedSincUpsampler {
    fn name(&self) -> &'static str {
        "simple-test"
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        let out_rate = self.source_rate_hz * (self.ratio as u32);
        if input.is_empty() {
            return Ok(StereoF64Buffer::empty(out_rate));
        }

        self.total_in_frames += input.left.len();
        self.hist_l.extend_from_slice(&input.left);
        self.hist_r.extend_from_slice(&input.right);

        let (left, right) = self.convolve_available();
        Ok(StereoF64Buffer {
            sample_rate_hz: out_rate,
            left,
            right,
        })
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        let out_rate = self.source_rate_hz * (self.ratio as u32);
        if self.flushed || self.total_in_frames == 0 {
            self.flushed = true;
            return Ok(StereoF64Buffer::empty(out_rate));
        }
        self.flushed = true;

        // Append `half - 1` trailing zeros (total padding: `half` leading + `half - 1` trailing
        // -> exact `total_in_frames` windows -> exact `total_in_frames * ratio` output samples!).
        let pad_tail = self.taps_per_phase / 2 - 1;
        if pad_tail > 0 {
            self.hist_l.resize(self.hist_l.len() + pad_tail, 0.0_f64);
            self.hist_r.resize(self.hist_r.len() + pad_tail, 0.0_f64);
        }

        let (left, right) = self.convolve_available();
        self.hist_l.clear();
        self.hist_r.clear();
        Ok(StereoF64Buffer {
            sample_rate_hz: out_rate,
            left,
            right,
        })
    }
}

fn build_polyphase_bank(ratio: usize, m: usize) -> Vec<Vec<f64>> {
    let half = (m / 2) as isize;
    let cutoff = 0.93_f64;
    let radius = (m as f64) * 0.5;

    let mut bank = Vec::with_capacity(ratio);
    for phase in 0..ratio {
        let frac = (phase as f64) / (ratio as f64);
        let mut weights = Vec::with_capacity(m);
        let mut sum = 0.0_f64;

        for k in 0..m {
            let dt = (k as isize - half) as f64 - frac;
            let sinc = if dt.abs() < 1e-12 {
                cutoff
            } else {
                let arg = PI * cutoff * dt;
                arg.sin() / (PI * dt)
            };
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
