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
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomDecision, HeadroomMode, TrackPeakMetadata, select_headroom};

use crate::convolution::{
    ConvolutionTail, NullConvolverFactory, RewConvolverFactory, StereoConvolver,
    StereoConvolverFactory,
};
use crate::delay::{ChannelDelayer, ChannelDelayerFactory, NullChannelDelayerFactory};
use crate::quantize::{StreamingQuantizer, apply_safe_gain_f64};
use crate::selector::{
    FilterArchetype, MAX_SUPPORTED_SOURCE_RATE_HZ, MIN_SUPPORTED_SOURCE_RATE_HZ,
    NoiseShaperProfile, SelectedDspPlan, TrackSourceInfo, UNSUPPORTED_SAMPLE_RATE_PREFIX,
    UpsamplerPicker, UpsamplerPolicy,
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
    pub output_rate_hz: u32,
    pub output_bits: u8,
    pub effective_shaper: NoiseShaperProfile,
    pub upsampler: Option<UpsamplerCarryover>,
    pub convolver: Option<ConvolutionTail>,
}

impl GaplessCarryover {
    pub fn is_empty(&self) -> bool {
        self.upsampler.is_none() && self.convolver.is_none()
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
    /// was flushed into `upsampler_tail_pcm` (to close Track A), and the resulting REW convolution
    /// tail was seamlessly attached to Track B without inserting a multi-second IR drain gap.
    ConvolverOnlyHandoff {
        upsampler_tail_pcm: InterleavedInt32Buffer,
    },
    /// Output sample rate changed (e.g. `352.8k -> 384k`): nothing was attached to Track B and the
    /// untouched carryover is returned so the caller can drain it completely onto Track A.
    IncompatibleRate(GaplessCarryover),
}

/// Extra safety margin (dB) required on ReplayGain sample peak for `0 dB` headroom whenever the
/// steep windowed-sinc upsampler is active, protecting against inter-sample peaks (`~+3 dB` on
/// `fs/4` signals) plus Gibbs ringing overshoot and `+1 LSB` TPDF dither (`-3.5 dBFS` ceiling).
pub const UPSAMPLER_ISP_SAFETY_MARGIN_DB: f64 = 0.5;

/// Diagnostic summary of every pipeline stage chosen for a track/stream.
#[derive(Debug, Clone)]
pub struct PipelineReport {
    pub source_spec: StreamSpec,
    pub output_rate_hz: u32,
    pub output_bits: u8,
    pub headroom: HeadroomDecision,
    pub dsp_plan: SelectedDspPlan,
    pub convolver_name: &'static str,
    pub convolver_description: String,
    pub delayer_name: &'static str,
    pub effective_shaper: NoiseShaperProfile,
}

impl PipelineReport {
    pub fn format_summary(&self, total_duration_s: Option<f64>) -> String {
        let dur_part = match total_duration_s {
            Some(s) => format!("{s:.1} s"),
            None => "stream".to_string(),
        };
        format!(
            "{}/{} Hz -> {} Hz f64 ({}, {}, {}, {}, {}) | WDM-KS Int32",
            self.source_spec.bits_per_sample,
            self.source_spec.sample_rate_hz,
            self.output_rate_hz,
            self.dsp_plan.filter,
            self.headroom.mode,
            self.convolver_description,
            self.delayer_name,
            dur_part,
        )
    }

    pub fn log_line(&self) -> String {
        format!(
            "{} | effective_shaper={} ({}-bit) | {} | conv={} | delay={}",
            self.dsp_plan.summary(),
            self.effective_shaper,
            self.output_bits,
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
    convolver: Box<dyn StereoConvolver>,
    delayer: Box<dyn ChannelDelayer>,
    quantizer: StreamingQuantizer,
}

impl StreamingPipelineSession {
    pub fn report(&self) -> &PipelineReport {
        &self.report
    }

    /// Process one bounded source `f64` block through all 5 stages -> ready `i32` PCM block.
    pub fn process_block(
        &mut self,
        mut block: StereoF64Buffer,
    ) -> Result<InterleavedInt32Buffer, String> {
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }

        // Stage 1: Hardcoded -3 dB / 0 dB headroom in f64.
        apply_safe_gain_f64(&mut block, self.report.headroom.mode.gain_db())?;

        // Stage 2: Dynamic per-track stateful f64 upsampler (or zero-copy NullUpsampler).
        let block = self.upsampler.process_block(block)?;
        if block.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(self.report.output_rate_hz));
        }

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

        // Stage 5: Single final f64 -> i32 quantization.
        Ok(self.quantizer.quantize_block(&block))
    }

    /// Attach a residual convolution tail carried over from the preceding equal-rate track so it
    /// overlap-adds smoothly onto this track's opening samples in `f64` (before delay & quantizer).
    pub fn set_incoming_convolver_tail(&mut self, tail: ConvolutionTail) {
        self.convolver.set_incoming_tail(tail);
    }

    /// Attach full gapless carryover (unpadded upsampler FIR history + REW convolution tail) from
    /// the preceding track.
    ///
    /// Returns `Ok(())` when the carryover's output rate and upsampler geometry match this track
    /// (so both FIR interpolation and REW convolution continue seamlessly across the boundary).
    /// Returns `Err(carryover)` untouched when incompatible (e.g. sample-rate or ratio change),
    /// allowing the caller to flush the previous track's tail via
    /// [`AudioPipeline::drain_gapless_carryover`] before starting this track.
    pub fn set_incoming_gapless_carryover(
        &mut self,
        mut carryover: GaplessCarryover,
    ) -> Result<(), GaplessCarryover> {
        if carryover.output_rate_hz != self.report.output_rate_hz {
            return Err(carryover);
        }
        if let Some(up) = carryover.upsampler.take() {
            if let Err(rejected_up) = self.upsampler.set_incoming_carryover(up) {
                carryover.upsampler = Some(rejected_up);
                return Err(carryover);
            }
        }
        if let Some(conv) = carryover.convolver.take() {
            self.convolver.set_incoming_tail(conv);
        }
        Ok(())
    }

    /// Flush all stateful FIR / convolution / delay tails cleanly at end-of-track (emits the full
    /// decaying convolution tail — ideal for single-file playback and end-of-queue).
    pub fn flush(&mut self) -> Result<InterleavedInt32Buffer, String> {
        let mut frames = Vec::new();
        let rate = self.report.output_rate_hz;

        // 1. Flush upsampler tail -> feed through convolver -> delayer -> quantizer.
        let up_tail = self.upsampler.flush()?;
        if !up_tail.is_empty() {
            let c_out = self.convolver.process_block(up_tail)?;
            if !c_out.is_empty() {
                let d_out = self.delayer.process_block(c_out)?;
                if !d_out.is_empty() {
                    frames.extend(self.quantizer.quantize_block(&d_out).frames);
                }
            }
        }

        // 2. Flush convolver tail -> feed through delayer -> quantizer.
        let conv_tail = self.convolver.flush()?;
        if !conv_tail.is_empty() {
            let d_out = self.delayer.process_block(conv_tail)?;
            if !d_out.is_empty() {
                frames.extend(self.quantizer.quantize_block(&d_out).frames);
            }
        }

        // 3. Flush delayer tail -> quantizer.
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            frames.extend(self.quantizer.quantize_block(&delay_tail).frames);
        }

        Ok(InterleavedInt32Buffer {
            sample_rate_hz: rate,
            frames,
        })
    }

    /// Flush end-of-track audio for gapless album continuity: skips trailing zero-padding on the
    /// polyphase upsampler, emits the exact convolved frames ready so far, and returns the
    /// unpadded upsampler FIR history + decaying REW convolution tail as [`GaplessCarryover`] so
    /// the next equal-rate track continues with zero boundary dip, click, or silence gap.
    pub fn flush_for_gapless(
        &mut self,
    ) -> Result<(InterleavedInt32Buffer, Option<GaplessCarryover>), String> {
        let mut frames = Vec::new();
        let rate = self.report.output_rate_hz;

        // 1. Hand off unpadded upsampler FIR history (no trailing zero-pad for equal-rate gapless).
        let (up_tail, up_carry) = self.upsampler.flush_for_gapless()?;
        if !up_tail.is_empty() {
            let c_out = self.convolver.process_block(up_tail)?;
            if !c_out.is_empty() {
                let d_out = self.delayer.process_block(c_out)?;
                if !d_out.is_empty() {
                    frames.extend(self.quantizer.quantize_block(&d_out).frames);
                }
            }
        }

        // 2. Complete the already-fed sample count through the convolver and split off its tail.
        let (track_end, conv_tail) = self.convolver.flush_split_tail()?;
        if !track_end.is_empty() {
            let d_out = self.delayer.process_block(track_end)?;
            if !d_out.is_empty() {
                frames.extend(self.quantizer.quantize_block(&d_out).frames);
            }
        }

        // 3. Flush delayer tail -> quantizer.
        let delay_tail = self.delayer.flush()?;
        if !delay_tail.is_empty() {
            frames.extend(self.quantizer.quantize_block(&delay_tail).frames);
        }

        let carryover = if up_carry.is_some() || conv_tail.is_some() {
            Some(GaplessCarryover {
                output_rate_hz: rate,
                output_bits: self.report.output_bits,
                effective_shaper: self.report.effective_shaper,
                upsampler: up_carry,
                convolver: conv_tail,
            })
        } else {
            None
        };

        Ok((
            InterleavedInt32Buffer {
                sample_rate_hz: rate,
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
}

impl AudioPipeline {
    /// Construct the default pipeline with dynamic upsampler picking + null REW/delay stages.
    pub fn new(upsampler_policy: UpsamplerPolicy) -> Result<Self, String> {
        Ok(Self {
            picker: UpsamplerPicker::new(upsampler_policy)?,
            convolver_factory: Box::new(NullConvolverFactory),
            delayer_factory: Box::new(NullChannelDelayerFactory),
        })
    }

    /// Construct a pipeline with configured REW stereo convolution (`ConvolutionConfig`) + null delay.
    pub fn with_rew_config(
        upsampler_policy: UpsamplerPolicy,
        rew_config: ConvolutionConfig,
    ) -> Result<Self, String> {
        Self::with_factories(
            upsampler_policy,
            Box::new(RewConvolverFactory::new(rew_config)),
            Box::new(NullChannelDelayerFactory),
        )
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

    fn start_track_internal(
        &self,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
        incoming_tail_hint: Option<(u32, f64, f64, bool)>,
    ) -> Result<StreamingPipelineSession, String> {
        if !(MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ)
            .contains(&spec.sample_rate_hz)
        {
            return Err(format!(
                "{UNSUPPORTED_SAMPLE_RATE_PREFIX} {} Hz is outside the supported {}..={} Hz range",
                spec.sample_rate_hz, MIN_SUPPORTED_SOURCE_RATE_HZ, MAX_SUPPORTED_SOURCE_RATE_HZ
            ));
        }

        let mut source_info = TrackSourceInfo::from_stream_spec(spec);
        source_info.is_streaming_source = is_streaming_source;

        let dsp_plan = self.picker.select(&source_info);
        let upsampler = build_upsampler(&dsp_plan)?;
        let convolver = self
            .convolver_factory
            .create_for_rate(dsp_plan.target_rate_hz)?;
        let delayer = self
            .delayer_factory
            .create_for_rate(dsp_plan.target_rate_hz)?;

        // When a verified True Peak tag is present, inter-sample peaks are already measured
        // directly; otherwise add the conservative sample-peak ISP safety margin whenever a
        // non-null upsampler reconstructs inter-sample peaks.
        let upsampler_isp_margin_db =
            if dsp_plan.filter != FilterArchetype::Null && !peak_meta.has_verified_true_peak() {
                UPSAMPLER_ISP_SAFETY_MARGIN_DB
            } else {
                0.0
            };
        let mut headroom = select_headroom(
            peak_meta,
            is_streaming_source,
            convolver.peak_gain_bound_db() + upsampler_isp_margin_db,
        );

        // If an equal-rate decaying REW tail from the previous track will overlap-add onto this
        // track's opening (either via full handoff or convolver-only handoff), ensure
        // `next_track_peak * conv_and_isp_bound + tail_peak <= 1.0`; if `0 dB` would risk
        // exceeding full scale, safely fall back to hardcoded `-3 dB`.
        if headroom.mode == HeadroomMode::ZeroDb {
            if let Some((tail_rate_hz, conv_tail_peak, up_hist_peak, has_conv_tail)) =
                incoming_tail_hint
            {
                if tail_rate_hz == dsp_plan.target_rate_hz {
                    let conv_l1_lin = 10.0_f64.powf(convolver.peak_gain_bound_db() / 20.0);
                    let extra_up_reverb = if has_conv_tail || convolver.name() != "null-convolver" {
                        up_hist_peak * conv_l1_lin
                    } else {
                        0.0
                    };
                    let total_tail_peak = conv_tail_peak + extra_up_reverb;
                    if total_tail_peak.is_finite() && total_tail_peak > 1e-6 {
                        let track_peak_lin = peak_meta
                            .effective_peak_linear()
                            .map_or(1.0, |(p, _)| p);
                        let bound_lin = 10.0_f64.powf(
                            (convolver.peak_gain_bound_db() + upsampler_isp_margin_db) / 20.0,
                        );
                        let combined_peak = track_peak_lin * bound_lin + total_tail_peak;
                        if combined_peak > 1.0 {
                            headroom = HeadroomDecision {
                                mode: HeadroomMode::Minus3Db,
                                peak_dbfs: headroom.peak_dbfs,
                                reason: format!(
                                    "incoming gapless REW tail ({:.3}) + track peak bound ({:.3}) = {:.3} > 1.0 -> -3 dB",
                                    total_tail_peak,
                                    track_peak_lin * bound_lin,
                                    combined_peak
                                ),
                            };
                        }
                    }
                }
            }
        }

        let untouched_native = dsp_plan.filter == FilterArchetype::Null
            && headroom.mode == HeadroomMode::ZeroDb
            && convolver.name() == "null-convolver"
            && delayer.name() == "null-delay";

        let (effective_shaper, output_bits) = if untouched_native {
            let out_bits = (spec.bits_per_sample as u8).clamp(16, 32);
            // Truly bit-perfect native pass-through: never add TPDF dither when no DSP modified
            // the signal and output bit depth can represent every source bit losslessly.
            let shaper = if !dsp_plan.shaper_overridden
                && u16::from(out_bits) >= spec.bits_per_sample
            {
                NoiseShaperProfile::Bypass
            } else {
                dsp_plan.shaper
            };
            (shaper, out_bits)
        } else if dsp_plan.shaper == NoiseShaperProfile::Bypass {
            (NoiseShaperProfile::TpdfDither, 24)
        } else {
            (dsp_plan.shaper, 24)
        };

        let quantizer = StreamingQuantizer::new(effective_shaper, output_bits)?;

        let report = PipelineReport {
            source_spec: spec,
            output_rate_hz: dsp_plan.target_rate_hz,
            output_bits,
            headroom,
            dsp_plan,
            convolver_name: convolver.name(),
            convolver_description: convolver.description(),
            delayer_name: delayer.name(),
            effective_shaper,
        };

        Ok(StreamingPipelineSession {
            report,
            upsampler,
            convolver,
            delayer,
            quantizer,
        })
    }

    /// Attach a previous track's [`GaplessCarryover`] to `session`:
    /// - Same output rate AND compatible upsampler geometry -> [`GaplessAttachOutcome::FullHandoff`].
    /// - Same output rate with different upsampler geometry (e.g. `44.1k -> 88.2k` or `44.1k ->
    ///   native 352.8k`) -> drains ONLY the short upsampler FIR history into `upsampler_tail_pcm`
    ///   and attaches the updated REW [`ConvolutionTail`] onto `session`
    ///   ([`GaplessAttachOutcome::ConvolverOnlyHandoff`]), avoiding multi-second IR drain gaps.
    /// - Different output rate (e.g. `352.8k -> 384k`) -> returns
    ///   [`GaplessAttachOutcome::IncompatibleRate`] untouched so the caller can drain it completely.
    pub fn attach_gapless_carryover(
        &self,
        session: &mut StreamingPipelineSession,
        carryover: GaplessCarryover,
    ) -> Result<GaplessAttachOutcome, String> {
        let rate = session.report().output_rate_hz;
        if carryover.output_rate_hz != rate {
            return Ok(GaplessAttachOutcome::IncompatibleRate(carryover));
        }

        match session.set_incoming_gapless_carryover(carryover) {
            Ok(()) => Ok(GaplessAttachOutcome::FullHandoff),
            Err(mut rejected) => {
                let mut frames = Vec::new();
                if let Some(up_carry) = rejected.upsampler.take() {
                    let up_tail_f64 = up_carry.flush_to_f64();
                    let mut conv = self.convolver_factory.create_for_rate(rate)?;
                    if let Some(conv_tail) = rejected.convolver.take() {
                        conv.set_incoming_tail(conv_tail);
                    }
                    let mut delayer = self.delayer_factory.create_for_rate(rate)?;
                    let mut quantizer =
                        StreamingQuantizer::new(rejected.effective_shaper, rejected.output_bits)?;

                    if !up_tail_f64.is_empty() {
                        let c_out = conv.process_block(up_tail_f64)?;
                        if !c_out.is_empty() {
                            let d_out = delayer.process_block(c_out)?;
                            if !d_out.is_empty() {
                                frames.extend(quantizer.quantize_block(&d_out).frames);
                            }
                        }
                    }

                    let (conv_immediate, updated_tail) = conv.flush_split_tail()?;
                    if !conv_immediate.is_empty() {
                        let d_out = delayer.process_block(conv_immediate)?;
                        if !d_out.is_empty() {
                            frames.extend(quantizer.quantize_block(&d_out).frames);
                        }
                    }
                    let delay_tail = delayer.flush()?;
                    if !delay_tail.is_empty() {
                        frames.extend(quantizer.quantize_block(&delay_tail).frames);
                    }
                    if let Some(tail) = updated_tail {
                        session.set_incoming_convolver_tail(tail);
                    }
                } else if let Some(conv_tail) = rejected.convolver.take() {
                    session.set_incoming_convolver_tail(conv_tail);
                }

                Ok(GaplessAttachOutcome::ConvolverOnlyHandoff {
                    upsampler_tail_pcm: InterleavedInt32Buffer {
                        sample_rate_hz: rate,
                        frames,
                    },
                })
            }
        }
    }

    /// Drain an unconsumed [`GaplessCarryover`] (upsampler FIR tail + REW convolution tail)
    /// cleanly into integer PCM at its native `output_rate_hz`.
    ///
    /// Called at end-of-queue, before a sample-rate/ratio change, or when the next track fails
    /// to open so the end of the preceding track and its room reverb decay are never truncated.
    pub fn drain_gapless_carryover(
        &self,
        carryover: GaplessCarryover,
    ) -> Result<InterleavedInt32Buffer, String> {
        let rate = carryover.output_rate_hz;
        if carryover.is_empty() {
            return Ok(InterleavedInt32Buffer::empty(rate));
        }

        let mut convolver = self.convolver_factory.create_for_rate(rate)?;
        if let Some(conv_tail) = carryover.convolver {
            convolver.set_incoming_tail(conv_tail);
        }
        let mut delayer = self.delayer_factory.create_for_rate(rate)?;
        let mut quantizer =
            StreamingQuantizer::new(carryover.effective_shaper, carryover.output_bits)?;

        let mut frames = Vec::new();
        if let Some(up_carry) = carryover.upsampler {
            let up_tail = up_carry.flush_to_f64();
            if !up_tail.is_empty() {
                let c_out = convolver.process_block(up_tail)?;
                if !c_out.is_empty() {
                    let d_out = delayer.process_block(c_out)?;
                    if !d_out.is_empty() {
                        frames.extend(quantizer.quantize_block(&d_out).frames);
                    }
                }
            }
        }

        let conv_tail = convolver.flush()?;
        if !conv_tail.is_empty() {
            let d_out = delayer.process_block(conv_tail)?;
            if !d_out.is_empty() {
                frames.extend(quantizer.quantize_block(&d_out).frames);
            }
        }

        let delay_tail = delayer.flush()?;
        if !delay_tail.is_empty() {
            frames.extend(quantizer.quantize_block(&delay_tail).frames);
        }

        Ok(InterleavedInt32Buffer {
            sample_rate_hz: rate,
            frames,
        })
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
