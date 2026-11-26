// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Tidy, linear, bounded-memory streaming pure-`f64` audio processing pipeline.
//!
//! Designed so multi-hour 32-bit/384 kHz FLACs and continuous live streams (Milestone 2
//! Tidal) flow cleanly through bounded blocks with **zero unbounded RAM** and **bit-identical**
//! output across arbitrary chunk sizes:
//! 1. **Headroom Selection & Scaling (`-3 dB` or `0 dB`, hardcoded)** in `f64`.
//! 2. **Dynamic Per-Track Upsampling** (`SimpleWindowedSincUpsampler` or zero-copy
//!    `NullUpsampler` for native `352.8 / 384 kHz` tracks) -> always `StereoF64Buffer`.
//! 3. **Core REW Stereo Convolution** (`StereoConvolver`; UPOLS pure-`f64` FFT when configured,
//!    or [`NullConvolver`] by default).
//! 4. **Per-Channel Room Alignment Delay** (`ChannelDelayer`; currently [`NullChannelDelayer`]).
//! 5. **Single Final `f64 -> i32` Quantization** ([`StreamingQuantizer`], stateful across blocks).

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::config::ConvolutionConfig;
use ap_core::format::{DacOutputCapabilities, DacWireFormat, StreamSpec};
use ap_core::headroom::{
    HeadroomDecision, HeadroomMode, TrackPeakMetadata, select_headroom_for_bits,
};

use crate::convolution::{
    ConvolutionTail, NullConvolverFactory, RewConvolverFactory, StereoConvolver,
    StereoConvolverFactory,
};
use crate::deemphasis::{DeemphasisCarryover, StereoDeemphasis};
use crate::delay::{ChannelDelayer, ChannelDelayerFactory, NullChannelDelayerFactory};
use crate::downsampler::{Downsampler, DownsamplerCarryover, build_downsampler};
use crate::quantize::{
    QuantizerCarryoverState, QuantizerStats, StreamingQuantizer, apply_safe_gain_f64,
};
use crate::selector::{
    FilterArchetype, FilterDesignMetrics, MAX_SUPPORTED_SOURCE_RATE_HZ,
    MIN_SUPPORTED_SOURCE_RATE_HZ, NoiseShaperProfile, SelectedDspPlan, TrackSourceInfo,
    UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsamplerPicker, UpsamplerPolicy,
};
use crate::upsampler::{Upsampler, UpsamplerCarryover, build_upsampler};

/// Stateful DSP carryover transferred across consecutive equal-rate gapless tracks so both the
/// polyphase upsampler FIR history and the REW UPOLS convolution tail continue seamlessly with
/// zero boundary discontinuity or silence gap.
///
/// If no compatible next track follows (end of queue, sample-rate change, or next-track error),
/// call [`AudioPipeline::drain_gapless_carryover`] to flush the carried state cleanly through
/// convolution, delay, and quantization instead of truncating the end of the track.
#[derive(Debug, Clone)]
pub struct GaplessCarryover {
    /// Internal locked `8fS` DSP operating rate (`352,800` or `384,000 Hz`).
    pub internal_rate_hz: u32,
    /// Actual DAC wire sample rate (`== internal_rate_hz` for 8fS DACs, or downsampled rate).
    pub output_rate_hz: u32,
    pub output_bits: u8,
    pub effective_shaper: NoiseShaperProfile,
    pub upsampler: Option<UpsamplerCarryover>,
    /// Optional 50/15 µs de-emphasis IIR state carried across equal-rate gapless transitions.
    pub deemphasis: Option<DeemphasisCarryover>,
    pub convolver: Option<ConvolutionTail>,
    /// Optional last-stage `f64` downsampler FIR state carried across equal-rate gapless transitions.
    pub downsampler: Option<DownsamplerCarryover>,
    /// Continuous PRNG + per-channel error-feedback shaper state preserved across equal-rate
    /// gapless boundaries (`docs/research/04-noise-shapers.md` §6.4).
    pub quantizer_state: Option<QuantizerCarryoverState>,
}

impl GaplessCarryover {
    pub fn is_empty(&self) -> bool {
        self.upsampler.is_none() && self.convolver.is_none() && self.downsampler.is_none()
    }

    /// Maximum absolute amplitude (`0.0` if absent) of any carried REW convolution tail.
    pub fn convolver_tail_peak_linear(&self) -> f64 {
        self.convolver
            .as_ref()
            .map_or(0.0, ConvolutionTail::peak_linear)
    }

    /// Maximum absolute amplitude (`0.0` if absent) of any carried unpadded upsampler FIR history.
    pub fn upsampler_history_peak_linear(&self) -> f64 {
        self.upsampler
            .as_ref()
            .map_or(0.0, UpsamplerCarryover::peak_linear)
    }
}

/// Outcome of attaching a preceding track's [`GaplessCarryover`] to a new streaming session.
#[derive(Debug)]
pub enum GaplessAttachOutcome {
    /// Output rate and upsampler geometry both matched: upsampler FIR history AND REW convolution
    /// tail were seamlessly handed off into the new session with zero extra drain frames.
    FullHandoff,
    /// Output sample rate matched (e.g. `44.1k -> 88.2k` or `44.1k -> native 352.8k`, both at
    /// `352.8k`), while the upsampler ratio/filter differed: only the short upsampler FIR history
    /// was flushed into `upsampler_tail_pcm` (to close Track A), and any resulting REW convolution
    /// tail was seamlessly attached to Track B without inserting a multi-second IR drain gap.
    ConvolverOnlyHandoff {
        upsampler_tail_pcm: InterleavedInt32Buffer,
        /// True iff a non-empty REW convolution tail was actually attached to Track B (false when
        /// REW convolution is inactive and only upsampler history was drained onto Track A).
        attached_convolver_tail: bool,
        /// Quantizer peak/over statistics from draining Track A's upsampler tail (Review #6 #18).
        upsampler_tail_stats: QuantizerStats,
    },
    /// Output sample rate changed (e.g. `352.8k -> 384k`) or Track B's convolver cannot accept a
    /// carried REW tail (e.g. `NullConvolver` after filter removal): nothing was attached to
    /// Track B and the untouched carryover is returned so the caller can drain it completely onto
    /// Track A.
    IncompatibleRate(GaplessCarryover),
}

/// Structured Ultimate Mode decision report (`docs/research/07-ultimate-mode.md` U13).
#[derive(Debug, Clone, PartialEq)]
pub struct UltimateDecisionReport {
    /// `"ultimate: cached"`, `"ultimate: live"`, or `"normal (<30 s)"`.
    pub mode: String,
    /// Winning selection rule (e.g. `"rule_A_apodizing"`, `"user_folder"`, `"short_track_bypass"`).
    pub winning_rule: String,
    /// Active filter name being played (e.g. `"gauss-long-lp"`).
    pub active_filter: Option<String>,
    /// Override provenance label when active filter came from user choices (`"mine: track"`,
    /// `"mine: folder"`, `"mine: tag"`, `"mine: default"`, or `"cli"`).
    pub override_label: Option<String>,
    /// Machine-suggested filter name (always populated for side-by-side comparison).
    pub suggested_filter: Option<String>,
    /// Machine-suggested winning rule name.
    pub suggested_rule: Option<String>,
    /// Ordered list of rules/signals that fired for this track.
    pub fired_rules: Vec<String>,
    /// True when whole-track analysis and/or exact post-chain peak `M` came from YAML cache.
    pub cache_hit: bool,
    /// Measured unity-gain 30 s window post-chain peak `M30` in dBFS (when `W2` ran).
    pub m30_dbfs: Option<f64>,
    /// Cached whole-track unity-gain post-chain peak `M` in dBFS (when cache hit on local track).
    pub cached_m_dbfs: Option<f64>,
    /// Mandatory fixed whole-track gain `G` in dB (`<= 0.0`).
    pub fixed_gain_db: f64,
    /// Quantizer margin `m_q` in dB (`(1.5 * sum(|c_k|) + 1.5)` LSB below FS).
    pub quantizer_margin_db: f64,
    /// Measured first-window throughput in multiples of real time (`× RT`).
    pub throughput_x_rt: Option<f64>,
    /// Non-fatal diagnostic notices / warnings (stale peak tags disproved, M4 stream guard, MQA, OQ-41).
    pub warnings: Vec<String>,
}

impl UltimateDecisionReport {
    pub fn summary(&self) -> String {
        let rules = if self.fired_rules.is_empty() {
            "none".to_string()
        } else {
            self.fired_rules.join(",")
        };
        let peak_str = match (self.cached_m_dbfs, self.m30_dbfs) {
            (Some(m), Some(m30)) => format!("M={m:+.2}dBFS(cached) M30={m30:+.2}dBFS"),
            (Some(m), None) => format!("M={m:+.2}dBFS(cached)"),
            (None, Some(m30)) => format!("M30={m30:+.2}dBFS"),
            (None, None) => "M=n/a".to_string(),
        };
        let speed_str = self
            .throughput_x_rt
            .map(|s| format!(" speed={s:.1}xRT"))
            .unwrap_or_default();
        let choice_str = match (&self.active_filter, &self.override_label, &self.suggested_filter) {
            (Some(active), Some(lbl), Some(sug)) => {
                let sug_r = self.suggested_rule.as_deref().unwrap_or("suggested");
                format!("playing: {active} ({lbl}) | suggested: {sug} ({sug_r}) | ")
            }
            _ => String::new(),
        };
        format!(
            "{choice_str}mode=[{}] rule={} fired=[{}] {peak_str} G={:+.3}dB m_q={:.5}dB{speed_str}",
            self.mode, self.winning_rule, rules, self.fixed_gain_db, self.quantizer_margin_db
        )
    }
}

/// Diagnostic summary of every pipeline stage chosen for a track/stream.
#[derive(Debug, Clone)]
pub struct PipelineReport {
    pub source_spec: StreamSpec,
    /// Internal locked DSP operating rate (`352,800` or `384,000 Hz` for all upsampled playback).
    pub internal_rate_hz: u32,
    /// Actual DAC wire sample rate (`== internal_rate_hz` for `8fS` DACs; lower when last-stage
    /// pure-`f64` downsampling is active).
    pub output_rate_hz: u32,
    pub wire_format: DacWireFormat,
    pub output_bits: u8,
    pub headroom: HeadroomDecision,
    pub dsp_plan: SelectedDspPlan,
    pub filter_metrics: Option<FilterDesignMetrics>,
    pub deemphasis_name: &'static str,
    pub convolver_name: &'static str,
    pub convolver_description: String,
    /// Deterministic content hash of the active REW IR (`None` for `NullConvolver`), used for
    /// `HEADROOM_DESIGN.md` §4 `post_chain_peaks` keying & invalidation.
    pub convolver_content_hash: Option<String>,
    pub delayer_name: &'static str,
    /// Last-stage `f64` downsampler name (`"bypass"` when DAC supports native `8fS`).
    pub downsampler_name: &'static str,
    pub effective_shaper: NoiseShaperProfile,
    /// Populated when Ultimate Mode evaluated this track (`U13`).
    pub ultimate_report: Option<UltimateDecisionReport>,
}

impl PipelineReport {
    pub fn format_summary(&self, total_duration_s: Option<f64>) -> String {
        let dur_part = match total_duration_s {
            Some(s) => format!("{s:.1} s"),
            None => "stream".to_string(),
        };
        let mode_prefix = match &self.ultimate_report {
            Some(u) => format!("[{}] ", u.mode),
            None => String::new(),
        };
        if self.output_rate_hz == self.internal_rate_hz {
            format!(
                "{mode_prefix}{}/{} Hz -> {} Hz f64 ({}, {}, {}, {}, {}) | WDM-KS {}",
                self.source_spec.bits_per_sample,
                self.source_spec.sample_rate_hz,
                self.output_rate_hz,
                self.dsp_plan.filter,
                self.headroom.mode,
                self.convolver_description,
                self.delayer_name,
                dur_part,
                self.wire_format.label(),
            )
        } else {
            format!(
                "{mode_prefix}{}/{} Hz -> {} Hz f64 -> {} Hz ({}, {}, {}, {}, {}, {}) | WDM-KS {}",
                self.source_spec.bits_per_sample,
                self.source_spec.sample_rate_hz,
                self.internal_rate_hz,
                self.output_rate_hz,
                self.dsp_plan.filter,
                self.headroom.mode,
                self.convolver_description,
                self.delayer_name,
                self.downsampler_name,
                dur_part,
                self.wire_format.label(),
            )
        }
    }

    pub fn log_line(&self) -> String {
        let metrics_part = match &self.filter_metrics {
            Some(m) => format!(" | filter_metrics=[{}]", m.summary()),
            None => String::new(),
        };
        let deemph_part = if self.dsp_plan.deemphasis {
            format!(" | deemph={}", self.deemphasis_name)
        } else {
            String::new()
        };
        let ult_part = match &self.ultimate_report {
            Some(u) => format!(" | ultimate=[{}]", u.summary()),
            None => String::new(),
        };
        let down_part = if self.output_rate_hz != self.internal_rate_hz {
            format!(
                " | downsample={} ({} -> {} Hz)",
                self.downsampler_name, self.internal_rate_hz, self.output_rate_hz
            )
        } else {
            String::new()
        };
        format!(
            "{}{}{}{}{} | effective_shaper={} ({}-bit, {}) | {} | conv={} | delay={}",
            self.dsp_plan.summary(),
            metrics_part,
            deemph_part,
            ult_part,
            down_part,
            self.effective_shaper,
            self.output_bits,
            self.wire_format.label(),
            self.headroom.summary(),
            self.convolver_description,
            self.delayer_name,
        )
    }
}

/// Fully processed one-shot track result (convenience helper for tests).
#[derive(Debug)]
pub struct ProcessedTrack {
    pub pcm: InterleavedInt32Buffer,
    pub report: PipelineReport,
}

/// Active per-track stateful streaming DSP session (`process_block` + `flush`).
pub struct StreamingPipelineSession {
    report: PipelineReport,
    upsampler: Box<dyn Upsampler>,
    deemphasis: StereoDeemphasis,
    convolver: Box<dyn StereoConvolver>,
    delayer: Box<dyn ChannelDelayer>,
    downsampler: Box<dyn Downsampler>,
    quantizer: StreamingQuantizer,
}

impl StreamingPipelineSession {
    pub fn report(&self) -> &PipelineReport {
        &self.report
    }

    /// True when this session's convolver actively accepts an incoming gapless [`ConvolutionTail`].
    pub fn accepts_convolver_tail(&self) -> bool {
        self.convolver.accepts_tail()
    }

    /// Return cumulative pre-quantizer peak and clipping backstop counts (`HEADROOM_DESIGN.md` §6).
    pub fn quantizer_stats(&self) -> QuantizerStats {
        self.quantizer.stats()
    }

    /// L1 sum `sum(|c_k|)` of the active noise-shaper feedback coefficients (for `quantizer_margin_db`).
    pub fn shaper_l1_sum(&self) -> f64 {
        self.quantizer.coeffs().iter().map(|c| c.abs()).sum()
    }

    /// Active convolver's peak frequency-domain boost `G_c` in dB (`0.0` for `NullConvolver`).
    pub fn convolver_max_boost_db(&self) -> f64 {
        if self.convolver.accepts_tail() {
            self.convolver.max_boost_db()
        } else {
            0.0
        }
    }

    /// Lock the track's mandatory fixed whole-track gain `HeadroomDecision` (Ultimate Mode `W2 -> W3`).
    pub fn set_fixed_headroom(&mut self, headroom: HeadroomDecision) {
        self.report.headroom = headroom;
        self.report.convolver_description = if self.report.headroom.convolution_scale_db < -1e-6 {
            format!(
                "{} [scale {:.2} dB]",
                self.convolver.description(),
                self.report.headroom.convolution_scale_db
            )
        } else {
            self.convolver.description()
        };
    }

    /// Re-evaluate bit-perfect `Bypass` vs dithered/shaped quantization once Ultimate Mode locks
    /// the real fixed whole-track [`HeadroomDecision`] (Review #7 F2).
    ///
    /// During `W2`, sessions are built with a placeholder `ZeroDb / 0.0 dB` headroom. When the
    /// filter is `Null` with no de-emphasis/REW/delay, that placeholder selects `Bypass` at source
    /// bit depth. If the real fixed gain turns out negative (`G < 0 dB`), scaling produces
    /// fractional LSB values that MUST be dithered/shaped at `dac_bits` rather than truncated.
    pub fn reconfigure_quantizer_for_headroom(
        &mut self,
        headroom: &HeadroomDecision,
    ) -> Result<(), String> {
        let (requested_shaper, output_bits) = resolve_quantizer_profile(
            &self.report.dsp_plan,
            self.report.source_spec.bits_per_sample,
            self.deemphasis.is_enabled(),
            headroom,
            self.convolver.name(),
            self.delayer.name(),
            self.report.output_rate_hz,
            self.report.wire_format,
            !self.downsampler.is_bypass(),
        );
        if requested_shaper != self.report.effective_shaper
            || output_bits != self.report.output_bits
        {
            self.quantizer = StreamingQuantizer::with_rate_options(
                requested_shaper,
                output_bits,
                self.report.output_rate_hz,
                self.report.dsp_plan.shaper_overridden,
            )?;
            self.report.effective_shaper = self.quantizer.effective_shaper();
            self.report.output_bits = output_bits;
        }
        Ok(())
    }

    /// Attach structured Ultimate Mode decision metadata (`U13`) to this session's report.
    pub fn set_ultimate_report(&mut self, report: UltimateDecisionReport) {
        self.report.ultimate_report = Some(report);
    }

    /// Restore compatible PRNG + error-feedback shaper state from the previous track without
    /// altering this track's linear chain or gain calculations (`OQ-40`).
    pub fn restore_quantizer_state_if_compatible(
        &mut self,
        q_state: QuantizerCarryoverState,
        shaper: NoiseShaperProfile,
        output_bits: u8,
    ) {
        if shaper == self.report.effective_shaper
            && output_bits == self.report.output_bits
            && shaper != NoiseShaperProfile::Bypass
        {
            self.quantizer.restore_state(q_state);
        }
    }

    /// Run Stages 2 -> 2.5 -> 3 -> 4 -> 4.5 (`upsampler -> deemphasis -> convolver -> delayer ->
    /// last-stage downsampler`) at **unity gain (`1.0`)** in pure `f64`, without quantizing yet.
    ///
    /// Used by Ultimate Mode `W2` and `W4` so the exact unity-gain post-chain peak (`M30` / `M`)
    /// is measured in a **single linear pass** right before the single quantizer (zero double-rendering!)
    /// and Track B's calculations remain 100% independent of Track A (`OQ-40`).
    pub fn process_block_unity_f64(
        &mut self,
        block: StereoF64Buffer,
    ) -> Result<StereoF64Buffer, String> {
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        let block = self.upsampler.process_block(block)?;
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        let block = self.deemphasis.process_block(block)?;
        let block = self.convolver.process_block(block)?;
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        let block = self.delayer.process_block(block)?;
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        self.downsampler.process_block(block)
    }

    /// Flush the unity-gain linear chain cleanly at EOF (non-gapless single-track or end-of-queue).
    pub fn flush_unity_f64(&mut self) -> Result<StereoF64Buffer, String> {
        let rate = self.report.output_rate_hz;
        let mut out_l = Vec::new();
        let mut out_r = Vec::new();

        let up_tail = self.upsampler.flush()?;
        if !up_tail.is_empty() {
            let de_out = self.deemphasis.process_block(up_tail)?;
            let c_out = self.convolver.process_block(de_out)?;
            if !c_out.is_empty() {
                let d_out = self.delayer.process_block(c_out)?;
                if !d_out.is_empty() {
                    let ds_out = self.downsampler.process_block(d_out)?;
                    out_l.extend_from_slice(&ds_out.left);
                    out_r.extend_from_slice(&ds_out.right);
                }
            }
        }
        let conv_tail = self.convolver.flush()?;
        if !conv_tail.is_empty() {
            let d_out = self.delayer.process_block(conv_tail)?;
            if !d_out.is_empty() {
                let ds_out = self.downsampler.process_block(d_out)?;
                out_l.extend_from_slice(&ds_out.left);
                out_r.extend_from_slice(&ds_out.right);
            }
        }
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            let ds_out = self.downsampler.process_block(delay_tail)?;
            out_l.extend_from_slice(&ds_out.left);
            out_r.extend_from_slice(&ds_out.right);
        }
        let down_tail = self.downsampler.flush()?;
        if !down_tail.is_empty() {
            out_l.extend_from_slice(&down_tail.left);
            out_r.extend_from_slice(&down_tail.right);
        }
        Ok(StereoF64Buffer {
            sample_rate_hz: rate,
            left: out_l,
            right: out_r,
        })
    }

    /// Flush the unity-gain linear chain at EOF for gapless continuity (`OQ-40` strict independence):
    /// - Returns `(ending_unity_f64, peak_over_ending_and_tail_unity, Option<GaplessCarryover>)`.
    /// - Any exported `ConvolutionTail` is pre-scaled by THIS track's fixed whole-track `fixed_gain_db`
    ///   (and decimated to `output_rate_hz` when last-stage downsampling is active) so the next track
    ///   receives Track A's exact already-gained decay `z_A[n]` as strictly additive output-only audio
    ///   right before quantization.
    pub fn flush_for_gapless_unity_f64(
        &mut self,
        fixed_gain_db: f64,
    ) -> Result<(StereoF64Buffer, f64, Option<GaplessCarryover>), String> {
        let internal_rate = self.report.internal_rate_hz;
        let out_rate = self.report.output_rate_hz;
        let mut out_l = Vec::new();
        let mut out_r = Vec::new();

        // Flush upsampler completely at track boundary so Track A and Track B never splice FIR
        // histories when their independent filters or fixed gains differ (`OQ-40`).
        let up_tail = self.upsampler.flush()?;
        if !up_tail.is_empty() {
            let de_out = self.deemphasis.process_block(up_tail)?;
            let c_out = self.convolver.process_block(de_out)?;
            if !c_out.is_empty() {
                let d_out = self.delayer.process_block(c_out)?;
                if !d_out.is_empty() {
                    let ds_out = self.downsampler.process_block(d_out)?;
                    out_l.extend_from_slice(&ds_out.left);
                    out_r.extend_from_slice(&ds_out.right);
                }
            }
        }
        let (track_end, mut conv_tail) = self.convolver.flush_split_tail()?;
        if !track_end.is_empty() {
            let d_out = self.delayer.process_block(track_end)?;
            if !d_out.is_empty() {
                let ds_out = self.downsampler.process_block(d_out)?;
                out_l.extend_from_slice(&ds_out.left);
                out_r.extend_from_slice(&ds_out.right);
            }
        }
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            let ds_out = self.downsampler.process_block(delay_tail)?;
            out_l.extend_from_slice(&ds_out.left);
            out_r.extend_from_slice(&ds_out.right);
        }

        // When last-stage downsampling is active, render Track A's decaying REW tail + downsampler
        // FIR tail through Track A's own downsampler into `output_rate_hz` so Track B receives
        // a strictly additive `z_A[n]` at `output_rate_hz` with zero cross-track coupling.
        if !self.downsampler.is_bypass() {
            let mut tail_out_l = Vec::new();
            let mut tail_out_r = Vec::new();
            if let Some(ct) = conv_tail.take() {
                let ds_c = self.downsampler.process_block(StereoF64Buffer {
                    sample_rate_hz: internal_rate,
                    left: ct.left,
                    right: ct.right,
                })?;
                tail_out_l.extend_from_slice(&ds_c.left);
                tail_out_r.extend_from_slice(&ds_c.right);
            }
            let ds_flush = self.downsampler.flush()?;
            tail_out_l.extend_from_slice(&ds_flush.left);
            tail_out_r.extend_from_slice(&ds_flush.right);
            if !tail_out_l.is_empty() {
                conv_tail = Some(ConvolutionTail {
                    sample_rate_hz: out_rate,
                    left: tail_out_l,
                    right: tail_out_r,
                });
            }
        }

        // Measure unity-gain peak over BOTH ending frames and the unscaled convolution/downsampler tail
        // so cached whole-track `M` covers the entire impulse response!
        let mut tail_peak_unity = 0.0_f64;
        for &s in out_l.iter().chain(out_r.iter()) {
            tail_peak_unity = tail_peak_unity.max(s.abs());
        }
        if let Some(ref ct) = conv_tail {
            tail_peak_unity = tail_peak_unity.max(ct.peak_linear());
        }

        // Pre-scale exported `conv_tail` by this track's fixed gain `G` so it becomes `z_A[n]`.
        if let Some(ref mut ct) = conv_tail {
            let g_lin = if fixed_gain_db.is_finite() && fixed_gain_db < 0.0 {
                10.0_f64.powf(fixed_gain_db / 20.0)
            } else {
                1.0
            };
            if (g_lin - 1.0).abs() > 1e-12 {
                for s in &mut ct.left {
                    *s *= g_lin;
                }
                for s in &mut ct.right {
                    *s *= g_lin;
                }
            }
        }

        let has_shaper_state = !self.quantizer.coeffs().is_empty();
        let carryover = if conv_tail.is_some() || has_shaper_state {
            let quantizer_state = if self.report.effective_shaper != NoiseShaperProfile::Bypass {
                Some(self.quantizer.export_state())
            } else {
                None
            };
            Some(GaplessCarryover {
                internal_rate_hz: internal_rate,
                output_rate_hz: out_rate,
                output_bits: self.report.output_bits,
                effective_shaper: self.report.effective_shaper,
                upsampler: None,
                deemphasis: None,
                convolver: conv_tail,
                downsampler: None,
                quantizer_state,
            })
        } else {
            None
        };

        Ok((
            StereoF64Buffer {
                sample_rate_hz: out_rate,
                left: out_l,
                right: out_r,
            },
            tail_peak_unity,
            carryover,
        ))
    }

    /// Apply this track's mandatory fixed whole-track gain `fixed_gain_db <= 0.0` to a unity-gain
    /// `f64` block, overlap-add any strictly additive already-gained previous-track tail `z_A[n]`,
    /// and quantize once to `InterleavedInt32Buffer`.
    pub fn quantize_scaled_block(
        &mut self,
        unity_block: &StereoF64Buffer,
        fixed_gain_db: f64,
        additive_tail: &mut Option<(ConvolutionTail, usize)>,
    ) -> Result<InterleavedInt32Buffer, String> {
        if unity_block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }
        let mut scaled = unity_block.clone();
        apply_safe_gain_f64(&mut scaled, fixed_gain_db)?;

        if let Some((tail, offset)) = additive_tail.as_mut() {
            let rem = tail.left.len().saturating_sub(*offset);
            let n = scaled.left.len().min(rem);
            for i in 0..n {
                scaled.left[i] += tail.left[*offset + i];
                scaled.right[i] += tail.right[*offset + i];
            }
            *offset += n;
            if *offset >= tail.left.len() {
                *additive_tail = None;
            }
        }

        Ok(self.quantizer.quantize_block(&scaled))
    }

    /// Process one bounded source `f64` block through all 5 stages -> ready `i32` PCM block.
    pub fn process_block(
        &mut self,
        mut block: StereoF64Buffer,
    ) -> Result<InterleavedInt32Buffer, String> {
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }

        // Stage 1: Hardcoded -3 dB / 0 dB headroom (`G_up`) + excess-only scaling (`S` / `S_B`)
        // folded in pure `f64` before the linear upsampler and convolver (Review #5 Findings #4 &
        // #14: guarantees Track A's carried upsampler history and REW tail carry A's exact gain
        // across FullHandoff / ConvolverOnlyHandoff / drain AND applies `S` for NullConvolver).
        apply_safe_gain_f64(&mut block, self.report.headroom.total_gain_db())?;

        // Stage 2: Dynamic per-track stateful f64 upsampler (or zero-copy NullUpsampler).
        let block = self.upsampler.process_block(block)?;
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }

        // Stage 2.5 (§4.1): Optional zero-latency f64 50/15 µs de-emphasis (post-upsampler, pre-REW).
        let block = self.deemphasis.process_block(block)?;

        // Stage 3: Stateful REW stereo convolution in f64 (currently NullConvolver).
        let block = self.convolver.process_block(block)?;
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }

        // Stage 4: Stateful per-channel room delay in f64 (currently NullChannelDelayer).
        let block = self.delayer.process_block(block)?;
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }

        // Stage 4.5: Optional last-stage pure-f64 anti-aliasing downsampler (zero-copy bypass at 8fS).
        let block = self.downsampler.process_block(block)?;
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }

        // Stage 5: Single final f64 -> i32 quantization at the DAC wire sample rate.
        Ok(self.quantizer.quantize_block(&block))
    }

    /// Attach a residual convolution tail carried over from the preceding equal-rate track so it
    /// overlap-adds smoothly onto this track's opening samples in `f64` (before delay & quantizer).
    pub fn set_incoming_convolver_tail(&mut self, tail: ConvolutionTail) {
        self.convolver.set_incoming_tail(tail);
    }

    /// Attach full gapless carryover (unpadded upsampler FIR history + REW convolution tail +
    /// optional last-stage downsampler FIR history) from the preceding track.
    pub fn set_incoming_gapless_carryover(
        &mut self,
        mut carryover: GaplessCarryover,
    ) -> Result<(), GaplessCarryover> {
        if carryover.internal_rate_hz != self.report.internal_rate_hz
            || carryover.output_rate_hz != self.report.output_rate_hz
        {
            return Err(carryover);
        }
        if carryover.convolver.is_some() && !self.convolver.accepts_tail() {
            return Err(carryover);
        }
        if let Some(up) = carryover.upsampler.take() {
            if let Err(rejected_up) = self.upsampler.set_incoming_carryover(up) {
                carryover.upsampler = Some(rejected_up);
                return Err(carryover);
            }
        }
        if let Some(down) = carryover.downsampler.take() {
            if let Err(rejected_down) = self.downsampler.set_incoming_carryover(down) {
                carryover.downsampler = Some(rejected_down);
                return Err(carryover);
            }
        }
        if let Some(deemph) = carryover.deemphasis.take() {
            let _ = self.deemphasis.restore_state(deemph);
        }
        if let Some(conv) = carryover.convolver.take() {
            self.convolver.set_incoming_tail(conv);
        }
        // §6.4: Preserve PRNG + error-feedback shaper state across equal-rate gapless transitions
        // when the effective shaper profile and bit depth match; reset cleanly otherwise.
        if let Some(q_state) = carryover.quantizer_state.take() {
            if carryover.effective_shaper == self.report.effective_shaper
                && carryover.output_bits == self.report.output_bits
            {
                self.quantizer.restore_state(q_state);
            }
        }
        Ok(())
    }

    /// Flush all stateful FIR / convolution / delay / downsampler tails cleanly at end-of-track
    /// (emits the full decaying convolution tail — ideal for single-file playback and end-of-queue).
    pub fn flush(&mut self) -> Result<InterleavedInt32Buffer, String> {
        let mut frames = Vec::new();
        let rate = self.report.output_rate_hz;

        // 1. Flush upsampler tail -> de-emphasis -> convolver -> delayer -> downsampler -> quantizer.
        let up_tail = self.upsampler.flush()?;
        if !up_tail.is_empty() {
            let de_out = self.deemphasis.process_block(up_tail)?;
            let c_out = self.convolver.process_block(de_out)?;
            if !c_out.is_empty() {
                let d_out = self.delayer.process_block(c_out)?;
                if !d_out.is_empty() {
                    let ds_out = self.downsampler.process_block(d_out)?;
                    if !ds_out.is_empty() {
                        frames.extend(self.quantizer.quantize_block(&ds_out).frames);
                    }
                }
            }
        }

        // 2. Flush convolver tail -> delayer -> downsampler -> quantizer.
        let conv_tail = self.convolver.flush()?;
        if !conv_tail.is_empty() {
            let d_out = self.delayer.process_block(conv_tail)?;
            if !d_out.is_empty() {
                let ds_out = self.downsampler.process_block(d_out)?;
                if !ds_out.is_empty() {
                    frames.extend(self.quantizer.quantize_block(&ds_out).frames);
                }
            }
        }

        // 3. Flush delayer tail -> downsampler -> quantizer.
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            let ds_out = self.downsampler.process_block(delay_tail)?;
            if !ds_out.is_empty() {
                frames.extend(self.quantizer.quantize_block(&ds_out).frames);
            }
        }

        // 4. Flush last-stage downsampler tail -> quantizer.
        let down_tail = self.downsampler.flush()?;
        if !down_tail.is_empty() {
            frames.extend(self.quantizer.quantize_block(&down_tail).frames);
        }

        Ok(InterleavedInt32Buffer {
            sample_rate_hz: rate,
            frames,
        })
    }

    /// Flush end-of-track audio for gapless album continuity: skips trailing zero-padding on the
    /// polyphase upsampler and optional last-stage downsampler, emits the exact convolved frames
    /// ready so far, and returns the unpadded FIR histories + decaying REW convolution tail as
    /// [`GaplessCarryover`] so the next equal-rate track continues with zero boundary dip or click.
    pub fn flush_for_gapless(
        &mut self,
    ) -> Result<(InterleavedInt32Buffer, Option<GaplessCarryover>), String> {
        let mut frames = Vec::new();
        let internal_rate = self.report.internal_rate_hz;
        let out_rate = self.report.output_rate_hz;

        // 1. Hand off unpadded upsampler FIR history (no trailing zero-pad for equal-rate gapless).
        let (up_tail, up_carry) = self.upsampler.flush_for_gapless()?;
        if !up_tail.is_empty() {
            let de_out = self.deemphasis.process_block(up_tail)?;
            let c_out = self.convolver.process_block(de_out)?;
            if !c_out.is_empty() {
                let d_out = self.delayer.process_block(c_out)?;
                if !d_out.is_empty() {
                    let ds_out = self.downsampler.process_block(d_out)?;
                    if !ds_out.is_empty() {
                        frames.extend(self.quantizer.quantize_block(&ds_out).frames);
                    }
                }
            }
        }

        // 2. Complete the already-fed sample count through the convolver and split off its tail.
        let (track_end, conv_tail) = self.convolver.flush_split_tail()?;
        if !track_end.is_empty() {
            let d_out = self.delayer.process_block(track_end)?;
            if !d_out.is_empty() {
                let ds_out = self.downsampler.process_block(d_out)?;
                if !ds_out.is_empty() {
                    frames.extend(self.quantizer.quantize_block(&ds_out).frames);
                }
            }
        }

        // 3. Flush delayer tail -> downsampler -> quantizer.
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            let ds_out = self.downsampler.process_block(delay_tail)?;
            if !ds_out.is_empty() {
                frames.extend(self.quantizer.quantize_block(&ds_out).frames);
            }
        }

        // 4. Hand off unpadded downsampler FIR history (None when bypass at 8fS).
        let (ds_tail, down_carry) = self.downsampler.flush_for_gapless()?;
        if !ds_tail.is_empty() {
            frames.extend(self.quantizer.quantize_block(&ds_tail).frames);
        }

        let deemph_state = self.deemphasis.export_state();
        let has_shaper_state = !self.quantizer.coeffs().is_empty();
        let carryover = if up_carry.is_some()
            || conv_tail.is_some()
            || down_carry.is_some()
            || deemph_state.is_some()
            || has_shaper_state
        {
            let quantizer_state = if self.report.effective_shaper != NoiseShaperProfile::Bypass {
                Some(self.quantizer.export_state())
            } else {
                None
            };
            Some(GaplessCarryover {
                internal_rate_hz: internal_rate,
                output_rate_hz: out_rate,
                output_bits: self.report.output_bits,
                effective_shaper: self.report.effective_shaper,
                upsampler: up_carry,
                deemphasis: deemph_state,
                convolver: conv_tail,
                downsampler: down_carry,
                quantizer_state,
            })
        } else {
            None
        };

        Ok((
            InterleavedInt32Buffer {
                sample_rate_hz: out_rate,
                frames,
            },
            carryover,
        ))
    }
}

/// Composable, pure-`f64` audiophile DSP pipeline factory.
pub struct AudioPipeline {
    picker: UpsamplerPicker,
    convolver_factory: Box<dyn StereoConvolverFactory>,
    delayer_factory: Box<dyn ChannelDelayerFactory>,
    dac_caps: DacOutputCapabilities,
}

impl AudioPipeline {
    /// Construct the default pipeline with dynamic upsampler picking + null REW/delay stages
    /// targeting native `8fS` (`352.8 / 384 kHz`) DACs.
    pub fn new(upsampler_policy: UpsamplerPolicy) -> Result<Self, String> {
        Ok(Self {
            picker: UpsamplerPicker::new(upsampler_policy)?,
            convolver_factory: Box::new(NullConvolverFactory),
            delayer_factory: Box::new(NullChannelDelayerFactory),
            dac_caps: DacOutputCapabilities::native_8fs(),
        })
    }

    /// Attach probed or configured [`DacOutputCapabilities`] for optional last-stage `f64`
    /// downsampling on `< 352.8 / 384 kHz` DACs (no-op bypass on native `8fS` DACs).
    pub fn with_dac_capabilities(mut self, dac_caps: DacOutputCapabilities) -> Self {
        self.dac_caps = dac_caps;
        self
    }

    /// Active DAC output capabilities attached to this pipeline factory.
    pub fn dac_capabilities(&self) -> DacOutputCapabilities {
        self.dac_caps
    }

    /// Construct a pipeline with configured REW stereo convolution (`ConvolutionConfig`) + null delay.
    pub fn with_rew_config(
        upsampler_policy: UpsamplerPolicy,
        rew_config: ConvolutionConfig,
    ) -> Result<Self, String> {
        Self::with_rew_config_full_tail(upsampler_policy, rew_config, false)
    }

    /// Construct a pipeline with configured REW stereo convolution and optional `U7` lossless
    /// full-tail mode (Ultimate Mode sets `full_tail = true` so REW tails are never truncated).
    pub fn with_rew_config_full_tail(
        upsampler_policy: UpsamplerPolicy,
        rew_config: ConvolutionConfig,
        full_tail: bool,
    ) -> Result<Self, String> {
        Self::with_factories(
            upsampler_policy,
            Box::new(RewConvolverFactory::new(rew_config).with_full_tail(full_tail)),
            Box::new(NullChannelDelayerFactory),
        )
    }

    /// Expose the active [`UpsamplerPolicy`] for Ultimate Mode selection.
    pub fn policy(&self) -> UpsamplerPolicy {
        self.picker.policy()
    }

    /// Open a fresh [`StreamingPipelineSession`] from an explicit pre-resolved [`SelectedDspPlan`]
    /// and [`HeadroomDecision`] (used by Ultimate Mode `W2` and `W3`).
    pub fn start_track_with_plan_and_headroom(
        &self,
        spec: StreamSpec,
        dsp_plan: SelectedDspPlan,
        headroom: HeadroomDecision,
    ) -> Result<StreamingPipelineSession, String> {
        if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ)
            .contains(&spec.sample_rate_hz)
        {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {} Hz is outside the supported {}..={} Hz range",
                spec.sample_rate_hz, MIN_SUPPORTED_SOURCE_RATE_HZ, MAX_SUPPORTED_SOURCE_RATE_HZ
            ));
        }

        let internal_rate = dsp_plan.target_rate_hz;
        let dac_target = self.dac_caps.resolve_for_internal_rate(internal_rate)?;
        let out_rate = dac_target.sample_rate_hz;
        let wire_format = dac_target.wire_format;

        let upsampler = build_upsampler(&dsp_plan)?;
        let deemphasis = if dsp_plan.deemphasis {
            StereoDeemphasis::new_50_15us(internal_rate)?
        } else {
            StereoDeemphasis::bypass(internal_rate)
        };
        let mut convolver = self.convolver_factory.create_for_rate(internal_rate)?;
        let delayer = self.delayer_factory.create_for_rate(internal_rate)?;
        let downsampler = build_downsampler(internal_rate, out_rate)?;

        convolver.set_output_scale(1.0);

        let (requested_shaper, output_bits) = resolve_quantizer_profile(
            &dsp_plan,
            spec.bits_per_sample,
            deemphasis.is_enabled(),
            &headroom,
            convolver.name(),
            delayer.name(),
            out_rate,
            wire_format,
            !downsampler.is_bypass(),
        );

        let quantizer = StreamingQuantizer::with_rate_options(
            requested_shaper,
            output_bits,
            out_rate,
            dsp_plan.shaper_overridden,
        )?;
        let effective_shaper = quantizer.effective_shaper();
        let conv_desc = if headroom.convolution_scale_db < -1e-6 {
            format!(
                "{} [scale {:.2} dB]",
                convolver.description(),
                headroom.convolution_scale_db
            )
        } else {
            convolver.description()
        };

        let report = PipelineReport {
            source_spec: spec,
            internal_rate_hz: internal_rate,
            output_rate_hz: out_rate,
            wire_format,
            output_bits,
            headroom,
            dsp_plan,
            filter_metrics: upsampler.design_metrics(),
            deemphasis_name: deemphasis.name(),
            convolver_name: convolver.name(),
            convolver_description: conv_desc,
            convolver_content_hash: convolver.content_hash().map(str::to_string),
            delayer_name: delayer.name(),
            downsampler_name: downsampler.name(),
            effective_shaper,
            ultimate_report: None,
        };

        Ok(StreamingPipelineSession {
            report,
            upsampler,
            deemphasis,
            convolver,
            delayer,
            downsampler,
            quantizer,
        })
    }

    /// Construct a pipeline with explicit convolver and channel delayer factories.
    pub fn with_factories(
        upsampler_policy: UpsamplerPolicy,
        convolver_factory: Box<dyn StereoConvolverFactory>,
        delayer_factory: Box<dyn ChannelDelayerFactory>,
    ) -> Result<Self, String> {
        Ok(Self {
            picker: UpsamplerPicker::new(upsampler_policy)?,
            convolver_factory,
            delayer_factory,
            dac_caps: DacOutputCapabilities::native_8fs(),
        })
    }

    /// Open a fresh stateful [`StreamingPipelineSession`] for one track or continuous stream.
    pub fn start_track(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
    ) -> Result<StreamingPipelineSession, String> {
        self.start_track_with_tail_hint(spec, peak_meta, is_streaming_source, None)
    }

    /// Open a fresh stateful [`StreamingPipelineSession`], inspecting an optional incoming
    /// [`GaplessCarryover`] so Track B's `0 dB` headroom decision accurately accounts for BOTH
    /// any carried REW convolution tail AND any carried upsampler FIR history that will be
    /// convolved onto Track B's opening — and never penalizes Track B when output rates differ.
    pub fn start_track_for_carryover(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
        incoming: Option<&GaplessCarryover>,
    ) -> Result<StreamingPipelineSession, String> {
        let hint = incoming.and_then(|c| {
            let conv_peak = c.convolver_tail_peak_linear();
            let up_peak = c.upsampler_history_peak_linear();
            if conv_peak <= 0.0 && up_peak <= 0.0 {
                return None;
            }
            // Conservative combined tail bound: if a REW convolver tail is present, also include
            // the carried upsampler history peak (scaled by active convolver L1 inside
            // start_track_with_tail_hint) so Track A's un-convolved final ~half-window samples
            // cannot push a 0 dB Track B over full scale.
            Some((c.output_rate_hz, conv_peak, up_peak, c.convolver.is_some()))
        });
        self.start_track_internal(spec, peak_meta, is_streaming_source, hint)
    }

    /// Open a fresh stateful [`StreamingPipelineSession`], accounting for an optional incoming
    /// gapless REW convolution tail `(tail_rate_hz, tail_peak_linear)` so Track B's `0 dB`
    /// headroom decision cannot clip when Track A's decaying tail overlap-adds onto Track B's
    /// opening samples.
    pub fn start_track_with_tail_hint(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
        incoming_tail_hint: Option<(u32, f64)>,
    ) -> Result<StreamingPipelineSession, String> {
        let hint = incoming_tail_hint.map(|(rate, peak)| (rate, peak, 0.0, peak > 0.0));
        self.start_track_internal(spec, peak_meta, is_streaming_source, hint)
    }

    /// Open a fresh stateful [`StreamingPipelineSession`] with full [`TrackSourceInfo`] hooks
    /// (supporting `deemphasis`, `effective_nyquist_hz`, `apodizing_needed`, `hf_noise_filter`).
    pub fn start_track_with_source_info(
        &self,
        spec: StreamSpec,
        source_info: TrackSourceInfo,
        peak_meta: &TrackPeakMetadata,
    ) -> Result<StreamingPipelineSession, String> {
        self.start_track_with_opts(spec, source_info, peak_meta)
    }

    fn start_track_internal(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
        incoming_tail_hint: Option<(u32, f64, f64, bool)>,
    ) -> Result<StreamingPipelineSession, String> {
        let _ = incoming_tail_hint;
        let mut source_info = TrackSourceInfo::from_stream_spec(spec);
        source_info.is_streaming_source = is_streaming_source;
        self.start_track_with_opts(spec, source_info, peak_meta)
    }

    fn start_track_with_opts(
        &self,
        spec: StreamSpec,
        source_info: TrackSourceInfo,
        peak_meta: &TrackPeakMetadata,
    ) -> Result<StreamingPipelineSession, String> {
        if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ)
            .contains(&spec.sample_rate_hz)
        {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {} Hz is outside the supported {}..={} Hz range",
                spec.sample_rate_hz, MIN_SUPPORTED_SOURCE_RATE_HZ, MAX_SUPPORTED_SOURCE_RATE_HZ
            ));
        }

        let is_streaming_source = source_info.is_streaming_source;
        let dsp_plan = self.picker.select(&source_info);
        let internal_rate = dsp_plan.target_rate_hz;
        let dac_target = self.dac_caps.resolve_for_internal_rate(internal_rate)?;
        let out_rate = dac_target.sample_rate_hz;
        let wire_format = dac_target.wire_format;

        let upsampler = build_upsampler(&dsp_plan)?;
        let deemphasis = if dsp_plan.deemphasis {
            StereoDeemphasis::new_50_15us(internal_rate)?
        } else {
            StereoDeemphasis::bypass(internal_rate)
        };
        let mut convolver = self.convolver_factory.create_for_rate(internal_rate)?;
        let delayer = self.delayer_factory.create_for_rate(internal_rate)?;
        let downsampler = build_downsampler(internal_rate, out_rate)?;

        let conv_boost_db = if convolver.accepts_tail() {
            convolver.max_boost_db()
        } else {
            0.0
        };
        let headroom = select_headroom_for_bits(
            peak_meta,
            spec.bits_per_sample,
            is_streaming_source,
            Some(conv_boost_db),
            None,
        );

        // Scale `S` / `S_B` is folded into Stage 1's linear f64 gain (Finding #4 & #14), so
        // the convolver itself runs at unity scale (1.0) and carried tails always match Track A.
        convolver.set_output_scale(1.0);

        let (requested_shaper, output_bits) = resolve_quantizer_profile(
            &dsp_plan,
            spec.bits_per_sample,
            deemphasis.is_enabled(),
            &headroom,
            convolver.name(),
            delayer.name(),
            out_rate,
            wire_format,
            !downsampler.is_bypass(),
        );

        let quantizer = StreamingQuantizer::with_rate_options(
            requested_shaper,
            output_bits,
            out_rate,
            dsp_plan.shaper_overridden,
        )?;
        let effective_shaper = quantizer.effective_shaper();
        let conv_desc = if headroom.convolution_scale_db < -1e-6 {
            format!(
                "{} [scale {:.2} dB]",
                convolver.description(),
                headroom.convolution_scale_db
            )
        } else {
            convolver.description()
        };

        let report = PipelineReport {
            source_spec: spec,
            internal_rate_hz: internal_rate,
            output_rate_hz: out_rate,
            wire_format,
            output_bits,
            headroom,
            dsp_plan,
            filter_metrics: upsampler.design_metrics(),
            deemphasis_name: deemphasis.name(),
            convolver_name: convolver.name(),
            convolver_description: conv_desc,
            convolver_content_hash: convolver.content_hash().map(str::to_string),
            delayer_name: delayer.name(),
            downsampler_name: downsampler.name(),
            effective_shaper,
            ultimate_report: None,
        };

        Ok(StreamingPipelineSession {
            report,
            upsampler,
            deemphasis,
            convolver,
            delayer,
            downsampler,
            quantizer,
        })
    }

    /// Attach a previous track's [`GaplessCarryover`] to `session`:
    /// - Same internal & output rates AND compatible upsampler geometry -> [`GaplessAttachOutcome::FullHandoff`].
    /// - Same internal & output rates with different upsampler geometry -> drains ONLY the short
    ///   upsampler FIR history into `upsampler_tail_pcm` and attaches the updated REW
    ///   [`ConvolutionTail`] + downsampler FIR state onto `session`
    ///   ([`GaplessAttachOutcome::ConvolverOnlyHandoff`]), avoiding multi-second IR drain gaps.
    /// - Different internal/output rate -> returns [`GaplessAttachOutcome::IncompatibleRate`]
    ///   untouched so the caller can drain it completely.
    pub fn attach_gapless_carryover(
        &self,
        session: &mut StreamingPipelineSession,
        carryover: GaplessCarryover,
    ) -> Result<GaplessAttachOutcome, String> {
        let internal_rate = session.report().internal_rate_hz;
        let out_rate = session.report().output_rate_hz;
        if carryover.internal_rate_hz != internal_rate
            || carryover.output_rate_hz != out_rate
            || (carryover.convolver.is_some() && !session.accepts_convolver_tail())
        {
            return Ok(GaplessAttachOutcome::IncompatibleRate(carryover));
        }

        match session.set_incoming_gapless_carryover(carryover) {
            Ok(()) => Ok(GaplessAttachOutcome::FullHandoff),
            Err(mut rejected) => {
                let mut frames = Vec::new();
                let mut attached_convolver_tail = false;
                let mut tail_stats = QuantizerStats::default();
                if let Some(up_carry) = rejected.upsampler.take() {
                    let exact_len = up_carry.exact_flush_len();
                    let up_tail_full = up_carry.flush_to_f64();
                    let split_at = exact_len.min(up_tail_full.left.len());
                    let up_tail_exact = StereoF64Buffer {
                        sample_rate_hz: up_tail_full.sample_rate_hz,
                        left: up_tail_full.left[..split_at].to_vec(),
                        right: up_tail_full.right[..split_at].to_vec(),
                    };
                    let up_post_ring = if split_at < up_tail_full.left.len() {
                        Some(StereoF64Buffer {
                            sample_rate_hz: up_tail_full.sample_rate_hz,
                            left: up_tail_full.left[split_at..].to_vec(),
                            right: up_tail_full.right[split_at..].to_vec(),
                        })
                    } else {
                        None
                    };

                    let mut conv = self.convolver_factory.create_for_rate(internal_rate)?;
                    if let Some(conv_tail) = rejected.convolver.take() {
                        conv.set_incoming_tail(conv_tail);
                    }
                    let mut delayer = self.delayer_factory.create_for_rate(internal_rate)?;
                    let mut downsampler = build_downsampler(internal_rate, out_rate)?;
                    if let Some(down_carry) = rejected.downsampler.take() {
                        let _ = downsampler.set_incoming_carryover(down_carry);
                    }
                    let mut quantizer = StreamingQuantizer::with_rate_options(
                        rejected.effective_shaper,
                        rejected.output_bits,
                        out_rate,
                        false,
                    )?;
                    if let Some(q_state) = rejected.quantizer_state.take() {
                        quantizer.restore_state(q_state);
                    }

                    if !up_tail_exact.is_empty() {
                        let de_out = if let Some(de_state) = rejected.deemphasis.take() {
                            let mut de = StereoDeemphasis::new_50_15us(internal_rate)?;
                            de.restore_state(de_state);
                            let out = de.process_block(up_tail_exact)?;
                            if let Some(next_de) = de.export_state() {
                                let _ = session.deemphasis.restore_state(next_de);
                            }
                            out
                        } else {
                            up_tail_exact
                        };
                        let c_out = conv.process_block(de_out)?;
                        if !c_out.is_empty() {
                            let d_out = delayer.process_block(c_out)?;
                            if !d_out.is_empty() {
                                let ds_out = downsampler.process_block(d_out)?;
                                if !ds_out.is_empty() {
                                    frames.extend(quantizer.quantize_block(&ds_out).frames);
                                }
                            }
                        }
                    } else if let Some(de_state) = rejected.deemphasis.take() {
                        let _ = session.deemphasis.restore_state(de_state);
                    }

                    let (conv_immediate, mut updated_tail) = conv.flush_split_tail()?;
                    if !conv_immediate.is_empty() {
                        let d_out = delayer.process_block(conv_immediate)?;
                        if !d_out.is_empty() {
                            let ds_out = downsampler.process_block(d_out)?;
                            if !ds_out.is_empty() {
                                frames.extend(quantizer.quantize_block(&ds_out).frames);
                            }
                        }
                    }
                    let delay_tail = delayer.flush()?;
                    if !delay_tail.is_empty() {
                        let ds_out = downsampler.process_block(delay_tail)?;
                        if !ds_out.is_empty() {
                            frames.extend(quantizer.quantize_block(&ds_out).frames);
                        }
                    }
                    let (ds_immediate, next_down_carry) = downsampler.flush_for_gapless()?;
                    if !ds_immediate.is_empty() {
                        frames.extend(quantizer.quantize_block(&ds_immediate).frames);
                    }
                    if let Some(dc) = next_down_carry {
                        let _ = session.downsampler.set_incoming_carryover(dc);
                    }
                    tail_stats = quantizer.stats();
                    if rejected.effective_shaper == session.report.effective_shaper
                        && rejected.output_bits == session.report.output_bits
                        && rejected.effective_shaper != NoiseShaperProfile::Bypass
                    {
                        session.quantizer.restore_state(quantizer.export_state());
                    }

                    // Review #6 Finding #13: preserve Track A's post-ringing FIR decay past
                    // `exact_len` by convolving it and overlap-adding onto the incoming tail of
                    // Track B (when Track B accepts a convolver tail) without shifting Track B.
                    if let Some(post_ring) = up_post_ring {
                        if session.accepts_convolver_tail() && !post_ring.is_empty() {
                            if let Ok(mut post_conv) =
                                self.convolver_factory.create_for_rate(internal_rate)
                            {
                                if post_conv.accepts_tail() {
                                    if let Ok(mut p_out) = post_conv.process_block(post_ring) {
                                        if let Ok(p_tail) = post_conv.flush() {
                                            p_out.left.extend_from_slice(&p_tail.left);
                                            p_out.right.extend_from_slice(&p_tail.right);
                                        }
                                        if !p_out.is_empty() {
                                            match &mut updated_tail {
                                                Some(t) => {
                                                    if p_out.left.len() > t.left.len() {
                                                        t.left.resize(p_out.left.len(), 0.0);
                                                        t.right.resize(p_out.right.len(), 0.0);
                                                    }
                                                    for (i, (&l, &r)) in p_out
                                                        .left
                                                        .iter()
                                                        .zip(p_out.right.iter())
                                                        .enumerate()
                                                    {
                                                        t.left[i] += l;
                                                        t.right[i] += r;
                                                    }
                                                }
                                                None => {
                                                    updated_tail = Some(ConvolutionTail {
                                                        sample_rate_hz: internal_rate,
                                                        left: p_out.left,
                                                        right: p_out.right,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if let Some(tail) = updated_tail {
                        if !tail.left.is_empty() && session.accepts_convolver_tail() {
                            session.set_incoming_convolver_tail(tail);
                            attached_convolver_tail = true;
                        }
                    }
                } else {
                    if let Some(conv_tail) = rejected.convolver.take() {
                        if !conv_tail.left.is_empty() && session.accepts_convolver_tail() {
                            session.set_incoming_convolver_tail(conv_tail);
                            attached_convolver_tail = true;
                        }
                    }
                    if let Some(down_carry) = rejected.downsampler.take() {
                        let _ = session.downsampler.set_incoming_carryover(down_carry);
                    }
                    if let Some(q_state) = rejected.quantizer_state.take() {
                        if rejected.effective_shaper == session.report.effective_shaper
                            && rejected.output_bits == session.report.output_bits
                        {
                            session.quantizer.restore_state(q_state);
                        }
                    }
                }

                Ok(GaplessAttachOutcome::ConvolverOnlyHandoff {
                    upsampler_tail_pcm: InterleavedInt32Buffer {
                        sample_rate_hz: out_rate,
                        frames,
                    },
                    attached_convolver_tail,
                    upsampler_tail_stats: tail_stats,
                })
            }
        }
    }

    /// Drain an unconsumed [`GaplessCarryover`] (upsampler FIR tail + REW convolution tail +
    /// optional last-stage downsampler FIR tail) cleanly into integer PCM at `output_rate_hz`.
    pub fn drain_gapless_carryover(
        &self,
        carryover: GaplessCarryover,
    ) -> Result<InterleavedInt32Buffer, String> {
        self.drain_gapless_carryover_with_stats(carryover)
            .map(|(pcm, _)| pcm)
    }

    /// Drain an unconsumed [`GaplessCarryover`] and also return the drain quantizer's
    /// [`QuantizerStats`] so callers can merge tail overs/peak into the track's report
    /// (Review #6 Finding #18).
    pub fn drain_gapless_carryover_with_stats(
        &self,
        carryover: GaplessCarryover,
    ) -> Result<(InterleavedInt32Buffer, QuantizerStats), String> {
        let internal_rate = carryover.internal_rate_hz;
        let out_rate = carryover.output_rate_hz;
        if carryover.is_empty() {
            return Ok((
                InterleavedInt32Buffer::empty(out_rate),
                QuantizerStats::default(),
            ));
        }

        let mut delayer = self.delayer_factory.create_for_rate(internal_rate)?;
        let mut downsampler = build_downsampler(internal_rate, out_rate)?;
        if let Some(down_carry) = carryover.downsampler {
            let _ = downsampler.set_incoming_carryover(down_carry);
        }
        let mut quantizer = StreamingQuantizer::with_rate_options(
            carryover.effective_shaper,
            carryover.output_bits,
            out_rate,
            false,
        )?;
        if let Some(q_state) = carryover.quantizer_state {
            quantizer.restore_state(q_state);
        }

        // Note: Ultimate Mode exports an already-decimated `ConvolutionTail` at `out_rate` when
        // downsampling is active (`upsampler.is_none() && downsampler.is_none()` and
        // `conv_tail.sample_rate_hz == out_rate != internal_rate`). Quantize it directly!
        if carryover.upsampler.is_none()
            && carryover.deemphasis.is_none()
            && internal_rate != out_rate
            && carryover
                .convolver
                .as_ref()
                .is_some_and(|c| c.sample_rate_hz == out_rate)
        {
            let ct = carryover.convolver.unwrap();
            let buf = StereoF64Buffer {
                sample_rate_hz: out_rate,
                left: ct.left,
                right: ct.right,
            };
            let pcm = quantizer.quantize_block(&buf);
            let stats = quantizer.stats();
            return Ok((pcm, stats));
        }

        let mut frames = Vec::new();
        let conv_opt = self.convolver_factory.create_for_rate(internal_rate).ok();
        let use_active_conv = conv_opt.as_ref().is_some_and(|c| c.accepts_tail());

        if use_active_conv {
            let mut convolver = conv_opt.unwrap();
            if let Some(conv_tail) = carryover.convolver {
                convolver.set_incoming_tail(conv_tail);
            }
            if let Some(up_carry) = carryover.upsampler {
                let up_tail = up_carry.flush_to_f64();
                if !up_tail.is_empty() {
                    let de_out = if let Some(de_state) = carryover.deemphasis {
                        let mut de = StereoDeemphasis::new_50_15us(internal_rate)?;
                        de.restore_state(de_state);
                        de.process_block(up_tail)?
                    } else {
                        up_tail
                    };
                    let c_out = convolver.process_block(de_out)?;
                    if !c_out.is_empty() {
                        let d_out = delayer.process_block(c_out)?;
                        if !d_out.is_empty() {
                            let ds_out = downsampler.process_block(d_out)?;
                            if !ds_out.is_empty() {
                                frames.extend(quantizer.quantize_block(&ds_out).frames);
                            }
                        }
                    }
                }
            }

            let conv_tail = convolver.flush()?;
            if !conv_tail.is_empty() {
                let d_out = delayer.process_block(conv_tail)?;
                if !d_out.is_empty() {
                    let ds_out = downsampler.process_block(d_out)?;
                    if !ds_out.is_empty() {
                        frames.extend(quantizer.quantize_block(&ds_out).frames);
                    }
                }
            }
        } else {
            let mut combined = match carryover.upsampler {
                Some(up_carry) => {
                    let up_tail = up_carry.flush_to_f64();
                    if let Some(de_state) = carryover.deemphasis {
                        let mut de = StereoDeemphasis::new_50_15us(internal_rate)?;
                        de.restore_state(de_state);
                        de.process_block(up_tail)?
                    } else {
                        up_tail
                    }
                }
                None => StereoF64Buffer::empty(internal_rate),
            };
            if let Some(conv_tail) = carryover.convolver {
                if conv_tail.left.len() > combined.left.len() {
                    combined.left.resize(conv_tail.left.len(), 0.0);
                    combined.right.resize(conv_tail.right.len(), 0.0);
                }
                for (i, (&l, &r)) in conv_tail.left.iter().zip(conv_tail.right.iter()).enumerate() {
                    combined.left[i] += l;
                    combined.right[i] += r;
                }
            }
            if !combined.is_empty() {
                let d_out = delayer.process_block(combined)?;
                if !d_out.is_empty() {
                    let ds_out = downsampler.process_block(d_out)?;
                    if !ds_out.is_empty() {
                        frames.extend(quantizer.quantize_block(&ds_out).frames);
                    }
                }
            }
        }

        let delay_tail = delayer.flush()?;
        if !delay_tail.is_empty() {
            let ds_out = downsampler.process_block(delay_tail)?;
            if !ds_out.is_empty() {
                frames.extend(quantizer.quantize_block(&ds_out).frames);
            }
        }

        let down_tail = downsampler.flush()?;
        if !down_tail.is_empty() {
            frames.extend(quantizer.quantize_block(&down_tail).frames);
        }

        let stats = quantizer.stats();
        Ok((
            InterleavedInt32Buffer {
                sample_rate_hz: out_rate,
                frames,
            },
            stats,
        ))
    }

    /// Convenience one-shot helper for small test buffers (delegates directly to `start_track`).
    pub fn process(
        &self,
        audio: StereoF64Buffer,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
    ) -> Result<ProcessedTrack, String> {
        let mut session = self.start_track(spec, peak_meta, is_streaming_source)?;
        let mut pcm = session.process_block(audio)?;
        let tail = session.flush()?;
        pcm.frames.extend(tail.frames);
        let report = session.report().clone();
        Ok(ProcessedTrack { pcm, report })
    }
}

/// Shared quantizer bit-depth & noise-shaper selection used by both normal-mode
/// `start_track_with_opts` and Ultimate Mode `start_track_with_plan_and_headroom` /
/// `reconfigure_quantizer_for_headroom` (Review #7 F2).
#[allow(clippy::too_many_arguments)]
fn resolve_quantizer_profile(
    dsp_plan: &SelectedDspPlan,
    source_bits_per_sample: u16,
    deemphasis_enabled: bool,
    headroom: &HeadroomDecision,
    convolver_name: &str,
    delayer_name: &str,
    output_rate_hz: u32,
    wire_format: DacWireFormat,
    downsampler_active: bool,
) -> (NoiseShaperProfile, u8) {
    let wire_bits = wire_format.bit_depth();
    let effective_dac_bits = dsp_plan.dac_bits.min(wire_bits).clamp(16, 32);

    let untouched_native = dsp_plan.filter == FilterArchetype::Null
        && !deemphasis_enabled
        && !downsampler_active
        && headroom.mode == HeadroomMode::ZeroDb
        && headroom.convolution_scale_db.abs() <= 1e-12
        && convolver_name == "null-convolver"
        && delayer_name == "null-delay";

    if untouched_native
        && u16::from(wire_bits) >= source_bits_per_sample
        && (effective_dac_bits == 24 || u16::from(effective_dac_bits) >= source_bits_per_sample)
    {
        let out_bits = (source_bits_per_sample as u8).clamp(16, wire_bits);
        // Truly bit-perfect native pass-through: never add TPDF dither when no DSP modified
        // the signal and output bit depth can represent every source bit losslessly.
        let shaper = if !dsp_plan.shaper_overridden && u16::from(out_bits) >= source_bits_per_sample
        {
            NoiseShaperProfile::Bypass
        } else {
            dsp_plan.shaper
        };
        (shaper, out_bits)
    } else if dsp_plan.shaper == NoiseShaperProfile::Bypass {
        let fallback = if !dsp_plan.shaper_overridden
            && matches!(output_rate_hz, 176_400 | 192_000 | 352_800 | 384_000)
        {
            NoiseShaperProfile::NsGentle2
        } else {
            NoiseShaperProfile::TpdfDither
        };
        (fallback, effective_dac_bits)
    } else {
        (dsp_plan.shaper, effective_dac_bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::TrackSourceInfo;

    #[test]
    fn reconfigure_quantizer_switches_to_tpdf_when_gain_negative() {
        let policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let pipeline = AudioPipeline::new(policy).unwrap();
        let spec = StreamSpec {
            sample_rate_hz: 44_100,
            bits_per_sample: 16,
            channels: 2,
        };
        let src = TrackSourceInfo::from_stream_spec(spec);
        let plan = UpsamplerPicker::new(policy).unwrap().select(&src);
        let placeholder_hr = HeadroomDecision {
            mode: HeadroomMode::ZeroDb,
            peak_dbfs: None,
            peak_kind: None,
            convolution_scale_db: 0.0,
            reason: "placeholder".to_string(),
        };
        let mut session = pipeline
            .start_track_with_plan_and_headroom(spec, plan, placeholder_hr)
            .unwrap();
        assert_eq!(session.report().effective_shaper, NoiseShaperProfile::Bypass);
        assert_eq!(session.report().output_bits, 16);

        let neg_hr = HeadroomDecision {
            mode: HeadroomMode::ZeroDb,
            peak_dbfs: Some(-0.01),
            peak_kind: None,
            convolution_scale_db: -0.4,
            reason: "negative fixed gain".to_string(),
        };
        session.reconfigure_quantizer_for_headroom(&neg_hr).unwrap();
        assert_eq!(
            session.report().effective_shaper,
            NoiseShaperProfile::TpdfDither
        );
        assert_eq!(session.report().output_bits, 24);
    }

    #[test]
    fn last_stage_downsampler_keeps_8fs_internal_pipeline_and_single_quantizer() {
        let policy = UpsamplerPolicy::default();
        let spec_44k = StreamSpec {
            sample_rate_hz: 44_100,
            bits_per_sample: 16,
            channels: 2,
        };
        let frames = 2048;
        let audio = StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: (0..frames)
                .map(|n| 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 44_100.0).sin())
                .collect(),
            right: (0..frames)
                .map(|n| 0.25 * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 44_100.0).sin())
                .collect(),
        };
        let peak_meta = TrackPeakMetadata::default();

        // 1. 192 kHz DAC (176.4 / 192 Int24Packed): internal stays 352,800 Hz, output 176,400 Hz,
        //    4fS ns-gentle-2 active, low 8 bits of every i32 sample are strictly zero!
        let caps_192 = DacOutputCapabilities::from_supported_rates(
            &[
                (176_400, DacWireFormat::Int24Packed),
                (192_000, DacWireFormat::Int24Packed),
            ],
            None,
        );
        let pipe_192 = AudioPipeline::new(policy)
            .unwrap()
            .with_dac_capabilities(caps_192);
        let out_192 = pipe_192
            .process(audio.clone(), spec_44k, &peak_meta, false)
            .unwrap();
        assert_eq!(out_192.report.internal_rate_hz, 352_800);
        assert_eq!(out_192.report.output_rate_hz, 176_400);
        assert_eq!(out_192.report.wire_format, DacWireFormat::Int24Packed);
        assert_eq!(out_192.report.downsampler_name, "decim-kaiser-2x");
        assert_eq!(out_192.report.effective_shaper, NoiseShaperProfile::NsGentle2);
        assert_eq!(out_192.report.output_bits, 24);
        assert!(out_192.pcm.frames.iter().all(|&[l, r]| (l & 0xFF) == 0 && (r & 0xFF) == 0));

        // 2. 44.1 / 48 kHz 16-bit DAC: internal stays 352,800 Hz, output 44,100 Hz Int16,
        //    flat TPDF dither at 16-bit (single quantizer -> low 16 bits strictly zero!).
        let caps_44_16 = DacOutputCapabilities::from_supported_rates(
            &[
                (44_100, DacWireFormat::Int16),
                (48_000, DacWireFormat::Int16),
            ],
            None,
        );
        let pipe_44 = AudioPipeline::new(policy)
            .unwrap()
            .with_dac_capabilities(caps_44_16);
        let out_44 = pipe_44
            .process(audio.clone(), spec_44k, &peak_meta, false)
            .unwrap();
        assert_eq!(out_44.report.internal_rate_hz, 352_800);
        assert_eq!(out_44.report.output_rate_hz, 44_100);
        assert_eq!(out_44.report.wire_format, DacWireFormat::Int16);
        assert_eq!(out_44.report.downsampler_name, "decim-kaiser-8x");
        assert_eq!(out_44.report.effective_shaper, NoiseShaperProfile::TpdfDither);
        assert_eq!(out_44.report.output_bits, 16);
        assert!(
            out_44
                .pcm
                .frames
                .iter()
                .all(|&[l, r]| (l & 0xFFFF) == 0 && (r & 0xFFFF) == 0)
        );

        // 3. 48/96/192-only DAC playing 44.1 kHz -> rational cross-family 352.8 -> 192 kHz + 4fS shaper!
        let caps_48_only = DacOutputCapabilities::from_supported_rates(
            &[(192_000, DacWireFormat::Int32), (48_000, DacWireFormat::Int32)],
            None,
        );
        let pipe_48 = AudioPipeline::new(policy)
            .unwrap()
            .with_dac_capabilities(caps_48_only);
        let out_48 = pipe_48.process(audio, spec_44k, &peak_meta, false).unwrap();
        assert_eq!(out_48.report.internal_rate_hz, 352_800);
        assert_eq!(out_48.report.output_rate_hz, 192_000);
        assert_eq!(out_48.report.downsampler_name, "decim-rational-kaiser");
        assert_eq!(out_48.report.effective_shaper, NoiseShaperProfile::NsGentle2);
    }
}
