//! TDD unit tests for the dynamic per-track `UpsamplerPicker`, Phase 1 dummy
//! `DefaultFilterSelector`, pluggable `FilterSelector` trait, and CLI overrides.

use std::str::FromStr;
use std::sync::Arc;

use ap_core::format::{StreamSpec, TargetDspRate};
use ap_core::headroom::TrackPeakMetadata;
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{
    DefaultFilterSelector, FilterArchetype, FilterSelector, NoiseShaperProfile, TrackSourceInfo,
    UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsampleRatio, UpsamplerPicker, UpsamplerPolicy,
    select_target_rate,
};
use ap_dsp::upsampler::build_upsampler;

fn track(rate: u32, bits: u16) -> TrackSourceInfo {
    TrackSourceInfo {
        sample_rate_hz: rate,
        bits_per_sample: bits,
        channels: 2,
        apodizing_needed: false,
        is_streaming_source: false,
        effective_nyquist_hz: None,
        hf_noise_filter: false,
        deemphasis: false,
    }
}

#[test]
fn default_selector_picks_gauss_long_for_1x_gauss_hires_for_hires_and_null_for_8fs() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    assert_eq!(picker.selector_name(), "default");

    // 1. 16/44.1 Redbook -> 8x 352.8 kHz f64, default gauss-long-lp
    let p_cd = picker.select(&track(44_100, 16));
    assert_eq!(p_cd.target_rate, Some(TargetDspRate::Hz352800));
    assert_eq!(p_cd.target_rate_hz, 352_800);
    assert_eq!(p_cd.ratio, UpsampleRatio::Integer(8));
    assert_eq!(p_cd.filter, FilterArchetype::GaussLongLp);
    assert_eq!(p_cd.target_audiophile_filter, FilterArchetype::GaussLongLp);
    assert_eq!(p_cd.shaper, NoiseShaperProfile::NsGentle2);
    assert!(p_cd.tap_budget > 0 && p_cd.tap_budget <= MAX_FILTER_TAPS);
    assert!(!p_cd.filter_overridden);

    // 2. 24/48 -> 8x 384 kHz f64, default gauss-long-lp
    let p_48 = picker.select(&track(48_000, 24));
    assert_eq!(p_48.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p_48.ratio, UpsampleRatio::Integer(8));
    assert_eq!(p_48.filter, FilterArchetype::GaussLongLp);

    // 3. 24/88.2 & 24/96 HiRes -> 4x 352.8/384 kHz f64, default gauss-hires-lp
    let p_88 = picker.select(&track(88_200, 24));
    assert_eq!(p_88.target_rate, Some(TargetDspRate::Hz352800));
    assert_eq!(p_88.ratio, UpsampleRatio::Integer(4));
    assert_eq!(p_88.filter, FilterArchetype::GaussHiresLp);

    let p_96 = picker.select(&track(96_000, 24));
    assert_eq!(p_96.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p_96.target_rate_hz, 384_000);
    assert_eq!(p_96.ratio, UpsampleRatio::Integer(4));
    assert_eq!(p_96.filter, FilterArchetype::GaussHiresLp);
    assert_eq!(p_96.target_audiophile_filter, FilterArchetype::GaussHiresLp);

    // 4. 24/176.4 & 24/192 HiRes -> 2x 352.8/384 kHz f64, default gauss-hires-lp
    let p_176 = picker.select(&track(176_400, 24));
    assert_eq!(p_176.ratio, UpsampleRatio::Integer(2));
    assert_eq!(p_176.filter, FilterArchetype::GaussHiresLp);

    let p_192 = picker.select(&track(192_000, 24));
    assert_eq!(p_192.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p_192.ratio, UpsampleRatio::Integer(2));
    assert_eq!(p_192.filter, FilterArchetype::GaussHiresLp);

    // 5. 32/384 & 32/352.8 pre-upsampled PGGB FLAC -> Null 1:1 f64 upsampler
    let p_384 = picker.select(&track(384_000, 32));
    assert_eq!(p_384.target_rate, Some(TargetDspRate::Hz384000));
    assert_eq!(p_384.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p_384.filter, FilterArchetype::Null);
    assert_eq!(p_384.shaper, NoiseShaperProfile::Bypass);
    assert_eq!(p_384.tap_budget, 0);

    let p_352 = picker.select(&track(352_800, 32));
    assert_eq!(p_352.target_rate, Some(TargetDspRate::Hz352800));
    assert_eq!(p_352.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p_352.filter, FilterArchetype::Null);

    // Distinct target rates & filters across playlist
    assert_ne!(p_cd.target_rate, p_96.target_rate);
    assert_ne!(p_cd.filter, p_96.filter);
    assert_ne!(p_192.filter, p_384.filter);
}

#[test]
fn dummy_default_selector_ignores_apodizing_flag_while_custom_selector_can_plug_in() {
    // Phase 1 dummy DefaultFilterSelector always picks the researched per-rate default.
    let default_picker = UpsamplerPicker::with_selector(
        UpsamplerPolicy::default(),
        Arc::new(DefaultFilterSelector),
    )
    .unwrap();
    let mut bad_master = track(96_000, 24);
    bad_master.apodizing_needed = true;
    let plan = default_picker.select(&bad_master);
    assert_eq!(plan.filter, FilterArchetype::GaussHiresLp);

    // Custom FilterSelector plugs cleanly into UpsamplerPicker (Phase 5 SmartFilterSelector hook).
    struct StubSmartSelector;
    impl FilterSelector for StubSmartSelector {
        fn name(&self) -> &'static str {
            "stub-smart"
        }
        fn select_filter(
            &self,
            source: &TrackSourceInfo,
            ratio: UpsampleRatio,
        ) -> (FilterArchetype, String) {
            if ratio == UpsampleRatio::Bypass1x {
                (FilterArchetype::Null, "stub: bypass".to_string())
            } else if source.apodizing_needed {
                (FilterArchetype::GaussApod4M, "stub: apodizing".to_string())
            } else {
                (FilterArchetype::GaussLongLp, "stub: default".to_string())
            }
        }
    }

    let custom_picker =
        UpsamplerPicker::with_selector(UpsamplerPolicy::default(), Arc::new(StubSmartSelector))
            .unwrap();
    assert_eq!(custom_picker.selector_name(), "stub-smart");
    let custom_plan = custom_picker.select(&bad_master);
    assert_eq!(custom_plan.filter, FilterArchetype::GaussApod4M);
    assert_eq!(custom_plan.target_audiophile_filter, FilterArchetype::GaussApod4M);
    assert!(custom_plan.reason.contains("stub: apodizing"));
}

#[test]
fn cli_filter_and_shaper_overrides_are_enforced_across_tracks() {
    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SincSteep4M),
        shaper_override: Some(NoiseShaperProfile::Lns15Smooth),
        max_taps: MAX_FILTER_TAPS,
        ..UpsamplerPolicy::default()
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
        ..UpsamplerPolicy::default()
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

    // In-range non-integer rates (e.g. 50 kHz, 37.8 kHz) also reject cleanly with
    // UNSUPPORTED_SAMPLE_RATE_PREFIX so Random/Sequential can skip them without pausing.
    for &odd_rate in &[37_800_u32, 50_000] {
        let spec = StreamSpec {
            sample_rate_hz: odd_rate,
            bits_per_sample: 24,
            channels: 2,
        };
        let err = pipeline
            .start_track(spec, &TrackPeakMetadata::default(), false)
            .err()
            .expect("default upsampler must reject non-integer rate");
        assert!(
            err.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX),
            "unexpected err for {odd_rate} Hz: {err}"
        );
    }

    // Valid low integer rate 8 kHz (48x <= 64) still succeeds cleanly under default gauss-long-lp!
    let p_8k = picker.select(&track(8_000, 16));
    assert_eq!(p_8k.ratio, UpsampleRatio::Integer(48));
    assert_eq!(p_8k.target_rate_hz, 384_000);
    assert_eq!(p_8k.filter, FilterArchetype::GaussLongLp);
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
        ..UpsamplerPolicy::default()
    };
    let picker = UpsamplerPicker::new(custom).unwrap();
    let p = picker.select(&track(44_100, 16));
    assert!(p.tap_budget <= 1_000_000 && p.tap_budget >= 999_980 && p.tap_budget % 2 == 1);
    assert_eq!(p.dac_bits, 24);
    assert_eq!(p.shaper, NoiseShaperProfile::NsGentle2);

    // Non-8fS `--filter null` pass-through in auto shaper mode cleanly picks TpdfDither.
    let null_auto_picker = UpsamplerPicker::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::Null),
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let p_null_44 = null_auto_picker.select(&track(44_100, 16));
    assert_eq!(p_null_44.target_rate_hz, 44_100);
    assert_eq!(p_null_44.shaper, NoiseShaperProfile::TpdfDither);

    // Invalid dac_bits rejected; dac_bits <= 20 auto-selects NsGentle2 per INV-04 §6.6.
    for bad_bits in [0_u8, 12, 15, 22, 32] {
        assert!(
            UpsamplerPicker::new(UpsamplerPolicy {
                dac_bits: bad_bits,
                ..UpsamplerPolicy::default()
            })
            .is_err(),
            "dac_bits={bad_bits} must be rejected"
        );
    }
    for low_bits in [16_u8, 18, 20] {
        let low_picker = UpsamplerPicker::new(UpsamplerPolicy {
            dac_bits: low_bits,
            ..UpsamplerPolicy::default()
        })
        .unwrap();
        let plan = low_picker.select(&track(44_100, 16));
        assert_eq!(plan.dac_bits, low_bits);
        assert_eq!(plan.shaper, NoiseShaperProfile::NsGentle2);
        assert!(!plan.shaper_overridden);
    }
}

#[test]
fn cli_names_and_compatibility_aliases_parse_roundtrip_for_all_filters_and_shapers() {
    for f in [
        FilterArchetype::Null,
        FilterArchetype::SimpleTest,
        FilterArchetype::GaussLongLp,
        FilterArchetype::GaussHiresLp,
        FilterArchetype::GaussApod4M,
        FilterArchetype::SincSteep4M,
        FilterArchetype::ExtSharpLp,
        FilterArchetype::MinphaseLong,
        FilterArchetype::IntermediateLong,
        FilterArchetype::ShortLp,
        FilterArchetype::ShortMp,
    ] {
        assert_eq!(FilterArchetype::from_str(f.cli_name()).unwrap(), f);
    }
    assert_eq!(FilterArchetype::from_str("bypass").unwrap(), FilterArchetype::Null);
    assert_eq!(
        FilterArchetype::from_str("gauss-hires-mp").unwrap(),
        FilterArchetype::MinphaseLong
    );
    assert_eq!(
        FilterArchetype::from_str("gauss-hires-ip").unwrap(),
        FilterArchetype::IntermediateLong
    );
    assert!(FilterArchetype::from_str("nonexistent").is_err());

    // Verify all curated filter archetypes are implemented (Phase 2 complete).
    assert!(FilterArchetype::Null.is_implemented());
    assert!(FilterArchetype::SimpleTest.is_implemented());
    assert!(FilterArchetype::GaussLongLp.is_implemented());
    assert!(FilterArchetype::GaussHiresLp.is_implemented());
    assert!(FilterArchetype::GaussApod4M.is_implemented());
    assert!(FilterArchetype::SincSteep4M.is_implemented());
    assert!(FilterArchetype::ExtSharpLp.is_implemented());
    assert!(FilterArchetype::MinphaseLong.is_implemented());
    assert!(FilterArchetype::IntermediateLong.is_implemented());
    assert!(FilterArchetype::ShortLp.is_implemented());
    assert!(FilterArchetype::ShortMp.is_implemented());

    for s in [
        NoiseShaperProfile::Bypass,
        NoiseShaperProfile::TpdfDither,
        NoiseShaperProfile::NsGentle2,
        NoiseShaperProfile::Ns5Multibit,
        NoiseShaperProfile::Lns15Smooth,
        NoiseShaperProfile::PggbOptimal,
    ] {
        assert_eq!(NoiseShaperProfile::from_str(s.cli_name()).unwrap(), s);
    }
    assert_eq!(
        NoiseShaperProfile::from_str("ns5-class").unwrap(),
        NoiseShaperProfile::Ns5Multibit
    );
    assert_eq!(
        NoiseShaperProfile::from_str("lns15-class").unwrap(),
        NoiseShaperProfile::Lns15Smooth
    );
    assert_eq!(
        NoiseShaperProfile::from_str("wide40-9").unwrap(),
        NoiseShaperProfile::PggbOptimal
    );
    assert!(NoiseShaperProfile::from_str("nonexistent").is_err());
}

#[test]
fn build_upsampler_low_f_eff_falls_back_not_errors_and_minphase_honours_f_eff_f11() {
    let spec_96 = StreamSpec {
        sample_rate_hz: 96_000,
        bits_per_sample: 24,
        channels: 2,
    };

    // 1. F11: Low/corrupt f_eff values below DR-1 floor at 96k->384k MUST gracefully degrade
    //    to native Nyquist instead of failing the track across all archetypes!
    for f_eff in [16_000_u32, 18_000, 19_000, 20_000] {
        for arch in [
            FilterArchetype::GaussLongLp,
            FilterArchetype::GaussHiresLp,
            FilterArchetype::ShortLp,
            FilterArchetype::ShortMp,
            FilterArchetype::MinphaseLong,
            FilterArchetype::IntermediateLong,
            FilterArchetype::GaussApod4M,
            FilterArchetype::SincSteep4M,
            FilterArchetype::ExtSharpLp,
        ] {
            let policy = UpsamplerPolicy {
                filter_override: Some(arch),
                max_taps: 16_384, // keep mega-tap designs fast in unit tests
                ..UpsamplerPolicy::default()
            };
            let picker = UpsamplerPicker::new(policy).unwrap();
            let mut src = TrackSourceInfo::from_stream_spec(spec_96);
            src.effective_nyquist_hz = Some(f_eff);
            let plan = picker.select(&src);
            let built = build_upsampler(&plan);
            assert!(
                built.is_ok(),
                "F11: archetype {arch} with f_eff={f_eff} Hz must fall back cleanly, got {:?}",
                built.err()
            );
        }
    }

    // 2. F11: MinphaseLong & IntermediateLong honor valid fake-hi-res f_eff = 22,050 Hz!
    for arch in [FilterArchetype::MinphaseLong, FilterArchetype::IntermediateLong] {
        let policy = UpsamplerPolicy {
            filter_override: Some(arch),
            ..UpsamplerPolicy::default()
        };
        let picker = UpsamplerPicker::new(policy).unwrap();
        let mut src = TrackSourceInfo::from_stream_spec(spec_96);
        src.effective_nyquist_hz = Some(22_050);
        let plan = picker.select(&src);
        let up = build_upsampler(&plan).expect("minphase/intermediate build failed");
        assert_eq!(
            up.identity().effective_nyquist_hz,
            Some(22_050),
            "F11: {arch} must honor effective_nyquist_hz=22050"
        );
    }
}
