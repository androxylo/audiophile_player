//! TDD unit tests for the dynamic per-track `UpsamplerPicker` and CLI overrides.

use std::str::FromStr;

use ap_core::format::{StreamSpec, TargetDspRate};
use ap_core::headroom::TrackPeakMetadata;
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{
    FilterArchetype, NoiseShaperProfile, TrackSourceInfo, UNSUPPORTED_SAMPLE_RATE_PREFIX,
    UpsampleRatio, UpsamplerPicker, UpsamplerPolicy, select_target_rate,
};
use ap_dsp::upsampler::build_upsampler;

fn track(rate: u32, bits: u16) -> TrackSourceInfo {
    TrackSourceInfo {
        sample_rate_hz: rate,
        bits_per_sample: bits,
        channels: 2,
        apodizing_needed: false,
        is_streaming_source: false,
    }
}

#[test]
fn auto_picker_uses_initial_simple_test_upsampler_and_tracks_future_curated_defaults() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    // 1. 16/44.1 Redbook -> 8x 352.8 kHz f64 (active: SimpleTest; future default: GaussApod4M)
    let p_cd = picker.select(&track(44_100, 16));
    assert_eq!(p_cd.target_rate, Some(TargetDspRate::Hz352800));
    assert_eq!(p_cd.target_rate_hz, 352_800);
    assert_eq!(p_cd.ratio, UpsampleRatio::Integer(8));
    assert_eq!(p_cd.filter, FilterArchetype::SimpleTest);
    assert_eq!(p_cd.target_audiophile_filter, FilterArchetype::GaussApod4M);
    assert_eq!(p_cd.shaper, NoiseShaperProfile::TpdfDither);
    assert!(p_cd.tap_budget > 0 && p_cd.tap_budget <= MAX_FILTER_TAPS);
    assert!(!p_cd.filter_overridden);

    // 2. 24/96 HiRes -> 4x 384 kHz f64 (active: SimpleTest; future default: GaussLongLp)
    let p_96 = picker.select(&track(96_000, 24));
    assert_eq!(p_96.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p_96.target_rate_hz, 384_000);
    assert_eq!(p_96.ratio, UpsampleRatio::Integer(4));
    assert_eq!(p_96.filter, FilterArchetype::SimpleTest);
    assert_eq!(p_96.target_audiophile_filter, FilterArchetype::GaussLongLp);

    // 3. 24/192 HiRes -> 2x 384 kHz f64 (active: SimpleTest; future default: GaussHiresIp)
    let p_192 = picker.select(&track(192_000, 24));
    assert_eq!(p_192.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p_192.ratio, UpsampleRatio::Integer(2));
    assert_eq!(p_192.filter, FilterArchetype::SimpleTest);
    assert_eq!(p_192.target_audiophile_filter, FilterArchetype::GaussHiresIp);

    // 4. 32/384 pre-upsampled PGGB FLAC -> Null 1:1 f64 upsampler (still in f64 domain!)
    let p_384 = picker.select(&track(384_000, 32));
    assert_eq!(p_384.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p_384.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p_384.filter, FilterArchetype::Null);
    assert_eq!(p_384.shaper, NoiseShaperProfile::Bypass);
    assert_eq!(p_384.tap_budget, 0);

    // 5. 32/352.8 pre-upsampled PGGB FLAC -> Null 1:1 f64 upsampler
    let p_352 = picker.select(&track(352_800, 32));
    assert_eq!(p_352.target_rate, Some(TargetDspRate::Hz352800));
    assert_eq!(p_352.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p_352.filter, FilterArchetype::Null);

    // Distinct target rates & future curated filters across the playlist sequence
    assert_ne!(p_cd.target_rate, p_96.target_rate);
    assert_ne!(p_cd.target_audiophile_filter, p_96.target_audiophile_filter);
    assert_ne!(p_96.target_audiophile_filter, p_192.target_audiophile_filter);
    assert_ne!(p_192.filter, p_384.filter);
}

#[test]
fn apodizing_flag_targets_gauss_apod_for_hires_sources() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let mut bad_master = track(96_000, 24);
    bad_master.apodizing_needed = true;

    let plan = picker.select(&bad_master);
    assert_eq!(plan.target_audiophile_filter, FilterArchetype::GaussApod4M);
    assert_eq!(plan.filter, FilterArchetype::SimpleTest); // sole implemented non-bypass filter for now
}

#[test]
fn cli_filter_and_shaper_overrides_are_enforced_across_tracks() {
    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SincSteep4M),
        shaper_override: Some(NoiseShaperProfile::Lns15Smooth),
        max_taps: MAX_FILTER_TAPS,
    };
    let picker = UpsamplerPicker::new(policy).unwrap();

    for &(rate, expected_ratio) in &[
        (44_100, UpsampleRatio::Integer(8)),
        (96_000, UpsampleRatio::Integer(4)),
        (192_000, UpsampleRatio::Integer(2)),
    ] {
        let plan = picker.select(&track(rate, 24));
        assert_eq!(plan.filter, FilterArchetype::SincSteep4M);
        assert_eq!(plan.shaper, NoiseShaperProfile::Lns15Smooth);
        assert_eq!(plan.ratio, expected_ratio);
        assert!(plan.filter_overridden);
        assert!(plan.shaper_overridden);
        assert!(plan.tap_budget <= MAX_FILTER_TAPS);
    }

    // Already 384 kHz must still select Null 1:1 f64 upsampler, while shaper override applies.
    let plan_384 = picker.select(&track(384_000, 32));
    assert_eq!(plan_384.filter, FilterArchetype::Null);
    assert_eq!(plan_384.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(plan_384.shaper, NoiseShaperProfile::Lns15Smooth);
}

#[test]
fn cli_null_override_disables_resampling_while_staying_in_f64_domain() {
    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::Null),
        shaper_override: Some(NoiseShaperProfile::Bypass),
        max_taps: MAX_FILTER_TAPS,
    };
    let picker = UpsamplerPicker::new(policy).unwrap();

    let p = picker.select(&track(44_100, 16));
    assert_eq!(p.filter, FilterArchetype::Null);
    assert_eq!(p.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p.target_rate, None);
    assert_eq!(p.target_rate_hz, 44_100);
    assert_eq!(p.tap_budget, 0);

    // Native 8fS rate under --filter null still sets target_rate consistently.
    let p384 = picker.select(&track(384_000, 24));
    assert_eq!(p384.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p384.target_rate_hz, 384_000);
}

#[test]
fn extreme_and_unsupported_source_rates_are_rejected_without_oom() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
    let null_pipeline = AudioPipeline::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::Null),
        ..UpsamplerPolicy::default()
    })
    .unwrap();

    // Crafted ultra-low rates (1 Hz, 10 Hz, 4 kHz) and above-384k rates (705.6k, 768k) must NEVER
    // yield an unbounded integer ratio and must fail cleanly with UNSUPPORTED_SAMPLE_RATE_PREFIX.
    for &bad_rate in &[0_u32, 1, 10, 4_000, 705_600, 768_000] {
        let (_, ratio) = select_target_rate(bad_rate);
        assert_eq!(
            ratio,
            UpsampleRatio::Arbitrary,
            "rate {bad_rate} Hz must not yield an integer upsample ratio"
        );
        let plan = picker.select(&track(bad_rate, 24));
        let err = build_upsampler(&plan)
            .err()
            .expect("build_upsampler must reject out-of-range rate");
        assert!(
            err.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX),
            "unexpected err for {bad_rate} Hz: {err}"
        );

        let spec = StreamSpec {
            sample_rate_hz: bad_rate,
            bits_per_sample: 24,
            channels: 2,
        };
        let err_auto = pipeline
            .start_track(spec, &TrackPeakMetadata::default(), false)
            .err()
            .expect("auto pipeline must reject out-of-range rate");
        assert!(err_auto.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX));

        let err_null = null_pipeline
            .start_track(spec, &TrackPeakMetadata::default(), false)
            .err()
            .expect("null pipeline must reject out-of-range rate");
        assert!(err_null.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX));
    }

    // In-range non-integer rates (e.g. 50 kHz, 37.8 kHz) also reject cleanly under SimpleTest
    // with UNSUPPORTED_SAMPLE_RATE_PREFIX so Random/Sequential can skip them without pausing.
    for &odd_rate in &[37_800_u32, 50_000] {
        let spec = StreamSpec {
            sample_rate_hz: odd_rate,
            bits_per_sample: 24,
            channels: 2,
        };
        let err = pipeline
            .start_track(spec, &TrackPeakMetadata::default(), false)
            .err()
            .expect("simple-test must reject non-integer rate");
        assert!(
            err.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX),
            "unexpected err for {odd_rate} Hz: {err}"
        );
    }

    // Valid low integer rate 8 kHz (48x <= 64) still succeeds cleanly!
    let p_8k = picker.select(&track(8_000, 16));
    assert_eq!(p_8k.ratio, UpsampleRatio::Integer(48));
    assert_eq!(p_8k.target_rate_hz, 384_000);
    assert!(build_upsampler(&p_8k).is_ok());
}

#[test]
fn policy_enforces_strict_4_million_tap_ceiling() {
    let too_big = UpsamplerPolicy {
        max_taps: MAX_FILTER_TAPS + 1,
        ..UpsamplerPolicy::default()
    };
    assert!(UpsamplerPicker::new(too_big).is_err());

    let custom = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SincSteep4M),
        shaper_override: None,
        max_taps: 1_000_000,
    };
    let picker = UpsamplerPicker::new(custom).unwrap();
    let p = picker.select(&track(44_100, 16));
    assert_eq!(p.tap_budget, 1_000_000);
}

#[test]
fn cli_names_parse_roundtrip_for_all_filters_and_shapers() {
    for f in [
        FilterArchetype::Null,
        FilterArchetype::SimpleTest,
        FilterArchetype::GaussApod4M,
        FilterArchetype::SincSteep4M,
        FilterArchetype::GaussLongLp,
        FilterArchetype::GaussHiresIp,
        FilterArchetype::GaussHiresMp,
        FilterArchetype::ExtSharpLp,
    ] {
        assert_eq!(FilterArchetype::from_str(f.cli_name()).unwrap(), f);
    }
    assert_eq!(FilterArchetype::from_str("bypass").unwrap(), FilterArchetype::Null);
    assert!(FilterArchetype::from_str("nonexistent").is_err());

    for s in [
        NoiseShaperProfile::Bypass,
        NoiseShaperProfile::TpdfDither,
        NoiseShaperProfile::Ns5Multibit,
        NoiseShaperProfile::Lns15Smooth,
        NoiseShaperProfile::PggbOptimal,
    ] {
        assert_eq!(NoiseShaperProfile::from_str(s.cli_name()).unwrap(), s);
    }
    assert!(NoiseShaperProfile::from_str("nonexistent").is_err());
}
