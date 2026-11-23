//! TDD end-to-end tests for the tidy 5-stage `AudioPipeline` (with null convolver & delay).

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomMode, TrackPeakMetadata};
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};

#[test]
fn pipeline_runs_all_stages_in_order_for_cd_44100_to_352800_f64() {
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let spec = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let audio = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: vec![0.5; 256],
        right: vec![-0.5; 256],
    };

    // Untagged track -> fail-safe -3 dB headroom, 8x simple-test -> 352,800 Hz, null conv/delay
    let processed = pipeline
        .process(audio, spec, &TrackPeakMetadata::default(), false)
        .expect("pipeline failed");

    assert_eq!(processed.pcm.sample_rate_hz, 352_800);
    assert_eq!(processed.pcm.frames.len(), 256 * 8);
    assert_eq!(processed.report.headroom.mode, HeadroomMode::Minus3Db);
    assert_eq!(processed.report.dsp_plan.filter, FilterArchetype::SimpleTest);
    assert_eq!(processed.report.convolver_name, "null-convolver");
    assert_eq!(processed.report.delayer_name, "null-delay");
    assert_eq!(processed.report.effective_shaper, NoiseShaperProfile::TpdfDither);
}

#[test]
fn pipeline_runs_native_384khz_quiet_track_through_null_stages_at_0_db() {
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 32,
        channels: 2,
    };
    let audio = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: vec![0.25; 128],
        right: vec![-0.25; 128],
    };
    let quiet_meta = TrackPeakMetadata {
        replaygain_track_peak: Some(0.5), // -6 dBFS -> 0 dB headroom
        ..TrackPeakMetadata::default()
    };

    let processed = pipeline
        .process(audio, spec, &quiet_meta, false)
        .expect("pipeline failed");

    assert_eq!(processed.pcm.sample_rate_hz, 384_000);
    assert_eq!(processed.pcm.frames.len(), 128);
    assert_eq!(processed.report.headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(processed.report.dsp_plan.filter, FilterArchetype::Null);
    assert_eq!(processed.report.convolver_name, "null-convolver");
    assert_eq!(processed.report.delayer_name, "null-delay");
    assert_eq!(processed.report.effective_shaper, NoiseShaperProfile::Bypass);
    assert_eq!(processed.report.output_bits, 32);
}
