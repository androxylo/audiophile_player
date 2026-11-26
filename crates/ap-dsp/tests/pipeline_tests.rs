// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! TDD end-to-end tests for the tidy 5-stage `AudioPipeline` (with null convolver & delay).

use std::sync::Arc;

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomMode, TrackPeakMetadata};
use ap_dsp::convolution::{
    NullConvolverFactory, PartitionedStereoConvolver, PreparedStereoIr, StereoConvolver,
    StereoConvolverFactory,
};
use ap_dsp::delay::NullChannelDelayerFactory;
use ap_dsp::pipeline::{AudioPipeline, GaplessAttachOutcome};
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
    let n_in = 1024;
    let audio = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: vec![0.5; n_in],
        right: vec![-0.5; n_in],
    };

    // Untagged track -> fail-safe -3 dB headroom, 8x gauss-long-lp -> 352,800 Hz, null conv/delay
    let processed = pipeline
        .process(audio, spec, &TrackPeakMetadata::default(), false)
        .expect("pipeline failed");

    assert_eq!(processed.pcm.sample_rate_hz, 352_800);
    assert_eq!(processed.pcm.frames.len(), n_in * 8);
    assert_eq!(processed.report.headroom.mode, HeadroomMode::Minus3Db);
    assert_eq!(processed.report.dsp_plan.filter, FilterArchetype::GaussLongLp);
    assert!(processed.report.filter_metrics.is_some());
    assert!(processed.report.log_line().contains("filter_metrics="));
    assert_eq!(processed.report.convolver_name, "null-convolver");
    assert_eq!(processed.report.delayer_name, "null-delay");
    assert_eq!(processed.report.effective_shaper, NoiseShaperProfile::NsGentle2);
    assert_eq!(processed.report.output_bits, 24);

    // Verify exact interior PCM values (~ 0.5 * 10^(-3/20) * 2^23 << 8, within +/- 3 LSB for ns-gentle-2,
    // and mean recovering the exact fractional sub-LSB f64 level to < 0.01 LSB!).
    let expected_l_f64 = 0.5_f64 * 10.0_f64.powf(-3.0 / 20.0) * (1_u64 << 23) as f64;
    let expected_r_f64 = -expected_l_f64;
    let margin = 340 * 8;
    let interior = &processed.pcm.frames[margin..(n_in * 8 - margin)];
    let mut sum_err_l = 0.0_f64;
    let mut sum_err_r = 0.0_f64;
    for (idx, &[l32, r32]) in interior.iter().enumerate() {
        // Lower 8 bits of 24-in-32 MSB-aligned sample must be zero.
        assert_eq!(l32 & 0xFF, 0, "non-zero LSB padding L at {idx}");
        assert_eq!(r32 & 0xFF, 0, "non-zero LSB padding R at {idx}");
        let got_l_24 = f64::from(l32 >> 8);
        let got_r_24 = f64::from(r32 >> 8);
        let err_l = got_l_24 - expected_l_f64;
        let err_r = got_r_24 - expected_r_f64;
        sum_err_l += err_l;
        sum_err_r += err_r;
        assert!(
            err_l.abs() <= 3.0,
            "interior L mismatch at {idx}: got {got_l_24}, expected {expected_l_f64:.2} +/- 3"
        );
        assert!(
            err_r.abs() <= 3.0,
            "interior R mismatch at {idx}: got {got_r_24}, expected {expected_r_f64:.2} +/- 3"
        );
    }
    let mean_err_l = sum_err_l / (interior.len() as f64);
    let mean_err_r = sum_err_r / (interior.len() as f64);
    assert!(
        mean_err_l.abs() < 0.01 && mean_err_r.abs() < 0.01,
        "ns-gentle-2 sub-LSB DC bias too large: L={mean_err_l:.5}, R={mean_err_r:.5} LSB"
    );
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
fn pipeline_uses_exact_minus_3_dbfs_ceiling_without_extra_upsampler_margin() {
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();

    // Per HEADROOM_DESIGN.md (Addendum 1): sample peak <= -3.00 dBFS suffices for 0 dB upsampler
    // headroom (A_isp = 3 dB -> -3 + 3 = 0 dBFS; the old 0.5 dB margin is removed).
    let at_minus_3 = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-3.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let cd_spec = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let s_cd = pipeline.start_track(cd_spec, &at_minus_3, false).unwrap();
    assert_eq!(s_cd.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(s_cd.report().headroom.convolution_scale_db, 0.0);

    // Slightly above -3.0 dBFS (-2.9 dBFS) drops cleanly to -3 dB.
    let at_minus_2_9 = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-2.9 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let s_cd_hot = pipeline.start_track(cd_spec, &at_minus_2_9, false).unwrap();
    assert_eq!(s_cd_hot.report().headroom.mode, HeadroomMode::Minus3Db);

    // Native 384 kHz at -3.00 dBFS -> 0 dB & bit-perfect bypass.
    let native_spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let s_native = pipeline
        .start_track(native_spec, &at_minus_3, false)
        .unwrap();
    assert_eq!(s_native.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(s_native.report().effective_shaper, NoiseShaperProfile::Bypass);

    // Verified TRUE_PEAK at -3.00 dBTP -> 0 dB.
    let at_minus_3_tp = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(-3.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let s_cd_tp = pipeline.start_track(cd_spec, &at_minus_3_tp, false).unwrap();
    assert_eq!(s_cd_tp.report().headroom.mode, HeadroomMode::ZeroDb);
}

#[test]
fn incoming_gapless_rew_tail_demotes_active_convolver_only_and_preserves_null_convolver_bit_perfect() {
    // Review #4 Findings #12 & #13:
    // 1. With an active REW convolver (`accepts_tail() == true`), a hot incoming tail forces
    //    -3 dB, and `up_hist_peak * conv_gain` is included in `start_track_for_carryover`!
    // 2. With `NullConvolver` (`accepts_tail() == false`), a carried REW tail does NOT demote
    //    Track B (bit-perfect `ZeroDb` + `Bypass` preserved!), AND `attach_gapless_carryover`
    //    returns `IncompatibleRate` so `drain_gapless_carryover` drains A's tail cleanly instead
    //    of silently dropping it!
    let mut h = vec![0.0_f64; 256];
    h[0] = 0.5;
    h[128] = 0.5; // unity DC gain (max |H| = 1.0 = 0 dB)
    let ir_352 = Arc::new(PreparedStereoIr::from_stereo_ir("unit-352k", 352_800, &h, &h).unwrap());

    let rew_pipeline = AudioPipeline::with_factories(
        UpsamplerPolicy::default(),
        Box::new(FixedRateEchoConvolverFactory { ir_352 }),
        Box::new(NullChannelDelayerFactory),
    )
    .unwrap();

    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let spec_352 = StreamSpec {
        sample_rate_hz: 352_800,
        bits_per_sample: 24,
        channels: 2,
    };
    let spec_384 = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };

    // Quiet verified True Peak Track B (0.60 = -4.44 dBTP): qualifies for 0 dB on its own.
    let quiet_tp = TrackPeakMetadata {
        true_peak_linear: Some(0.60),
        ..TrackPeakMetadata::default()
    };

    // Small incoming tail (0.10): 0.60 + 0.10 = 0.70 <= 1.0 -> stays 0 dB, S_B = 0 dB.
    let s_small = rew_pipeline
        .start_track_with_tail_hint(spec_352, &quiet_tp, false, Some((352_800, 0.10)))
        .unwrap();
    assert_eq!(s_small.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(s_small.report().headroom.convolution_scale_db, 0.0);

    // Review #6 Finding #1: Loud incoming tail (0.50) must NOT attenuate all of Track B with a
    // whole-track S_B cut (which dropped Track B by up to -60 dB on normal-level gapless REW pairs).
    // Track B keeps its own independent G_up=0 dB and S=0 dB, relying on the quantizer backstop.
    let s_hot = rew_pipeline
        .start_track_with_tail_hint(spec_352, &quiet_tp, false, Some((352_800, 0.50)))
        .unwrap();
    assert_eq!(s_hot.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(
        s_hot.report().headroom.convolution_scale_db,
        0.0,
        "Track B must not be attenuated for incoming gapless tail (Review #6 #1)"
    );

    // Different-rate incoming tail (384k vs 352.8k) -> stays 0 dB, S = 0 dB.
    let s_other_rate = rew_pipeline
        .start_track_with_tail_hint(spec_384, &quiet_tp, false, Some((352_800, 0.50)))
        .unwrap();
    assert_eq!(s_other_rate.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(s_other_rate.report().headroom.convolution_scale_db, 0.0);

    // Exercise `start_track_for_carryover` with active REW + upsampler carryover:
    // Track A (44.1k) ends loud so carry has conv_peak (~0.25) + up_hist_peak (~0.55).
    // Track B MUST preserve its own ZeroDb + S=0 dB gain (never cutting all of Track B!).
    let mut sess_a = rew_pipeline
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .unwrap();
    let _ = sess_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.8; 512],
            right: vec![0.8; 512],
        })
        .unwrap();
    let (_, carry_a) = sess_a.flush_for_gapless().unwrap();
    let carry_a = carry_a.expect("expected carryover from 44.1k REW track");
    let s_b_rew = rew_pipeline
        .start_track_for_carryover(spec_44, &quiet_tp, false, Some(&carry_a))
        .unwrap();
    assert_eq!(s_b_rew.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(
        s_b_rew.report().headroom.convolution_scale_db,
        0.0,
        "same-rate active-convolver carry must not cut Track B's whole-track gain (Review #6 #1)"
    );

    // Null-convolver pipeline (e.g. REW filter removed before native 352.8k Track B starts):
    // MUST keep ZeroDb + bit-perfect Bypass, reject attach as IncompatibleRate, and drain Track A's
    // REW tail with non-zero reverb energy instead of silently discarding it!
    let null_pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let mut s_b_null = null_pipeline
        .start_track_for_carryover(spec_352, &quiet_tp, false, Some(&carry_a))
        .unwrap();
    assert_eq!(s_b_null.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(
        s_b_null.report().effective_shaper,
        NoiseShaperProfile::Bypass,
        "NullConvolver native Track B must stay bit-perfect untouched_native"
    );
    let outcome = null_pipeline
        .attach_gapless_carryover(&mut s_b_null, carry_a)
        .unwrap();
    let rejected = match outcome {
        GaplessAttachOutcome::IncompatibleRate(c) => c,
        other => panic!("NullConvolver must not silently drop carried REW tail, got {other:?}"),
    };
    let drained = null_pipeline.drain_gapless_carryover(rejected).unwrap();
    let drained_energy: i64 = drained
        .frames
        .iter()
        .map(|f| i64::from(f[0]).abs())
        .sum();
    assert!(
        drained_energy > 0,
        "drain_gapless_carryover must preserve pre-convolved REW tail even when factory returns NullConvolver"
    );
}

#[test]
fn pipeline_runs_all_inv04_noise_shapers_and_rejects_unsupported_rates() {
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    for shaper in [
        NoiseShaperProfile::NsGentle2,
        NoiseShaperProfile::NsSharp5,
        NoiseShaperProfile::NsSmooth15,
        NoiseShaperProfile::NsWide9,
    ] {
        let p = AudioPipeline::new(UpsamplerPolicy {
            shaper_override: Some(shaper),
            ..UpsamplerPolicy::default()
        })
        .unwrap();
        let session = p
            .start_track(spec_44, &TrackPeakMetadata::default(), false)
            .expect("all INV-04 noise shapers must start cleanly at 8fS");
        assert_eq!(session.report().effective_shaper, shaper);
        assert_eq!(session.report().output_rate_hz, 352_800);
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

#[test]
fn gapless_pipeline_sessions_eliminate_boundary_dip_and_drain_on_rate_change() {
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let spec_48 = StreamSpec {
        sample_rate_hz: 48_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let meta = TrackPeakMetadata::default();
    let n_a = 512_usize;
    let n_b = 512_usize;

    // 1. Two equal-rate gapless tracks carrying constant DC 0.5: boundary must be completely flat!
    let mut sess_a = pipeline.start_track(spec_44, &meta, false).unwrap();
    let mut combined = sess_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.5; n_a],
            right: vec![-0.5; n_a],
        })
        .unwrap()
        .frames;
    let (tail_a, carry_a) = sess_a.flush_for_gapless().unwrap();
    combined.extend(tail_a.frames);
    let carry_a = carry_a.expect("expected gapless carryover from 44.1 kHz upsampler");

    // Verify incompatible 48 kHz track rejects the 44.1 kHz carryover and drain_gapless_carryover
    // recovers Track A's full duration PLUS full zero-input FIR post-ringing decay (Review #6 #13).
    let post_ring_a = carry_a
        .upsampler
        .as_ref()
        .map_or(0, |u| u.taps_per_phase.saturating_sub(1).saturating_sub(u.pad_tail) * u.ratio);
    let mut sess_48 = pipeline.start_track(spec_48, &meta, false).unwrap();
    let rejected = sess_48
        .set_incoming_gapless_carryover(carry_a.clone())
        .expect_err("48 kHz session must reject 44.1 kHz carryover");
    let drained_a = pipeline.drain_gapless_carryover(rejected).unwrap();
    assert_eq!(drained_a.sample_rate_hz, 352_800);
    assert_eq!(
        combined.len() + drained_a.frames.len(),
        n_a * 8 + post_ring_a,
        "draining unconsumed carryover must restore Track A's full length + FIR post-ring decay"
    );

    // Now test compatible 44.1 kHz Track B accepting carry_a.
    let mut sess_b = pipeline.start_track(spec_44, &meta, false).unwrap();
    sess_b
        .set_incoming_gapless_carryover(carry_a)
        .expect("equal-rate 44.1 kHz session must accept carryover");
    combined.extend(
        sess_b
            .process_block(StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: vec![0.5; n_b],
                right: vec![-0.5; n_b],
            })
            .unwrap()
            .frames,
    );
    combined.extend(sess_b.flush().unwrap().frames);

    assert_eq!(combined.len(), (n_a + n_b) * 8);
    let expected_l_f64 = 0.5_f64 * 10.0_f64.powf(-3.0 / 20.0) * (1_u64 << 23) as f64;
    let boundary = n_a * 8;
    for idx in (boundary - 128)..(boundary + 128) {
        let got_l = f64::from(combined[idx][0] >> 8);
        let got_r = f64::from(combined[idx][1] >> 8);
        assert!(
            (got_l - expected_l_f64).abs() <= 3.0,
            "gapless pipeline boundary L dip at {idx}: got {got_l}, expected {expected_l_f64:.2} +/- 3"
        );
        assert!(
            (got_r + expected_l_f64).abs() <= 3.0,
            "gapless pipeline boundary R dip at {idx}: got {got_r}, expected {:.2} +/- 3",
            -expected_l_f64
        );
    }
}

struct FixedRateEchoConvolverFactory {
    ir_352: Arc<PreparedStereoIr>,
}

impl StereoConvolverFactory for FixedRateEchoConvolverFactory {
    fn create_for_rate(&self, target_rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String> {
        if target_rate_hz == 352_800 {
            Ok(Box::new(PartitionedStereoConvolver::new(Arc::clone(
                &self.ir_352,
            ))))
        } else {
            NullConvolverFactory.create_for_rate(target_rate_hz)
        }
    }
}

#[test]
fn same_output_rate_different_upsampler_ratio_hands_off_rew_tail_without_gap() {
    // Review #3 Findings #18 & #12:
    // Track A is 44.1 kHz (8x -> 352.8 kHz) with an active 4096-tap REW echo IR (echo at +3000
    // output samples). Track B is 88.2 kHz (4x -> 352.8 kHz, same output rate, different ratio!).
    // attach_gapless_carryover MUST drain only the short upsampler history (~1024 samples) and
    // seamlessly overlap-add Track A's REW echo onto Track B with ZERO 3000-sample silence gap!
    let mut h = vec![0.0_f64; 4096];
    h[0] = 0.5;
    h[3000] = 0.25; // max|H| <= 0.75 (0 dB), L1 = 0.75 (0 dB), echo 3000 samples later
    let ir_352 = Arc::new(PreparedStereoIr::from_stereo_ir("echo-352k", 352_800, &h, &h).unwrap());

    let pipeline = AudioPipeline::with_factories(
        UpsamplerPolicy::default(),
        Box::new(FixedRateEchoConvolverFactory { ir_352 }),
        Box::new(NullChannelDelayerFactory),
    )
    .unwrap();

    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let spec_88 = StreamSpec {
        sample_rate_hz: 88_200,
        bits_per_sample: 24,
        channels: 2,
    };
    let spec_48 = StreamSpec {
        sample_rate_hz: 48_000,
        bits_per_sample: 24,
        channels: 2,
    };

    // Track A (44.1 kHz): 512 frames ending with a strong impulse near the end (sample 500 -> output ~4000).
    let n_a = 512_usize;
    let mut a_sig = vec![0.0_f64; n_a];
    a_sig[500] = 0.8;
    let mut sess_a = pipeline
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .unwrap();
    let mut a_frames = sess_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: a_sig.clone(),
            right: a_sig,
        })
        .unwrap()
        .frames;
    let (a_flush, carry_a) = sess_a.flush_for_gapless().unwrap();
    a_frames.extend(a_flush.frames);
    let carry_a = carry_a.expect("expected gapless carryover from 44.1k REW track");
    assert!(carry_a.upsampler.is_some());
    assert!(carry_a.convolver.is_some());

    // Verify incompatible output rate (48k -> 384k) does NOT demote a quiet Track C to -3 dB!
    let quiet_meta = TrackPeakMetadata {
        true_peak_linear: Some(0.65), // -3.74 dBTP -> qualifies for 0 dB
        ..TrackPeakMetadata::default()
    };
    let sess_48 = pipeline
        .start_track_for_carryover(spec_48, &quiet_meta, false, Some(&carry_a))
        .unwrap();
    assert_eq!(
        sess_48.report().headroom.mode,
        HeadroomMode::ZeroDb,
        "mismatched output rate (352.8k vs 384k) must NOT demote Track C to -3 dB"
    );

    // Track B (88.2 kHz -> 352.8 kHz): same output rate, different upsampler ratio (4x vs 8x).
    let mut sess_b = pipeline
        .start_track_for_carryover(spec_88, &TrackPeakMetadata::default(), false, Some(&carry_a))
        .unwrap();
    let outcome = pipeline
        .attach_gapless_carryover(&mut sess_b, carry_a)
        .unwrap();
    let (up_tail_pcm, attached_conv_tail) = match outcome {
        GaplessAttachOutcome::ConvolverOnlyHandoff {
            upsampler_tail_pcm,
            attached_convolver_tail,
            ..
        } => (upsampler_tail_pcm, attached_convolver_tail),
        other => panic!("expected ConvolverOnlyHandoff for 44.1k->88.2k, got {other:?}"),
    };
    assert!(attached_conv_tail, "REW echo tail must be flagged as attached");
    a_frames.extend(up_tail_pcm.frames);
    assert_eq!(
        a_frames.len(),
        n_a * 8,
        "Track A must have EXACTLY n_a * 8 frames (only short upsampler tail drained, NO 3000-sample REW gap!)"
    );

    // Feed silence into Track B (88.2k -> 352.8k): Track A's +3000-sample echo from sample 500
    // (output index 4000, so echo lands at global index 7000 = index 7000 - 4096 = 2904 in Track B!)
    // must appear cleanly inside Track B!
    let n_b = 1024_usize; // 4096 output frames at 4x
    let mut b_frames = sess_b
        .process_block(StereoF64Buffer {
            sample_rate_hz: 88_200,
            left: vec![0.0; n_b],
            right: vec![0.0; n_b],
        })
        .unwrap()
        .frames;
    b_frames.extend(sess_b.flush().unwrap().frames);

    let peak_near_echo = b_frames[2880..2930]
        .iter()
        .map(|&[l, _]| (l as f64).abs() / 2_147_483_648.0)
        .fold(0.0_f64, f64::max);
    assert!(
        peak_near_echo > 0.10,
        "Track A's REW echo must decay seamlessly into Track B around index 2904, got peak {peak_near_echo:.5}"
    );
}

#[test]
fn review5_carryover_preserves_convolution_scale_and_null_convolver_applies_s() {
    // Finding #4: Track A with a +6 dB REW IR (delay tap = 2.0 at index 2000) receives S = -6 dB
    // (total_gain_db = G_up + S). Draining carryover OR doing ConvolverOnlyHandoff MUST preserve
    // Track A's exact scaled level with NO +6 dB jump or clipping!
    let mut ir_l = vec![0.0_f64; 2500];
    ir_l[0] = 0.0;
    ir_l[2000] = 2.0; // +6.02 dB gain across all frequencies
    let ir = Arc::new(PreparedStereoIr::from_stereo_ir("boost-6db", 352_800, &ir_l, &ir_l).unwrap());
    assert!(
        (ir.max_boost_db - 6.0206).abs() < 0.05,
        "expected ~+6.02 dB boost, got {:.3}",
        ir.max_boost_db
    );

    struct FixedIrFactory(Arc<PreparedStereoIr>);
    impl StereoConvolverFactory for FixedIrFactory {
        fn create_for_rate(&self, rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String> {
            if rate_hz == self.0.sample_rate_hz {
                Ok(Box::new(PartitionedStereoConvolver::new(Arc::clone(&self.0))))
            } else {
                NullConvolverFactory.create_for_rate(rate_hz)
            }
        }
    }

    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SimpleTest),
        shaper_override: Some(NoiseShaperProfile::TpdfDither),
        ..UpsamplerPolicy::default()
    };
    let pipeline = AudioPipeline::with_factories(
        policy.clone(),
        Box::new(FixedIrFactory(ir)),
        Box::new(NullChannelDelayerFactory),
    )
    .unwrap();
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 24,
        channels: 2,
    };
    let spec_88 = StreamSpec {
        sample_rate_hz: 88_200,
        bits_per_sample: 24,
        channels: 2,
    };
    // Review #5 Finding #4 exact scenario: untagged 44.1k Track A -> G_up = -3 dB, G_c = +6.02 dB,
    // predicted = 0 + 3 - 3 + 6.02 = +6.02 dB -> S = -6.02 dB!
    let untagged = TrackPeakMetadata::default();

    // Case 1: drain_gapless_carryover after Track A impulse near end.
    let n_a = 512usize;
    let mut sig = vec![0.0_f64; n_a];
    sig[450] = 0.90;
    let mut sess_a = pipeline.start_track(spec_44, &untagged, false).unwrap();
    assert_eq!(sess_a.report().headroom.mode, HeadroomMode::Minus3Db);
    assert!(
        (sess_a.report().headroom.convolution_scale_db - (-6.0206)).abs() < 0.05,
        "expected S ~= -6.02 dB, got {:.3}",
        sess_a.report().headroom.convolution_scale_db
    );
    let mut full_a = sess_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: sig.clone(),
            right: sig.clone(),
        })
        .unwrap()
        .frames;
    let (flush_a, carry_a) = sess_a.flush_for_gapless().unwrap();
    full_a.extend(flush_a.frames);
    let carry_a = carry_a.expect("expected carryover");
    let drained = pipeline.drain_gapless_carryover(carry_a).unwrap();
    let max_drained = drained
        .frames
        .iter()
        .map(|&[l, r]| (l as f64).abs().max((r as f64).abs()) / 2_147_483_648.0)
        .fold(0.0_f64, f64::max);
    // Without the fix, S was undone on drain and 0.90 * 0.707 * 2.0 = 1.27 clipped to 1.0!
    // With S folded into Stage 1, 0.90 * 0.7071 * 0.5 * 2.0 ~= 0.636 (zero step, zero clipping!).
    assert!(
        max_drained > 0.50 && max_drained < 0.72,
        "drained +6 dB IR tail must preserve exact S=-6 dB scaling without +6 dB jump/clip, got peak {max_drained:.5}"
    );

    // Case 2: ConvolverOnlyHandoff (44.1k -> 88.2k) preserves exact S scaling too.
    let mut sess_a2 = pipeline.start_track(spec_44, &untagged, false).unwrap();
    let _ = sess_a2
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: sig.clone(),
            right: sig,
        })
        .unwrap();
    let (_, carry_a2) = sess_a2.flush_for_gapless().unwrap();
    let carry_a2 = carry_a2.unwrap();
    let mut sess_b = pipeline
        .start_track_for_carryover(spec_88, &untagged, false, Some(&carry_a2))
        .unwrap();
    let outcome = pipeline.attach_gapless_carryover(&mut sess_b, carry_a2).unwrap();
    assert!(matches!(outcome, GaplessAttachOutcome::ConvolverOnlyHandoff { .. }));
    let mut b_frames = sess_b
        .process_block(StereoF64Buffer {
            sample_rate_hz: 88_200,
            left: vec![0.0; 1024],
            right: vec![0.0; 1024],
        })
        .unwrap()
        .frames;
    b_frames.extend(sess_b.flush().unwrap().frames);
    let max_b = b_frames
        .iter()
        .map(|&[l, r]| (l as f64).abs().max((r as f64).abs()) / 2_147_483_648.0)
        .fold(0.0_f64, f64::max);
    assert!(
        max_b > 0.50 && max_b < 0.72,
        "ConvolverOnlyHandoff +6 dB IR tail must stay scaled cleanly without clipping, got {max_b:.5}"
    );

    // Finding #14: NullConvolver (G_c = 0 dB) with +4 dBTP hot master MUST still apply
    // G_up = -3 dB AND S = -1 dB (total -4 dB) in Stage 1!
    let null_pipe = AudioPipeline::new(policy).unwrap();
    let hot_tp = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(4.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let mut sess_hot = null_pipe.start_track(spec_44, &hot_tp, false).unwrap();
    assert_eq!(sess_hot.report().headroom.mode, HeadroomMode::Minus3Db);
    assert!((sess_hot.report().headroom.convolution_scale_db - (-1.0)).abs() < 1e-6);
    // Feed constant 0.5 DC: output steady-state amplitude must be 0.5 * 10^(-4/20) ~= 0.3155 (not 0.354!).
    let mut out_hot = sess_hot
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.5; 256],
            right: vec![0.5; 256],
        })
        .unwrap()
        .frames;
    out_hot.extend(sess_hot.flush().unwrap().frames);
    let mid_amp = (out_hot[out_hot.len() / 2][0] as f64) / 2_147_483_648.0;
    let expected_amp = 0.5 * 10.0_f64.powf(-4.0 / 20.0);
    assert!(
        (mid_amp - expected_amp).abs() < 0.005,
        "NullConvolver +4 dBTP track must apply full -4 dB (G_up=-3, S=-1): expected {expected_amp:.4}, got {mid_amp:.4}"
    );
}

#[test]
fn review5_quantizer_counts_overs_and_auto_selector_degrades_for_max_taps() {
    // Finding #15: StreamingQuantizer counts clipped samples and tracks max pre-quant peak.
    let spec_352 = StreamSpec {
        sample_rate_hz: 352_800,
        bits_per_sample: 24,
        channels: 2,
    };
    let pipe = AudioPipeline::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SimpleTest),
        shaper_override: Some(NoiseShaperProfile::TpdfDither),
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    // Claim quiet true peak (-6 dBTP -> G_up = 0 dB) then feed an out-of-range +1.25 transient.
    let lied_meta = TrackPeakMetadata {
        true_peak_linear: Some(0.5),
        ..TrackPeakMetadata::default()
    };
    let mut sess = pipe.start_track(spec_352, &lied_meta, false).unwrap();
    let _ = sess
        .process_block(StereoF64Buffer {
            sample_rate_hz: 352_800,
            left: vec![0.2, 1.25, -1.30, 0.2],
            right: vec![0.1, 0.1, 0.1, 0.1],
        })
        .unwrap();
    let _ = sess.flush().unwrap();
    let stats = sess.quantizer_stats();
    assert!(
        stats.overs_count >= 2,
        "expected >= 2 clipped samples, got {}",
        stats.overs_count
    );
    assert!(
        stats.max_pre_quant_peak > 1.29,
        "expected max_pre_quant_peak ~= 1.30, got {:.4}",
        stats.max_pre_quant_peak
    );
    assert!(stats.max_pre_quant_peak_dbfs() > 2.2);

    // Finding #18: --max-taps 4096 in default auto mode must gracefully degrade to SimpleTest
    // instead of failing every 44.1 kHz track, while explicit Gaussian override fails cleanly
    // with the true shape-derived tap count.
    let low_cap_auto = AudioPipeline::new(UpsamplerPolicy {
        max_taps: 4_096,
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let auto_sess = low_cap_auto
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .expect("auto mode with --max-taps 4096 must degrade to SimpleTest instead of failing");
    assert_eq!(auto_sess.report().dsp_plan.filter, FilterArchetype::SimpleTest);
    assert!(auto_sess.report().dsp_plan.tap_budget <= 4_096);

    let low_cap_explicit = AudioPipeline::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::GaussLongLp),
        max_taps: 4_096,
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let err = low_cap_explicit
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .err()
        .expect("explicit Gaussian override exceeding --max-taps must return a clear error");
    assert!(
        err.contains("exceeds --max-taps") && err.contains("5063"),
        "unexpected error message: {err}"
    );
}

#[test]
fn headroom_design_issue7_14bit_zero_db_and_per_track_gapless_scale_continuity() {
    struct StaticIrFactory(Arc<PreparedStereoIr>);
    impl StereoConvolverFactory for StaticIrFactory {
        fn create_for_rate(&self, rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String> {
            if rate_hz == self.0.sample_rate_hz {
                Ok(Box::new(PartitionedStereoConvolver::new(Arc::clone(&self.0))))
            } else {
                NullConvolverFactory.create_for_rate(rate_hz)
            }
        }
    }

    let spec_44_16 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };

    // HEADROOM_DESIGN.md §8 Test 4: 14-bit content in a 16-bit file (-12.04 dBFS sample peak)
    // passes with total gain strictly 0 dB through BOTH an unboosted filter AND a +5 dB filter!
    let meta_14bit = TrackPeakMetadata {
        replaygain_track_peak: Some(0.25), // -12.04 dBFS
        ..TrackPeakMetadata::default()
    };
    let amp_14bit = 0.25_f64;

    // 4a. Unboosted filter (unity impulse).
    let ir_unity = Arc::new(
        PreparedStereoIr::from_stereo_ir("unity-352k", 352_800, &[1.0], &[1.0]).unwrap(),
    );
    let pipe_unity = AudioPipeline::with_factories(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::SimpleTest),
            ..UpsamplerPolicy::default()
        },
        Box::new(StaticIrFactory(Arc::clone(&ir_unity))),
        Box::new(NullChannelDelayerFactory),
    )
    .unwrap();
    let res_unity = pipe_unity
        .process(
            StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: vec![amp_14bit; 512],
                right: vec![amp_14bit; 512],
            },
            spec_44_16,
            &meta_14bit,
            false,
        )
        .unwrap();
    assert_eq!(res_unity.report.headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(res_unity.report.headroom.convolution_scale_db, 0.0);
    assert_eq!(res_unity.report.headroom.total_gain_db(), 0.0);
    assert_eq!(
        res_unity.report.convolver_content_hash.as_deref(),
        Some(ir_unity.content_hash.as_str())
    );
    let mid_unity = (res_unity.pcm.frames[2048][0] as f64) / 2_147_483_648.0;
    assert!(
        (mid_unity - amp_14bit).abs() < 1e-4,
        "14-bit content through unboosted filter must pass at 0 dB: got {mid_unity:.5}, expected {amp_14bit:.5}"
    );

    // 4b. +5 dB boosted filter (-12.04 + 3 + 0 + 5 = -4.04 dBFS <= 0 -> G_up=0, S=0, total=0 dB!).
    let boost_5db = 10.0_f64.powf(5.0 / 20.0);
    let ir_boost5 = Arc::new(
        PreparedStereoIr::from_stereo_ir("boost5-352k", 352_800, &[boost_5db], &[boost_5db])
            .unwrap(),
    );
    let pipe_boost5 = AudioPipeline::with_factories(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::SimpleTest),
            ..UpsamplerPolicy::default()
        },
        Box::new(StaticIrFactory(ir_boost5)),
        Box::new(NullChannelDelayerFactory),
    )
    .unwrap();
    let res_boost5 = pipe_boost5
        .process(
            StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: vec![amp_14bit; 512],
                right: vec![amp_14bit; 512],
            },
            spec_44_16,
            &meta_14bit,
            false,
        )
        .unwrap();
    assert_eq!(res_boost5.report.headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(res_boost5.report.headroom.convolution_scale_db, 0.0);
    assert_eq!(res_boost5.report.headroom.total_gain_db(), 0.0);
    let mid_boost5 = (res_boost5.pcm.frames[2048][0] as f64) / 2_147_483_648.0;
    let expected_boost5 = amp_14bit * boost_5db;
    assert!(
        (mid_boost5 - expected_boost5).abs() < 1e-4,
        "14-bit content through +5 dB filter must pass at 0 dB: got {mid_boost5:.5}, expected {expected_boost5:.5}"
    );

    // HEADROOM_DESIGN.md §8 Test 5: Two consecutive tracks with different peaks get their own
    // independent G_up & S, while Track A's carried REW tail keeps Track A's exact scale!
    let mut h_echo = vec![0.0_f64; 3000];
    h_echo[0] = 1.0;
    h_echo[2000] = 0.5; // max |H| = 1.5 (+3.52 dB)
    let ir_echo = Arc::new(
        PreparedStereoIr::from_stereo_ir("echo-boost-352k", 352_800, &h_echo, &h_echo).unwrap(),
    );
    let pipe_echo = AudioPipeline::with_factories(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::SimpleTest),
            ..UpsamplerPolicy::default()
        },
        Box::new(StaticIrFactory(ir_echo)),
        Box::new(NullChannelDelayerFactory),
    )
    .unwrap();

    // Track A: quiet true peak -6 dBTP -> G_up = 0 dB, B_out = -6 + 0 + 0 + 3.52 = -2.48 <= 0 -> S = 0 dB!
    let meta_a = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(-6.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let mut sig_a = vec![0.0_f64; 512];
    // Place impulse at index 350 (output index 2800, well before the upsampler carryover window):
    // main impulse (h[0]=1.0) emits inside Track A, while the +2000-sample echo lands at output
    // index 4800 (index 704 inside Track B!).
    sig_a[350] = 0.5;
    let mut sess_a = pipe_echo.start_track(spec_44_16, &meta_a, false).unwrap();
    assert_eq!(sess_a.report().headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(sess_a.report().headroom.convolution_scale_db, 0.0);
    let _ = sess_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: sig_a.clone(),
            right: sig_a,
        })
        .unwrap();
    let (_flush_a, carry_a) = sess_a.flush_for_gapless().unwrap();
    let carry_a = carry_a.expect("expected gapless carryover from Track A");

    // Track B: untagged (unknown peak) -> G_up = -3 dB, S <= -3.52 dB (strictly independent of Track A!).
    let mut sess_b = pipe_echo
        .start_track_for_carryover(spec_44_16, &TrackPeakMetadata::default(), false, Some(&carry_a))
        .unwrap();
    assert_eq!(sess_b.report().headroom.mode, HeadroomMode::Minus3Db);
    assert!(
        sess_b.report().headroom.convolution_scale_db <= -3.50,
        "Track B must compute its own S <= -3.52 dB independently of Track A, got {:.3}",
        sess_b.report().headroom.convolution_scale_db
    );
    let outcome = pipe_echo
        .attach_gapless_carryover(&mut sess_b, carry_a)
        .unwrap();
    assert!(matches!(outcome, GaplessAttachOutcome::FullHandoff));

    // Feed silence into Track B: Track A's +2000-sample echo must arrive at Track A's exact
    // unattenuated scale (0.5 * 0.5 = 0.25, NOT scaled down by Track B's -6.5+ dB total cut!).
    let mut b_frames = sess_b
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.0; 512],
            right: vec![0.0; 512],
        })
        .unwrap()
        .frames;
    b_frames.extend(sess_b.flush().unwrap().frames);
    let peak_b_echo = b_frames
        .iter()
        .map(|&[l, _]| (l as f64).abs() / 2_147_483_648.0)
        .fold(0.0_f64, f64::max);
    assert!(
        (peak_b_echo - 0.25).abs() < 0.03,
        "Track A's carried REW echo inside Track B must preserve Track A's 0 dB scale (~0.25), got {peak_b_echo:.4}"
    );
}

#[test]
fn review6_gapless_rew_pair_preserves_equal_gain_full_drain_decay_and_pre_dither_overs() {
    use ap_dsp::quantize::StreamingQuantizer;
    use std::f64::consts::PI;

    // 1. Finding #1 [HIGH]: Continuous -1 dBFS 1 kHz sine split across Track A and Track B through
    //    a dispersive REW convolver MUST keep Track B's gain identical to Track A (NEVER cut B by
    //    -60 dB or -12 dB!) and render continuously with zero step vs one-shot.
    let mut h_rew = vec![0.0_f64; 4096];
    h_rew[0] = 0.85;
    h_rew[1200] = 0.35;
    h_rew[2600] = -0.25;
    let ir_384 = Arc::new(
        PreparedStereoIr::from_stereo_ir("rew-disp-384k", 384_000, &h_rew, &h_rew).unwrap(),
    );
    let spec_48_24 = StreamSpec {
        sample_rate_hz: 48_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let amp = 10.0_f64.powf(-1.0 / 20.0); // -1 dBFS
    let n_half = 4096_usize;
    let total_n = n_half * 2;
    let full_sine: Vec<f64> = (0..total_n)
        .map(|i| amp * (2.0 * PI * 1000.0 * (i as f64) / 48_000.0).sin())
        .collect();
    let meta = TrackPeakMetadata {
        true_peak_linear: Some(amp),
        ..TrackPeakMetadata::default()
    };

    struct LocalStaticIrFactory(Arc<PreparedStereoIr>);
    impl StereoConvolverFactory for LocalStaticIrFactory {
        fn create_for_rate(&self, rate_hz: u32) -> Result<Box<dyn StereoConvolver>, String> {
            assert_eq!(rate_hz, self.0.sample_rate_hz);
            Ok(Box::new(PartitionedStereoConvolver::new(Arc::clone(&self.0))))
        }
    }

    for &arch in &[FilterArchetype::SimpleTest, FilterArchetype::GaussLongLp] {
        let pipe = AudioPipeline::with_factories(
            UpsamplerPolicy {
                filter_override: Some(arch),
                ..UpsamplerPolicy::default()
            },
            Box::new(LocalStaticIrFactory(Arc::clone(&ir_384))),
            Box::new(NullChannelDelayerFactory),
        )
        .unwrap();

        let oneshot = pipe
            .process(
                StereoF64Buffer {
                    sample_rate_hz: 48_000,
                    left: full_sine.clone(),
                    right: full_sine.clone(),
                },
                spec_48_24,
                &meta,
                false,
            )
            .unwrap();

        let mut sess_a = pipe.start_track(spec_48_24, &meta, false).unwrap();
        let mut gap_frames = sess_a
            .process_block(StereoF64Buffer {
                sample_rate_hz: 48_000,
                left: full_sine[..n_half].to_vec(),
                right: full_sine[..n_half].to_vec(),
            })
            .unwrap()
            .frames;
        let (a_flush, carry_a) = sess_a.flush_for_gapless().unwrap();
        gap_frames.extend(a_flush.frames);
        let carry_a = carry_a.expect("expected carryover from Track A");

        let mut sess_b = pipe
            .start_track_for_carryover(spec_48_24, &meta, false, Some(&carry_a))
            .unwrap();
        assert!(
            (sess_b.report().headroom.total_gain_db() - sess_a.report().headroom.total_gain_db())
                .abs()
                < 1e-9,
            "{arch}: Track B gain ({:.2} dB) must equal Track A gain ({:.2} dB) on continuous REW pair!",
            sess_b.report().headroom.total_gain_db(),
            sess_a.report().headroom.total_gain_db(),
        );
        assert!(
            (sess_b.report().headroom.convolution_scale_db
                - sess_a.report().headroom.convolution_scale_db)
                .abs()
                < 1e-9,
            "{arch}: Track B S ({:.2} dB) must equal Track A S ({:.2} dB)!",
            sess_b.report().headroom.convolution_scale_db,
            sess_a.report().headroom.convolution_scale_db,
        );

        let outcome = pipe.attach_gapless_carryover(&mut sess_b, carry_a).unwrap();
        assert!(matches!(outcome, GaplessAttachOutcome::FullHandoff));

        gap_frames.extend(
            sess_b
                .process_block(StereoF64Buffer {
                    sample_rate_hz: 48_000,
                    left: full_sine[n_half..].to_vec(),
                    right: full_sine[n_half..].to_vec(),
                })
                .unwrap()
                .frames,
        );
        gap_frames.extend(sess_b.flush().unwrap().frames);

        assert_eq!(gap_frames.len(), oneshot.pcm.frames.len());
        let max_diff = gap_frames
            .iter()
            .zip(oneshot.pcm.frames.iter())
            .map(|(g, o)| ((g[0] as f64) - (o[0] as f64)).abs() / 2_147_483_648.0)
            .fold(0.0_f64, f64::max);
        assert!(
            max_diff < 1e-5,
            "{arch}: gapless REW pair must match one-shot continuously (max_diff={max_diff:.6})"
        );
    }

    // 2. Finding #13 [LOW]: drain_gapless_carryover matches continuous zero-input render across
    //    the FULL FIR decay (no -54 dBFS truncation past exact_flush_len).
    for &arch in &[FilterArchetype::SimpleTest, FilterArchetype::GaussLongLp] {
        let pipe = AudioPipeline::new(UpsamplerPolicy {
            filter_override: Some(arch),
            shaper_override: Some(NoiseShaperProfile::TpdfDither),
            ..UpsamplerPolicy::default()
        })
        .unwrap();
        let n_a = 2048_usize;
        let a_sig: Vec<f64> = (0..n_a)
            .map(|i| {
                0.4 * (2.0 * PI * 60.0 * (i as f64) / 48_000.0).sin()
                    + 0.4 * (2.0 * PI * 1000.0 * (i as f64) / 48_000.0).sin()
            })
            .collect();

        // Continuous A followed by zeros (zero-input continuation).
        let mut cont_sig = a_sig.clone();
        cont_sig.resize(n_a + 1024, 0.0);
        let mut s_cont = pipe.start_track(spec_48_24, &meta, false).unwrap();
        let mut cont_frames = s_cont
            .process_block(StereoF64Buffer {
                sample_rate_hz: 48_000,
                left: cont_sig.clone(),
                right: cont_sig,
            })
            .unwrap()
            .frames;
        cont_frames.extend(s_cont.flush().unwrap().frames);

        // Split A -> drain_gapless_carryover.
        let mut s_a = pipe.start_track(spec_48_24, &meta, false).unwrap();
        let mut drain_frames = s_a
            .process_block(StereoF64Buffer {
                sample_rate_hz: 48_000,
                left: a_sig.clone(),
                right: a_sig,
            })
            .unwrap()
            .frames;
        let (fl_a, carry_a) = s_a.flush_for_gapless().unwrap();
        drain_frames.extend(fl_a.frames);
        let (drained, _stats) = pipe
            .drain_gapless_carryover_with_stats(carry_a.unwrap())
            .unwrap();
        drain_frames.extend(drained.frames);

        // Compare every sample of the drained output against the continuous zero-input render.
        let cmp_len = drain_frames.len().min(cont_frames.len());
        let max_diff = drain_frames[..cmp_len]
            .iter()
            .zip(cont_frames[..cmp_len].iter())
            .map(|(d, c)| ((d[0] as f64) - (c[0] as f64)).abs() / 2_147_483_648.0)
            .fold(0.0_f64, f64::max);
        assert!(
            max_diff < 1e-5,
            "{arch}: drain_gapless_carryover must match continuous zero-input FIR decay (max_diff={max_diff:.6})"
        );
    }

    // 3. Finding #18 [LOW]: StreamingQuantizer counts overs BEFORE adding TPDF dither so a
    //    representable 0 dBFS signal (`max_int / levels`) records 0 overs.
    for &bits in &[16_u8, 24_u8] {
        let levels = (1_u64 << (bits - 1)) as f64;
        let max_rep = (((1_i64 << (bits - 1)) - 1) as f64) / levels;
        let mut q = StreamingQuantizer::new(NoiseShaperProfile::TpdfDither, bits).unwrap();
        let _ = q.quantize_block(&StereoF64Buffer {
            sample_rate_hz: 48_000,
            left: vec![max_rep; 10_000],
            right: vec![-1.0; 10_000],
        });
        assert_eq!(
            q.stats().overs_count,
            0,
            "{bits}-bit representable full-scale samples must record 0 pre-dither overs, got {}",
            q.stats().overs_count
        );

        // Actual > 0 dBFS over (+1.5 LSB above max_int) IS still counted.
        let over_val = (((1_i64 << (bits - 1)) as f64) + 0.5) / levels;
        let _ = q.quantize_block(&StereoF64Buffer {
            sample_rate_hz: 48_000,
            left: vec![over_val; 4],
            right: vec![0.0; 4],
        });
        assert_eq!(q.stats().overs_count, 4);
    }
}

#[test]
fn inv04_dac_bits_auto_shaper_and_gapless_shaper_continuity_matches_continuous_stream() {
    // 1. dac_bits = 16 auto-selects NsGentle2 and quantizes to exact 16-bit left-justified PCM.
    let policy_16 = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SimpleTest),
        dac_bits: 16,
        ..UpsamplerPolicy::default()
    };
    let pipe_16 = AudioPipeline::new(policy_16).unwrap();
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let mut sess = pipe_16
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .unwrap();
    assert_eq!(sess.report().output_bits, 16);
    assert_eq!(sess.report().effective_shaper, NoiseShaperProfile::NsGentle2);
    let blk = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: (0..256).map(|i| 0.25 * (0.07 * i as f64).sin()).collect(),
        right: (0..256).map(|i| -0.20 * (0.11 * i as f64).cos()).collect(),
    };
    let pcm = sess.process_block(blk).unwrap();
    assert!(!pcm.frames.is_empty());
    for &[l, r] in &pcm.frames {
        assert_eq!(l & 0xFFFF, 0, "16-bit left-justified sample must zero lower 16 bits");
        assert_eq!(r & 0xFFFF, 0, "16-bit left-justified sample must zero lower 16 bits");
    }

    // 2. Gapless FullHandoff with 15th-order NsSmooth15 preserves both upsampler FIR history
    //    AND quantizer/shaper state, producing bit-for-bit identical PCM to one continuous track!
    let policy_lns15 = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SimpleTest),
        shaper_override: Some(NoiseShaperProfile::NsSmooth15),
        dac_bits: 18,
        ..UpsamplerPolicy::default()
    };
    let pipe_lns = AudioPipeline::new(policy_lns15).unwrap();
    let total_src = 2_048_usize;
    let split = 1_024_usize;
    let full_l: Vec<f64> = (0..total_src)
        .map(|i| 0.3 * (2.0 * std::f64::consts::PI * 997.0 * (i as f64) / 44_100.0).sin())
        .collect();
    let full_r: Vec<f64> = (0..total_src)
        .map(|i| 0.25 * (2.0 * std::f64::consts::PI * 1_481.0 * (i as f64) / 44_100.0).cos())
        .collect();

    let continuous = pipe_lns
        .process(
            StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: full_l.clone(),
                right: full_r.clone(),
            },
            spec_44,
            &TrackPeakMetadata::default(),
            false,
        )
        .unwrap();

    let mut track_a = pipe_lns
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .unwrap();
    let mut stitched = track_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: full_l[..split].to_vec(),
            right: full_r[..split].to_vec(),
        })
        .unwrap()
        .frames;
    let (tail_a, carryover) = track_a.flush_for_gapless().unwrap();
    stitched.extend(tail_a.frames);
    let carryover = carryover.expect("expected gapless carryover with quantizer state");
    assert!(carryover.quantizer_state.is_some());

    let mut track_b = pipe_lns
        .start_track(spec_44, &TrackPeakMetadata::default(), false)
        .unwrap();
    let outcome = pipe_lns
        .attach_gapless_carryover(&mut track_b, carryover)
        .unwrap();
    assert!(matches!(outcome, GaplessAttachOutcome::FullHandoff));
    stitched.extend(
        track_b
            .process_block(StereoF64Buffer {
                sample_rate_hz: 44_100,
                left: full_l[split..].to_vec(),
                right: full_r[split..].to_vec(),
            })
            .unwrap()
            .frames,
    );
    stitched.extend(track_b.flush().unwrap().frames);

    assert_eq!(
        stitched, continuous.pcm.frames,
        "equal-rate gapless handoff with NsSmooth15 must match continuous stream bit-for-bit"
    );
}
