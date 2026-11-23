//! Tidy, linear, end-to-end pure-`f64` audio processing pipeline.
//!
//! Exact processing order for every track:
//! 1. **Headroom Selection & Scaling (`-3 dB` or `0 dB`, hardcoded)** in `f64`.
//! 2. **Dynamic Per-Track Upsampling** (`SimpleWindowedSincUpsampler` or zero-copy
//!    `NullUpsampler` for native `352.8 / 384 kHz` tracks) -> always `StereoF64Buffer`.
//! 3. **Core REW Stereo Convolution** (`StereoConvolver`; currently [`NullConvolver`]).
//! 4. **Per-Channel Room Alignment Delay** (`ChannelDelayer`; currently [`NullChannelDelayer`]).
//! 5. **Single Final `f64 -> i32` Quantization** (TPDF dither or `Bypass` for untouched 32-bit).

use std::time::{Duration, Instant};

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomDecision, HeadroomMode, TrackPeakMetadata, select_headroom};

use crate::convolution::{NullConvolver, StereoConvolver};
use crate::delay::{ChannelDelayer, NullChannelDelayer};
use crate::quantize::{apply_safe_gain_f64, quantize_to_int32};
use crate::selector::{
    FilterArchetype, NoiseShaperProfile, SelectedDspPlan, TrackSourceInfo, UpsamplerPicker,
    UpsamplerPolicy,
};
use crate::upsampler::build_upsampler;

/// Diagnostic summary of every stage executed for one track.
#[derive(Debug, Clone)]
pub struct PipelineReport {
    pub source_spec: StreamSpec,
    pub output_rate_hz: u32,
    pub output_bits: u8,
    pub headroom: HeadroomDecision,
    pub dsp_plan: SelectedDspPlan,
    pub convolver_name: &'static str,
    pub delayer_name: &'static str,
    pub effective_shaper: NoiseShaperProfile,
    pub elapsed: Duration,
}

impl PipelineReport {
    pub fn format_summary(&self, duration_s: f64) -> String {
        format!(
            "{}/{} Hz -> {} Hz f64 ({}, {}, {}, {}, {:.1} s, DSP {:.0} ms) | WDM-KS Int32",
            self.source_spec.bits_per_sample,
            self.source_spec.sample_rate_hz,
            self.output_rate_hz,
            self.dsp_plan.filter,
            self.headroom.mode,
            self.convolver_name,
            self.delayer_name,
            duration_s,
            self.elapsed.as_secs_f64() * 1000.0,
        )
    }

    pub fn log_line(&self) -> String {
        format!(
            "{} | {} | conv={} | delay={}",
            self.dsp_plan.summary(),
            self.headroom.summary(),
            self.convolver_name,
            self.delayer_name,
        )
    }
}

/// Fully processed track sitting ready in RAM for hiccup-free WDM-KS output.
#[derive(Debug)]
pub struct ProcessedTrack {
    pub pcm: InterleavedInt32Buffer,
    pub report: PipelineReport,
}

/// Composable, pure-`f64` audiophile DSP pipeline.
///
/// Missing future stages (REW stereo convolution, per-channel delay) default cleanly to
/// zero-copy null implementations ([`NullConvolver`], [`NullChannelDelayer`]).
pub struct AudioPipeline {
    picker: UpsamplerPicker,
    convolver: Box<dyn StereoConvolver>,
    delayer: Box<dyn ChannelDelayer>,
}

impl AudioPipeline {
    /// Construct the default pipeline with dynamic upsampler picking + null REW/delay stages.
    pub fn new(upsampler_policy: UpsamplerPolicy) -> Result<Self, String> {
        Ok(Self {
            picker: UpsamplerPicker::new(upsampler_policy)?,
            convolver: Box::new(NullConvolver),
            delayer: Box::new(NullChannelDelayer),
        })
    }

    /// Construct a pipeline with explicit convolver and channel delayer implementations.
    pub fn with_stages(
        upsampler_policy: UpsamplerPolicy,
        convolver: Box<dyn StereoConvolver>,
        delayer: Box<dyn ChannelDelayer>,
    ) -> Result<Self, String> {
        Ok(Self {
            picker: UpsamplerPicker::new(upsampler_policy)?,
            convolver,
            delayer,
        })
    }

    /// Execute the full 5-stage floating-point pipeline on a decoded track buffer.
    pub fn process(
        &self,
        mut audio: StereoF64Buffer,
        spec: StreamSpec,
        peak_meta: &TrackPeakMetadata,
        is_streaming_source: bool,
    ) -> Result<ProcessedTrack, String> {
        let started = Instant::now();

        // Stage 1: Hardcoded -3 dB / 0 dB headroom selection & pure f64 scaling.
        let headroom = select_headroom(
            peak_meta,
            is_streaming_source,
            self.convolver.max_boost_db(),
        );
        apply_safe_gain_f64(&mut audio, headroom.mode.gain_db())?;

        // Stage 2: Dynamic per-track upsampler selection & f64 upsampling (or NullUpsampler).
        let mut source_info = TrackSourceInfo::from_stream_spec(spec);
        source_info.is_streaming_source = is_streaming_source;
        let dsp_plan = self.picker.select(&source_info);
        let upsampler = build_upsampler(&dsp_plan)?;
        let audio = upsampler.upsample(audio, &dsp_plan)?;

        // Stage 3: Core REW stereo convolution in f64 (currently NullConvolver).
        let audio = self.convolver.convolve(audio)?;

        // Stage 4: Optional per-channel room alignment delay in f64 (currently NullChannelDelayer).
        let audio = self.delayer.apply(audio)?;

        // Stage 5: Single final f64 -> i32 quantization for the DAC.
        let untouched_native = dsp_plan.filter == FilterArchetype::Null
            && headroom.mode == HeadroomMode::ZeroDb
            && self.convolver.name() == "null-convolver"
            && self.delayer.name() == "null-delay";

        let (effective_shaper, output_bits) = if untouched_native {
            (dsp_plan.shaper, (spec.bits_per_sample as u8).clamp(16, 32))
        } else if dsp_plan.shaper == NoiseShaperProfile::Bypass {
            (NoiseShaperProfile::TpdfDither, 24)
        } else {
            (dsp_plan.shaper, 24)
        };

        let output_rate_hz = audio.sample_rate_hz;
        let pcm = quantize_to_int32(&audio, effective_shaper, output_bits);

        let report = PipelineReport {
            source_spec: spec,
            output_rate_hz,
            output_bits,
            headroom,
            dsp_plan,
            convolver_name: self.convolver.name(),
            delayer_name: self.delayer.name(),
            effective_shaper,
            elapsed: started.elapsed(),
        };

        Ok(ProcessedTrack { pcm, report })
    }
}
