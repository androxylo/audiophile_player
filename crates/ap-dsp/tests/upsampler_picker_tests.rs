//! TDD unit tests for the dynamic per-track `UpsamplerPicker` and CLI overrides.

use std::str::FromStr;

use ap_core::format::TargetDspRate;
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::selector::{
    FilterArchetype, NoiseShaperProfile, TrackSourceInfo, UpsampleRatio, UpsamplerPicker,
    UpsamplerPolicy,
};

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
fn auto_picker_switches_upsampler_dynamically_across_different_track_rates() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();

    // 1. 16/44.1 Redbook -> 8x 352.8 kHz, GaussApod4M + NS5
    let p_cd = picker.select(&track(44_100, 16));
    assert_eq!(p_cd.target_rate, TargetDspRate::Hz352800);
    assert_eq!(p_cd.target_rate_hz, 352_800);
    assert_eq!(p_cd.ratio, UpsampleRatio::Integer(8));
    assert_eq!(p_cd.filter, FilterArchetype::GaussApod4M);
    assert_eq!(p_cd.shaper, NoiseShaperProfile::Ns5Multibit);
    assert!(p_cd.tap_budget > 0 && p_cd.tap_budget <= MAX_FILTER_TAPS);
    assert!(!p_cd.filter_overridden);

    // 2. 24/96 HiRes -> 4x 384 kHz, GaussLongLp + NS5
    let p_96 = picker.select(&track(96_000, 24));
    assert_eq!(p_96.target_rate, TargetDspRate::Hz384000);
    assert_eq!(p_96.target_rate_hz, 384_000);
    assert_eq!(p_96.ratio, UpsampleRatio::Integer(4));
    assert_eq!(p_96.filter, FilterArchetype::GaussLongLp);
    assert_eq!(p_96.shaper, NoiseShaperProfile::Ns5Multibit);
    assert!(p_96.tap_budget <= MAX_FILTER_TAPS);

    // 3. 24/192 HiRes -> 2x 384 kHz, GaussHiresIp + NS5
    let p_192 = picker.select(&track(192_000, 24));
    assert_eq!(p_192.target_rate, TargetDspRate::Hz384000);
    assert_eq!(p_192.ratio, UpsampleRatio::Integer(2));
    assert_eq!(p_192.filter, FilterArchetype::GaussHiresIp);

    // 4. 32/384 pre-upsampled PGGB FLAC -> 1x Bypass + Bypass shaper (never double-upsample!)
    let p_384 = picker.select(&track(384_000, 32));
    assert_eq!(p_384.target_rate, TargetDspRate::Hz384000);
    assert_eq!(p_384.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p_384.filter, FilterArchetype::Bypass);
    assert_eq!(p_384.shaper, NoiseShaperProfile::Bypass);
    assert_eq!(p_384.tap_budget, 0);

    // 5. 32/352.8 pre-upsampled PGGB FLAC -> 1x Bypass
    let p_352 = picker.select(&track(352_800, 32));
    assert_eq!(p_352.target_rate, TargetDspRate::Hz352800);
    assert_eq!(p_352.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p_352.filter, FilterArchetype::Bypass);

    // Confirm distinct filters were selected across the playlist sequence
    assert_ne!(p_cd.filter, p_96.filter);
    assert_ne!(p_96.filter, p_192.filter);
    assert_ne!(p_192.filter, p_384.filter);
}

#[test]
fn apodizing_flag_overrides_hires_auto_filter_to_gauss_apod() {
    let picker = UpsamplerPicker::new(UpsamplerPolicy::default()).unwrap();
    let mut bad_master = track(96_000, 24);
    bad_master.apodizing_needed = true;

    let plan = picker.select(&bad_master);
    assert_eq!(plan.filter, FilterArchetype::GaussApod4M);
    assert!(plan.reason.contains("apodizing"));
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

    // Already 384 kHz must still stay 1x bypass (cannot upsample 1:1), while shaper override applies.
    let plan_384 = picker.select(&track(384_000, 32));
    assert_eq!(plan_384.filter, FilterArchetype::Bypass);
    assert_eq!(plan_384.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(plan_384.shaper, NoiseShaperProfile::Lns15Smooth);
}

#[test]
fn cli_bypass_override_disables_upsampling_on_all_rates() {
    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::Bypass),
        shaper_override: Some(NoiseShaperProfile::Bypass),
        max_taps: MAX_FILTER_TAPS,
    };
    let picker = UpsamplerPicker::new(policy).unwrap();

    let p = picker.select(&track(44_100, 16));
    assert_eq!(p.filter, FilterArchetype::Bypass);
    assert_eq!(p.ratio, UpsampleRatio::Bypass1x);
    assert_eq!(p.target_rate_hz, 44_100);
    assert_eq!(p.tap_budget, 0);
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
        FilterArchetype::Bypass,
        FilterArchetype::GaussApod4M,
        FilterArchetype::SincSteep4M,
        FilterArchetype::GaussLongLp,
        FilterArchetype::GaussHiresIp,
        FilterArchetype::GaussHiresMp,
        FilterArchetype::ExtSharpLp,
    ] {
        assert_eq!(FilterArchetype::from_str(f.cli_name()).unwrap(), f);
    }
    assert!(FilterArchetype::from_str("nonexistent").is_err());

    for s in [
        NoiseShaperProfile::Bypass,
        NoiseShaperProfile::Ns5Multibit,
        NoiseShaperProfile::Lns15Smooth,
        NoiseShaperProfile::PggbOptimal,
        NoiseShaperProfile::TpdfDither,
    ] {
        assert_eq!(NoiseShaperProfile::from_str(s.cli_name()).unwrap(), s);
    }
    assert!(NoiseShaperProfile::from_str("nonexistent").is_err());
}
