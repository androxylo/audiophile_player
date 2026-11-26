// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Pure-`f64` last-stage anti-aliasing output downsampler for DACs limited to `< 352.8 / 384 kHz`.
//!
//! Hard architectural invariants:
//! - The main DSP pipeline (upsampling, de-emphasis, REW room convolution, channel delay) ALWAYS
//!   runs at locked `8fS` (`352,800` or `384,000 Hz`). This stage runs strictly at the very end,
//!   immediately before the single final quantizer (`quantize.rs`).
//! - When `input_rate_hz == output_rate_hz` (all `352.8 / 384 kHz` DACs), [`NullDownsampler`] is a
//!   **zero-copy, zero-overhead bypass** producing bit-identical output to the direct `8fS` path.
//! - Same-family decimation (`÷2`, `÷4`, `÷8`: `352.8 -> 176.4/88.2/44.1`, `384 -> 192/96/48`)
//!   uses an exact linear-phase odd-length Kaiser-windowed sinc (`A = 200 dB`, double-double `Dd`
//!   kernel synthesis) with flat `±0.01 dB` passband and `<= -190 dB` aliasing rejection.
//! - Cross-family last-resort fallback (e.g. `352.8 -> 192/96/48` on 48 kHz-only HDMI/SPDIF DACs)
//!   uses an exact reduced-ratio `L / M` polyphase Kaiser-windowed sinc decimator in pure `f64`.
//! - Both decimators are strictly streaming-chunk-invariant (bit-identical across arbitrary chunk
//!   sizes) and export/restore [`DownsamplerCarryover`] for zero-click gapless album transitions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use ap_core::buffer::StereoF64Buffer;

use crate::dd::{DD_ONE, DD_PI, DD_TWO_PI, DD_ZERO, Dd, dd_bessel_i0, dd_sum_f64};
use crate::upsampler::KAISER_STEEP_BETA;

/// Stateful carryover for seamless equal-output-rate gapless transitions across a downsampler.
#[derive(Debug, Clone)]
pub struct DownsamplerCarryover {
    pub input_rate_hz: u32,
    pub output_rate_hz: u32,
    pub hist_l: Vec<f64>,
    pub hist_r: Vec<f64>,
    /// Integer decimator phase remainder (`0..factor`), or rational phase accumulator `phase_m`.
    pub phase_pos: usize,
    /// Trailing zero-padding count needed to align exact output duration on non-gapless flush.
    pub pad_tail: usize,
    /// Cached linear-phase kernel (for integer decimator) or polyphase bank (for rational).
    pub(crate) kind: DownsamplerCarryKind,
}

#[derive(Debug, Clone)]
pub(crate) enum DownsamplerCarryKind {
    Integer {
        factor: usize,
        taps: Arc<Vec<f64>>,
    },
    Rational {
        up_l: usize,
        down_m: usize,
        taps_per_phase: usize,
        bank: Arc<Vec<Vec<f64>>>,
    },
}

impl DownsamplerCarryover {
    /// Flush the carried downsampler FIR history against trailing zeros into `f64` samples at
    /// `output_rate_hz` (used when draining an unconsumed gapless carryover).
    pub fn flush_to_f64(self) -> StereoF64Buffer {
        match self.kind {
            DownsamplerCarryKind::Integer { factor, taps } => {
                let mut dec = IntegerDecimator {
                    input_rate_hz: self.input_rate_hz,
                    output_rate_hz: self.output_rate_hz,
                    factor,
                    taps,
                    hist_l: self.hist_l,
                    hist_r: self.hist_r,
                    phase_rem: self.phase_pos,
                    pad_tail: self.pad_tail,
                };
                dec.flush().unwrap_or_else(|_| StereoF64Buffer::empty(self.output_rate_hz))
            }
            DownsamplerCarryKind::Rational {
                up_l,
                down_m,
                taps_per_phase,
                bank,
            } => {
                let mut dec = RationalDecimator {
                    input_rate_hz: self.input_rate_hz,
                    output_rate_hz: self.output_rate_hz,
                    up_l,
                    down_m,
                    taps_per_phase,
                    bank,
                    hist_l: self.hist_l,
                    hist_r: self.hist_r,
                    phase_m: self.phase_pos,
                    pad_tail: self.pad_tail,
                };
                dec.flush().unwrap_or_else(|_| StereoF64Buffer::empty(self.output_rate_hz))
            }
        }
    }
}

/// Pluggable stateful pure-`f64` last-stage output downsampler (`8fS -> DAC rate`).
pub trait Downsampler: Send {
    fn name(&self) -> &'static str;
    fn input_rate_hz(&self) -> u32;
    fn output_rate_hz(&self) -> u32;
    fn is_bypass(&self) -> bool {
        self.input_rate_hz() == self.output_rate_hz()
    }
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String>;
    fn flush(&mut self) -> Result<StereoF64Buffer, String>;
    fn flush_for_gapless(
        &mut self,
    ) -> Result<(StereoF64Buffer, Option<DownsamplerCarryover>), String>;
    fn set_incoming_carryover(
        &mut self,
        carry: DownsamplerCarryover,
    ) -> Result<(), DownsamplerCarryover>;
}

/// Zero-copy pass-through downsampler used for all native `352.8 / 384 kHz` (`8fS`) DACs.
pub struct NullDownsampler {
    rate_hz: u32,
}

impl NullDownsampler {
    pub fn new(rate_hz: u32) -> Self {
        Self { rate_hz }
    }
}

impl Downsampler for NullDownsampler {
    fn name(&self) -> &'static str {
        "bypass"
    }
    fn input_rate_hz(&self) -> u32 {
        self.rate_hz
    }
    fn output_rate_hz(&self) -> u32 {
        self.rate_hz
    }
    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        Ok(input)
    }
    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        Ok(StereoF64Buffer::empty(self.rate_hz))
    }
    fn flush_for_gapless(
        &mut self,
    ) -> Result<(StereoF64Buffer, Option<DownsamplerCarryover>), String> {
        Ok((StereoF64Buffer::empty(self.rate_hz), None))
    }
    fn set_incoming_carryover(
        &mut self,
        carry: DownsamplerCarryover,
    ) -> Result<(), DownsamplerCarryover> {
        Err(carry)
    }
}

/// Design a linear-phase odd-length Kaiser-windowed sinc low-pass prototype (`A = 200 dB`) in
/// double-double (`Dd`) arithmetic with exact unity DC gain.
fn design_kaiser_lowpass_dd(num_taps: usize, cutoff_norm_cycles: f64) -> Vec<f64> {
    debug_assert!(num_taps % 2 == 1, "num_taps must be odd");
    debug_assert!(cutoff_norm_cycles > 0.0 && cutoff_norm_cycles < 0.5);

    let half = (num_taps - 1) / 2;
    let half_dd = Dd::from_f64(half as f64);
    let beta_dd = Dd::from_f64(KAISER_STEEP_BETA);
    let inv_i0_beta = DD_ONE.div(dd_bessel_i0(beta_dd));
    let fc_dd = Dd::from_f64(cutoff_norm_cycles);
    let two_fc = fc_dd.add(fc_dd);

    let mut raw = vec![0.0_f64; num_taps];
    for (i, slot) in raw.iter_mut().enumerate() {
        let m = i as isize - half as isize;
        let sinc_dd = if m == 0 {
            two_fc
        } else {
            let m_dd = Dd::from_f64(m as f64);
            let x = DD_TWO_PI.mul(fc_dd).mul(m_dd);
            let (sin_x, _) = x.sin_cos();
            sin_x.div(DD_PI.mul(m_dd))
        };
        let r = Dd::from_f64(m as f64).div(half_dd);
        let one_minus_r2 = DD_ONE.sub(r.sqr());
        let arg = if one_minus_r2.hi <= 0.0 {
            DD_ZERO
        } else {
            one_minus_r2.sqrt().mul(beta_dd)
        };
        let win = dd_bessel_i0(arg).mul(inv_i0_beta);
        *slot = sinc_dd.mul(win).to_f64();
    }

    let dc = dd_sum_f64(&raw).to_f64();
    let inv_dc = if dc.abs() > 1e-18 { 1.0 / dc } else { 1.0 };
    for h in &mut raw {
        *h *= inv_dc;
    }
    raw
}

/// Cached integer decimation kernels keyed by `(input_rate_hz, output_rate_hz)`.
fn integer_kernel_cache() -> &'static Mutex<HashMap<(u32, u32), Arc<Vec<f64>>>> {
    static CACHE: OnceLock<Mutex<HashMap<(u32, u32), Arc<Vec<f64>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn integer_decimator_spec(factor: usize) -> Result<(usize, f64), String> {
    // Odd linear-phase Kaiser A=200 dB low-pass parameters:
    //   delta_f_norm ≈ (A - 8) / (2.285 * 2π * (N - 1)) ≈ 13.37 / (N - 1)
    // Passband edge = fc - delta_f/2, stopband (-200 dB) = fc + delta_f/2 <= 0.5 / factor.
    //   D=2 (4fS, nyq=88.2/96 kHz):  N=257  -> delta_f=0.0522  -> fc=0.2239 (pass >= 70 kHz, stop <= 88.2/96 kHz)
    //   D=4 (2fS, nyq=44.1/48 kHz):  N=513  -> delta_f=0.0261  -> fc=0.1119 (pass >= 35 kHz, stop <= 44.1/48 kHz)
    //   D=8 (1fS, nyq=22.05/24 kHz): N=4097 -> delta_f=0.00326 -> fc=0.06087 (pass >= 20.89 kHz @ 352.8k, stop <= 22.05 kHz!)
    match factor {
        2 => Ok((257, 0.2238)),
        4 => Ok((513, 0.1119)),
        8 => Ok((4097, 0.06086)),
        other => Err(format!(
            "unsupported integer decimation factor {other} (expected 2, 4, or 8)"
        )),
    }
}

fn get_or_build_integer_kernel(
    input_rate_hz: u32,
    output_rate_hz: u32,
    factor: usize,
) -> Result<Arc<Vec<f64>>, String> {
    let key = (input_rate_hz, output_rate_hz);
    {
        let cache = integer_kernel_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(k) = cache.get(&key) {
            return Ok(Arc::clone(k));
        }
    }
    let (num_taps, fc_norm) = integer_decimator_spec(factor)?;
    let taps = Arc::new(design_kaiser_lowpass_dd(num_taps, fc_norm));
    let mut cache = integer_kernel_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.insert(key, Arc::clone(&taps));
    Ok(taps)
}

/// Exact integer power-of-2 linear-phase pure-`f64` Kaiser anti-aliasing decimator (`÷2`, `÷4`, `÷8`).
pub struct IntegerDecimator {
    input_rate_hz: u32,
    output_rate_hz: u32,
    factor: usize,
    taps: Arc<Vec<f64>>,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    /// Index of the next input sample within `hist` that aligns with the center tap (`half`) of
    /// the next decimated output sample. Initially `0`, advancing by `factor` per output sample.
    phase_rem: usize,
    pad_tail: usize,
}

impl IntegerDecimator {
    pub fn new(input_rate_hz: u32, output_rate_hz: u32) -> Result<Self, String> {
        if output_rate_hz == 0 || input_rate_hz <= output_rate_hz || input_rate_hz % output_rate_hz != 0 {
            return Err(format!(
                "IntegerDecimator requires exact integer divisor: {input_rate_hz} -> {output_rate_hz}"
            ));
        }
        let factor = (input_rate_hz / output_rate_hz) as usize;
        let taps = get_or_build_integer_kernel(input_rate_hz, output_rate_hz, factor)?;
        let half = (taps.len() - 1) / 2;
        Ok(Self {
            input_rate_hz,
            output_rate_hz,
            factor,
            taps,
            // Pre-seed `half` leading zeros so sample index `0` of the input stream aligns with
            // the filter's center tap (exact zero group-delay shift).
            hist_l: vec![0.0_f64; half],
            hist_r: vec![0.0_f64; half],
            phase_rem: 0,
            pad_tail: half,
        })
    }

    fn drain_available(&mut self) -> (Vec<f64>, Vec<f64>) {
        let n_taps = self.taps.len();
        let avail = self.hist_l.len();
        if avail < self.phase_rem + n_taps {
            return (Vec::new(), Vec::new());
        }
        let out_count = (avail - self.phase_rem - n_taps) / self.factor + 1;
        let mut out_l = Vec::with_capacity(out_count);
        let mut out_r = Vec::with_capacity(out_count);
        let taps = self.taps.as_slice();
        let hl = self.hist_l.as_slice();
        let hr = self.hist_r.as_slice();

        let mut start = self.phase_rem;
        for _ in 0..out_count {
            let sl = &hl[start..start + n_taps];
            let sr = &hr[start..start + n_taps];
            let mut acc_l = 0.0_f64;
            let mut acc_r = 0.0_f64;
            for k in 0..n_taps {
                let h = taps[k];
                acc_l += sl[k] * h;
                acc_r += sr[k] * h;
            }
            out_l.push(acc_l);
            out_r.push(acc_r);
            start += self.factor;
        }

        // Discard fully consumed prefix so `hist` stays strictly bounded (`< n_taps + factor`).
        let consumed = start;
        if consumed > 0 {
            self.hist_l.drain(..consumed);
            self.hist_r.drain(..consumed);
        }
        self.phase_rem = 0;

        (out_l, out_r)
    }
}

impl Downsampler for IntegerDecimator {
    fn name(&self) -> &'static str {
        match self.factor {
            2 => "decim-kaiser-2x",
            4 => "decim-kaiser-4x",
            8 => "decim-kaiser-8x",
            _ => "decim-kaiser",
        }
    }

    fn input_rate_hz(&self) -> u32 {
        self.input_rate_hz
    }

    fn output_rate_hz(&self) -> u32 {
        self.output_rate_hz
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.is_empty() {
            return Ok(StereoF64Buffer::empty(self.output_rate_hz));
        }
        self.hist_l.extend_from_slice(&input.left);
        self.hist_r.extend_from_slice(&input.right);
        let (left, right) = self.drain_available();
        Ok(StereoF64Buffer {
            sample_rate_hz: self.output_rate_hz,
            left,
            right,
        })
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        if self.pad_tail > 0 {
            self.hist_l.resize(self.hist_l.len() + self.pad_tail, 0.0);
            self.hist_r.resize(self.hist_r.len() + self.pad_tail, 0.0);
            self.pad_tail = 0;
        }
        let (left, right) = self.drain_available();
        Ok(StereoF64Buffer {
            sample_rate_hz: self.output_rate_hz,
            left,
            right,
        })
    }

    fn flush_for_gapless(
        &mut self,
    ) -> Result<(StereoF64Buffer, Option<DownsamplerCarryover>), String> {
        let (left, right) = self.drain_available();
        let carry = DownsamplerCarryover {
            input_rate_hz: self.input_rate_hz,
            output_rate_hz: self.output_rate_hz,
            hist_l: std::mem::take(&mut self.hist_l),
            hist_r: std::mem::take(&mut self.hist_r),
            phase_pos: self.phase_rem,
            pad_tail: self.pad_tail,
            kind: DownsamplerCarryKind::Integer {
                factor: self.factor,
                taps: Arc::clone(&self.taps),
            },
        };
        self.pad_tail = 0;
        Ok((
            StereoF64Buffer {
                sample_rate_hz: self.output_rate_hz,
                left,
                right,
            },
            Some(carry),
        ))
    }

    fn set_incoming_carryover(
        &mut self,
        carry: DownsamplerCarryover,
    ) -> Result<(), DownsamplerCarryover> {
        if carry.input_rate_hz != self.input_rate_hz || carry.output_rate_hz != self.output_rate_hz {
            return Err(carry);
        }
        if !matches!(carry.kind, DownsamplerCarryKind::Integer { factor, .. } if factor == self.factor)
        {
            return Err(carry);
        }
        self.hist_l = carry.hist_l;
        self.hist_r = carry.hist_r;
        self.phase_rem = carry.phase_pos;
        self.pad_tail = carry.pad_tail;
        Ok(())
    }
}

fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

/// Cached polyphase banks for cross-family rational decimation `(in_rate, out_rate)`.
type RationalBankEntry = (usize, usize, usize, Arc<Vec<Vec<f64>>>);

fn rational_bank_cache() -> &'static Mutex<HashMap<(u32, u32), RationalBankEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(u32, u32), RationalBankEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_or_build_rational_bank(
    input_rate_hz: u32,
    output_rate_hz: u32,
) -> Result<RationalBankEntry, String> {
    let key = (input_rate_hz, output_rate_hz);
    {
        let cache = rational_bank_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&key) {
            return Ok((entry.0, entry.1, entry.2, Arc::clone(&entry.3)));
        }
    }

    let g = gcd_u32(input_rate_hz, output_rate_hz);
    let up_l = (output_rate_hz / g) as usize;
    let down_m = (input_rate_hz / g) as usize;
    if up_l == 0 || down_m == 0 || up_l > 512 {
        return Err(format!(
            "unsupported rational decimation ratio {input_rate_hz} -> {output_rate_hz} (L={up_l}, M={down_m})"
        ));
    }

    // Effective ratio R = down_m / up_l (> 1.0 for downsampling, < 1.0 for rare 352.8 -> 384 fallback).
    let ratio = (down_m as f64) / (up_l as f64);
    let taps_per_phase: usize = if output_rate_hz >= 176_400 {
        257
    } else if output_rate_hz >= 88_200 {
        513
    } else {
        2049
    };
    let half_tpp = (taps_per_phase - 1) / 2;

    // Prototype cutoff relative to input sample rate `fs_in`:
    // Must reject everything >= min(0.5 * fs_in, 0.5 * fs_out) with Kaiser A=200 dB.
    let nyq_rel_in = 0.5_f64.min(0.5 / ratio);
    let delta_f_in = 13.4 / ((taps_per_phase - 1) as f64);
    let fc_in = (nyq_rel_in - 0.5 * delta_f_in).max(0.25 * nyq_rel_in);

    // Build `up_l` polyphase branches: phase `p` (`0..up_l`) evaluates the continuous sinc-Kaiser
    // kernel shifted by fractional offset `frac = p / up_l` in input sample units.
    // For output index `k`, continuous input time is `t = k * (down_m / up_l) = base + p / up_l`.
    // Convolution is `sum_{j=0..taps_per_phase} x[base - half_tpp + j] * h(t - (base - half_tpp + j))`
    // where `tau = (p / up_l) + half_tpp - j`.
    let beta_dd = Dd::from_f64(KAISER_STEEP_BETA);
    let inv_i0_beta = DD_ONE.div(dd_bessel_i0(beta_dd));
    let half_dd = Dd::from_f64(half_tpp as f64 + 0.5);
    let fc_dd = Dd::from_f64(fc_in);
    let two_fc = fc_dd.add(fc_dd);
    let inv_l_dd = DD_ONE.div(Dd::from_f64(up_l as f64));

    let mut bank = Vec::with_capacity(up_l);
    for p in 0..up_l {
        let frac = Dd::from_f64(p as f64).mul(inv_l_dd);
        let mut branch = vec![0.0_f64; taps_per_phase];
        for (j, slot) in branch.iter_mut().enumerate() {
            let tau = frac
                .add(Dd::from_f64(half_tpp as f64))
                .sub(Dd::from_f64(j as f64));
            let sinc_dd = if tau.to_f64().abs() < 1e-15 {
                two_fc
            } else {
                let x = DD_TWO_PI.mul(fc_dd).mul(tau);
                let (sin_x, _) = x.sin_cos();
                sin_x.div(DD_PI.mul(tau))
            };
            let r = tau.div(half_dd);
            let win = if r.to_f64().abs() >= 1.0 {
                DD_ZERO
            } else {
                let one_minus_r2 = DD_ONE.sub(r.sqr());
                let arg = if one_minus_r2.hi <= 0.0 {
                    DD_ZERO
                } else {
                    one_minus_r2.sqrt().mul(beta_dd)
                };
                dd_bessel_i0(arg).mul(inv_i0_beta)
            };
            *slot = sinc_dd.mul(win).to_f64();
        }
        // Exact per-phase unity DC normalization eliminates fractional-phase gain ripple.
        let dc = dd_sum_f64(&branch).to_f64();
        if dc.abs() > 1e-18 {
            let inv_dc = 1.0 / dc;
            for h in &mut branch {
                *h *= inv_dc;
            }
        }
        bank.push(branch);
    }

    let bank_arc = Arc::new(bank);
    let mut cache = rational_bank_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.insert(key, (up_l, down_m, taps_per_phase, Arc::clone(&bank_arc)));
    Ok((up_l, down_m, taps_per_phase, bank_arc))
}

/// Exact cross-family rational polyphase Kaiser anti-aliasing decimator in pure `f64`
/// (used ONLY when a DAC refuses all rates in the track's own clock family, e.g. `352.8 -> 192/96/48`).
pub struct RationalDecimator {
    input_rate_hz: u32,
    output_rate_hz: u32,
    up_l: usize,
    down_m: usize,
    taps_per_phase: usize,
    bank: Arc<Vec<Vec<f64>>>,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    /// Next output sample's fractional phase `p = phase_m` in `0..up_l`, relative to `hist[0]`
    /// aligned at `half_tpp` before sample `0`.
    phase_m: usize,
    pad_tail: usize,
}

impl RationalDecimator {
    pub fn new(input_rate_hz: u32, output_rate_hz: u32) -> Result<Self, String> {
        if input_rate_hz == 0 || output_rate_hz == 0 || input_rate_hz == output_rate_hz {
            return Err(format!(
                "RationalDecimator requires distinct nonzero rates: {input_rate_hz} -> {output_rate_hz}"
            ));
        }
        let (up_l, down_m, taps_per_phase, bank) =
            get_or_build_rational_bank(input_rate_hz, output_rate_hz)?;
        let half_tpp = (taps_per_phase - 1) / 2;
        Ok(Self {
            input_rate_hz,
            output_rate_hz,
            up_l,
            down_m,
            taps_per_phase,
            bank,
            hist_l: vec![0.0_f64; half_tpp],
            hist_r: vec![0.0_f64; half_tpp],
            phase_m: 0,
            pad_tail: half_tpp + 1,
        })
    }

    fn drain_available(&mut self) -> (Vec<f64>, Vec<f64>) {
        let tpp = self.taps_per_phase;
        let up_l = self.up_l;
        let down_m = self.down_m;
        let avail = self.hist_l.len();
        if avail < tpp {
            return (Vec::new(), Vec::new());
        }

        let mut out_l = Vec::new();
        let mut out_r = Vec::new();
        let hl = self.hist_l.as_slice();
        let hr = self.hist_r.as_slice();
        let mut base = 0_usize;
        let mut p = self.phase_m;

        while base + tpp <= avail {
            let branch = self.bank[p].as_slice();
            let sl = &hl[base..base + tpp];
            let sr = &hr[base..base + tpp];
            let mut acc_l = 0.0_f64;
            let mut acc_r = 0.0_f64;
            for j in 0..tpp {
                let h = branch[j];
                acc_l += sl[j] * h;
                acc_r += sr[j] * h;
            }
            out_l.push(acc_l);
            out_r.push(acc_r);

            let next_p = p + down_m;
            base += next_p / up_l;
            p = next_p % up_l;
        }

        if base > 0 {
            let drop_n = base.min(self.hist_l.len());
            self.hist_l.drain(..drop_n);
            self.hist_r.drain(..drop_n);
        }
        self.phase_m = p;

        (out_l, out_r)
    }
}

impl Downsampler for RationalDecimator {
    fn name(&self) -> &'static str {
        "decim-rational-kaiser"
    }

    fn input_rate_hz(&self) -> u32 {
        self.input_rate_hz
    }

    fn output_rate_hz(&self) -> u32 {
        self.output_rate_hz
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.is_empty() {
            return Ok(StereoF64Buffer::empty(self.output_rate_hz));
        }
        self.hist_l.extend_from_slice(&input.left);
        self.hist_r.extend_from_slice(&input.right);
        let (left, right) = self.drain_available();
        Ok(StereoF64Buffer {
            sample_rate_hz: self.output_rate_hz,
            left,
            right,
        })
    }

    fn flush(&mut self) -> Result<StereoF64Buffer, String> {
        if self.pad_tail > 0 {
            self.hist_l.resize(self.hist_l.len() + self.pad_tail, 0.0);
            self.hist_r.resize(self.hist_r.len() + self.pad_tail, 0.0);
            self.pad_tail = 0;
        }
        let (left, right) = self.drain_available();
        Ok(StereoF64Buffer {
            sample_rate_hz: self.output_rate_hz,
            left,
            right,
        })
    }

    fn flush_for_gapless(
        &mut self,
    ) -> Result<(StereoF64Buffer, Option<DownsamplerCarryover>), String> {
        let (left, right) = self.drain_available();
        let carry = DownsamplerCarryover {
            input_rate_hz: self.input_rate_hz,
            output_rate_hz: self.output_rate_hz,
            hist_l: std::mem::take(&mut self.hist_l),
            hist_r: std::mem::take(&mut self.hist_r),
            phase_pos: self.phase_m,
            pad_tail: self.pad_tail,
            kind: DownsamplerCarryKind::Rational {
                up_l: self.up_l,
                down_m: self.down_m,
                taps_per_phase: self.taps_per_phase,
                bank: Arc::clone(&self.bank),
            },
        };
        self.pad_tail = 0;
        Ok((
            StereoF64Buffer {
                sample_rate_hz: self.output_rate_hz,
                left,
                right,
            },
            Some(carry),
        ))
    }

    fn set_incoming_carryover(
        &mut self,
        carry: DownsamplerCarryover,
    ) -> Result<(), DownsamplerCarryover> {
        if carry.input_rate_hz != self.input_rate_hz || carry.output_rate_hz != self.output_rate_hz {
            return Err(carry);
        }
        if !matches!(
            carry.kind,
            DownsamplerCarryKind::Rational { up_l, down_m, .. }
                if up_l == self.up_l && down_m == self.down_m
        ) {
            return Err(carry);
        }
        self.hist_l = carry.hist_l;
        self.hist_r = carry.hist_r;
        self.phase_m = carry.phase_pos;
        self.pad_tail = carry.pad_tail;
        Ok(())
    }
}

/// Build the appropriate pure-`f64` last-stage downsampler for `(input_rate_hz -> output_rate_hz)`:
/// - `input_rate_hz == output_rate_hz` -> [`NullDownsampler`] (zero-copy bypass!)
/// - `input_rate_hz / output_rate_hz ∈ {2, 4, 8}` -> [`IntegerDecimator`]
/// - Any other cross-family pair (e.g. `352,800 -> 192,000 / 96,000 / 48,000`) -> [`RationalDecimator`]
pub fn build_downsampler(
    input_rate_hz: u32,
    output_rate_hz: u32,
) -> Result<Box<dyn Downsampler>, String> {
    if input_rate_hz == output_rate_hz {
        return Ok(Box::new(NullDownsampler::new(output_rate_hz)));
    }
    if output_rate_hz > 0
        && input_rate_hz > output_rate_hz
        && input_rate_hz % output_rate_hz == 0
        && matches!(input_rate_hz / output_rate_hz, 2 | 4 | 8)
    {
        return Ok(Box::new(IntegerDecimator::new(
            input_rate_hz,
            output_rate_hz,
        )?));
    }
    Ok(Box::new(RationalDecimator::new(
        input_rate_hz,
        output_rate_hz,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn sine_buf(rate_hz: u32, freq_hz: f64, frames: usize) -> StereoF64Buffer {
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        for n in 0..frames {
            let v = (2.0 * PI * freq_hz * (n as f64) / f64::from(rate_hz)).sin();
            left.push(v);
            right.push(0.5 * v);
        }
        StereoF64Buffer {
            sample_rate_hz: rate_hz,
            left,
            right,
        }
    }

    fn rms_steady(samples: &[f64], skip: usize) -> f64 {
        let slice = &samples[skip..samples.len().saturating_sub(skip)];
        let sum_sq: f64 = slice.iter().map(|&x| x * x).sum();
        (sum_sq / (slice.len().max(1) as f64)).sqrt()
    }

    #[test]
    fn null_downsampler_is_bit_identical_zero_copy() {
        let mut d = build_downsampler(384_000, 384_000).unwrap();
        assert!(d.is_bypass());
        assert_eq!(d.name(), "bypass");
        let inp = sine_buf(384_000, 1_000.0, 256);
        let out = d.process_block(inp.clone()).unwrap();
        assert_eq!(out.left, inp.left);
        assert_eq!(out.right, inp.right);
        assert!(d.flush().unwrap().is_empty());
    }

    #[test]
    fn integer_decimators_pass_audio_band_flat_and_reject_aliasing_below_minus_180_db() {
        for &(in_rate, out_rate, pass_hz, alias_hz) in &[
            (384_000_u32, 192_000_u32, 20_000.0, 100_000.0),
            (352_800, 176_400, 20_000.0, 92_000.0),
            (384_000, 96_000, 20_000.0, 52_000.0),
            (352_800, 88_200, 20_000.0, 48_000.0),
            (384_000, 48_000, 20_000.0, 25_000.0),
            (352_800, 44_100, 19_000.0, 22_500.0),
        ] {
            let frames = 32_768;
            let mut dec = build_downsampler(in_rate, out_rate).unwrap();
            let pass_in = sine_buf(in_rate, pass_hz, frames);
            let mut pass_out = dec.process_block(pass_in).unwrap();
            let t = dec.flush().unwrap();
            pass_out.left.extend_from_slice(&t.left);
            let gain_db = 20.0 * (rms_steady(&pass_out.left, 800) / (0.5_f64.sqrt())).log10();
            assert!(
                gain_db.abs() < 0.01,
                "{in_rate}->{out_rate} passband {pass_hz} Hz gain {gain_db:+.4} dB outside ±0.01 dB"
            );

            let mut dec_alias = build_downsampler(in_rate, out_rate).unwrap();
            let alias_in = sine_buf(in_rate, alias_hz, frames);
            let mut alias_out = dec_alias.process_block(alias_in).unwrap();
            let t2 = dec_alias.flush().unwrap();
            alias_out.left.extend_from_slice(&t2.left);
            let alias_rms = rms_steady(&alias_out.left, 800);
            let alias_db = 20.0 * (alias_rms / (0.5_f64.sqrt())).max(1e-15).log10();
            assert!(
                alias_db < -180.0,
                "{in_rate}->{out_rate} stopband {alias_hz} Hz rejection {alias_db:.1} dB (expected < -180 dB)"
            );
        }
    }

    #[test]
    fn streaming_chunks_and_gapless_carryover_are_bit_identical_to_one_shot() {
        for &(in_rate, out_rate) in &[
            (384_000_u32, 192_000_u32),
            (352_800, 88_200),
            (352_800, 44_100),
            (352_800, 192_000), // cross-family rational!
            (352_800, 48_000),  // cross-family rational!
        ] {
            let full = sine_buf(in_rate, 3_333.0, 12_345);

            // 1. One-shot reference
            let mut ref_dec = build_downsampler(in_rate, out_rate).unwrap();
            let mut ref_out = ref_dec.process_block(full.clone()).unwrap();
            let ref_tail = ref_dec.flush().unwrap();
            ref_out.left.extend_from_slice(&ref_tail.left);
            ref_out.right.extend_from_slice(&ref_tail.right);

            // 2. Arbitrary chunk sizes
            let mut chunk_dec = build_downsampler(in_rate, out_rate).unwrap();
            let mut chunk_l = Vec::new();
            let mut chunk_r = Vec::new();
            let mut pos = 0;
            for step in [17, 131, 509, 1024, 37, 2048, 513] {
                if pos >= full.left.len() {
                    break;
                }
                let end = (pos + step).min(full.left.len());
                let blk = StereoF64Buffer {
                    sample_rate_hz: in_rate,
                    left: full.left[pos..end].to_vec(),
                    right: full.right[pos..end].to_vec(),
                };
                let o = chunk_dec.process_block(blk).unwrap();
                chunk_l.extend_from_slice(&o.left);
                chunk_r.extend_from_slice(&o.right);
                pos = end;
            }
            if pos < full.left.len() {
                let blk = StereoF64Buffer {
                    sample_rate_hz: in_rate,
                    left: full.left[pos..].to_vec(),
                    right: full.right[pos..].to_vec(),
                };
                let o = chunk_dec.process_block(blk).unwrap();
                chunk_l.extend_from_slice(&o.left);
                chunk_r.extend_from_slice(&o.right);
            }
            let ct = chunk_dec.flush().unwrap();
            chunk_l.extend_from_slice(&ct.left);
            chunk_r.extend_from_slice(&ct.right);

            assert_eq!(
                chunk_l, ref_out.left,
                "{in_rate}->{out_rate} chunked left diverged from one-shot"
            );
            assert_eq!(
                chunk_r, ref_out.right,
                "{in_rate}->{out_rate} chunked right diverged from one-shot"
            );

            // 3. Gapless split across two sessions with DownsamplerCarryover
            let split = 6_111;
            let mut dec_a = build_downsampler(in_rate, out_rate).unwrap();
            let mut gap_l = Vec::new();
            let mut gap_r = Vec::new();
            let o1 = dec_a
                .process_block(StereoF64Buffer {
                    sample_rate_hz: in_rate,
                    left: full.left[..split].to_vec(),
                    right: full.right[..split].to_vec(),
                })
                .unwrap();
            gap_l.extend_from_slice(&o1.left);
            gap_r.extend_from_slice(&o1.right);
            let (end_a, carry) = dec_a.flush_for_gapless().unwrap();
            gap_l.extend_from_slice(&end_a.left);
            gap_r.extend_from_slice(&end_a.right);

            let mut dec_b = build_downsampler(in_rate, out_rate).unwrap();
            dec_b.set_incoming_carryover(carry.unwrap()).unwrap();
            let o2 = dec_b
                .process_block(StereoF64Buffer {
                    sample_rate_hz: in_rate,
                    left: full.left[split..].to_vec(),
                    right: full.right[split..].to_vec(),
                })
                .unwrap();
            gap_l.extend_from_slice(&o2.left);
            gap_r.extend_from_slice(&o2.right);
            let end_b = dec_b.flush().unwrap();
            gap_l.extend_from_slice(&end_b.left);
            gap_r.extend_from_slice(&end_b.right);

            assert_eq!(
                gap_l, ref_out.left,
                "{in_rate}->{out_rate} gapless carryover left diverged from continuous one-shot"
            );
            assert_eq!(
                gap_r, ref_out.right,
                "{in_rate}->{out_rate} gapless carryover right diverged from continuous one-shot"
            );
        }
    }

    #[test]
    fn rational_cross_family_decimator_passes_audio_band_and_rejects_aliasing() {
        for &(in_rate, out_rate, pass_hz, alias_hz) in &[
            (352_800_u32, 192_000_u32, 20_000.0, 105_000.0),
            (352_800, 96_000, 20_000.0, 56_000.0),
            (352_800, 48_000, 19_000.0, 28_000.0),
        ] {
            let frames = 32_768;
            let mut dec = build_downsampler(in_rate, out_rate).unwrap();
            assert_eq!(dec.name(), "decim-rational-kaiser");
            let mut pass_out = dec.process_block(sine_buf(in_rate, pass_hz, frames)).unwrap();
            let t = dec.flush().unwrap();
            pass_out.left.extend_from_slice(&t.left);
            let gain_db = 20.0 * (rms_steady(&pass_out.left, 400) / (0.5_f64.sqrt())).log10();
            assert!(
                gain_db.abs() < 0.01,
                "rational {in_rate}->{out_rate} passband {pass_hz} Hz gain {gain_db:+.4} dB"
            );

            let mut dec_alias = build_downsampler(in_rate, out_rate).unwrap();
            let mut alias_out = dec_alias
                .process_block(sine_buf(in_rate, alias_hz, frames))
                .unwrap();
            let t2 = dec_alias.flush().unwrap();
            alias_out.left.extend_from_slice(&t2.left);
            let alias_rms = rms_steady(&alias_out.left, 400);
            let alias_db = 20.0 * (alias_rms / (0.5_f64.sqrt())).max(1e-15).log10();
            assert!(
                alias_db < -170.0,
                "rational {in_rate}->{out_rate} stopband {alias_hz} Hz rejection {alias_db:.1} dB"
            );
        }
    }
}
