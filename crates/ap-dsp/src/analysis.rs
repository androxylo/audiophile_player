//! Dual-mode pure-`f64` content analysers (`H`, `A`, `L`, `B`, 4× BS.1770 true-peak meter, and
//! report-only pre-emphasis shelf heuristic) shared by 30 s window analysis (`W30`), streaming
//! whole-track learn-on-play (`WT`), and offline library scanning (`ap scan`).
//!
//! References: `FILTER_SELECTION_RESEARCH.md` §7, `docs/research/07-ultimate-mode.md` §§2, 5.3,
//! and `OQ-37` (4fS ultrasonic ADC/DSD noise detection above 40 kHz).

use std::f64::consts::PI;

use ap_core::buffer::StereoF64Buffer;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Detector version bumped whenever detector math or thresholds change (invalidates stale YAML).
pub const DETECTOR_VERSION: u32 = 1;

/// Three-valued detector outcome (`FILTER_SELECTION_RESEARCH.md` §7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DetectorVerdict {
    Positive,
    Negative,
    #[default]
    Inconclusive,
}

impl DetectorVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Positive => "positive",
            Self::Negative => "negative",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// Immutable snapshot of all content-analyser measurements (produced from either `W30` or `WT`).
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisResult {
    pub sample_rate_hz: u32,
    pub container_bits: u16,
    pub analyzed_frames: u64,
    pub duration_s: f64,
    pub is_whole_track: bool,

    // Detector H: effective bandwidth, fake hi-res, lossy note & OQ-37 ultrasonic ADC/DSD noise.
    pub effective_bandwidth_hz: u32,
    pub upsampled_from_1x: DetectorVerdict,
    pub effective_nyquist_hz: Option<u32>,
    pub lossy_origin_suspected: bool,
    pub ultrasonic_noise_rise_db: f64,
    pub ultrasonic_noise_level_dbfs: f64,
    pub ultrasonic_noise_detected: DetectorVerdict,

    // Detector A: near-Nyquist onset pre-ringing events.
    pub onset_count: u32,
    pub apod_events: u32,
    pub apodizing_needed: DetectorVerdict,

    // Detector L: BS.1770 K-weighted loudness, clipped runs, exact sample peak & 4x true peak.
    pub integrated_lufs: Option<f64>,
    pub sample_peak_linear: f64,
    pub true_peak_linear: f64,
    pub true_peak_dbtp: f64,
    pub clipped_runs: u32,
    pub loud_and_clipped: DetectorVerdict,

    // Detector B: exact bitwise effective bit depth.
    pub effective_bits: u16,

    // Report-only pre-emphasis shelf heuristic (NEVER auto-activates de-emphasis).
    pub pre_emphasis_shelf_suspected: bool,
}

/// Lightweight 4× oversampling ITU-R BS.1770-4-class true-peak estimator (48-tap / 12 taps/phase
/// Kaiser-windowed sinc, pure `f64`, zero allocation in streaming loop).
struct TruePeakMeter4x {
    bank: [[f64; 12]; 4],
    hist_l: [f64; 12],
    hist_r: [f64; 12],
    pos: usize,
    peak: f64,
}

impl TruePeakMeter4x {
    fn new() -> Self {
        let mut bank = [[0.0_f64; 12]; 4];
        let beta = 7.0_f64;
        let i0_beta = bessel_i0_f64(beta);
        for (p, coeffs) in bank.iter_mut().enumerate() {
            let frac = (p as f64) / 4.0;
            let mut sum = 0.0;
            for (k, slot) in coeffs.iter_mut().enumerate() {
                let j = (k as f64) - 5.0 - frac;
                let sinc = if j.abs() < 1e-12 {
                    1.0
                } else {
                    (PI * j).sin() / (PI * j)
                };
                let x = j / 6.0;
                let win = if x.abs() < 1.0 {
                    bessel_i0_f64(beta * (1.0 - x * x).sqrt()) / i0_beta
                } else {
                    0.0
                };
                let v = sinc * win;
                *slot = v;
                sum += v;
            }
            if sum.abs() > 1e-12 {
                for c in coeffs.iter_mut() {
                    *c /= sum;
                }
            }
        }
        Self {
            bank,
            hist_l: [0.0; 12],
            hist_r: [0.0; 12],
            pos: 0,
            peak: 0.0,
        }
    }

    #[inline]
    fn push_sample(&mut self, l: f64, r: f64) {
        self.peak = self.peak.max(l.abs()).max(r.abs());
        self.hist_l[self.pos] = l;
        self.hist_r[self.pos] = r;
        let p_cur = self.pos;
        self.pos = (self.pos + 1) % 12;

        // Evaluate all 4 sub-sample phases directly on circular 12-tap history.
        for coeffs in &self.bank {
            let mut acc_l = 0.0;
            let mut acc_r = 0.0;
            let mut idx = self.pos; // oldest sample
            for &c in coeffs {
                acc_l += self.hist_l[idx] * c;
                acc_r += self.hist_r[idx] * c;
                idx += 1;
                if idx == 12 {
                    idx = 0;
                }
            }
            let _ = p_cur;
            self.peak = self.peak.max(acc_l.abs()).max(acc_r.abs());
        }
    }
}

fn bessel_i0_f64(x: f64) -> f64 {
    let y = 0.25 * x * x;
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..=40 {
        let kf = k as f64;
        term *= y / (kf * kf);
        sum += term;
        if term <= sum * 1e-16 {
            break;
        }
    }
    sum
}

/// Pure-`f64` Direct Form II Transposed biquad for ITU-R BS.1770-4 K-weighting at any sample rate.
#[derive(Debug, Clone, Copy)]
struct BiquadF64 {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    s1: f64,
    s2: f64,
}

impl BiquadF64 {
    /// Stage 1 BS.1770 pre-filter (+4 dB high-shelf at ~1681.97 Hz, Q = 0.707175).
    fn bs1770_shelf(fs: f64) -> Self {
        let f0 = 1681.974_450_955_533_f64;
        let g_db = 3.999_843_853_973_347_f64;
        let q = 0.707_175_236_955_419_6_f64;
        let k = (PI * f0 / fs).tan();
        let vh = 10.0_f64.powf(g_db / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + k / q + k * k;
        Self {
            b0: (vh + vb * k / q + k * k) / a0,
            b1: 2.0 * (k * k - vh) / a0,
            b2: (vh - vb * k / q + k * k) / a0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / q + k * k) / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    /// Stage 2 BS.1770 RLB high-pass filter (38.135 Hz, Q = 0.500327).
    fn bs1770_rlb_hp(fs: f64) -> Self {
        let f0 = 38.135_470_876_024_44_f64;
        let q = 0.500_327_037_323_877_3_f64;
        let k = (PI * f0 / fs).tan();
        let a0 = 1.0 + k / q + k * k;
        Self {
            b0: 1.0 / a0,
            b1: -2.0 / a0,
            b2: 1.0 / a0,
            a1: 2.0 * (k * k - 1.0) / a0,
            a2: (1.0 - k / q + k * k) / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Shared streaming content analyser supporting BOTH 30 s window snapshot (`W30`) and continuous
/// whole-track accumulation (`WT`) in a single pass.
pub struct TrackAnalyzer {
    sample_rate_hz: u32,
    container_bits: u16,
    analyzed_frames: u64,

    // Detector H (Welch PSD accumulator)
    fft_len: usize,
    r2c: Arc<dyn RealToComplex<f64>>,
    hann: Vec<f64>,
    hann_power_norm: f64,
    fft_pending: Vec<f64>,
    fft_time: Vec<f64>,
    fft_spec: Vec<realfft::num_complex::Complex<f64>>,
    fft_scratch: Vec<realfft::num_complex::Complex<f64>>,
    psd_sum: Vec<f64>,
    psd_frames: u64,

    // Detector A (short-window STFT onset & near-Nyquist pre-ringing tracker)
    apod_win: usize,
    apod_hop: usize,
    apod_r2c: Arc<dyn RealToComplex<f64>>,
    apod_hann: Vec<f64>,
    apod_pending: Vec<f64>,
    apod_time: Vec<f64>,
    apod_spec: Vec<realfft::num_complex::Complex<f64>>,
    apod_scratch: Vec<realfft::num_complex::Complex<f64>>,
    /// Sliding ring of recent frame band energies: `(broadband_hf_db, near_nyq_db)`.
    apod_ring: Vec<(f64, f64)>,
    apod_refractory: usize,
    onset_count: u32,
    apod_events: u32,

    // Detector L (BS.1770 K-weighted loudness, clipped runs, sample peak, 4x true peak)
    k_shelf_l: BiquadF64,
    k_shelf_r: BiquadF64,
    k_hp_l: BiquadF64,
    k_hp_r: BiquadF64,
    hop_frames_100ms: usize,
    cur_hop_frames: usize,
    cur_hop_energy: f64,
    recent_hops: [f64; 4],
    recent_hops_filled: usize,
    block_mean_squares: Vec<f64>,
    sample_peak_linear: f64,
    true_peak_meter: TruePeakMeter4x,
    clip_thresh: f64,
    clip_run_l: u32,
    clip_run_r: u32,
    clipped_runs: u32,

    // Detector B (bitwise OR mask of 32-bit MSB-aligned signed samples)
    bit_or_mask: u32,
}

impl TrackAnalyzer {
    pub fn new(sample_rate_hz: u32, container_bits: u16) -> Self {
        let fs = sample_rate_hz.max(8_000) as f64;
        let fft_len = if sample_rate_hz >= 176_400 {
            16_384
        } else if sample_rate_hz >= 88_200 {
            8_192
        } else {
            4_096
        };
        let mut planner = RealFftPlanner::<f64>::new();
        let r2c = planner.plan_fft_forward(fft_len);
        let hann: Vec<f64> = (0..fft_len)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * (i as f64) / (fft_len as f64)).cos()))
            .collect();
        let hann_power_norm: f64 = hann.iter().map(|w| w * w).sum::<f64>() * (fft_len as f64);

        // Short STFT for Detector A (~2.9 ms window @ 44.1k, ~0.72 ms hop).
        let apod_win = if sample_rate_hz >= 176_400 {
            512
        } else if sample_rate_hz >= 88_200 {
            256
        } else {
            128
        };
        let apod_hop = apod_win / 4;
        let apod_r2c = planner.plan_fft_forward(apod_win);
        let apod_hann: Vec<f64> = (0..apod_win)
            .map(|i| 0.5 * (1.0 - (2.0 * PI * (i as f64) / (apod_win as f64)).cos()))
            .collect();

        let hop_frames_100ms = ((fs * 0.1).round() as usize).max(64);
        let bits_clamped = container_bits.clamp(8, 32);
        let lsb = 1.0 / ((1_u64 << (bits_clamped - 1)) as f64);
        let clip_thresh = (1.0 - 2.5 * lsb).max(0.99);

        Self {
            sample_rate_hz: sample_rate_hz.max(8_000),
            container_bits: bits_clamped,
            analyzed_frames: 0,
            fft_len,
            fft_spec: r2c.make_output_vec(),
            fft_scratch: r2c.make_scratch_vec(),
            r2c,
            hann,
            hann_power_norm: hann_power_norm.max(1.0),
            fft_pending: Vec::with_capacity(fft_len),
            fft_time: vec![0.0; fft_len],
            psd_sum: vec![0.0; fft_len / 2 + 1],
            psd_frames: 0,
            apod_win,
            apod_hop,
            apod_spec: apod_r2c.make_output_vec(),
            apod_scratch: apod_r2c.make_scratch_vec(),
            apod_r2c,
            apod_hann,
            apod_pending: Vec::with_capacity(apod_win * 2),
            apod_time: vec![0.0; apod_win],
            apod_ring: Vec::with_capacity(32),
            apod_refractory: 0,
            onset_count: 0,
            apod_events: 0,
            k_shelf_l: BiquadF64::bs1770_shelf(fs),
            k_shelf_r: BiquadF64::bs1770_shelf(fs),
            k_hp_l: BiquadF64::bs1770_rlb_hp(fs),
            k_hp_r: BiquadF64::bs1770_rlb_hp(fs),
            hop_frames_100ms,
            cur_hop_frames: 0,
            cur_hop_energy: 0.0,
            recent_hops: [0.0; 4],
            recent_hops_filled: 0,
            block_mean_squares: Vec::new(),
            sample_peak_linear: 0.0,
            true_peak_meter: TruePeakMeter4x::new(),
            clip_thresh,
            clip_run_l: 0,
            clip_run_r: 0,
            clipped_runs: 0,
            bit_or_mask: 0,
        }
    }

    pub fn analyzed_frames(&self) -> u64 {
        self.analyzed_frames
    }

    /// Feed one decoded stereo `f64` source block into all detectors incrementally.
    pub fn push_block(&mut self, block: &StereoF64Buffer) {
        let frames = block.left.len().min(block.right.len());
        if frames == 0 {
            return;
        }
        self.analyzed_frames += frames as u64;
        let scale_i32 = (1_u64 << 31) as f64;

        for i in 0..frames {
            let l = block.left[i];
            let r = block.right[i];
            let al = l.abs();
            let ar = r.abs();

            // Sample peak + 4x BS.1770 true peak
            self.sample_peak_linear = self.sample_peak_linear.max(al).max(ar);
            self.true_peak_meter.push_sample(l, r);

            // Clipped runs (>= 3 consecutive full-scale samples on L or R)
            if al >= self.clip_thresh {
                self.clip_run_l += 1;
                if self.clip_run_l == 3 {
                    self.clipped_runs += 1;
                }
            } else {
                self.clip_run_l = 0;
            }
            if ar >= self.clip_thresh {
                self.clip_run_r += 1;
                if self.clip_run_r == 3 {
                    self.clipped_runs += 1;
                }
            } else {
                self.clip_run_r = 0;
            }

            // Detector B: bitwise OR of exact 32-bit MSB-aligned signed representation
            let il = (l * scale_i32).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            let ir = (r * scale_i32).round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
            self.bit_or_mask |= (il as u32) | (ir as u32);

            // Detector L: BS.1770 K-weighted energy accumulation in 100 ms hops (400 ms sliding block)
            let kl = self.k_hp_l.process(self.k_shelf_l.process(l));
            let kr = self.k_hp_r.process(self.k_shelf_r.process(r));
            self.cur_hop_energy += kl * kl + kr * kr;
            self.cur_hop_frames += 1;
            if self.cur_hop_frames >= self.hop_frames_100ms {
                let hop_ms = self.cur_hop_energy / (self.cur_hop_frames as f64);
                self.recent_hops.rotate_left(1);
                self.recent_hops[3] = hop_ms;
                self.recent_hops_filled = (self.recent_hops_filled + 1).min(4);
                if self.recent_hops_filled == 4 {
                    let block_ms = 0.25 * self.recent_hops.iter().sum::<f64>();
                    self.block_mean_squares.push(block_ms);
                }
                self.cur_hop_energy = 0.0;
                self.cur_hop_frames = 0;
            }

            let mono = 0.5 * (l + r);
            self.fft_pending.push(mono);
            self.apod_pending.push(mono);
        }

        // Process ready Welch PSD frames for Detector H (50% overlap).
        let hop_h = self.fft_len / 2;
        while self.fft_pending.len() >= self.fft_len {
            let mut sq = 0.0;
            for j in 0..self.fft_len {
                let s = self.fft_pending[j];
                sq += s * s;
                self.fft_time[j] = s * self.hann[j];
            }
            let rms_sq = sq / (self.fft_len as f64);
            // Ignore digital silence frames (< -70 dBFS) so silence does not dilute spectral shape.
            if rms_sq > 1e-7 {
                if self
                    .r2c
                    .process_with_scratch(&mut self.fft_time, &mut self.fft_spec, &mut self.fft_scratch)
                    .is_ok()
                {
                    let inv_norm = 2.0 / self.hann_power_norm;
                    for (acc, c) in self.psd_sum.iter_mut().zip(self.fft_spec.iter()) {
                        *acc += (c.re * c.re + c.im * c.im) * inv_norm;
                    }
                    self.psd_frames += 1;
                }
            }
            self.fft_pending.drain(0..hop_h);
        }

        // Process ready short STFT frames for Detector A (near-Nyquist onset pre-ringing).
        self.process_apod_frames();
    }

    fn process_apod_frames(&mut self) {
        let win = self.apod_win;
        let hop = self.apod_hop;
        let spec_len = win / 2 + 1;
        // Broadband transient band: 20%..42% of fs_in; near-Nyquist ringing band: 43%..49.5% of fs_in.
        let bb_lo = ((0.20 * (win as f64)).round() as usize).clamp(1, spec_len - 1);
        let bb_hi = ((0.42 * (win as f64)).round() as usize).clamp(bb_lo, spec_len - 1);
        let nyq_lo = ((0.43 * (win as f64)).round() as usize).clamp(1, spec_len - 1);
        let nyq_hi = ((0.495 * (win as f64)).round() as usize).clamp(nyq_lo, spec_len - 1);

        while self.apod_pending.len() >= win {
            for j in 0..win {
                self.apod_time[j] = self.apod_pending[j] * self.apod_hann[j];
            }
            if self
                .apod_r2c
                .process_with_scratch(
                    &mut self.apod_time,
                    &mut self.apod_spec,
                    &mut self.apod_scratch,
                )
                .is_ok()
            {
                let mut bb_e = 0.0;
                for c in &self.apod_spec[bb_lo..=bb_hi] {
                    bb_e += c.re * c.re + c.im * c.im;
                }
                let bb_db = 10.0 * (bb_e / ((bb_hi - bb_lo + 1) as f64)).max(1e-24).log10();

                let mut nyq_e = 0.0;
                for c in &self.apod_spec[nyq_lo..=nyq_hi] {
                    nyq_e += c.re * c.re + c.im * c.im;
                }
                let nyq_db = 10.0 * (nyq_e / ((nyq_hi - nyq_lo + 1) as f64)).max(1e-24).log10();

                self.apod_ring.push((bb_db, nyq_db));
                if self.apod_ring.len() > 36 {
                    self.apod_ring.remove(0);
                }

                if self.apod_refractory > 0 {
                    self.apod_refractory -= 1;
                } else if self.apod_ring.len() >= 28 {
                    // Center onset detection strictly on the local broadband peak at `idx = len - 7`:
                    //   background: [0..idx-11]   (far before peak)
                    //   pre-ring:   [idx-5..idx-4] (strictly non-overlapping BEFORE peak window)
                    //   onset peak: idx           (local maximum over [idx-3..=idx+3])
                    //   post-ring:  [idx+4..idx+5] (symmetric non-overlapping AFTER peak window)
                    // A linear-phase brickwall produces symmetric near-Nyquist ringing (`pre_nyq`
                    // rises >= 12 dB above background and tracks `post_nyq` within 12 dB); causal
                    // minimum-phase/apodizing filters have zero pre-ringing (`pre_nyq ~= bg_nyq`).
                    let len = self.apod_ring.len();
                    let idx = len - 7;
                    let center_bb = self.apod_ring[idx].0;
                    let is_local_max = self.apod_ring[(idx - 3)..=(idx + 3)]
                        .iter()
                        .all(|x| center_bb >= x.0 - 1e-9);
                    if !is_local_max {
                        self.apod_pending.drain(0..hop);
                        continue;
                    }

                    let bg_bb = avg_f64(self.apod_ring[0..(idx - 11)].iter().map(|x| x.0));
                    let bg_nyq = avg_f64(self.apod_ring[0..(idx - 11)].iter().map(|x| x.1));
                    let pre_nyq = self.apod_ring[(idx - 5)..=(idx - 4)]
                        .iter()
                        .map(|x| x.1)
                        .fold(f64::NEG_INFINITY, f64::max);
                    let post_nyq = self.apod_ring[(idx + 4)..=(idx + 5)]
                        .iter()
                        .map(|x| x.1)
                        .fold(f64::NEG_INFINITY, f64::max);

                    let onset_jump = center_bb - bg_bb;
                    if onset_jump >= 15.0 && center_bb > -55.0 {
                        self.onset_count += 1;
                        self.apod_refractory = 24; // ~17-20 ms refractory window
                        if post_nyq > -95.0
                            && pre_nyq > -95.0
                            && pre_nyq >= bg_nyq + 12.0
                            && pre_nyq >= post_nyq - 12.0
                        {
                            self.apod_events += 1;
                        }
                    }
                }
            }
            self.apod_pending.drain(0..hop);
        }
    }

    /// Produce a non-destructive snapshot of the analysis so far (e.g., at the end of `W30`),
    /// allowing the same [`TrackAnalyzer`] to keep accumulating through `W4` to whole-track EOF.
    pub fn snapshot(
        &self,
        is_whole_track: bool,
        early_digital_era: bool,
        orig_rate_below_container: bool,
        dsd_tag_hint: bool,
    ) -> AnalysisResult {
        let fs = self.sample_rate_hz as f64;
        let duration_s = (self.analyzed_frames as f64) / fs;
        let nyq_hz = self.sample_rate_hz / 2;

        // --- Detector L: BS.1770 integrated loudness (absolute -70 LUFS + relative -10 LU gate) ---
        let integrated_lufs = compute_bs1770_integrated_lufs(&self.block_mean_squares);
        let true_peak_linear = self.true_peak_meter.peak.max(self.sample_peak_linear);
        let true_peak_dbtp = if true_peak_linear > 0.0 {
            20.0 * true_peak_linear.log10()
        } else {
            -144.0
        };

        let clips_per_min = if duration_s > 1.0 {
            (self.clipped_runs as f64) / (duration_s / 60.0)
        } else {
            0.0
        };
        let loud_and_clipped = if duration_s < 5.0 {
            DetectorVerdict::Inconclusive
        } else if integrated_lufs.is_some_and(|l| l >= -10.0)
            && self.clipped_runs >= 1
            && clips_per_min >= 1.0
        {
            DetectorVerdict::Positive
        } else if !is_whole_track && integrated_lufs.is_none_or(|l| l < -14.0) && self.clipped_runs == 0 {
            // Quiet intro in 30 s window: inconclusive rather than falsely clearing rule L.
            DetectorVerdict::Inconclusive
        } else {
            DetectorVerdict::Negative
        };

        // --- Detector A: apodizing verdict (window vs whole-track thresholds, Rule E era prior) ---
        let apodizing_needed = if duration_s < 5.0 {
            DetectorVerdict::Inconclusive
        } else if is_whole_track {
            let thresh = if early_digital_era { 7 } else { 10 };
            if self.apod_events > thresh {
                DetectorVerdict::Positive
            } else if self.onset_count < 10 {
                DetectorVerdict::Inconclusive
            } else {
                DetectorVerdict::Negative
            }
        } else {
            let event_thresh = if early_digital_era { 3 } else { 4 };
            let ratio_hit =
                self.onset_count >= 10 && self.apod_events >= 3 && self.apod_events * 10 >= self.onset_count;
            if self.apod_events >= event_thresh || ratio_hit {
                DetectorVerdict::Positive
            } else if self.onset_count < 10 {
                DetectorVerdict::Inconclusive
            } else {
                DetectorVerdict::Negative
            }
        };

        // --- Detector H: spectral cliff, fake hi-res, lossy origin, OQ-37 ultrasonic noise ---
        let mut effective_bandwidth_hz = nyq_hz;
        let mut upsampled_from_1x = DetectorVerdict::Inconclusive;
        let mut effective_nyquist_hz = None;
        let mut lossy_origin_suspected = false;
        let mut ultrasonic_noise_rise_db = 0.0;
        let mut ultrasonic_noise_level_dbfs = -144.0;
        let mut ultrasonic_noise_detected = DetectorVerdict::Negative;
        let mut pre_emphasis_shelf_suspected = false;

        if self.psd_frames >= 4 && duration_s >= 5.0 {
            let inv_frames = 1.0 / (self.psd_frames as f64);
            let bin_hz = fs / (self.fft_len as f64);
            let band_db = |f_lo: f64, f_hi: f64| -> f64 {
                let lo = ((f_lo / bin_hz).floor() as usize).clamp(1, self.psd_sum.len() - 1);
                let hi = ((f_hi / bin_hz).ceil() as usize).clamp(lo, self.psd_sum.len() - 1);
                let mut sum = 0.0;
                for &p in &self.psd_sum[lo..=hi] {
                    sum += p * inv_frames;
                }
                let mean = sum / ((hi - lo + 1) as f64);
                10.0 * mean.max(1e-24).log10()
            };

            let ref_mid_db = band_db(1_000.0, 10_000.0);

            if self.sample_rate_hz >= 88_200 {
                // Check fake hi-res cliff inside 19.5–24.5 kHz.
                let pre_cliff_db = band_db(12_000.0, 18_500.0);
                let edge_lo_db = band_db(19_000.0, 20_500.0);
                let above_25k_db = band_db(25_500.0, (0.46 * fs).min(44_000.0));
                let cliff_drop = edge_lo_db - above_25k_db;
                let rel_above_25k = ref_mid_db - above_25k_db;
                let has_hf_music = (pre_cliff_db - ref_mid_db) > -42.0;

                let drop_thresh = if orig_rate_below_container { 22.0 } else { 30.0 };
                let floor_thresh = if orig_rate_below_container { 45.0 } else { 55.0 };

                if has_hf_music && cliff_drop >= drop_thresh && rel_above_25k >= floor_thresh {
                    upsampled_from_1x = DetectorVerdict::Positive;
                    let f_eff = if self.sample_rate_hz % 44_100 == 0 {
                        22_050
                    } else {
                        24_000
                    };
                    effective_nyquist_hz = Some(f_eff);
                    effective_bandwidth_hz = f_eff;
                } else if rel_above_25k < 42.0 {
                    upsampled_from_1x = DetectorVerdict::Negative;
                    effective_bandwidth_hz = nyq_hz;
                } else {
                    upsampled_from_1x = DetectorVerdict::Inconclusive;
                }

                // OQ-37: Ultrasonic ADC/DSD noise shelf above 40 kHz (4fS) or 35 kHz (2fS).
                let (lo_band_db, hi_band_db) = if self.sample_rate_hz >= 176_400 {
                    (band_db(22_000.0, 36_000.0), band_db(45_000.0, (0.48 * fs).max(50_000.0)))
                } else {
                    (band_db(20_000.0, 30_000.0), band_db(36_000.0, 0.48 * fs))
                };
                ultrasonic_noise_rise_db = hi_band_db - lo_band_db;
                ultrasonic_noise_level_dbfs = hi_band_db;
                let rise_thresh = if dsd_tag_hint { 3.0 } else { 6.0 };
                if ultrasonic_noise_rise_db >= rise_thresh && hi_band_db > -95.0 {
                    ultrasonic_noise_detected = DetectorVerdict::Positive;
                } else {
                    ultrasonic_noise_detected = DetectorVerdict::Negative;
                }
            } else {
                // 1x source (<= 48 kHz): check lossy-origin cliff inside 15.5–19.5 kHz.
                let pre_db = band_db(10_000.0, 15_000.0);
                let hi_db = band_db(19_600.0, (nyq_hz as f64) - 500.0);
                if (pre_db - ref_mid_db) > -40.0 && (pre_db - hi_db) >= 35.0 {
                    lossy_origin_suspected = true;
                    effective_bandwidth_hz = 16_000;
                }
                // Report-only pre-emphasis shelf heuristic (8–18 kHz unusually hot vs 1–4 kHz).
                let low_mid = band_db(1_000.0, 4_000.0);
                let upper = band_db(8_000.0, 18_000.0);
                if (upper - low_mid) > -3.0 && low_mid > -60.0 {
                    pre_emphasis_shelf_suspected = true;
                }
            }
        }

        // --- Detector B: exact bitwise effective bit depth ---
        let effective_bits = if self.bit_or_mask == 0 {
            0
        } else {
            (32 - self.bit_or_mask.trailing_zeros())
                .min(u32::from(self.container_bits)) as u16
        };

        AnalysisResult {
            sample_rate_hz: self.sample_rate_hz,
            container_bits: self.container_bits,
            analyzed_frames: self.analyzed_frames,
            duration_s,
            is_whole_track,
            effective_bandwidth_hz,
            upsampled_from_1x,
            effective_nyquist_hz,
            lossy_origin_suspected,
            ultrasonic_noise_rise_db,
            ultrasonic_noise_level_dbfs,
            ultrasonic_noise_detected,
            onset_count: self.onset_count,
            apod_events: self.apod_events,
            apodizing_needed,
            integrated_lufs,
            sample_peak_linear: self.sample_peak_linear,
            true_peak_linear,
            true_peak_dbtp,
            clipped_runs: self.clipped_runs,
            loud_and_clipped,
            effective_bits,
            pre_emphasis_shelf_suspected,
        }
    }
}

fn avg_f64(iter: impl Iterator<Item = f64>) -> f64 {
    let mut sum = 0.0;
    let mut n = 0usize;
    for v in iter {
        sum += v;
        n += 1;
    }
    if n == 0 { -144.0 } else { sum / (n as f64) }
}

/// Compute ITU-R BS.1770-4 gated integrated loudness in LUFS from 400 ms block mean-squares.
fn compute_bs1770_integrated_lufs(blocks: &[f64]) -> Option<f64> {
    if blocks.is_empty() {
        return None;
    }
    // Absolute gate: -70 LUFS => z_i >= 10^((-70 + 0.691) / 10)
    let abs_gate = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
    let mut sum_abs = 0.0;
    let mut cnt_abs = 0usize;
    for &z in blocks {
        if z >= abs_gate {
            sum_abs += z;
            cnt_abs += 1;
        }
    }
    if cnt_abs == 0 {
        return None;
    }
    let ungated_mean = sum_abs / (cnt_abs as f64);
    let rel_gate = ungated_mean * 0.1; // -10 LU below ungated loudness
    let mut sum_rel = 0.0;
    let mut cnt_rel = 0usize;
    for &z in blocks {
        if z >= abs_gate && z >= rel_gate {
            sum_rel += z;
            cnt_rel += 1;
        }
    }
    if cnt_rel == 0 {
        return None;
    }
    let mean = sum_rel / (cnt_rel as f64);
    Some(-0.691 + 10.0 * mean.max(1e-24).log10())
}
