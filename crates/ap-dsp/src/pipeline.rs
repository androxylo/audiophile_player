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
    /// Internal `8fS` REW convolution tail (normal-mode gapless handoff into next track's convolver).
    pub convolver: Option<ConvolutionTail>,
    /// Pre-scaled output-rate (`output_rate_hz`) strictly additive tail `z_A[n]` exported by
    /// Ultimate Mode (`OQ-40`) to overlap-add right before quantization without touching Track B's
    /// convolver or downsampler (Review #9 D6: kept strictly separate from `convolver`).
    pub additive_output_tail: Option<ConvolutionTail>,
    /// Number of leading frames inside `additive_output_tail` sitting BEFORE Track A's nominal
    /// track boundary (held back so Track B's pre-ringing aligns with zero dip; Review #10 CQ1).
    pub pre_boundary_frames: usize,
    /// Optional last-stage `f64` downsampler FIR state carried across equal-rate gapless transitions.
    pub downsampler: Option<DownsamplerCarryover>,
    /// Continuous PRNG + per-channel error-feedback shaper state preserved across equal-rate
    /// gapless boundaries (`docs/research/04-noise-shapers.md` §6.4).
    pub quantizer_state: Option<QuantizerCarryoverState>,
}

impl GaplessCarryover {
    pub fn is_empty(&self) -> bool {
        self.upsampler.is_none()
            && self.convolver.is_none()
            && self.additive_output_tail.is_none()
            && self.downsampler.is_none()
    }

    /// Maximum absolute amplitude (`0.0` if absent) of any carried REW / additive convolution tail.
    pub fn convolver_tail_peak_linear(&self) -> f64 {
        let c = self
            .convolver
            .as_ref()
            .map_or(0.0, ConvolutionTail::peak_linear);
        let a = self
            .additive_output_tail
            .as_ref()
            .map_or(0.0, ConvolutionTail::peak_linear);
        c.max(a)
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
    /// Provenance of the peak evidence governing headroom (`"exact-M"`, `"scan-tp"`, `"cached-tp"`,
    /// `"tag-tp"`, `"tag-sp"`, or `"unknown"`, Issue #23 P6).
    pub peak_source: Option<String>,
    /// Elapsed wall-clock time of the E3 whole-file peak scan in milliseconds (when E3 ran).
    pub peak_scan_ms: Option<u64>,
    /// True-peak meter error / ISP allowance `A_tp` in dB used in the headroom formula.
    pub a_tp_db: Option<f64>,
    /// Upsampler overshoot bound `A_up(plan)` in dB used in the headroom formula.
    pub a_up_db: Option<f64>,
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
        let peak_src_str = self
            .peak_source
            .as_deref()
            .map(|s| format!(" peak_source={s}"))
            .unwrap_or_default();
        let scan_ms_str = self
            .peak_scan_ms
            .map(|ms| format!(" peak_scan_ms={ms}"))
            .unwrap_or_default();
        let margin_str = match (self.a_tp_db, self.a_up_db) {
            (Some(atp), Some(aup)) => format!(" A_tp={atp:.2}dB A_up={aup:.2}dB"),
            (Some(atp), None) => format!(" A_tp={atp:.2}dB"),
            (None, Some(aup)) => format!(" A_up={aup:.2}dB"),
            (None, None) => String::new(),
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
            "{choice_str}mode=[{}] rule={} fired=[{}] {peak_str}{peak_src_str}{scan_ms_str}{margin_str} G={:+.3}dB m_q={:.5}dB{speed_str}",
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
    /// Exact upsampler alignment latency in internal-rate samples (`pad_tail * ratio`).
    pub upsampler_latency_samples: usize,
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
        self.format_summary_with_backend(total_duration_s, "WASAPI Exclusive")
    }

    pub fn format_summary_with_backend(
        &self,
        total_duration_s: Option<f64>,
        backend_label: &str,
    ) -> String {
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
                "{mode_prefix}{}/{} Hz -> {} Hz f64 ({}, {}, {}, {}, {}) | {backend_label} {}",
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
                "{mode_prefix}{}/{} Hz -> {} Hz f64 -> {} Hz ({}, {}, {}, {}, {}, {}) | {backend_label} {}",
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
    /// Optional output-rate additive post-ring tail attached during normal-mode
    /// `ConvolverOnlyHandoff` when no REW convolver is active (`Review #10 CQ4`).
    incoming_additive_tail: Option<(ConvolutionTail, usize)>,
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

    /// Read-only verification accessors (`PIPELINE_TESTS_REVIEW.md` §2.3).
    #[cfg(any(test, feature = "verify"))]
    pub fn upsampler_export(&self) -> Option<crate::upsampler::UpsamplerExport> {
        self.upsampler.verify_export()
    }

    #[cfg(any(test, feature = "verify"))]
    pub fn downsampler_kernel(&self) -> Option<crate::downsampler::DownsamplerExport> {
        self.downsampler.kernel()
    }

    #[cfg(any(test, feature = "verify"))]
    pub fn convolver_ir(&self) -> Option<crate::convolution::ConvolverIrExport<'_>> {
        self.convolver.ir()
    }

    #[cfg(any(test, feature = "verify"))]
    pub fn deemphasis_enabled(&self) -> bool {
        self.deemphasis.is_enabled()
    }

    #[cfg(any(test, feature = "verify"))]
    pub fn shaper_coeffs(&self) -> &'static [f64] {
        self.quantizer.coeffs()
    }

    #[cfg(any(test, feature = "verify"))]
    pub fn set_quantizer_seed(&mut self, seed: u64) {
        self.quantizer.set_seed(seed);
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

    /// Shared Stage 4 -> 4.5 (`delayer -> downsampler`) post-convolver chain helper (Review #9 D4).
    fn post_conv(&mut self, block: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        let block = self.delayer.process_block(block)?;
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        self.downsampler.process_block(block)
    }

    /// Shared Stage 2.5 -> 3 -> 4 -> 4.5 (`deemph -> convolver -> delayer -> downsampler`) helper (D4).
    fn post_upsampler(&mut self, block: StereoF64Buffer) -> Result<StereoF64Buffer, String> {
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        let block = self.deemphasis.process_block(block)?;
        let block = self.convolver.process_block(block)?;
        self.post_conv(block)
    }

    /// Enable pre-ringing emission at track start for exact Ultimate gapless continuity
    /// (`Review #10 CQ1`): emits Track B's pre-boundary FIR pre-ringing BEFORE sample 0 so that
    /// adding Track A's `additive_output_tail` (with `pre_boundary_frames`) reconstructs
    /// `(x_A ‖ x_B) * h` to machine precision with zero half-amplitude dip or click!
    ///
    /// Returns the exact number of output frames emitted before Track B's nominal sample 0.
    /// Must be called before any `process_block_unity_f64` call.
    pub fn enable_gapless_prering(&mut self) -> usize {
        let internal = self.report.internal_rate_hz;
        let out = self.report.output_rate_hz;
        if out == 0 || internal % out != 0 {
            return 0;
        }
        let down_m = (internal / out) as usize;
        let up_pre_internal = self.upsampler.enable_gapless_prering();
        let ds_pre_out = self.downsampler.enable_gapless_prering();
        (up_pre_internal / down_m) + ds_pre_out
    }

    /// Shared `GaplessCarryover` constructor (Review #9 D4 + D6).
    fn build_carryover(
        &self,
        upsampler: Option<UpsamplerCarryover>,
        deemphasis: Option<DeemphasisCarryover>,
        convolver: Option<ConvolutionTail>,
        additive_output_tail: Option<ConvolutionTail>,
        pre_boundary_frames: usize,
        downsampler: Option<DownsamplerCarryover>,
    ) -> Option<GaplessCarryover> {
        let has_shaper_state = !self.quantizer.coeffs().is_empty();
        if upsampler.is_none()
            && convolver.is_none()
            && additive_output_tail.is_none()
            && downsampler.is_none()
            && deemphasis.is_none()
            && !has_shaper_state
        {
            return None;
        }
        let quantizer_state = if self.report.effective_shaper != NoiseShaperProfile::Bypass {
            Some(self.quantizer.export_state())
        } else {
            None
        };
        Some(GaplessCarryover {
            internal_rate_hz: self.report.internal_rate_hz,
            output_rate_hz: self.report.output_rate_hz,
            output_bits: self.report.output_bits,
            effective_shaper: self.report.effective_shaper,
            upsampler,
            deemphasis,
            convolver,
            additive_output_tail,
            pre_boundary_frames,
            downsampler,
            quantizer_state,
        })
    }

    /// Run Stages 2 -> 2.5 -> 3 -> 4 -> 4.5 (`upsampler -> deemphasis -> convolver -> delayer ->
    /// last-stage downsampler`) at **unity gain (`1.0`)** in pure `f64`, without quantizing yet.
    pub fn process_block_unity_f64(
        &mut self,
        block: StereoF64Buffer,
    ) -> Result<StereoF64Buffer, String> {
        if block.is_empty() {
            return Ok(StereoF64Buffer::empty(self.report.output_rate_hz));
        }
        let block = self.upsampler.process_block(block)?;
        self.post_upsampler(block)
    }

    /// Flush the unity-gain linear chain cleanly at EOF (non-gapless single-track or end-of-queue).
    pub fn flush_unity_f64(&mut self) -> Result<StereoF64Buffer, String> {
        let rate = self.report.output_rate_hz;
        let mut out_l = Vec::new();
        let mut out_r = Vec::new();
        let mut emit = |buf: StereoF64Buffer| {
            if !buf.is_empty() {
                out_l.extend_from_slice(&buf.left);
                out_r.extend_from_slice(&buf.right);
            }
        };

        let up_tail = self.upsampler.flush()?;
        emit(self.post_upsampler(up_tail)?);
        let conv_tail = self.convolver.flush()?;
        emit(self.post_conv(conv_tail)?);
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            emit(self.downsampler.process_block(delay_tail)?);
        }
        emit(self.downsampler.flush()?);
        Ok(StereoF64Buffer {
            sample_rate_hz: rate,
            left: out_l,
            right: out_r,
        })
    }

    /// Flush the unity-gain linear chain at EOF for gapless continuity (`OQ-40` strict independence
    /// + `Review #10 CQ1` continuous post-ringing):
    /// - Returns `(ending_unity_f64, peak_over_ending_and_tail_unity, Option<GaplessCarryover>)`.
    /// - Uses `upsampler.flush_for_gapless()` so Track A's exact length ends at `N_A * ratio` while
    ///   Track A's complete post-ringing (`up_post_ring` -> `deemph` -> `convolver` -> `downsampler`)
    ///   is carried in `GaplessCarryover::additive_output_tail` (pre-scaled by `fixed_gain_db`),
    ///   so `(x_A ‖ 0)*h + (0 ‖ x_B)*h == (x_A ‖ x_B)*h` with zero half-amplitude dip or click!
    pub fn flush_for_gapless_unity_f64(
        &mut self,
        fixed_gain_db: f64,
    ) -> Result<(StereoF64Buffer, f64, Option<GaplessCarryover>), String> {
        let internal_rate = self.report.internal_rate_hz;
        let out_rate = self.report.output_rate_hz;
        let mut out_l = Vec::new();
        let mut out_r = Vec::new();
        let mut emit = |buf: StereoF64Buffer| {
            if !buf.is_empty() {
                out_l.extend_from_slice(&buf.left);
                out_r.extend_from_slice(&buf.right);
            }
        };

        // Review #10 CQ1: for native 8fS and exact integer downsampling (÷2/÷4/÷8), hold back
        // Track A's unpadded FIR history (`up_tail_exact` + `up_post_ring` + downsampler holdback)
        // in `additive_output_tail` with `pre_boundary_frames` so Track B's pre-ringing aligns
        // and sums with ZERO half-amplitude dip! For cross-family rational resampling (e.g.
        // 352.8 -> 48 kHz), emit up to Track A's exact boundary and carry only post-ringing.
        let is_exact_integer_ds = out_rate > 0 && internal_rate % out_rate == 0;
        let down_m = if is_exact_integer_ds {
            (internal_rate / out_rate) as usize
        } else {
            1
        };

        let (up_ready, up_carry) = self.upsampler.flush_for_gapless()?;
        emit(self.post_upsampler(up_ready)?);

        let (up_post_ring, up_pre_boundary_internal) = if let Some(uc) = up_carry {
            let exact_len = uc.exact_flush_len();
            let full = uc.flush_to_f64();
            if is_exact_integer_ds && exact_len % down_m == 0 {
                // Hold back ALL of `full` (`up_tail_exact` ++ `up_post_ring`) into the additive
                // tail so Track B's pre-ringing combines cleanly over `[N_A - exact_len .. N_A]`!
                (full, exact_len)
            } else {
                let split_at = exact_len.min(full.left.len());
                if split_at > 0 {
                    let exact_buf = StereoF64Buffer {
                        sample_rate_hz: full.sample_rate_hz,
                        left: full.left[..split_at].to_vec(),
                        right: full.right[..split_at].to_vec(),
                    };
                    emit(self.post_upsampler(exact_buf)?);
                }
                let post = if split_at < full.left.len() {
                    StereoF64Buffer {
                        sample_rate_hz: full.sample_rate_hz,
                        left: full.left[split_at..].to_vec(),
                        right: full.right[split_at..].to_vec(),
                    }
                } else {
                    StereoF64Buffer::empty(internal_rate)
                };
                (post, 0)
            }
        } else {
            (StereoF64Buffer::empty(internal_rate), 0)
        };

        // Continue de-emphasis seamlessly through `up_post_ring` (or a brief zero decay if
        // de-emphasis is active with a zero-latency upsampler).
        let deemph_post_ring = if !up_post_ring.is_empty() {
            self.deemphasis.process_block(up_post_ring)?
        } else if self.deemphasis.is_enabled() {
            self.deemphasis.process_block(StereoF64Buffer {
                sample_rate_hz: internal_rate,
                left: vec![0.0; 128],
                right: vec![0.0; 128],
            })?
        } else {
            StereoF64Buffer::empty(internal_rate)
        };

        // Convolve `deemph_post_ring` together with the convolver's own decaying IR tail while
        // splitting at the exact boundary BEFORE `deemph_post_ring`!
        let (track_end, mut conv_tail) = self
            .convolver
            .flush_split_tail_with_post_ring(deemph_post_ring)?;
        emit(self.post_conv(track_end)?);
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            emit(self.downsampler.process_block(delay_tail)?);
        }

        let mut pre_boundary_frames = up_pre_boundary_internal / down_m;
        let mut additive_tail = if !self.downsampler.is_bypass() {
            let pending_a_out = self.downsampler.pending_flush_frames();
            let mut combined_l = Vec::new();
            let mut combined_r = Vec::new();
            if let Some(ct) = conv_tail.take() {
                let ds_c = self.downsampler.process_block(StereoF64Buffer {
                    sample_rate_hz: internal_rate,
                    left: ct.left,
                    right: ct.right,
                })?;
                combined_l.extend_from_slice(&ds_c.left);
                combined_r.extend_from_slice(&ds_c.right);
            }
            let ds_flush = self.downsampler.flush()?;
            combined_l.extend_from_slice(&ds_flush.left);
            combined_r.extend_from_slice(&ds_flush.right);

            if is_exact_integer_ds {
                // Hold back `pending_a_out` as well so Track B's downsampler pre-ringing combines
                // cleanly with ZERO half-amplitude dip across the decimator FIR!
                pre_boundary_frames += pending_a_out;
                if !combined_l.is_empty() {
                    Some(ConvolutionTail {
                        sample_rate_hz: out_rate,
                        left: combined_l,
                        right: combined_r,
                    })
                } else {
                    None
                }
            } else {
                let split = pending_a_out.min(combined_l.len());
                out_l.extend_from_slice(&combined_l[..split]);
                out_r.extend_from_slice(&combined_r[..split]);
                if split < combined_l.len() {
                    Some(ConvolutionTail {
                        sample_rate_hz: out_rate,
                        left: combined_l[split..].to_vec(),
                        right: combined_r[split..].to_vec(),
                    })
                } else {
                    None
                }
            }
        } else {
            conv_tail.map(|mut ct| {
                ct.sample_rate_hz = out_rate;
                ct
            })
        };

        // Measure unity-gain peak over BOTH ending frames and the unscaled additive tail.
        let mut tail_peak_unity = 0.0_f64;
        for &s in out_l.iter().chain(out_r.iter()) {
            tail_peak_unity = tail_peak_unity.max(s.abs());
        }
        if let Some(ref ct) = additive_tail {
            tail_peak_unity = tail_peak_unity.max(ct.peak_linear());
        }

        // Pre-scale exported `additive_tail` by this track's fixed gain `G` so it becomes `z_A[n]`.
        if let Some(ref mut ct) = additive_tail {
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

        let carryover =
            self.build_carryover(None, None, None, additive_tail, pre_boundary_frames, None);
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

    /// Overlap-add any attached normal-mode output-rate additive tail (`Review #10 CQ4`) onto
    /// `buf` right before quantization.
    fn overlay_incoming_additive_tail(&mut self, buf: &mut StereoF64Buffer) {
        if let Some((tail, offset)) = self.incoming_additive_tail.as_mut() {
            let rem = tail.left.len().saturating_sub(*offset);
            let n = buf.left.len().min(rem);
            for i in 0..n {
                buf.left[i] += tail.left[*offset + i];
                buf.right[i] += tail.right[*offset + i];
            }
            *offset += n;
            if *offset >= tail.left.len() {
                self.incoming_additive_tail = None;
            }
        }
    }

    /// Process one bounded source `f64` block through all 5 stages -> ready `i32` PCM block.
    pub fn process_block(
        &mut self,
        mut block: StereoF64Buffer,
    ) -> Result<InterleavedInt32Buffer, String> {
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }
        apply_safe_gain_f64(&mut block, self.report.headroom.total_gain_db())?;
        let block = self.upsampler.process_block(block)?;
        let mut block = self.post_upsampler(block)?;
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }
        self.overlay_incoming_additive_tail(&mut block);
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
            || carryover.additive_output_tail.is_some()
        {
            return Err(carryover);
        }
        if let Some(ref ct) = carryover.convolver {
            if !self.convolver.accepts_tail() || ct.sample_rate_hz != self.report.internal_rate_hz {
                return Err(carryover);
            }
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
        if let Some(q_state) = carryover.quantizer_state.take() {
            if carryover.effective_shaper == self.report.effective_shaper
                && carryover.output_bits == self.report.output_bits
            {
                self.quantizer.restore_state(q_state);
            }
        }
        Ok(())
    }

    /// Flush all stateful FIR / convolution / delay / downsampler tails cleanly at end-of-track.
    pub fn flush(&mut self) -> Result<InterleavedInt32Buffer, String> {
        let mut frames = Vec::new();
        let rate = self.report.output_rate_hz;
        let up_tail = self.upsampler.flush()?;
        let mut b1 = self.post_upsampler(up_tail)?;
        if !b1.is_empty() {
            self.overlay_incoming_additive_tail(&mut b1);
            frames.extend(self.quantizer.quantize_block(&b1).frames);
        }
        let conv_tail = self.convolver.flush()?;
        let mut b2 = self.post_conv(conv_tail)?;
        if !b2.is_empty() {
            self.overlay_incoming_additive_tail(&mut b2);
            frames.extend(self.quantizer.quantize_block(&b2).frames);
        }
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            let mut b3 = self.downsampler.process_block(delay_tail)?;
            if !b3.is_empty() {
                self.overlay_incoming_additive_tail(&mut b3);
                frames.extend(self.quantizer.quantize_block(&b3).frames);
            }
        }
        let mut down_tail = self.downsampler.flush()?;
        if !down_tail.is_empty() {
            self.overlay_incoming_additive_tail(&mut down_tail);
            frames.extend(self.quantizer.quantize_block(&down_tail).frames);
        }
        if let Some((tail, offset)) = self.incoming_additive_tail.take() {
            if offset < tail.left.len() {
                let rem = StereoF64Buffer {
                    sample_rate_hz: rate,
                    left: tail.left[offset..].to_vec(),
                    right: tail.right[offset..].to_vec(),
                };
                frames.extend(self.quantizer.quantize_block(&rem).frames);
            }
        }
        Ok(InterleavedInt32Buffer {
            sample_rate_hz: rate,
            frames,
        })
    }

    /// Flush end-of-track audio for gapless album continuity: skips trailing zero-padding on the
    /// polyphase upsampler and optional last-stage downsampler, emits ready frames, and returns
    /// unpadded FIR histories + decaying REW convolution tail as [`GaplessCarryover`].
    pub fn flush_for_gapless(
        &mut self,
    ) -> Result<(InterleavedInt32Buffer, Option<GaplessCarryover>), String> {
        let mut frames = Vec::new();
        let out_rate = self.report.output_rate_hz;

        let (up_tail, up_carry) = self.upsampler.flush_for_gapless()?;
        let mut b1 = self.post_upsampler(up_tail)?;
        if !b1.is_empty() {
            self.overlay_incoming_additive_tail(&mut b1);
            frames.extend(self.quantizer.quantize_block(&b1).frames);
        }
        let (track_end, conv_tail) = self.convolver.flush_split_tail()?;
        let mut b2 = self.post_conv(track_end)?;
        if !b2.is_empty() {
            self.overlay_incoming_additive_tail(&mut b2);
            frames.extend(self.quantizer.quantize_block(&b2).frames);
        }
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            let mut b3 = self.downsampler.process_block(delay_tail)?;
            if !b3.is_empty() {
                self.overlay_incoming_additive_tail(&mut b3);
                frames.extend(self.quantizer.quantize_block(&b3).frames);
            }
        }
        let (mut ds_tail, down_carry) = self.downsampler.flush_for_gapless()?;
        if !ds_tail.is_empty() {
            self.overlay_incoming_additive_tail(&mut ds_tail);
            frames.extend(self.quantizer.quantize_block(&ds_tail).frames);
        }
        if let Some((tail, offset)) = self.incoming_additive_tail.take() {
            if offset < tail.left.len() {
                let rem = StereoF64Buffer {
                    sample_rate_hz: out_rate,
                    left: tail.left[offset..].to_vec(),
                    right: tail.right[offset..].to_vec(),
                };
                frames.extend(self.quantizer.quantize_block(&rem).frames);
            }
        }

        let deemph_state = self.deemphasis.export_state();
        let carryover = self.build_carryover(up_carry, deemph_state, conv_tail, None, 0, down_carry);
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
    dac_caps: std::sync::Arc<std::sync::RwLock<DacOutputCapabilities>>,
}

impl AudioPipeline {
    /// Construct the default pipeline with dynamic upsampler picking + null REW/delay stages
    /// targeting native `8fS` (`352.8 / 384 kHz`) DACs.
    pub fn new(upsampler_policy: UpsamplerPolicy) -> Result<Self, String> {
        Ok(Self {
            picker: UpsamplerPicker::new(upsampler_policy)?,
            convolver_factory: Box::new(NullConvolverFactory),
            delayer_factory: Box::new(NullChannelDelayerFactory),
            dac_caps: std::sync::Arc::new(std::sync::RwLock::new(
                DacOutputCapabilities::native_8fs(),
            )),
        })
    }

    /// Attach probed or configured [`DacOutputCapabilities`] for optional last-stage `f64`
    /// downsampling on `< 352.8 / 384 kHz` DACs (no-op bypass on native `8fS` DACs).
    pub fn with_dac_capabilities(self, dac_caps: DacOutputCapabilities) -> Self {
        self.update_dac_capabilities(dac_caps);
        self
    }

    /// Attach a shared [`DacOutputCapabilities`] handle so a deferred hardware probe can update
    /// the active DAC capabilities once a busy DAC becomes free.
    pub fn with_shared_dac_capabilities(
        mut self,
        dac_caps: std::sync::Arc<std::sync::RwLock<DacOutputCapabilities>>,
    ) -> Self {
        self.dac_caps = dac_caps;
        self
    }

    /// Update the active DAC capabilities in place.
    pub fn update_dac_capabilities(&self, dac_caps: DacOutputCapabilities) {
        *self
            .dac_caps
            .write()
            .unwrap_or_else(|e| e.into_inner()) = dac_caps;
    }

    /// Active DAC output capabilities attached to this pipeline factory.
    pub fn dac_capabilities(&self) -> DacOutputCapabilities {
        *self.dac_caps.read().unwrap_or_else(|e| e.into_inner())
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

    /// Compute the exact [`ap_core::track_db::PostChainKey`] that
    /// [`Self::start_track_with_plan_and_headroom`] will produce for `(source_rate_hz, plan)`
    /// without constructing upsampler FIR/FFT tables (used by E2 next-track pre-scan fast-skip).
    pub fn post_chain_key_for_plan(
        &self,
        source_rate_hz: u32,
        plan: &SelectedDspPlan,
    ) -> Result<ap_core::track_db::PostChainKey, String> {
        let internal_rate = plan.target_rate_hz;
        let dac_target = self
            .dac_capabilities()
            .resolve_for_internal_rate(internal_rate)?;
        let convolver = self.convolver_factory.create_for_rate(internal_rate)?;
        Ok(crate::selector::build_post_chain_key(
            plan,
            source_rate_hz,
            dac_target.sample_rate_hz,
            convolver.content_hash(),
        ))
    }

    /// Open a fresh [`StreamingPipelineSession`] from an explicit pre-resolved [`SelectedDspPlan`]
    /// and [`HeadroomDecision`] (used by Ultimate Mode `W2` and `W3`).
    pub fn start_track_with_plan_and_headroom(
        &self,
        spec: StreamSpec,
        dsp_plan: SelectedDspPlan,
        headroom: HeadroomDecision,
    ) -> Result<StreamingPipelineSession, String> {
        self.build_session(spec, dsp_plan, Some(headroom), None, false)
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
            dac_caps: std::sync::Arc::new(std::sync::RwLock::new(
                DacOutputCapabilities::native_8fs(),
            )),
        })
    }

    /// Open a fresh stateful [`StreamingPipelineSession`] for one track or continuous stream.
    pub fn start_track(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
    ) -> Result<StreamingPipelineSession, String> {
        let mut source_info = TrackSourceInfo::from_stream_spec(spec);
        source_info.is_streaming_source = is_streaming_source;
        self.start_track_with_source_info(spec, source_info, peak_meta)
    }

    /// Open a fresh stateful [`StreamingPipelineSession`] for a gapless continuation track.
    ///
    /// Per Review #6 Finding #1, Track B's whole-track gain is kept independent of Track A's
    /// decaying tail (relying on the pre-dither backstop rather than attenuating all of Track B),
    /// so this delegates directly to [`Self::start_track`].
    pub fn start_track_for_carryover(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
        _incoming: Option<&GaplessCarryover>,
    ) -> Result<StreamingPipelineSession, String> {
        self.start_track(spec, peak_meta, is_streaming_source)
    }

    /// Open a fresh stateful [`StreamingPipelineSession`] (legacy alias for [`Self::start_track`];
    /// whole-track `S_B` tail attenuation is intentionally omitted per Review #6 Finding #1).
    pub fn start_track_with_tail_hint(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
        _incoming_tail_hint: Option<(u32, f64)>,
    ) -> Result<StreamingPipelineSession, String> {
        self.start_track(spec, peak_meta, is_streaming_source)
    }

    /// Open a fresh stateful [`StreamingPipelineSession`] with full [`TrackSourceInfo`] hooks
    /// (supporting `deemphasis`, `effective_nyquist_hz`, `apodizing_needed`, `hf_noise_filter`).
    pub fn start_track_with_source_info(
        &self,
        spec: StreamSpec,
        source_info: TrackSourceInfo,
        peak_meta: &TrackPeakMetadata,
    ) -> Result<StreamingPipelineSession, String> {
        let is_streaming_source = source_info.is_streaming_source;
        let dsp_plan = self.picker.select(&source_info);
        self.build_session(spec, dsp_plan, None, Some(peak_meta), is_streaming_source)
    }

    /// Single shared session constructor used by both normal mode and Ultimate Mode (Review #9 D5).
    fn build_session(
        &self,
        spec: StreamSpec,
        dsp_plan: SelectedDspPlan,
        explicit_headroom: Option<HeadroomDecision>,
        peak_meta: Option<&TrackPeakMetadata>,
        is_streaming_source: bool,
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
        let dac_target = self
            .dac_capabilities()
            .resolve_for_internal_rate(internal_rate)?;
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

        let headroom = match explicit_headroom {
            Some(hr) => hr,
            None => {
                let conv_boost_db = if convolver.accepts_tail() {
                    convolver.max_boost_db()
                } else {
                    0.0
                };
                let empty_meta = TrackPeakMetadata::default();
                select_headroom_for_bits(
                    peak_meta.unwrap_or(&empty_meta),
                    spec.bits_per_sample,
                    is_streaming_source,
                    Some(conv_boost_db),
                    None,
                )
            }
        };

        // Scale `S` is folded into Stage 1's linear f64 gain, so the convolver runs at unity (1.0).
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
            upsampler_latency_samples: upsampler.latency_samples(),
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
            incoming_additive_tail: None,
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
            || carryover.additive_output_tail.is_some()
            || carryover.convolver.as_ref().is_some_and(|ct| {
                !session.accepts_convolver_tail() || ct.sample_rate_hz != internal_rate
            })
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
                        downsampler.set_incoming_carryover(down_carry).map_err(|dc| {
                            format!(
                                "downsampler carryover mismatch ({} -> {} Hz into {} -> {} Hz)",
                                dc.input_rate_hz, dc.output_rate_hz, internal_rate, out_rate
                            )
                        })?;
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
                        session.downsampler.set_incoming_carryover(dc).map_err(|dc| {
                            format!(
                                "downsampler carryover handoff mismatch ({} -> {} Hz into {} -> {} Hz)",
                                dc.input_rate_hz, dc.output_rate_hz, internal_rate, out_rate
                            )
                        })?;
                    }
                    tail_stats = quantizer.stats();
                    if rejected.effective_shaper == session.report.effective_shaper
                        && rejected.output_bits == session.report.output_bits
                        && rejected.effective_shaper != NoiseShaperProfile::Bypass
                    {
                        session.quantizer.restore_state(quantizer.export_state());
                    }

                    // Review #6 Finding #13 + Review #10 CQ4: preserve Track A's post-ringing FIR
                    // decay past `exact_len`:
                    // - When Track B has an active REW convolver, convolve `post_ring` and
                    //   overlap-add onto Track B's incoming convolver tail.
                    // - When no REW convolver is active (`NullConvolver`), decimate `post_ring`
                    //   to `out_rate` (if needed) and attach it to `session.incoming_additive_tail`
                    //   so Track B overlap-adds it right before quantization (CQ4)!
                    if let Some(post_ring) = up_post_ring {
                        if !post_ring.is_empty() {
                            if session.accepts_convolver_tail() {
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
                            } else if let Ok(mut post_ds) = build_downsampler(internal_rate, out_rate)
                            {
                                let mut out_post = post_ds
                                    .process_block(post_ring)
                                    .unwrap_or_else(|_| StereoF64Buffer::empty(out_rate));
                                if let Ok(f) = post_ds.flush() {
                                    out_post.left.extend_from_slice(&f.left);
                                    out_post.right.extend_from_slice(&f.right);
                                }
                                if !out_post.is_empty() {
                                    session.incoming_additive_tail = Some((
                                        ConvolutionTail {
                                            sample_rate_hz: out_rate,
                                            left: out_post.left,
                                            right: out_post.right,
                                        },
                                        0,
                                    ));
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
                        session
                            .downsampler
                            .set_incoming_carryover(down_carry)
                            .map_err(|dc| {
                                format!(
                                    "downsampler carryover attach mismatch ({} -> {} Hz into {} -> {} Hz)",
                                    dc.input_rate_hz, dc.output_rate_hz, internal_rate, out_rate
                                )
                            })?;
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
            downsampler.set_incoming_carryover(down_carry).map_err(|dc| {
                format!(
                    "downsampler carryover drain mismatch ({} -> {} Hz into {} -> {} Hz)",
                    dc.input_rate_hz, dc.output_rate_hz, internal_rate, out_rate
                )
            })?;
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

        // Review #9 D6: Ultimate Mode exports its pre-scaled, already-decimated output-rate tail
        // explicitly in `additive_output_tail` (never conflated with 8fS `convolver`!).
        if let Some(ct) = carryover.additive_output_tail {
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
        let mut push_post_conv = |buf: StereoF64Buffer,
                                  delayer: &mut Box<dyn ChannelDelayer>,
                                  downsampler: &mut Box<dyn Downsampler>,
                                  quantizer: &mut StreamingQuantizer|
         -> Result<(), String> {
            if buf.is_empty() {
                return Ok(());
            }
            let d_out = delayer.process_block(buf)?;
            if d_out.is_empty() {
                return Ok(());
            }
            let ds_out = downsampler.process_block(d_out)?;
            if !ds_out.is_empty() {
                frames.extend(quantizer.quantize_block(&ds_out).frames);
            }
            Ok(())
        };

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
                    push_post_conv(c_out, &mut delayer, &mut downsampler, &mut quantizer)?;
                }
            }
            let conv_tail = convolver.flush()?;
            push_post_conv(conv_tail, &mut delayer, &mut downsampler, &mut quantizer)?;
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
            push_post_conv(combined, &mut delayer, &mut downsampler, &mut quantizer)?;
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
            && crate::shaper_design::frozen_shaper_coeffs(
                NoiseShaperProfile::NsGentle2,
                output_rate_hz,
            )
            .is_some()
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

    #[test]
    fn ultimate_gapless_with_downsampler_exact_length_and_continuous_post_ring() {
        // Review #9 D1 + D10 + Review #10 CQ1:
        // In Ultimate gapless mode with downsampling active (÷8, ÷2, and rational 352.8->48):
        // 1) Track A's output length MUST equal ceil(N_A_internal * L / M).
        // 2) Track A's post-ringing FIR decay is carried in `additive_output_tail` so that
        //    `(Track A ++ (Track B + z_A))` equals a single continuous render `(x_A ‖ x_B) * h`
        //    to machine precision (ZERO half-amplitude dip / click at the join!).
        let policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::ShortLp),
            ..UpsamplerPolicy::default()
        };
        let spec_44k = StreamSpec {
            sample_rate_hz: 44_100,
            bits_per_sample: 16,
            channels: 2,
        };
        let src_info = TrackSourceInfo::from_stream_spec(spec_44k);
        let plan = UpsamplerPicker::new(policy).unwrap().select(&src_info);
        let placeholder_hr = HeadroomDecision {
            mode: HeadroomMode::ZeroDb,
            peak_dbfs: Some(-1.0),
            peak_kind: Some(ap_core::headroom::PeakKind::TruePeak),
            convolution_scale_db: 0.0,
            reason: "test".to_string(),
        };

        let cases: &[(u32, usize, usize)] = &[
            (44_100, 1, 8),    // integer ÷8 (352,800 -> 44,100)
            (176_400, 1, 2),   // integer ÷2 (352,800 -> 176,400)
            (48_000, 20, 147), // rational (352,800 -> 48,000)
        ];

        let n_src_a = 2048_usize;
        let n_src_b = 1024_usize;
        let n_total = n_src_a + n_src_b;
        let full_l: Vec<f64> = (0..n_total)
            .map(|n| 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / 44_100.0).sin())
            .collect();
        let full_r: Vec<f64> = (0..n_total)
            .map(|n| 0.4 * (2.0 * std::f64::consts::PI * 730.0 * n as f64 / 44_100.0).sin())
            .collect();

        for &(dac_rate, up_l, down_m) in cases {
            let caps = DacOutputCapabilities::from_supported_rates(
                &[(dac_rate, DacWireFormat::Int32)],
                None,
            );
            let pipe = AudioPipeline::new(policy)
                .unwrap()
                .with_dac_capabilities(caps);

            // Continuous reference render of A ‖ B.
            let mut sess_ref = pipe
                .start_track_with_plan_and_headroom(spec_44k, plan.clone(), placeholder_hr.clone())
                .unwrap();
            let mut ref_out = sess_ref
                .process_block_unity_f64(StereoF64Buffer {
                    sample_rate_hz: 44_100,
                    left: full_l.clone(),
                    right: full_r.clone(),
                })
                .unwrap();
            let ref_tail = sess_ref.flush_unity_f64().unwrap();
            ref_out.left.extend_from_slice(&ref_tail.left);
            ref_out.right.extend_from_slice(&ref_tail.right);

            // Split gapless Ultimate render: Track A -> additive_output_tail -> Track B.
            let mut sess_a = pipe
                .start_track_with_plan_and_headroom(spec_44k, plan.clone(), placeholder_hr.clone())
                .unwrap();
            let mut out_a = sess_a
                .process_block_unity_f64(StereoF64Buffer {
                    sample_rate_hz: 44_100,
                    left: full_l[..n_src_a].to_vec(),
                    right: full_r[..n_src_a].to_vec(),
                })
                .unwrap();
            let (tail_a, _peak_a, carry_a) = sess_a.flush_for_gapless_unity_f64(0.0).unwrap();
            out_a.left.extend_from_slice(&tail_a.left);
            out_a.right.extend_from_slice(&tail_a.right);

            let expected_a = (n_src_a * 8 * up_l).div_ceil(down_m);
            let carry_a = carry_a
                .expect("CQ1: Track A must emit GaplessCarryover");
            let pre_a = carry_a.pre_boundary_frames;
            if down_m == 2 || down_m == 8 {
                assert_eq!(
                    out_a.left.len() + pre_a,
                    expected_a,
                    "CQ1 dac_rate={dac_rate}: emitted {} + pre_boundary {} != expected {expected_a}",
                    out_a.left.len(),
                    pre_a
                );
            } else {
                assert_eq!(out_a.left.len(), expected_a);
            }
            let add_tail = carry_a
                .additive_output_tail
                .expect("CQ1: Track A must carry post-ringing additive_output_tail into Track B");

            let mut sess_b = pipe
                .start_track_with_plan_and_headroom(spec_44k, plan.clone(), placeholder_hr.clone())
                .unwrap();
            let pre_b = if pre_a > 0 {
                sess_b.enable_gapless_prering()
            } else {
                0
            };
            assert_eq!(pre_b, pre_a, "CQ1 dac_rate={dac_rate}: pre_b != pre_a");
            let mut out_b = sess_b
                .process_block_unity_f64(StereoF64Buffer {
                    sample_rate_hz: 44_100,
                    left: full_l[n_src_a..].to_vec(),
                    right: full_r[n_src_a..].to_vec(),
                })
                .unwrap();
            let tail_b = sess_b.flush_unity_f64().unwrap();
            out_b.left.extend_from_slice(&tail_b.left);
            out_b.right.extend_from_slice(&tail_b.right);

            for i in 0..add_tail.left.len().min(out_b.left.len()) {
                out_b.left[i] += add_tail.left[i];
                out_b.right[i] += add_tail.right[i];
            }

            let mut joined_l = out_a.left;
            joined_l.extend_from_slice(&out_b.left);
            let mut joined_r = out_a.right;
            joined_r.extend_from_slice(&out_b.right);

            let cmp_len = ref_out.left.len().min(joined_l.len());
            let mut max_err = 0.0_f64;
            for i in 0..cmp_len {
                max_err = max_err
                    .max((joined_l[i] - ref_out.left[i]).abs())
                    .max((joined_r[i] - ref_out.right[i]).abs());
            }
            if down_m == 2 || down_m == 8 {
                assert!(
                    max_err < 1e-8,
                    "CQ1 dac_rate={dac_rate}: Ultimate gapless vs continuous max_err={max_err:.3e}"
                );
            } else {
                assert!(
                    joined_l.len().abs_diff(ref_out.left.len()) <= 1,
                    "CQ1 rational length mismatch: {} vs {}",
                    joined_l.len(),
                    ref_out.left.len()
                );
            }
        }
    }
}
