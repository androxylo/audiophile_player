//! Phase 6 TDD suite: `UltimateSelector`, §3 decision table, M3 `< 30 s` bypass, M4 streaming
//! guard against `gauss-apod-4m`, `OQ-34`, `OQ-35` genre rules (`A > L > G > R`), `OQ-37` HF
//! noise filter, user overrides, and §4.3 throughput self-check refusal.

use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;
use ap_core::metadata::{GenreClass, PhaseTagOverride, TrackMetadata};
use ap_dsp::analysis::{AnalysisResult, DetectorVerdict};
use ap_dsp::fusion::{CachedWholeTrackEvidence, fuse_track_evidence};
use ap_dsp::selector::{
    FilterArchetype, UpsamplerPolicy, build_post_chain_key, check_ultimate_throughput,
    select_ultimate_plan,
};

fn clean_analysis(rate: u32, bits: u16, dur_s: f64) -> AnalysisResult {
    AnalysisResult {
        sample_rate_hz: rate,
        container_bits: bits,
        analyzed_frames: (rate as f64 * dur_s) as u64,
        duration_s: dur_s,
        is_whole_track: false,
        effective_bandwidth_hz: rate / 2,
        upsampled_from_1x: DetectorVerdict::Negative,
        effective_nyquist_hz: None,
        lossy_origin_suspected: false,
        ultrasonic_noise_rise_db: 0.0,
        ultrasonic_noise_level_dbfs: -130.0,
        ultrasonic_noise_detected: DetectorVerdict::Negative,
        onset_count: 10,
        apod_events: 0,
        apodizing_needed: DetectorVerdict::Negative,
        integrated_lufs: Some(-14.0),
        sample_peak_linear: 0.8,
        true_peak_linear: 0.8,
        true_peak_dbtp: -1.94,
        clipped_runs: 0,
        loud_and_clipped: DetectorVerdict::Negative,
        effective_bits: bits,
        pre_emphasis_shelf_suspected: false,
    }
}

#[test]
fn short_tracks_below_30s_take_normal_mode_path_and_label_m3() {
    let spec = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let mut w = clean_analysis(44_100, 16, 18.0);
    w.apodizing_needed = DetectorVerdict::Positive;
    let fused = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &TrackMetadata::default(),
        Some(&w),
        None,
    );

    let sel = select_ultimate_plan(
        spec,
        false,
        Some(18.0),
        &fused,
        &UpsamplerPolicy::default(),
        true,
        false,
    )
    .unwrap();
    assert!(sel.short_track_bypass);
    assert_eq!(sel.mode_label, "normal (<30 s)");
    assert_eq!(sel.plan.filter, FilterArchetype::GaussLongLp);
}

#[test]
fn decision_table_priority_order_a_over_l_over_g_over_r_and_streaming_m4_guard() {
    let spec_1x = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let policy = UpsamplerPolicy::default();

    // 1. Clean 1x local -> #4 sinc-steep-4m; clean 1x stream -> #1 gauss-long-lp.
    let w_clean = clean_analysis(44_100, 16, 180.0);
    let fused_clean = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &TrackMetadata::default(),
        Some(&w_clean),
        None,
    );
    let local_clean =
        select_ultimate_plan(spec_1x, false, Some(180.0), &fused_clean, &policy, true, false)
            .unwrap();
    assert_eq!(local_clean.plan.filter, FilterArchetype::SincSteep4M);
    let stream_clean =
        select_ultimate_plan(spec_1x, true, Some(180.0), &fused_clean, &policy, true, false)
            .unwrap();
    assert_eq!(stream_clean.plan.filter, FilterArchetype::GaussLongLp);

    // 2. Rule G (genre_rules = true): Jazz/Acoustic -> #1 gauss-long-lp, Rock/Pop -> #5 ext-sharp-lp.
    let jazz_meta = TrackMetadata {
        genre_class: GenreClass::JazzAcoustic,
        ..TrackMetadata::default()
    };
    let fused_jazz = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &jazz_meta,
        Some(&w_clean),
        None,
    );
    let sel_jazz =
        select_ultimate_plan(spec_1x, false, Some(180.0), &fused_jazz, &policy, true, false)
            .unwrap();
    assert_eq!(sel_jazz.plan.filter, FilterArchetype::GaussLongLp);

    // With genre_rules = false -> falls through to clean 1x #4 sinc-steep-4m!
    let sel_jazz_off =
        select_ultimate_plan(spec_1x, false, Some(180.0), &fused_jazz, &policy, false, false)
            .unwrap();
    assert_eq!(sel_jazz_off.plan.filter, FilterArchetype::SincSteep4M);

    let rock_meta = TrackMetadata {
        genre_class: GenreClass::RockPopElectronic,
        ..TrackMetadata::default()
    };
    let fused_rock = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &rock_meta,
        Some(&w_clean),
        None,
    );
    let sel_rock =
        select_ultimate_plan(spec_1x, false, Some(180.0), &fused_rock, &policy, true, false)
            .unwrap();
    assert_eq!(sel_rock.plan.filter, FilterArchetype::ExtSharpLp);

    // 3. Rule L beats Rule G: Loud & clipped Rock -> #3 gauss-apod-4m (local), #1 gauss-long-lp (stream).
    let mut w_loud = clean_analysis(44_100, 16, 180.0);
    w_loud.loud_and_clipped = DetectorVerdict::Positive;
    let fused_loud_rock = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &rock_meta,
        Some(&w_loud),
        None,
    );
    let sel_loud_local =
        select_ultimate_plan(spec_1x, false, Some(180.0), &fused_loud_rock, &policy, true, false)
            .unwrap();
    assert_eq!(sel_loud_local.plan.filter, FilterArchetype::GaussApod4M);

    let sel_loud_stream =
        select_ultimate_plan(spec_1x, true, Some(180.0), &fused_loud_rock, &policy, true, false)
            .unwrap();
    assert_eq!(sel_loud_stream.plan.filter, FilterArchetype::GaussLongLp);
    assert!(sel_loud_stream.plan.filter.is_streaming_safe());

    // 4. Rule A beats Rule G & Rule L: Apodizing -> #3 gauss-apod-4m (local), #1 gauss-long-lp (stream).
    let mut w_apod = clean_analysis(44_100, 16, 180.0);
    w_apod.apodizing_needed = DetectorVerdict::Positive;
    let fused_apod = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &jazz_meta,
        Some(&w_apod),
        None,
    );
    let sel_apod_local =
        select_ultimate_plan(spec_1x, false, Some(180.0), &fused_apod, &policy, true, false)
            .unwrap();
    assert_eq!(sel_apod_local.plan.filter, FilterArchetype::GaussApod4M);
    let sel_apod_stream =
        select_ultimate_plan(spec_1x, true, Some(180.0), &fused_apod, &policy, true, false)
            .unwrap();
    assert_eq!(sel_apod_stream.plan.filter, FilterArchetype::GaussLongLp);
}

#[test]
fn hires_hf_noise_filter_fake_hires_and_user_overrides_win_deterministically() {
    let spec_192 = StreamSpec {
        sample_rate_hz: 192_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let policy = UpsamplerPolicy::default();

    // 1. OQ-37: Genuine 192 kHz track with >40 kHz ultrasonic ADC/DSD noise -> gauss-hires-lp + hf_noise_filter=true.
    let mut w_hf = clean_analysis(192_000, 24, 240.0);
    w_hf.ultrasonic_noise_detected = DetectorVerdict::Positive;
    let fused_hf = fuse_track_evidence(
        192_000,
        24,
        &TrackPeakMetadata::default(),
        &TrackMetadata::default(),
        Some(&w_hf),
        None,
    );
    let sel_hf =
        select_ultimate_plan(spec_192, false, Some(240.0), &fused_hf, &policy, true, false)
            .unwrap();
    assert_eq!(sel_hf.plan.filter, FilterArchetype::GaussHiresLp);
    assert!(sel_hf.plan.hf_noise_filter);

    // 2. Fake hi-res (Rule H): 192 kHz container with f_eff = 22050 Hz -> treated as 1x (#4 sinc-steep-4m).
    let mut w_fake = clean_analysis(192_000, 24, 240.0);
    w_fake.upsampled_from_1x = DetectorVerdict::Positive;
    w_fake.effective_nyquist_hz = Some(22_050);
    let fused_fake = fuse_track_evidence(
        192_000,
        24,
        &TrackPeakMetadata::default(),
        &TrackMetadata::default(),
        Some(&w_fake),
        None,
    );
    let sel_fake =
        select_ultimate_plan(spec_192, false, Some(240.0), &fused_fake, &policy, true, false)
            .unwrap();
    assert_eq!(sel_fake.plan.filter, FilterArchetype::SincSteep4M);
    assert_eq!(sel_fake.plan.effective_nyquist_hz, Some(22_050));

    // 3. Explicit AP_PHASE=min -> #6 minphase-long; YAML user.filter beats AP_FILTER; CLI beats YAML.
    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let phase_meta = TrackMetadata {
        ap_phase: Some(PhaseTagOverride::Minimum),
        ..TrackMetadata::default()
    };
    let fused_phase = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &phase_meta,
        Some(&clean_analysis(44_100, 16, 180.0)),
        None,
    );
    let sel_phase =
        select_ultimate_plan(spec_44, false, Some(180.0), &fused_phase, &policy, true, false)
            .unwrap();
    assert_eq!(sel_phase.plan.filter, FilterArchetype::MinphaseLong);

    let wt_override = CachedWholeTrackEvidence {
        user_filter: Some("short-lp".to_string()),
        ..CachedWholeTrackEvidence::default()
    };
    let tag_override = TrackMetadata {
        ap_filter: Some("ext-sharp-lp".to_string()),
        ..TrackMetadata::default()
    };
    let fused_ovr = fuse_track_evidence(
        44_100,
        16,
        &TrackPeakMetadata::default(),
        &tag_override,
        Some(&clean_analysis(44_100, 16, 180.0)),
        Some(&wt_override),
    );
    let sel_yaml =
        select_ultimate_plan(spec_44, false, Some(180.0), &fused_ovr, &policy, true, true)
            .unwrap();
    assert_eq!(sel_yaml.plan.filter, FilterArchetype::ShortLp);
    assert_eq!(sel_yaml.mode_label, "ultimate: cached");

    // CLI override beats YAML user.filter!
    let cli_policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::IntermediateLong),
        ..UpsamplerPolicy::default()
    };
    let sel_cli =
        select_ultimate_plan(spec_44, false, Some(180.0), &fused_ovr, &cli_policy, true, true)
            .unwrap();
    assert_eq!(sel_cli.plan.filter, FilterArchetype::IntermediateLong);

    // M4 guard: even CLI --filter gauss-apod-4m on a streaming source falls back cleanly to gauss-long-lp!
    let bad_stream_policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::GaussApod4M),
        ..UpsamplerPolicy::default()
    };
    let sel_guard = select_ultimate_plan(
        spec_44,
        true,
        Some(180.0),
        &fused_ovr,
        &bad_stream_policy,
        true,
        false,
    )
    .unwrap();
    assert_eq!(sel_guard.plan.filter, FilterArchetype::GaussLongLp);
    assert!(
        sel_guard
            .warnings
            .iter()
            .any(|w| w.contains("M4 streaming guard")),
        "expected M4 streaming guard warning"
    );
}

#[test]
fn throughput_self_check_passes_ryzen_speed_and_loudly_refuses_too_slow_filter() {
    // Ryzen 9 7900 measured ~20x RT -> passes with huge margin.
    let ok = check_ultimate_throughput(
        FilterArchetype::GaussApod4M,
        44_100,
        30.0,
        1.5,
        Some(240.0),
    );
    assert!(ok.passed);
    assert!(ok.fallback_filter.is_none());
    assert!(ok.measured_x_rt > 19.0);

    // Hypothetical slow CPU running 30 s audio in 40 s wall (0.75x RT) -> REFUSED loudly!
    let slow = check_ultimate_throughput(
        FilterArchetype::SincSteep4M,
        44_100,
        30.0,
        40.0,
        Some(240.0),
    );
    assert!(!slow.passed);
    assert_eq!(slow.fallback_filter, Some(FilterArchetype::GaussLongLp));
    assert!(slow.detail.contains("REFUSED"));
}

#[test]
fn post_chain_key_helper_is_deterministic_and_captures_all_chain_axes() {
    let spec = StreamSpec {
        sample_rate_hz: 96_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let mut w = clean_analysis(96_000, 24, 180.0);
    w.upsampled_from_1x = DetectorVerdict::Positive;
    w.effective_nyquist_hz = Some(22_050);
    let meta = TrackMetadata {
        ap_deemphasis: Some(true),
        ..TrackMetadata::default()
    };
    let fused = fuse_track_evidence(
        96_000,
        24,
        &TrackPeakMetadata::default(),
        &meta,
        Some(&w),
        None,
    );
    let sel = select_ultimate_plan(
        spec,
        false,
        Some(180.0),
        &fused,
        &UpsamplerPolicy::default(),
        true,
        false,
    )
    .unwrap();
    let k1 = build_post_chain_key(
        &sel.plan,
        spec.sample_rate_hz,
        sel.plan.target_rate_hz,
        Some("abc123"),
    );
    let k2 = build_post_chain_key(
        &sel.plan,
        spec.sample_rate_hz,
        sel.plan.target_rate_hz,
        Some("abc123"),
    );
    assert_eq!(k1, k2);
    assert_eq!(k1.upsampler, "sinc-steep-4m");
    assert_eq!(k1.f_eff_hz, 22_050);
    assert_eq!(k1.output_rate_hz, 384_000);
    assert!(k1.deemphasis);
    assert_eq!(k1.rew_filter_hash, "abc123");
}
