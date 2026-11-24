//! TDD end-to-end tests for the tidy 5-stage `AudioPipeline` (with null convolver & delay).

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomMode, TrackPeakMetadata};
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{
    FilterArchetype, NoiseShaperProfile, UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsamplerPolicy,
};

#[test]
fn pipeline_runs_all_stages_in_order_for_cd_44100_to_352800_f64() {
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let spec = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let n_in = 512;
    let audio = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: vec![0.5; n_in],
        right: vec![-0.5; n_in],
    };

    // Untagged track -> fail-safe -3 dB headroom, 8x simple-test -> 352,800 Hz, null conv/delay
    let processed = pipeline
        .process(audio, spec, &TrackPeakMetadata::default(), false)
        .expect("pipeline failed");

    assert_eq!(processed.pcm.sample_rate_hz, 352_800);
    assert_eq!(processed.pcm.frames.len(), n_in * 8);
    assert_eq!(processed.report.headroom.mode, HeadroomMode::Minus3Db);
    assert_eq!(processed.report.dsp_plan.filter, FilterArchetype::SimpleTest);
    assert_eq!(processed.report.convolver_name, "null-convolver");
    assert_eq!(processed.report.delayer_name, "null-delay");
    assert_eq!(processed.report.effective_shaper, NoiseShaperProfile::TpdfDither);
    assert_eq!(processed.report.output_bits, 24);

    // Verify exact interior PCM values (~ 0.5 * 10^(-3/20) * 2^23 << 8, within +/- 1 LSB TPDF).
    let expected_l_24 = (0.5_f64 * 10.0_f64.powf(-3.0 / 20.0) * (1_u64 << 23) as f64).round() as i64;
    let expected_r_24 = -expected_l_24;
    let margin = 136 * 8;
    for (idx, &[l32, r32]) in processed.pcm.frames[margin..(n_in * 8 - margin)]
        .iter()
        .enumerate()
    {
        // Lower 8 bits of 24-in-32 MSB-aligned sample must be zero.
        assert_eq!(l32 & 0xFF, 0, "non-zero LSB padding L at {idx}");
        assert_eq!(r32 & 0xFF, 0, "non-zero LSB padding R at {idx}");
        let got_l_24 = i64::from(l32 >> 8);
        let got_r_24 = i64::from(r32 >> 8);
        assert!(
            (got_l_24 - expected_l_24).abs() <= 1,
            "interior L mismatch at {idx}: got {got_l_24}, expected {expected_l_24} +/- 1"
        );
        assert!(
            (got_r_24 - expected_r_24).abs() <= 1,
            "interior R mismatch at {idx}: got {got_r_24}, expected {expected_r_24} +/- 1"
        );
    }
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

    // Exact bit-for-bit verification: 0.25 * 2^31 = 536_870_912
    let expected_l = 1_i32 << 29;
    let expected_r = -(1_i32 << 29);
    for (idx, &[l, r]) in processed.pcm.frames.iter().enumerate() {
        assert_eq!(l, expected_l, "32-bit native L mismatch at {idx}");
        assert_eq!(r, expected_r, "32-bit native R mismatch at {idx}");
    }
}

#[test]
fn pipeline_native_24bit_and_16bit_untouched_paths_are_bit_perfect_without_dither() {
    let quiet_meta = TrackPeakMetadata {
        replaygain_track_peak: Some(0.5), // -6 dBFS -> 0 dB
        ..TrackPeakMetadata::default()
    };

    // 1. Native 24-bit / 352.8 kHz track -> auto Bypass, bit-exact `s24 << 8` on every sample!
    let auto_pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let spec_24 = StreamSpec {
        sample_rate_hz: 352_800,
        bits_per_sample: 24,
        channels: 2,
    };
    let raw_l_24: Vec<i32> = (-128..128).map(|k| k * 32_767 + 17).collect();
    let raw_r_24: Vec<i32> = (-128..128).map(|k| -(k * 29_123) - 9).collect();
    let scale_24 = 1.0_f64 / ((1_u64 << 23) as f64);
    let audio_24 = StereoF64Buffer {
        sample_rate_hz: 352_800,
        left: raw_l_24.iter().map(|&s| (s as f64) * scale_24).collect(),
        right: raw_r_24.iter().map(|&s| (s as f64) * scale_24).collect(),
    };
    let out_24 = auto_pipeline
        .process(audio_24, spec_24, &quiet_meta, false)
        .expect("24-bit native pipeline failed");
    assert_eq!(out_24.report.headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(out_24.report.effective_shaper, NoiseShaperProfile::Bypass);
    assert_eq!(out_24.report.output_bits, 24);
    for (i, &[l, r]) in out_24.pcm.frames.iter().enumerate() {
        assert_eq!(l, raw_l_24[i] << 8, "24-bit native L not bit-perfect at {i}");
        assert_eq!(r, raw_r_24[i] << 8, "24-bit native R not bit-perfect at {i}");
    }

    // 2. Native 16-bit / 44.1 kHz with `--filter null` -> auto Bypass, bit-exact `s16 << 16`!
    let null_pipeline = AudioPipeline::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::Null),
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let spec_16 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let raw_l_16: Vec<i32> = (-128..128).map(|k| k * 123 + 5).collect();
    let raw_r_16: Vec<i32> = (-128..128).map(|k| -(k * 111) - 3).collect();
    let scale_16 = 1.0_f64 / ((1_u64 << 15) as f64);
    let audio_16 = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: raw_l_16.iter().map(|&s| (s as f64) * scale_16).collect(),
        right: raw_r_16.iter().map(|&s| (s as f64) * scale_16).collect(),
    };
    let out_16 = null_pipeline
        .process(audio_16, spec_16, &quiet_meta, false)
        .expect("16-bit null-filter pipeline failed");
    assert_eq!(out_16.report.headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(out_16.report.effective_shaper, NoiseShaperProfile::Bypass);
    assert_eq!(out_16.report.output_bits, 16);
    for (i, &[l, r]) in out_16.pcm.frames.iter().enumerate() {
        assert_eq!(l, raw_l_16[i] << 16, "16-bit null L not bit-perfect at {i}");
        assert_eq!(r, raw_r_16[i] << 16, "16-bit null R not bit-perfect at {i}");
    }
}

#[test]
fn pipeline_adds_upsampler_isp_safety_margin_on_0_db_headroom_selection() {
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();

    // Sample peak right at -3.00 dBFS (0.7079):
    // - When upsampling (44.1 -> 352.8 kHz), the +0.5 dB ISP/Gibbs safety margin requires <= -3.5 dBFS
    //   so this track cleanly drops back to hardcoded -3 dB!
    let at_minus_3 = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-3.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let cd_spec = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let s_cd_hot = pipeline.start_track(cd_spec, &at_minus_3, false).unwrap();
    assert_eq!(s_cd_hot.report().headroom.mode, HeadroomMode::Minus3Db);

    // - A quieter track at -3.6 dBFS clears the -3.5 dBFS upsampling ceiling -> 0 dB.
    let at_minus_3_6 = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-3.6 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let s_cd_quiet = pipeline.start_track(cd_spec, &at_minus_3_6, false).unwrap();
    assert_eq!(s_cd_quiet.report().headroom.mode, HeadroomMode::ZeroDb);

    // - Native 384 kHz (no upsampling) at -3.00 dBFS has no upsampler ISP reconstruction -> 0 dB.
    let native_spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let s_native = pipeline
        .start_track(native_spec, &at_minus_3, false)
        .unwrap();
    assert_eq!(s_native.report().headroom.mode, HeadroomMode::ZeroDb);
}

#[test]
fn pipeline_rejects_unimplemented_noise_shapers_and_unsupported_rates() {
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    for shaper in [
        NoiseShaperProfile::Ns5Multibit,
        NoiseShaperProfile::Lns15Smooth,
        NoiseShaperProfile::PggbOptimal,
    ] {
        let p = AudioPipeline::new(UpsamplerPolicy {
            shaper_override: Some(shaper),
            ..UpsamplerPolicy::default()
        })
        .unwrap();
        let err = p
            .start_track(spec_44, &TrackPeakMetadata::default(), false)
            .err()
            .expect("expected unimplemented noise shaper error");
        assert!(
            err.contains("not implemented yet"),
            "unexpected err for {shaper}: {err}"
        );
    }

    let p_default = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let dxd_spec = StreamSpec {
        sample_rate_hz: 705_600,
        bits_per_sample: 24,
        channels: 2,
    };
    let err_dxd = p_default
        .start_track(dxd_spec, &TrackPeakMetadata::default(), false)
        .err()
        .expect("expected DXD 705.6k unsupported-rate error");
    assert!(err_dxd.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX));
}

#[test]
fn streaming_pipeline_session_matches_oneshot_bit_for_bit_across_blocks() {
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let spec = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let n = 600;
    let left: Vec<f64> = (0..n).map(|i| 0.4 * ((i as f64) * 0.11).sin()).collect();
    let right: Vec<f64> = (0..n).map(|i| 0.3 * ((i as f64) * 0.17).cos()).collect();
    let meta = TrackPeakMetadata::default();

    let oneshot = pipeline
        .process(
            StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: left.clone(),
                right: right.clone(),
            },
            spec,
            &meta,
            false,
        )
        .unwrap();

    let mut session = pipeline.start_track(spec, &meta, false).unwrap();
    let mut streamed_frames = Vec::new();
    let mut pos = 0;
    while pos < n {
        let end = (pos + 113).min(n);
        let blk = session
            .process_block(StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: left[pos..end].to_vec(),
                right: right[pos..end].to_vec(),
            })
            .unwrap();
        streamed_frames.extend(blk.frames);
        pos = end;
    }
    streamed_frames.extend(session.flush().unwrap().frames);

    assert_eq!(streamed_frames, oneshot.pcm.frames);
}
