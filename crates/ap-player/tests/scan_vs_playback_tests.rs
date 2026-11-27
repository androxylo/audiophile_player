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
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ap_control::state::PlayModeLabel;
use ap_core::config::{ConvolutionConfig, UltimateConfig};
use ap_core::filter_db::{FilterDbStore, LibraryManifest};
use ap_core::format::{DacOutputCapabilities, DacWireFormat};
use ap_core::user_filters::FilterMode;
use ap_dsp::analysis::DETECTOR_VERSION;
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{FilterArchetype, UpsamplerPolicy};
use ap_player::prefetch::{
    BoundedBlockStreamBuffer, StreamEvent, ULTIMATE_MAX_BUFFERED_FRAMES,
    ULTIMATE_MAX_BUFFERED_SECONDS,
};
use ap_player::scanner::{ScanOptions, scan_folder};
use ap_player::ultimate::{UltimateRuntimeContext, UltimateStreamResult, stream_track_ultimate};
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
