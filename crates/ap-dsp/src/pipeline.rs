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
use crate::upsampler::{Upsampler, build_upsampler};

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

    /// Flush end-of-track audio for gapless album continuity: emits the exact remaining samples
    /// of the current track and returns the decaying REW convolution tail separately so the next
    /// equal-rate track can overlap-add it without inserting a multi-second silence gap.
    pub fn flush_for_gapless(
        &mut self,
    ) -> Result<(InterleavedInt32Buffer, Option<ConvolutionTail>), String> {
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

        // 2. Complete the current track's exact sample count and split off the decaying room tail.
        let (track_end, carried_tail) = self.convolver.flush_split_tail()?;
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

        Ok((
            InterleavedInt32Buffer {
                sample_rate_hz: rate,
                frames,
            },
            carried_tail,
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

        let upsampler_isp_margin_db = if dsp_plan.filter != FilterArchetype::Null {
            UPSAMPLER_ISP_SAFETY_MARGIN_DB
        } else {
            0.0
        };
        let headroom = select_headroom(
            peak_meta,
            is_streaming_source,
            convolver.peak_gain_bound_db() + upsampler_isp_margin_db,
        );

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
