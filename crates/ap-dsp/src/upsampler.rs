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

use crate::MAX_FILTER_TAPS;
use crate::selector::{
    FilterArchetype, MAX_SUPPORTED_SOURCE_RATE_HZ, MAX_UPSAMPLE_RATIO,
    MIN_SUPPORTED_SOURCE_RATE_HZ, SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE,
    SIMPLE_TEST_MIN_TAPS_PER_PHASE, SelectedDspPlan, UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsampleRatio,
    simple_test_taps_per_phase,
};

/// Unpadded trailing FIR input history carried between consecutive equal-rate gapless tracks
/// so interpolation across track boundaries matches a single continuous one-shot upsample
/// with zero boundary dip or click.
#[derive(Debug, Clone)]
pub struct UpsamplerCarryover {
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    pub taps_per_phase: usize,
    pub hist_l: Vec<f64>,
    pub hist_r: Vec<f64>,
}

impl UpsamplerCarryover {
    /// Maximum absolute sample amplitude present in the carried unpadded FIR history (`0.0` if empty).
    pub fn peak_linear(&self) -> f64 {
        let max_l = self
            .hist_l
            .iter()
            .copied()
            .map(f64::abs)
            .fold(0.0_f64, f64::max);
        let max_r = self
            .hist_r
            .iter()
            .copied()
            .map(f64::abs)
            .fold(0.0_f64, f64::max);
        max_l.max(max_r)
    }

    /// Flush the carried unpadded FIR history against trailing zeros into `f64` samples
    /// (used when a gapless sequence ends, switches sample rate, or encounters an error).
    pub fn flush_to_f64(self) -> StereoF64Buffer {
        if self.hist_l.is_empty() || self.ratio == 0 || self.taps_per_phase < 2 {
            return StereoF64Buffer::empty(self.target_rate_hz);
        }
        let mut up =
            SimpleWindowedSincUpsampler::new(self.source_rate_hz, self.ratio, self.taps_per_phase);
        up.hist_l = self.hist_l;
        up.hist_r = self.hist_r;
        up.total_in_frames = 1; // enable trailing zero flush
        up.flush()
            .unwrap_or_else(|_| StereoF64Buffer::empty(self.target_rate_hz))
    }
}

/// Pluggable stateful floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`).
pub trait Upsampler: Send {
    fn name(&self) -> &'static str;

    /// Total polyphase FIR kernel taps (`0` for [`NullUpsampler`]).
    fn total_taps(&self) -> usize;

    /// Process one bounded source block, emitting all ready upsampled `f64` samples.
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;

    /// Flush trailing FIR overlap tail at end-of-track (emits remaining exact samples).
    fn flush(&mut self) -> Result<StereoF64Buffer, String>;

    /// Attach unpadded FIR history from the preceding equal-rate gapless track instead of
    /// leading zeros. Returns `Err(carryover)` if the carryover is incompatible with this
    /// upsampler's source rate, ratio, or filter length.
    fn set_incoming_carryover(
        &mut self,
        carryover: UpsamplerCarryover,
    ) -> Result<(), UpsamplerCarryover> {
        Err(carryover)
    }

    /// End a track for gapless continuity: skip trailing zero-padding and hand the unpadded
    /// FIR history to the next equal-rate session so there is zero boundary discontinuity.
    fn flush_for_gapless(&mut self) -> Result<(StereoF64Buffer, Option<UpsamplerCarryover>), String> {
        self.flush().map(|buf| (buf, None))
    }
}

/// Build the concrete stateful floating-point [`Upsampler`] for a [`SelectedDspPlan`].
pub fn build_upsampler(plan: &SelectedDspPlan) -> Result<Box<dyn Upsampler>, String> {
    if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ)
        .contains(&plan.source_rate_hz)
    {
        return Err(format!(
            "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {} Hz is outside the supported {}..={} Hz range",
            plan.source_rate_hz, MIN_SUPPORTED_SOURCE_RATE_HZ, MAX_SUPPORTED_SOURCE_RATE_HZ
        ));
    }

    match plan.filter {
        FilterArchetype::Null => Ok(Box::new(NullUpsampler::new(plan.target_rate_hz))),
        FilterArchetype::SimpleTest => {
            let ratio_u32 = match plan.ratio {
                UpsampleRatio::Bypass1x => {
                    return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
                }
                UpsampleRatio::Integer(r) if r >= 2 => r,
                UpsampleRatio::Integer(_) => {
                    return Ok(Box::new(NullUpsampler::new(plan.target_rate_hz)));
                }
                UpsampleRatio::Arbitrary => {
                    return Err(format!(
                        "{UNSUPPORTED_SAMPLE_RATE_PREFIX} simple-test upsampler requires an exact \
                         integer ratio to 352.8/384 kHz (source rate {} Hz is non-integer)",
                        plan.source_rate_hz
                    ));
                }
            };
            if ratio_u32 > MAX_UPSAMPLE_RATIO {
                return Err(format!(
                    "{UNSUPPORTED_SAMPLE_RATE_PREFIX} integer upsampling ratio {}x exceeds \
                     hard cap {}x (source rate {} Hz)",
                    ratio_u32, MAX_UPSAMPLE_RATIO, plan.source_rate_hz
                ));
            }
            let ratio = ratio_u32 as usize;
            let taps_per_phase = simple_test_taps_per_phase(ratio, plan.tap_budget).ok_or_else(|| {
                format!(
                    "--max-taps budget is too small for {ratio}x simple-test upsampling \
                     (minimum is {} taps = {SIMPLE_TEST_MIN_TAPS_PER_PHASE} taps/phase)",
                    ratio * SIMPLE_TEST_MIN_TAPS_PER_PHASE
                )
            })?;
            let total_taps = ratio.checked_mul(taps_per_phase).ok_or_else(|| {
                format!(
                    "upsampler tap count overflow (ratio={ratio}, taps_per_phase={taps_per_phase})"
                )
            })?;
            if total_taps > MAX_FILTER_TAPS {
                return Err(format!(
                    "upsampler total taps ({total_taps}) exceeds hard {MAX_FILTER_TAPS}-tap ceiling"
                ));
            }
            Ok(Box::new(SimpleWindowedSincUpsampler::new(
                plan.source_rate_hz,
                ratio,
                taps_per_phase,
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
    flushed: bool,
}

impl NullUpsampler {
    pub fn new(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz,
            flushed: false,
        }
    }
}

impl Upsampler for NullUpsampler {
    fn name(&self) -> &'static str {
        "null"
    }

    fn total_taps(&self) -> usize {
        0
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if self.flushed {
            return Err("null upsampler cannot accept blocks after flush()".to_string());
        }
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        if input.sample_rate_hz != self.sample_rate_hz {
            return Err(format!(
                "null upsampler sample rate mismatch: expected {} Hz, got {} Hz",
                self.sample_rate_hz, input.sample_rate_hz
            ));
        }
        Ok(input)
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        self.flushed = true;
        Ok(StereoF64Buffer::empty(self.sample_rate_hz))
    }
}

/// Initial stateful polyphase Kaiser-windowed sinc `f64` upsampler.
///
/// Retains strictly `taps_per_phase - 1` overlap samples across streaming blocks so arbitrary
/// chunk sizes produce **bit-for-bit identical** output to whole-track processing:
/// - Pure IEEE 754 `f64` throughout, zero external crates.
/// - Exact Type-I linear phase with centered symmetric polyphase branches (`h[-d] == h[d]`,
///   exact zero group-delay shift).
/// - Exact unity DC gain per polyphase branch (`sum(weights) == 1.0`).
/// - Flat passband through `20 kHz` on `44.1 -> 352.8 kHz` (`< 0.01 dB` droop at `256` taps/phase)
///   with `> 110 dB` ultrasonic imaging stopband attenuation (Kaiser `beta = 12.0`).
#[derive(Debug)]
pub struct SimpleWindowedSincUpsampler {
    source_rate_hz: u32,
    ratio: usize,
    taps_per_phase: usize,
    bank: Vec<Vec<f64>>,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    total_in_frames: usize,
    has_incoming_carryover: bool,
    flushed: bool,
}

impl SimpleWindowedSincUpsampler {
    pub fn new(source_rate_hz: u32, ratio: usize, taps_per_phase: usize) -> Self {
        let m = taps_per_phase.max(8) & !1; // enforce even >= 8
        let half = m / 2;
        let bank = build_polyphase_bank(ratio, m);
        // Pre-seed history with `half - 1` leading zeros so tap `j = 0` (`k = half - 1`) aligns
        // strictly with `x[0]`, and taps `j in -half + 1 ..= half` are centered symmetrically.
        let lead = half - 1;
        Self {
            source_rate_hz,
            ratio,
            taps_per_phase: m,
            bank,
            hist_l: vec![0.0_f64; lead],
            hist_r: vec![0.0_f64; lead],
            total_in_frames: 0,
            has_incoming_carryover: false,
            flushed: false,
        }
    }

    /// Expose polyphase coefficient bank for mathematical symmetry & response inspection.
    pub fn polyphase_bank(&self) -> &[Vec<f64>] {
        &self.bank
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

    fn total_taps(&self) -> usize {
        self.ratio * self.taps_per_phase
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if self.flushed {
            return Err("simple-test upsampler cannot accept blocks after flush()".to_string());
        }
        if input.left.len() != input.right.len() {
            return Err("stereo f64 buffer L/R length mismatch".to_string());
        }
        if input.sample_rate_hz != self.source_rate_hz {
            return Err(format!(
                "simple-test upsampler sample rate mismatch: expected {} Hz, got {} Hz",
                self.source_rate_hz, input.sample_rate_hz
            ));
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
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok(StereoF64Buffer::empty(out_rate));
        }
        self.flushed = true;

        // Append `half` trailing zeros (total padding: `half - 1` leading + `half` trailing
        // -> exact `total_in_frames` windows -> exact `total_in_frames * ratio` output samples!).
        let pad_tail = self.taps_per_phase / 2;
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

    fn set_incoming_carryover(
        &mut self,
        carryover: UpsamplerCarryover,
    ) -> Result<(), UpsamplerCarryover> {
        if self.flushed
            || self.total_in_frames != 0
            || carryover.source_rate_hz != self.source_rate_hz
            || carryover.ratio != self.ratio
            || carryover.taps_per_phase != self.taps_per_phase
            || carryover.hist_l.len() != carryover.hist_r.len()
            || carryover.hist_l.len() >= self.taps_per_phase
        {
            return Err(carryover);
        }
        self.hist_l = carryover.hist_l;
        self.hist_r = carryover.hist_r;
        self.has_incoming_carryover = true;
        Ok(())
    }

    fn flush_for_gapless(&mut self) -> Result<(StereoF64Buffer, Option<UpsamplerCarryover>), String> {
        let out_rate = self.source_rate_hz * (self.ratio as u32);
        if self.flushed || (self.total_in_frames == 0 && !self.has_incoming_carryover) {
            self.flushed = true;
            return Ok((StereoF64Buffer::empty(out_rate), None));
        }
        self.flushed = true;
        let hist_l = core::mem::take(&mut self.hist_l);
        let hist_r = core::mem::take(&mut self.hist_r);
        if hist_l.is_empty() {
            return Ok((StereoF64Buffer::empty(out_rate), None));
        }
        Ok((
            StereoF64Buffer::empty(out_rate),
            Some(UpsamplerCarryover {
                source_rate_hz: self.source_rate_hz,
                target_rate_hz: out_rate,
                ratio: self.ratio,
                taps_per_phase: self.taps_per_phase,
                hist_l,
                hist_r,
            }),
        ))
    }
}

/// Zeroth-order modified Bessel function of the first kind `I_0(x)` computed in pure `f64`
/// to full machine precision (`~1e-16` relative accuracy).
fn bessel_i0(x: f64) -> f64 {
    let half_x = 0.5 * x;
    let y = half_x * half_x;
    let mut sum = 1.0_f64;
    let mut term = 1.0_f64;
    let mut k = 1.0_f64;
    while term > sum * 1e-16 {
        term *= y / (k * k);
        sum += term;
        k += 1.0;
        if k > 200.0 {
            break;
        }
    }
    sum
}

/// Choose Kaiser `(cutoff, beta)` for a given taps-per-phase length `m`.
///
/// At the default `256` taps/phase, returns exact `(0.955, 12.0)` (`< 0.01 dB` 20 kHz droop,
/// `> 115 dB` ultrasonic stopband). For smaller user-capped `--max-taps` budgets, scales target
/// stopband attenuation `A` and Kaiser `beta(A)` smoothly with `m` so the transition band does
/// not collapse into severe audible passband droop.
fn kaiser_params_for_taps_per_phase(m: usize) -> (f64, f64) {
    if m >= SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE {
        return (0.955_f64, 12.0_f64);
    }
    let m_f = (m.max(SIMPLE_TEST_MIN_TAPS_PER_PHASE)) as f64;
    let a_db = (18.0 + 0.85 * m_f).clamp(28.0, 118.0);
    let beta = if a_db > 50.0 {
        0.1102 * (a_db - 8.7)
    } else if a_db >= 21.0 {
        0.5842 * (a_db - 21.0).powf(0.4) + 0.07886 * (a_db - 21.0)
    } else {
        0.0
    };
    let half_tw_nyq = ((a_db - 8.0) / (7.1785 * m_f)) * 0.5;
    let cutoff = (0.91 + half_tw_nyq).clamp(0.955, 0.995);
    (cutoff, beta)
}

/// Build an exact linear-phase Kaiser-windowed sinc polyphase bank (`ratio` phases x `m` taps).
///
/// Using tap grid `j = -half + 1 ..= half` with strict window support `|dt| < half`:
/// - Phase `0` (`frac = 0`) is strictly symmetric around `j = 0` (`j = half` gets weight `0.0`).
/// - Phases `p` and `ratio - p` satisfy `bank[ratio - p][m - 1 - k] == bank[p][k]` to machine
///   precision, yielding an exact odd-length Type-I zero-phase prototype filter.
fn build_polyphase_bank(ratio: usize, m: usize) -> Vec<Vec<f64>> {
    let half = (m / 2) as isize;
    let (cutoff, beta) = kaiser_params_for_taps_per_phase(m);
    let inv_i0_beta = 1.0 / bessel_i0(beta);
    let radius = (m as f64) * 0.5;

    let mut bank = Vec::with_capacity(ratio);
    for phase in 0..ratio {
        let frac = (phase as f64) / (ratio as f64);
        let mut weights = Vec::with_capacity(m);
        let mut sum = 0.0_f64;

        for k in 0..m {
            let j = (k as isize) - half + 1;
            let dt = (j as f64) - frac;
            let sinc = if dt.abs() < 1e-12 {
                cutoff
            } else {
                let arg = PI * cutoff * dt;
                arg.sin() / (PI * dt)
            };
            let win = if dt.abs() < radius {
                let x = dt / radius;
                let arg = beta * (1.0 - x * x).max(0.0).sqrt();
                bessel_i0(arg) * inv_i0_beta
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
