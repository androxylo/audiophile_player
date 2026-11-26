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

use crate::dd::{DD_ONE, Dd, dd_bessel_i0, dd_sum_f64};
use crate::upsampler::{
    KAISER_STEEP_ATTENUATION_DB, KAISER_STEEP_BETA, kaiser_sinc_dd, kaiser_transition_width_norm,
};

/// Stateful carryover for seamless equal-output-rate gapless transitions across a downsampler
/// (Review #9 D2 + D3: minimal state only, no unused cached kernel fields).
#[derive(Debug, Clone)]
pub struct DownsamplerCarryover {
    pub input_rate_hz: u32,
    pub output_rate_hz: u32,
    pub up_l: usize,
    pub down_m: usize,
    pub hist_l: Vec<f64>,
    pub hist_r: Vec<f64>,
    /// Next output sample's fractional phase `0..up_l` (always `0` for integer decimation).
    pub phase_pos: usize,
    /// Trailing zero-padding count needed to align exact output duration on non-gapless flush.
    pub pad_tail: usize,
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
    /// Exact number of output frames that `flush()` would emit for inputs already pushed so far
    /// (excluding any future convolution tail appended after this point).
    fn pending_flush_frames(&self) -> usize;
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
        if input.left.len() != input.right.len() {
            return Err(format!(
                "NullDownsampler channel length mismatch: left={} right={}",
                input.left.len(),
                input.right.len()
            ));
        }
        if input.sample_rate_hz != self.rate_hz {
            return Err(format!(
                "NullDownsampler rate mismatch: expected {} Hz, got {} Hz",
                self.rate_hz, input.sample_rate_hz
            ));
        }
        Ok(input)
    }
    fn pending_flush_frames(&self) -> usize {
        0
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

fn gcd_u32(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

/// Cached polyphase decimation bank keyed by `(in_rate, out_rate)` for BOTH integer (`L = 1`,
/// `M ∈ {2, 4, 8}`) and cross-family rational (`L > 1`) ratios (Review #9 D2).
type DecimatorBankEntry = (usize, usize, usize, Arc<Vec<Vec<f64>>>);

fn decimator_bank_cache() -> &'static Mutex<HashMap<(u32, u32), DecimatorBankEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(u32, u32), DecimatorBankEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn get_or_build_decimator_bank(
    input_rate_hz: u32,
    output_rate_hz: u32,
) -> Result<DecimatorBankEntry, String> {
    let key = (input_rate_hz, output_rate_hz);
    {
        let cache = decimator_bank_cache()
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
            "unsupported decimation ratio {input_rate_hz} -> {output_rate_hz} (L={up_l}, M={down_m})"
        ));
    }

    let taps_per_phase: usize = if up_l == 1 {
        // Same-family integer decimation (`÷2`, `÷4`, `÷8`): steep 1fS uses 4097 taps so the
        // passband clears >= 20.8 kHz @ 352.8k while stopping at <= 22.05 kHz.
        match down_m {
            2 => 257,
            4 => 513,
            8 => 4097,
            _ if output_rate_hz >= 176_400 => 257,
            _ if output_rate_hz >= 88_200 => 513,
            _ => 2049,
        }
    } else if output_rate_hz >= 176_400 {
        257
    } else if output_rate_hz >= 88_200 {
        513
    } else {
        2049
    };
    let half_tpp = (taps_per_phase - 1) / 2;

    // Derive cutoff cleanly from the shared Kaiser A = 200 dB transition-width formula (D7):
    // stopband (-200 dB) sits at `nyq_rel_in = min(0.5, 0.5 * L / M)`.
    let ratio = (down_m as f64) / (up_l as f64);
    let nyq_rel_in = 0.5_f64.min(0.5 / ratio);
    let delta_f_in = kaiser_transition_width_norm(KAISER_STEEP_ATTENUATION_DB, taps_per_phase);
    let fc_in = (nyq_rel_in - 0.5 * delta_f_in).max(0.25 * nyq_rel_in);

    // Build `up_l` polyphase branches in ~106-bit double-double arithmetic using shared
    // `kaiser_sinc_dd` (D2 + D7). For integer decimation (`up_l == 1`), `half_dd = half_tpp`
    // matches the exact symmetric odd prototype; for rational (`up_l > 1`), `half_tpp + 0.5`
    // keeps window support positive across all fractional shifts `0..up_l`.
    let beta_dd = Dd::from_f64(KAISER_STEEP_BETA);
    let inv_i0_beta = DD_ONE.div(dd_bessel_i0(beta_dd));
    let half_dd = Dd::from_f64(if up_l == 1 {
        half_tpp as f64
    } else {
        half_tpp as f64 + 0.5
    });
    let fc_dd = Dd::from_f64(fc_in);
    let inv_l_dd = DD_ONE.div(Dd::from_f64(up_l as f64));

    let mut bank = Vec::with_capacity(up_l);
    for p in 0..up_l {
        let frac = Dd::from_f64(p as f64).mul(inv_l_dd);
        let mut branch = vec![0.0_f64; taps_per_phase];
        for (j, slot) in branch.iter_mut().enumerate() {
            let tau = frac
                .add(Dd::from_f64(half_tpp as f64))
                .sub(Dd::from_f64(j as f64));
            *slot = kaiser_sinc_dd(tau, fc_dd, half_dd, beta_dd, inv_i0_beta).to_f64();
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
    let mut cache = decimator_bank_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.insert(key, (up_l, down_m, taps_per_phase, Arc::clone(&bank_arc)));
    Ok((up_l, down_m, taps_per_phase, bank_arc))
}

/// Unified pure-`f64` polyphase Kaiser anti-aliasing decimator handling BOTH exact same-family
/// integer ratios (`L = 1`, `M ∈ {2, 4, 8}`) and cross-family rational ratios (`L > 1`, e.g.
/// `352.8 -> 192 / 96 / 48 kHz`) in one streaming state machine (Review #9 D2).
pub struct PolyphaseDecimator {
    input_rate_hz: u32,
    output_rate_hz: u32,
    up_l: usize,
    down_m: usize,
    taps_per_phase: usize,
    bank: Arc<Vec<Vec<f64>>>,
    hist_l: Vec<f64>,
    hist_r: Vec<f64>,
    /// Next output sample's fractional phase `0..up_l` (always `0` for `up_l == 1`).
    phase_m: usize,
    pad_tail: usize,
}

impl PolyphaseDecimator {
    pub fn new(input_rate_hz: u32, output_rate_hz: u32) -> Result<Self, String> {
        if input_rate_hz == 0 || output_rate_hz == 0 || input_rate_hz == output_rate_hz {
            return Err(format!(
                "PolyphaseDecimator requires distinct nonzero rates: {input_rate_hz} -> {output_rate_hz}"
            ));
        }
        let (up_l, down_m, taps_per_phase, bank) =
            get_or_build_decimator_bank(input_rate_hz, output_rate_hz)?;
        let half_tpp = (taps_per_phase - 1) / 2;
        Ok(Self {
            input_rate_hz,
            output_rate_hz,
            up_l,
            down_m,
            taps_per_phase,
            bank,
            // Pre-seed `half_tpp` leading zeros so sample index `0` aligns with the center tap.
            hist_l: vec![0.0_f64; half_tpp],
            hist_r: vec![0.0_f64; half_tpp],
            phase_m: 0,
            pad_tail: half_tpp,
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

        let est_out = ((avail - tpp) * up_l) / down_m + 1;
        let mut out_l = Vec::with_capacity(est_out);
        let mut out_r = Vec::with_capacity(est_out);
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

impl Downsampler for PolyphaseDecimator {
    fn name(&self) -> &'static str {
        if self.up_l == 1 {
            match self.down_m {
                2 => "decim-kaiser-2x",
                4 => "decim-kaiser-4x",
                8 => "decim-kaiser-8x",
                _ => "decim-kaiser",
            }
        } else {
            "decim-rational-kaiser"
        }
    }

    fn input_rate_hz(&self) -> u32 {
        self.input_rate_hz
    }

    fn output_rate_hz(&self) -> u32 {
        self.output_rate_hz
    }

    fn process_block(&mut self, input: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if input.left.len() != input.right.len() {
            return Err(format!(
                "PolyphaseDecimator channel length mismatch: left={} right={}",
                input.left.len(),
                input.right.len()
            ));
        }
        if input.sample_rate_hz != self.input_rate_hz {
            return Err(format!(
                "PolyphaseDecimator rate mismatch: expected {} Hz, got {} Hz",
                self.input_rate_hz, input.sample_rate_hz
            ));
        }
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

    fn pending_flush_frames(&self) -> usize {
        let total = self.hist_l.len() + self.pad_tail;
        let tpp = self.taps_per_phase;
        if total < tpp {
            return 0;
        }
        if self.up_l == 1 {
            return (total - tpp) / self.down_m + 1;
        }
        let mut base = 0_usize;
        let mut p = self.phase_m;
        let mut count = 0_usize;
        while base + tpp <= total {
            count += 1;
            let next_p = p + self.down_m;
            base += next_p / self.up_l;
            p = next_p % self.up_l;
        }
        count
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
            up_l: self.up_l,
            down_m: self.down_m,
            hist_l: std::mem::take(&mut self.hist_l),
            hist_r: std::mem::take(&mut self.hist_r),
            phase_pos: self.phase_m,
            pad_tail: self.pad_tail,
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
        if carry.input_rate_hz != self.input_rate_hz
            || carry.output_rate_hz != self.output_rate_hz
            || carry.up_l != self.up_l
            || carry.down_m != self.down_m
        {
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
/// - Otherwise -> unified [`PolyphaseDecimator`] (`L = 1` for exact integer `÷2/÷4/÷8`, or
///   reduced `L / M` for cross-family fallbacks like `352,800 -> 192,000 / 96,000 / 48,000`).
pub fn build_downsampler(
    input_rate_hz: u32,
    output_rate_hz: u32,
) -> Result<Box<dyn Downsampler>, String> {
    if input_rate_hz == output_rate_hz {
        return Ok(Box::new(NullDownsampler::new(output_rate_hz)));
    }
    Ok(Box::new(PolyphaseDecimator::new(
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

    #[test]
    fn rational_decimator_exact_output_length_and_process_block_validation() {
        // D10: output length for any N_in must equal ceil(N_in * L / M) (zero +1 overrun).
        for &(in_rate, out_rate, up_l, down_m) in &[
            (352_800_u32, 192_000_u32, 80_usize, 147_usize),
            (352_800, 96_000, 40, 147),
            (352_800, 48_000, 20, 147),
        ] {
            for &n_in in &[0_usize, 1, 7, 147, 1000, 4096, 8193] {
                let mut dec = build_downsampler(in_rate, out_rate).unwrap();
                let mut total_out = 0_usize;
                if n_in > 0 {
                    let o = dec.process_block(sine_buf(in_rate, 1_000.0, n_in)).unwrap();
                    total_out += o.len();
                }
                let pending = dec.pending_flush_frames();
                let tail = dec.flush().unwrap();
                assert_eq!(tail.len(), pending, "{in_rate}->{out_rate} n_in={n_in} pending mismatch");
                total_out += tail.len();
                let expected = if n_in == 0 {
                    0
                } else {
                    (n_in * up_l).div_ceil(down_m)
                };
                assert_eq!(
                    total_out, expected,
                    "D10 rational {in_rate}->{out_rate} n_in={n_in}: got {total_out}, expected {expected}"
                );
            }
        }

        // D11: process_block validates sample rate and matching L/R lengths on all downsamplers.
        for &(in_rate, out_rate) in &[(384_000_u32, 384_000_u32), (384_000, 48_000), (352_800, 48_000)] {
            let mut dec = build_downsampler(in_rate, out_rate).unwrap();
            let bad_rate = StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: vec![0.0; 16],
                right: vec![0.0; 16],
            };
            assert!(dec.process_block(bad_rate).is_err());

            let bad_len = StereoF64Buffer {
                sample_rate_hz: in_rate,
                left: vec![0.0; 16],
                right: vec![0.0; 15],
            };
            assert!(dec.process_block(bad_len).is_err());
        }
    }
}
