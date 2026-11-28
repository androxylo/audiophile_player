// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! **L3 · Scan and Playback Agreement Enforcement (R4 / U2 class)** (`PIPELINE_TESTS_REVIEW.md` §3).
//!
//! Verifies for the exact same FLAC file and DSP settings:
//! 1. `scanner::scan_folder --post-chain` and `ultimate::stream_track_ultimate` store identical
//!    `suggested`, `rule`, `PostChainKey`, and post-chain peak `M` (within `1e-9 dB`).
//! 2. `scan --filter X` (`UpsamplerPolicy { filter_override: Some(X), .. }`) NEVER overwrites
//!    `suggested` or `rule` with the forced override `X` (U2).
//! 3. Downsampled playback (`output_rate_hz != 8fS`) keys its peak to `output_rate_hz` so it
//!    never mistakenly matches an 8fS scanner entry.

use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ap_control::state::PlayModeLabel;
use ap_core::config::{ConvolutionConfig, UltimateConfig};
use ap_core::filter_db::{FilterDbStore, LibraryManifest, TRUE_PEAK_METER_8X_64};
use ap_core::format::{DacOutputCapabilities, DacWireFormat};
use ap_core::user_filters::FilterMode;
use ap_dsp::analysis::DETECTOR_VERSION;
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{FilterArchetype, UpsamplerPolicy, build_post_chain_key};
use ap_player::prefetch::{
    BoundedBlockStreamBuffer, StreamEvent, ULTIMATE_MAX_BUFFERED_FRAMES,
    ULTIMATE_MAX_BUFFERED_SECONDS,
};
use ap_player::scanner::{ScanOptions, scan_folder};
use ap_player::ultimate::{
    E2_MIN_SPEED_X_RT, NextTrackPreScanner, PreScanOutcome, UltimateRuntimeContext,
    UltimateStreamResult, prescan_local_track_post_chain, stream_track_ultimate,
};
use ap_source::flac::FlacBlockReader;
use flac_codec::encode::{FlacSampleWriter, Options};

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ap_l3_{label}_{}_{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a real stereo 16-bit 44.1 kHz FLAC file of `duration_s` seconds (with valid MD5).
fn write_stereo_flac_16(path: &Path, sample_rate_hz: u32, duration_s: usize) {
    let total_frames = (sample_rate_hz as usize) * duration_s;
    let mut writer = FlacSampleWriter::create(
        path,
        Options::default(),
        sample_rate_hz,
        16,
        2,
        Some((total_frames * 2) as u64),
    )
    .expect("create flac writer");

    let chunk = 8_192_usize;
    let mut interleaved = vec![0_i32; chunk * 2];
    let mut pos = 0_usize;
    while pos < total_frames {
        let take = (total_frames - pos).min(chunk);
        for i in 0..take {
            let t = ((pos + i) as f64) / f64::from(sample_rate_hz);
            // Smooth multi-tone chord peaking near -4 dBFS (clean 1x material).
            let l = 0.45 * (2.0 * PI * 440.0 * t).sin() + 0.15 * (2.0 * PI * 1_320.0 * t).sin();
            let r = 0.42 * (2.0 * PI * 554.37 * t).sin() + 0.16 * (2.0 * PI * 1_661.22 * t).sin();
            interleaved[i * 2] = (l * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i32;
            interleaved[i * 2 + 1] = (r * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i32;
        }
        writer
            .write(&interleaved[..take * 2])
            .expect("write flac samples");
        pos += take;
    }
    writer.finalize().expect("finalize flac");
}

fn drain_track(buf: &BoundedBlockStreamBuffer, ep: u64) {
    loop {
        match buf.pop_for_epoch_timeout(ep, Duration::from_secs(60)) {
            Some(StreamEvent::TrackEnd) => break,
            Some(StreamEvent::TrackStart { .. }) | Some(StreamEvent::PcmBlock(_)) => {}
            other => panic!("unexpected event while draining L3 track: {other:?}"),
        }
    }
}

#[test]
fn l3_scan_and_ultimate_playback_agree_and_cli_filter_preserves_suggested() {
    let root = temp_dir("agree");
    let music_dir = root.join("music");
    fs::create_dir_all(&music_dir).unwrap();
    let flac_path = music_dir.join("01_clean_chord_31s.flac");
    write_stereo_flac_16(&flac_path, 44_100, 31);

    // Use a fast curated filter cap (65,536 taps) identical between scan and playback so
    // the 31 s 8x render finishes in < 3 s while exercising real upsampling & post-chain M.
    let shared_policy = UpsamplerPolicy {
        max_taps: 65_536,
        ..UpsamplerPolicy::default()
    };

    // --- 1. Offline scan --post-chain into filterdb_scan ---
    let filterdb_scan = root.join("filterdb_scan");
    let cfg_path = root.join("empty_config.toml");
    fs::write(&cfg_path, "").unwrap();
    let scan_opts = ScanOptions {
        folder: Some(music_dir.clone()),
        report: false,
        post_chain: true,
        force: true,
        jobs: Some(1),
        trackdb_dir: Some(root.join("trackdb_scan")),
        filterdb_dir: Some(filterdb_scan.clone()),
        choices_file: Some(root.join("no_choices.yaml")),
        config_path: Some(cfg_path),
        rew_override: Some(ConvolutionConfig::disabled()),
        upsampler_policy: shared_policy,
    };
    let summary = scan_folder(&scan_opts, None).expect("scan_folder failed");
    assert_eq!(summary.scanned, 1);
    assert_eq!(summary.failed, 0);

    let scan_store = FilterDbStore::new(filterdb_scan);
    let scan_album = scan_store
        .load_album("main", "")
        .expect("scan album YAML missing");
    let scan_track = scan_album
        .get_track("01_clean_chord_31s.flac")
        .expect("scanned track missing");
    assert_eq!(
        scan_track.post_chain_peaks.len(),
        1,
        "expected exactly 1 post-chain peak from scan"
    );

    // --- 2. Ultimate playback learn-on-play into filterdb_play ---
    let filterdb_play = root.join("filterdb_play");
    let mut manifest = LibraryManifest::new(DETECTOR_VERSION);
    manifest.upsert_root("main", &music_dir);
    let ult_ctx = UltimateRuntimeContext::new(
        UltimateConfig {
            enabled: true,
            genre_rules: true,
            learn_on_play: true,
        },
        FilterDbStore::new(filterdb_play.clone()),
        manifest,
        root.join("no_choices.yaml"),
        FilterMode::Suggested,
    );
    let pipeline = AudioPipeline::with_rew_config_full_tail(
        shared_policy,
        ConvolutionConfig::disabled(),
        true,
    )
    .unwrap();
    let buf = BoundedBlockStreamBuffer::with_limits(
        ULTIMATE_MAX_BUFFERED_FRAMES,
        Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
    );
    let ep = buf.current_epoch();
    let buf_c = Arc::clone(&buf);
    let drain_h = std::thread::spawn(move || drain_track(&buf_c, ep));
    let mut reader = FlacBlockReader::open(&flac_path).expect("open flac");
    let mut carry = None;
    let mut backup = None;
    let mut pend = false;
    let res = stream_track_ultimate(
        &pipeline,
        &ult_ctx,
        &buf,
        ep,
        || false,
        &mut reader,
        &flac_path,
        Some(&flac_path),
        None,
        None,
        PlayModeLabel::SingleFile,
        None,
        &mut carry,
        &mut backup,
        &mut pend,
        false,
        16_384,
    );
    drain_h.join().unwrap();
    assert!(
        matches!(res, UltimateStreamResult::Played { wrote_yaml: true, .. }),
        "expected clean Played with learn-on-play YAML, got {res:?}"
    );

    let play_store = FilterDbStore::new(filterdb_play);
    let play_album = play_store
        .load_album("main", "")
        .expect("play album YAML missing");
    let play_track = play_album
        .get_track("01_clean_chord_31s.flac")
        .expect("played track missing");
    assert_eq!(play_track.post_chain_peaks.len(), 1);

    // Assert exact agreement between scanner and Ultimate playback!
    assert_eq!(
        scan_track.suggested, play_track.suggested,
        "L3: scan suggested ({}) != playback suggested ({})",
        scan_track.suggested, play_track.suggested
    );
    assert_eq!(
        scan_track.rule, play_track.rule,
        "L3: scan rule ({}) != playback rule ({})",
        scan_track.rule, play_track.rule
    );
    assert_eq!(
        scan_track.post_chain_peaks[0].key, play_track.post_chain_peaks[0].key,
        "L3: scan PostChainKey != playback PostChainKey"
    );
    let m_diff = (scan_track.post_chain_peaks[0].peak_dbfs
        - play_track.post_chain_peaks[0].peak_dbfs)
        .abs();
    assert!(
        m_diff <= 1e-9,
        "L3: scan M ({:.10}) != playback M ({:.10}), diff={m_diff:.3e} dB",
        scan_track.post_chain_peaks[0].peak_dbfs,
        play_track.post_chain_peaks[0].peak_dbfs
    );

    // --- 3. U2: `scan --filter short-lp` MUST leave `suggested` and `rule` unchanged! ---
    let filterdb_ov = root.join("filterdb_ov");
    let ov_opts = ScanOptions {
        filterdb_dir: Some(filterdb_ov.clone()),
        trackdb_dir: Some(root.join("trackdb_ov")),
        upsampler_policy: UpsamplerPolicy {
            filter_override: Some(FilterArchetype::ShortLp),
            ..shared_policy
        },
        ..scan_opts
    };
    let ov_sum = scan_folder(&ov_opts, None).expect("override scan failed");
    assert_eq!(ov_sum.scanned, 1);
    let ov_album = FilterDbStore::new(filterdb_ov)
        .load_album("main", "")
        .expect("override album YAML missing");
    let ov_track = ov_album
        .get_track("01_clean_chord_31s.flac")
        .expect("override track missing");
    assert_eq!(
        ov_track.suggested, scan_track.suggested,
        "L3/U2: --filter short-lp must NEVER overwrite machine suggested ({} vs {})",
        ov_track.suggested, scan_track.suggested
    );
    assert_eq!(ov_track.rule, scan_track.rule);
    assert!(
        ov_track
            .post_chain_peaks
            .iter()
            .any(|p| p.key.upsampler == "short-lp"),
        "expected forced short-lp post-chain peak alongside preserved suggestion"
    );

    // --- 4. Downsampled playback key (176.4 kHz) must NEVER collide with 8fS scanner key (352.8 kHz) ---
    let ds_caps =
        DacOutputCapabilities::from_supported_rates(&[(176_400, DacWireFormat::Int32)], None);
    let ds_pipeline = AudioPipeline::with_rew_config_full_tail(
        shared_policy,
        ConvolutionConfig::disabled(),
        true,
    )
    .unwrap()
    .with_dac_capabilities(ds_caps);
    let filterdb_ds = root.join("filterdb_ds");
    let mut ds_manifest = LibraryManifest::new(DETECTOR_VERSION);
    ds_manifest.upsert_root("main", &music_dir);
    let ds_ult = UltimateRuntimeContext::new(
        UltimateConfig {
            enabled: true,
            genre_rules: true,
            learn_on_play: true,
        },
        FilterDbStore::new(filterdb_ds.clone()),
        ds_manifest,
        root.join("no_choices.yaml"),
        FilterMode::Suggested,
    );
    let ep2 = buf.reset_and_flush();
    let buf_c2 = Arc::clone(&buf);
    let drain_h2 = std::thread::spawn(move || drain_track(&buf_c2, ep2));
    let mut reader2 = FlacBlockReader::open(&flac_path).expect("open flac 2");
    let _ = stream_track_ultimate(
        &ds_pipeline,
        &ds_ult,
        &buf,
        ep2,
        || false,
        &mut reader2,
        &flac_path,
        Some(&flac_path),
        None,
        None,
        PlayModeLabel::SingleFile,
        None,
        &mut carry,
        &mut backup,
        &mut pend,
        false,
        16_384,
    );
    drain_h2.join().unwrap();
    let ds_album = FilterDbStore::new(filterdb_ds)
        .load_album("main", "")
        .expect("ds album YAML");
    let ds_track = ds_album
        .get_track("01_clean_chord_31s.flac")
        .expect("ds track");
    assert_eq!(ds_track.post_chain_peaks[0].key.output_rate_hz, 176_400);
    assert_ne!(
        ds_track.post_chain_peaks[0].key, scan_track.post_chain_peaks[0].key,
        "downsampled playback key must not collide with 8fS scanner key"
    );

    let _ = fs::remove_dir_all(&root);
}

/// **L3 + Issue #23 P5 (`§3.6`)**: Verify that E2 next-track post-chain pre-scan produces the
/// exact same `suggested`, `rule`, `PostChainKey`, and `M` (within `1e-9 dB`) as both offline
/// `scan --post-chain` and live `stream_track_ultimate` — including user overrides (preserving
/// machine `suggested`, U2), `< 8fS` downsampled DACs, and fast-skip when already cached.
#[test]
fn l3_e2_prescan_agrees_with_scan_and_playback_including_overrides_and_downsampling() {
    let root = temp_dir("e2_l3");
    let music_dir = root.join("music");
    fs::create_dir_all(&music_dir).unwrap();
    let flac_path = music_dir.join("01_e2_chord_31s.flac");
    write_stereo_flac_16(&flac_path, 44_100, 31);

    let shared_policy = UpsamplerPolicy {
        max_taps: 65_536,
        ..UpsamplerPolicy::default()
    };

    // 1. Offline `scan --post-chain` baseline into `filterdb_scan`.
    let filterdb_scan = root.join("filterdb_scan");
    let cfg_path = root.join("empty_config.toml");
    fs::write(&cfg_path, "").unwrap();
    let scan_opts = ScanOptions {
        folder: Some(music_dir.clone()),
        report: false,
        post_chain: true,
        force: true,
        jobs: Some(1),
        trackdb_dir: Some(root.join("trackdb_scan")),
        filterdb_dir: Some(filterdb_scan.clone()),
        choices_file: Some(root.join("no_choices.yaml")),
        config_path: Some(cfg_path),
        rew_override: Some(ConvolutionConfig::disabled()),
        upsampler_policy: shared_policy,
    };
    let summary = scan_folder(&scan_opts, None).expect("scan_folder failed");
    assert_eq!(summary.scanned, 1);
    let scan_track = FilterDbStore::new(&filterdb_scan)
        .load_album("main", "")
        .unwrap()
        .get_track("01_e2_chord_31s.flac")
        .unwrap()
        .clone();

    // 2. E2 pre-scan into fresh `filterdb_e2`.
    let filterdb_e2 = root.join("filterdb_e2");
    let mut manifest = LibraryManifest::new(DETECTOR_VERSION);
    manifest.upsert_root("main", &music_dir);
    let ult_e2 = UltimateRuntimeContext::new(
        UltimateConfig {
            enabled: true,
            genre_rules: true,
            learn_on_play: true,
        },
        FilterDbStore::new(filterdb_e2.clone()),
        manifest,
        root.join("no_choices.yaml"),
        FilterMode::Auto,
    );
    let pipeline_8fs = AudioPipeline::with_rew_config_full_tail(
        shared_policy,
        ConvolutionConfig::disabled(),
        true,
    )
    .unwrap();

    let out = prescan_local_track_post_chain(&flac_path, &pipeline_8fs, &ult_e2, &|| false);
    let (e2_key, e2_m) = match out {
        PreScanOutcome::Completed {
            post_key,
            peak_dbfs,
        } => (post_key, peak_dbfs),
        other => panic!("expected PreScanOutcome::Completed, got {other:?}"),
    };
    assert_eq!(
        e2_key, scan_track.post_chain_peaks[0].key,
        "L3: E2 PostChainKey != offline scan PostChainKey"
    );
    assert!(
        (e2_m - scan_track.post_chain_peaks[0].peak_dbfs).abs() <= 1e-9,
        "L3: E2 M ({e2_m:.10}) != scan M ({:.10})",
        scan_track.post_chain_peaks[0].peak_dbfs
    );

    let e2_track = FilterDbStore::new(&filterdb_e2)
        .load_album("main", "")
        .unwrap()
        .get_track("01_e2_chord_31s.flac")
        .unwrap()
        .clone();
    assert_eq!(e2_track.suggested, scan_track.suggested);
    assert_eq!(e2_track.rule, scan_track.rule);
    assert_eq!(
        e2_track.effective_true_peak_meter(),
        Some(TRUE_PEAK_METER_8X_64),
        "E2 pre-scan must persist tp8x-64 true peak alongside exact M"
    );

    // 3. Re-running E2 immediately hits fast-skip (`AlreadyCached`).
    let out_again = prescan_local_track_post_chain(&flac_path, &pipeline_8fs, &ult_e2, &|| false);
    assert_eq!(
        out_again,
        PreScanOutcome::AlreadyCached {
            post_key: e2_key.clone()
        }
    );

    // 4. Playing Track 1 via `stream_track_ultimate` after E2 immediately hits E1 (`exact-M`)!
    let buf = BoundedBlockStreamBuffer::with_limits(
        ULTIMATE_MAX_BUFFERED_FRAMES,
        Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
    );
    let ep = buf.current_epoch();
    let buf_c = Arc::clone(&buf);
    let drain_h = std::thread::spawn(move || {
        let mut first_report = None;
        loop {
            match buf_c.pop_for_epoch_timeout(ep, Duration::from_secs(60)) {
                Some(StreamEvent::TrackStart { report, .. }) => first_report = Some(report),
                Some(StreamEvent::PcmBlock(_)) => {}
                Some(StreamEvent::TrackEnd) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        first_report.expect("TrackStart missing")
    });
    let mut reader = FlacBlockReader::open(&flac_path).unwrap();
    let (mut carry, mut backup, mut pend) = (None, None, false);
    let play_res = stream_track_ultimate(
        &pipeline_8fs,
        &ult_e2,
        &buf,
        ep,
        || false,
        &mut reader,
        &flac_path,
        Some(&flac_path),
        None,
        None,
        PlayModeLabel::Sequential,
        None,
        &mut carry,
        &mut backup,
        &mut pend,
        false,
        8_192,
    );
    let report = drain_h.join().unwrap();
    assert!(matches!(play_res, UltimateStreamResult::Played { .. }));
    let u = report.ultimate_report.as_ref().expect("ultimate_report");
    assert_eq!(u.peak_source.as_deref(), Some("exact-M"));
    assert_eq!(u.mode, "ultimate: cached");
    assert!(
        (u.cached_m_dbfs.unwrap() - e2_m).abs() <= 1e-9,
        "playback cached M must match E2 pre-scan M"
    );
    let play_key = build_post_chain_key(
        &report.dsp_plan,
        report.source_spec.sample_rate_hz,
        report.output_rate_hz,
        report.convolver_content_hash.as_deref(),
    );
    assert_eq!(play_key, e2_key);

    // 5. Downsampled `< 8fS` DAC (176.4 kHz): E2 computes and caches the exact 176.4 kHz key!
    let ds_caps =
        DacOutputCapabilities::from_supported_rates(&[(176_400, DacWireFormat::Int32)], None);
    let pipeline_ds = AudioPipeline::with_rew_config_full_tail(
        shared_policy,
        ConvolutionConfig::disabled(),
        true,
    )
    .unwrap()
    .with_dac_capabilities(ds_caps);
    let ds_out = prescan_local_track_post_chain(&flac_path, &pipeline_ds, &ult_e2, &|| false);
    let ds_key = match ds_out {
        PreScanOutcome::Completed { post_key, .. } => post_key,
        other => panic!("expected Completed for downsampled DAC key, got {other:?}"),
    };
    assert_eq!(ds_key.output_rate_hz, 176_400);
    assert_ne!(ds_key, e2_key);

    // 6. User override (`my-filters.yaml`: short-lp + deemphasis: true) preserves machine `suggested`.
    let choices_path = root.join("override_choices.yaml");
    fs::write(
        &choices_path,
        "schema: 2\n\ntracks:\n  \"main:01_e2_chord_31s.flac\":\n    filter: short-lp\n    deemphasis: true\n",
    )
    .unwrap();
    let filterdb_ov = root.join("filterdb_ov");
    let mut manifest_ov = LibraryManifest::new(DETECTOR_VERSION);
    manifest_ov.upsert_root("main", &music_dir);
    let ult_ov = UltimateRuntimeContext::new(
        UltimateConfig {
            enabled: true,
            genre_rules: true,
            learn_on_play: true,
        },
        FilterDbStore::new(filterdb_ov.clone()),
        manifest_ov,
        choices_path,
        FilterMode::Auto,
    );
    let ov_out = prescan_local_track_post_chain(&flac_path, &pipeline_8fs, &ult_ov, &|| false);
    let ov_key = match ov_out {
        PreScanOutcome::Completed { post_key, .. } => post_key,
        other => panic!("expected Completed for user override, got {other:?}"),
    };
    assert_eq!(ov_key.upsampler, "short-lp");
    assert!(ov_key.deemphasis);
    let ov_track = FilterDbStore::new(&filterdb_ov)
        .load_album("main", "")
        .unwrap()
        .get_track("01_e2_chord_31s.flac")
        .unwrap()
        .clone();
    assert_eq!(
        ov_track.suggested, scan_track.suggested,
        "U2: E2 pre-scan with user filter override must NEVER overwrite machine suggested"
    );

    let _ = fs::remove_dir_all(&root);
}

/// **Issue #23 T13b & T13c (`§3.6`, §5)**:
/// - Throughput guard refuses to start E2 when `speed_x_rt < E2_MIN_SPEED_X_RT` (3.0x RT).
/// - Cancellation (pre-set or mid-scan) aborts promptly and writes ZERO partial YAML files.
#[test]
fn t13_e2_prescan_throughput_guard_and_cancel_writes_no_partial_yaml() {
    let root = temp_dir("t13_guard_cancel");
    let music_dir = root.join("music");
    fs::create_dir_all(&music_dir).unwrap();
    let flac_path = music_dir.join("02_cancel_target_31s.flac");
    write_stereo_flac_16(&flac_path, 44_100, 31);

    let filterdb_dir = root.join("filterdb");
    let mut manifest = LibraryManifest::new(DETECTOR_VERSION);
    manifest.upsert_root("main", &music_dir);
    let ult = UltimateRuntimeContext::new(
        UltimateConfig {
            enabled: true,
            genre_rules: true,
            learn_on_play: true,
        },
        FilterDbStore::new(filterdb_dir.clone()),
        manifest,
        root.join("no_choices.yaml"),
        FilterMode::Auto,
    );
    let pipeline = Arc::new(
        AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy {
                max_taps: 65_536,
                ..UpsamplerPolicy::default()
            },
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap(),
    );

    // 1. Throughput guard (T13c): below `E2_MIN_SPEED_X_RT` (3.0x RT) -> refuses to start!
    let mut prescanner = NextTrackPreScanner::new();
    let epoch_cancel = Arc::new(AtomicBool::new(false));
    assert!(
        !prescanner.maybe_start(
            &flac_path,
            E2_MIN_SPEED_X_RT - 0.1,
            Arc::clone(&pipeline),
            ult.clone(),
            Arc::clone(&epoch_cancel),
        ),
        "E2 must not start when spare capacity < 2x RT (speed_x_rt < 3.0)"
    );
    assert!(!prescanner.is_running());
    assert!(
        FilterDbStore::new(&filterdb_dir)
            .load_album("main", "")
            .is_none(),
        "no YAML may be written when throughput guard declines E2"
    );

    // 2. Mid-scan cancellation (T13b): cancel after 3 blocks -> Cancelled with ZERO partial YAML!
    let calls = AtomicUsize::new(0);
    let mid_cancel = prescan_local_track_post_chain(&flac_path, &pipeline, &ult, &|| {
        calls.fetch_add(1, Ordering::Relaxed) >= 3
    });
    assert_eq!(mid_cancel, PreScanOutcome::Cancelled);
    assert!(
        FilterDbStore::new(&filterdb_dir)
            .load_album("main", "")
            .is_none(),
        "mid-scan cancel must write ZERO partial YAML"
    );

    // 3. Background `NextTrackPreScanner` cancelled via `epoch_cancel` (simulating skip/pause) ->
    //    exits cleanly with ZERO partial YAML!
    let pre_cancelled = Arc::new(AtomicBool::new(true));
    assert!(prescanner.maybe_start(
        &flac_path,
        12.0,
        Arc::clone(&pipeline),
        ult.clone(),
        pre_cancelled,
    ));
    let bg_out = prescanner.join_if_active();
    assert_eq!(bg_out, Some(PreScanOutcome::Cancelled));
    assert!(
        FilterDbStore::new(&filterdb_dir)
            .load_album("main", "")
            .is_none(),
        "epoch-cancelled E2 worker must write ZERO partial YAML"
    );

    let _ = fs::remove_dir_all(&root);
}
