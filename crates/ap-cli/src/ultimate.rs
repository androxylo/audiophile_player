// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Ultimate Mode `W0 -> W1 -> W2 -> W3 -> W4` bounded 30 s look-ahead streaming track engine
//! (`docs/research/07-ultimate-mode.md` §§2–5, Mandates M1–M4, `OQ-33..OQ-41`).
//!
//! Hard invariants enforced here:
//! - **M1 (30 s cap):** `W0` reads strictly `<= 30 s` of source audio before `W1..W3`; the
//!   downstream buffer is capped at `ULTIMATE_MAX_BUFFERED_SECONDS = 30`.
//! - **M3 (`< 30 s` bypass):** If header duration `< 30 s` OR EOF occurs inside `W0`, bypass
//!   ultimate rules cleanly and play via normal-mode defaults labelled `"normal (<30 s)"`.
//! - **M4 (Streaming vs. Local):** Streaming sources NEVER buffer beyond 30 s, NEVER use mega-tap
//!   `gauss-apod-4m`, and ALWAYS apply `-3 dB` upsampling safety (`OQ-33`).
//! - **Strict Per-Track Independence (`OQ-40`):** Track B's detectors, filter choice, `M30`, `M`,
//!   and fixed gain `G_B` depend 100% on Track B alone. Track A's carried gapless tail `z_A[n]`
//!   (already scaled by `G_A`) is strictly additive output-only right before quantization.
//! - **Single-Pass Unity Render (`W2 -> W3`):** Renders `W0` ONCE at unity gain -> measures exact
//!   `M30` -> computes mandatory fixed whole-track gain `G` -> quantizes `W2` and continues `W4`
//!   seamlessly through EOF measuring whole-track `M` -> atomic YAML learn-on-play (`U5b`) ONLY on
//!   clean EOF with verified `STREAMINFO.md5`. Mid-track skip discards cleanly with zero YAML write.

use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ap_control::state::PlayModeLabel;
use ap_core::buffer::StereoF64Buffer;
use ap_core::config::UltimateConfig;
use ap_core::filter_db::{
    AlbumAnalysisFile, AlbumTrackAnalysis, FilterDbStore, LibraryManifest, file_head_4k_hash,
};
use ap_core::headroom::{
    HeadroomDecision, HeadroomMode, quantizer_margin_db, select_ultimate_headroom,
};
use ap_core::metadata::PhaseTagOverride;
use ap_core::track_db::{TrackDbStore, file_size_and_mtime};
use ap_core::user_filters::{
    FilterMode, ResolvedUserOverrides, UserFilterProvenance, UserFiltersCache,
};
use ap_dsp::analysis::{DETECTOR_VERSION, TrackAnalyzer};
use ap_dsp::convolution::ConvolutionTail;
use ap_dsp::fusion::{
    CachedWholeTrackEvidence, EvidenceSource, build_track_db_entry, fuse_track_evidence,
};
use ap_dsp::pipeline::{AudioPipeline, GaplessCarryover, UltimateDecisionReport};
use ap_dsp::selector::{
    FilterArchetype, NoiseShaperProfile, ULTIMATE_LOOKAHEAD_SECONDS, UpsamplerPicker,
    UpsamplerPolicy, build_post_chain_key, check_ultimate_throughput_with_buffer,
    select_ultimate_plan,
};
use ap_source::flac::AudioBlockSource;
use ap_source::md5::md5_to_hex;

use crate::prefetch::{BoundedBlockStreamBuffer, StreamEvent};

/// Runtime configuration & Schema-2 Filter Database handle active when `[ultimate] enabled = true`
/// or `--ultimate`.
#[derive(Debug, Clone)]
pub struct UltimateRuntimeContext {
    pub config: UltimateConfig,
    /// Legacy Schema-1 TrackDbStore (used for read-only fallback & opt-in `--trackdb-sidecar`).
    pub store: TrackDbStore,
    /// Schema-2 album-folder mirror store (`%APPDATA%\audiophile-player\filterdb`).
    pub filter_db: FilterDbStore,
    /// Shared library manifest mapping named roots (`main`, `classical`, ...) to disk paths.
    pub manifest: Arc<Mutex<LibraryManifest>>,
    /// Hot-reloading cache for user-owned `my-filters.yaml`.
    pub user_filters: Arc<UserFiltersCache>,
    /// Global filter mode (`Auto` -> `Mine` if `my-filters.yaml` exists, else `Suggested`).
    pub filter_mode: FilterMode,
    /// Single-album hot cache `(root_name, rel_folder, AlbumAnalysisFile)` to avoid re-parsing
    /// the same album YAML across consecutive tracks.
    pub album_cache: Arc<Mutex<Option<(String, String, AlbumAnalysisFile)>>>,
}

impl UltimateRuntimeContext {
    /// Production constructor wiring Schema-2 `FilterDbStore`, `LibraryManifest`, `UserFiltersCache`,
    /// and legacy Schema-1 `TrackDbStore` fallback.
    pub fn new(
        config: UltimateConfig,
        store: TrackDbStore,
        filter_db: FilterDbStore,
        manifest: LibraryManifest,
        choices_path: std::path::PathBuf,
        filter_mode: FilterMode,
    ) -> Self {
        Self {
            config,
            store,
            filter_db,
            manifest: Arc::new(Mutex::new(manifest)),
            user_filters: Arc::new(UserFiltersCache::new(choices_path)),
            filter_mode,
            album_cache: Arc::new(Mutex::new(None)),
        }
    }

    /// Backwards-compatible helper for legacy tests using only Schema-1 `TrackDbStore`.
    #[cfg(test)]
    pub fn from_legacy(config: UltimateConfig, store: TrackDbStore) -> Self {
        let base = store.root_dir().to_path_buf();
        Self::new(
            config,
            store,
            FilterDbStore::new(base.join("_filterdb")),
            LibraryManifest::default(),
            base.join("_my-filters.yaml"),
            FilterMode::Auto,
        )
    }

    fn resolve_track_location(&self, local_path: &Path) -> Option<(String, String, String)> {
        let guard = self.manifest.lock().unwrap_or_else(|e| e.into_inner());
        guard.resolve_track_path(local_path)
    }

    fn load_album_cached(&self, root: &str, rel_folder: &str) -> Option<AlbumAnalysisFile> {
        {
            let guard = self.album_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((ref r, ref f, ref album)) = *guard {
                if r.eq_ignore_ascii_case(root) && f.eq_ignore_ascii_case(rel_folder) {
                    return Some(album.clone());
                }
            }
        }
        let loaded = self.filter_db.load_album(root, rel_folder)?;
        let mut guard = self.album_cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some((root.to_string(), rel_folder.to_string(), loaded.clone()));
        Some(loaded)
    }

    fn update_album_cache(
        &self,
        root: &str,
        rel_folder: &str,
        file_name: &str,
        track: AlbumTrackAnalysis,
    ) {
        let mut guard = self.album_cache.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some((r, f, album))
                if r.eq_ignore_ascii_case(root) && f.eq_ignore_ascii_case(rel_folder) =>
            {
                let key = album
                    .find_track_key(file_name)
                    .unwrap_or_else(|| file_name.to_string());
                album.tracks.insert(key, track);
                album.recompute_header_detector_version(DETECTOR_VERSION);
            }
            _ => {
                let mut album = self
                    .filter_db
                    .load_album(root, rel_folder)
                    .unwrap_or_else(|| {
                        AlbumAnalysisFile::new(
                            root.to_string(),
                            rel_folder.to_string(),
                            DETECTOR_VERSION,
                        )
                    });
                let key = album
                    .find_track_key(file_name)
                    .unwrap_or_else(|| file_name.to_string());
                album.tracks.insert(key, track);
                album.recompute_header_detector_version(DETECTOR_VERSION);
                *guard = Some((root.to_string(), rel_folder.to_string(), album));
            }
        }
    }
}

/// Outcome of streaming a single track through the Ultimate `W0..W4` engine.
#[derive(Debug)]
pub enum UltimateStreamResult {
    Played {
        next_carryover: Option<GaplessCarryover>,
        next_pending_end: bool,
        #[allow(dead_code)]
        wrote_yaml: bool,
        /// Non-fatal mid-track warning/error logged after `TrackStart` (Review #7 F7/F-B2).
        warning: Option<String>,
    },
    Aborted,
    Failed(String),
}

#[cfg(test)]
/// Cross-thread panic hook injected right after `incoming_carryover.take()` for engine-level F8/C5
/// recovery tests driving `producer_loop` directly. Stores a set of target path suffixes so
/// concurrent tests in the same binary never overwrite or accidentally consume each other's hook.
static TEST_PANIC_TARGET_SUFFIXES: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn arm_test_panic_after_carry_take(path_suffix: Option<&str>) {
    let lock = TEST_PANIC_TARGET_SUFFIXES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = path_suffix {
        guard.push(s.to_string());
    }
}

#[cfg(test)]
pub(crate) fn disarm_test_panic_after_carry_take(path_suffix: &str) {
    let lock = TEST_PANIC_TARGET_SUFFIXES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|s| s != path_suffix);
}

#[cfg(test)]
fn take_test_panic_after_carry_take(display_path: &Path) -> bool {
    let Some(lock) = TEST_PANIC_TARGET_SUFFIXES.get() else {
        return false;
    };
    let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let path_str = display_path.to_string_lossy();
    if let Some(idx) = guard.iter().position(|suffix| path_str.ends_with(suffix.as_str())) {
        guard.swap_remove(idx);
        return true;
    }
    false
}

#[cfg(test)]
std::thread_local! {
    /// Optional per-block sleep (ms) injected during W2 render for F9 early-abort tests.
    static TEST_W2_BLOCK_DELAY_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Count of W2 blocks rendered during the last `stream_track_ultimate` call (for F9 tests).
    static TEST_W2_BLOCKS_RENDERED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Optional forced quantize failure during EOF flush for F-B2 tests.
    static TEST_FAIL_EOF_FLUSH_QUANTIZE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Optional forced quantize failure at a specific 0-based W3 block index (Review #8 C4).
    static TEST_FAIL_W3_QUANTIZE_AT_BLOCK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Optional forced quantize failure at a specific 0-based W4 block index (Review #8 C4).
    static TEST_FAIL_W4_QUANTIZE_AT_BLOCK: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn peak_linear_of_buffer(buf: &StereoF64Buffer) -> f64 {
    let mut peak = 0.0_f64;
    for &s in buf.left.iter().chain(buf.right.iter()) {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}

fn linear_to_dbfs(peak_linear: f64) -> f64 {
    if peak_linear > 0.0 && peak_linear.is_finite() {
        20.0 * peak_linear.log10()
    } else {
        -120.0
    }
}

/// Execute the full `W0 -> W1 -> W2 -> W3 -> W4` Ultimate Mode track streaming lifecycle.
///
/// Works identically for local `FlacBlockReader` (`local_path = Some(...)`) and streaming
/// `HttpAudioSource` (`local_path = None`, `is_streaming = true`).
#[allow(clippy::too_many_arguments)]
pub fn stream_track_ultimate<S, F>(
    pipeline: &AudioPipeline,
    ult: &UltimateRuntimeContext,
    buf: &BoundedBlockStreamBuffer,
    epoch: u64,
    is_aborted: F,
    source: &mut S,
    display_path: &Path,
    local_path: Option<&Path>,
    upnp_uri: Option<String>,
    upnp_next_gen: Option<u64>,
    mode_label: PlayModeLabel,
    duration_hint_s: Option<f64>,
    incoming_carryover: &mut Option<GaplessCarryover>,
    panic_backup_carry: &mut Option<GaplessCarryover>,
    had_pending_prev_end: &mut bool,
    gapless_carryover: bool,
    src_block_frames: usize,
) -> UltimateStreamResult
where
    S: AudioBlockSource,
    F: Fn() -> bool,
{
    let spec = source.spec();
    let is_streaming = source.is_streaming();
    let raw_peak_meta = source.peak_meta();
    let track_meta = source.track_meta();
    let header_md5 = source.streaminfo_md5();
    let header_md5_hex = header_md5.as_ref().map(md5_to_hex);
    let total_duration_s = duration_hint_s.or_else(|| {
        source
            .total_frames()
            .filter(|&f| f > 0)
            .map(|f| f as f64 / f64::from(spec.sample_rate_hz.max(1)))
    });

    // Helper: drain Track A's unconsumed carryover cleanly onto Track A before starting Track B
    // when rates differ or when Track B fails before producing audio.
    let flush_prev_on_incompatible = |buf: &BoundedBlockStreamBuffer,
                                      carry: &mut Option<GaplessCarryover>,
                                      backup: &mut Option<GaplessCarryover>,
                                      pending_end: &mut bool|
     -> bool {
        if let Some(tail) = carry.take() {
            *backup = Some(tail.clone());
            if let Ok(pcm) = pipeline.drain_gapless_carryover(tail) {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    *backup = None;
                    *pending_end = false;
                    return false;
                }
            }
            *backup = None;
        }
        if *pending_end {
            *pending_end = false;
            if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                return false;
            }
        }
        true
    };

    // --- Resolve Schema-2 User Choices (`my-filters.yaml` hot-reload) & Effective Filter Mode ---
    let mut user_warnings: Vec<String> = Vec::new();
    let (choices_exist, user_file, choices_err) = ult.user_filters.load_fresh();
    if let Some(ref uf) = user_file {
        user_warnings.extend(uf.parse_warnings.iter().cloned());
    }
    if let Some(err) = choices_err {
        user_warnings.push(err);
    }
    let effective_filter_mode = ult
        .filter_mode
        .resolve(choices_exist && user_file.is_some());
    let track_loc = if is_streaming {
        None
    } else {
        local_path.and_then(|p| ult.resolve_track_location(p))
    };
    let resolved_user_overrides = if effective_filter_mode == FilterMode::Mine {
        if let Some(ref uf) = user_file {
            if let Some((ref root, ref rel_folder, ref file_name)) = track_loc {
                Some(uf.resolve_for_track(root, rel_folder, file_name, &track_meta))
            } else {
                Some(uf.resolve_for_track("", "", "", &track_meta))
            }
        } else {
            None
        }
    } else {
        None
    };

    // --- M3 Early Short-Track Check (< 30 s known from header / DIDL) ---
    let header_short = total_duration_s
        .is_some_and(|d| d.is_finite() && d < f64::from(ULTIMATE_LOOKAHEAD_SECONDS));
    if header_short {
        return stream_short_track_normal(
            pipeline,
            buf,
            epoch,
            &is_aborted,
            source,
            Vec::new(),
            true,
            display_path,
            upnp_uri,
            upnp_next_gen,
            mode_label,
            total_duration_s,
            incoming_carryover,
            panic_backup_carry,
            had_pending_prev_end,
            gapless_carryover,
            src_block_frames,
            effective_filter_mode,
            resolved_user_overrides.as_ref(),
            &user_warnings,
        );
    }

    // --- W0: Bounded <= 30 s Window Acquisition (Mandates M1, M2, M4) ---
    let max_window_frames = u64::from(spec.sample_rate_hz) * u64::from(ULTIMATE_LOOKAHEAD_SECONDS);
    let mut w0_blocks: Vec<StereoF64Buffer> = Vec::new();
    let mut w0_frames: u64 = 0;
    let mut hit_eof_in_w0 = false;
    let mut analyzer = TrackAnalyzer::new(spec.sample_rate_hz, spec.bits_per_sample);

    while w0_frames < max_window_frames {
        buf.drain_deferred_drops();
        if is_aborted() || buf.current_epoch() != epoch {
            return UltimateStreamResult::Aborted;
        }
        let remaining = (max_window_frames - w0_frames) as usize;
        let ask = src_block_frames.min(remaining).max(1);
        match source.read_next_block(ask) {
            Ok(Some(b)) if b.is_empty() => continue,
            Ok(Some(b)) => {
                analyzer.push_block(&b);
                w0_frames += b.left.len() as u64;
                w0_blocks.push(b);
            }
            Ok(None) => {
                hit_eof_in_w0 = true;
                break;
            }
            Err(err) => {
                if !flush_prev_on_incompatible(
                    buf,
                    incoming_carryover,
                    panic_backup_carry,
                    had_pending_prev_end,
                ) {
                    return UltimateStreamResult::Aborted;
                }
                return UltimateStreamResult::Failed(format!(
                    "FLAC/stream decode error during W0 on '{}': {err}",
                    display_path.display()
                ));
            }
        }
    }

    if w0_blocks.is_empty() {
        if !flush_prev_on_incompatible(
            buf,
            incoming_carryover,
            panic_backup_carry,
            had_pending_prev_end,
        ) {
            return UltimateStreamResult::Aborted;
        }
        return UltimateStreamResult::Failed(format!(
            "Skipped empty (0-frame) track '{}'",
            display_path.display()
        ));
    }

    // --- M3 Runtime Short-Track Check (e.g. total_samples == 0 header with < 30 s body) ---
    if hit_eof_in_w0 && w0_frames < max_window_frames {
        let actual_dur_s = w0_frames as f64 / f64::from(spec.sample_rate_hz.max(1));
        return stream_short_track_normal(
            pipeline,
            buf,
            epoch,
            &is_aborted,
            source,
            w0_blocks,
            false,
            display_path,
            upnp_uri,
            upnp_next_gen,
            mode_label,
            Some(actual_dur_s),
            incoming_carryover,
            panic_backup_carry,
            had_pending_prev_end,
            gapless_carryover,
            src_block_frames,
            effective_filter_mode,
            resolved_user_overrides.as_ref(),
            &user_warnings,
        );
    }

    // --- W1: Strict Per-Track Cache Lookup + Fusion + Selection (Schema 2 -> Schema 1 fallback) ---
    let (file_size, file_mtime) = local_path
        .and_then(file_size_and_mtime)
        .unwrap_or((0, 0));

    let cached_s2_track: Option<(AlbumTrackAnalysis, bool)> = if is_streaming {
        None
    } else if let Some((ref root, ref rel_folder, ref file_name)) = track_loc {
        ult.load_album_cached(root, rel_folder).and_then(|album| {
            let track = album.get_track(file_name)?.clone();
            let current = track.detector_version == DETECTOR_VERSION
                && track.matches_identity(
                    header_md5_hex.as_deref(),
                    Some(file_size),
                    Some(file_mtime),
                );
            Some((track, current))
        })
    } else {
        None
    };

    let cached_s1_entry = if is_streaming || cached_s2_track.as_ref().is_some_and(|(_, cur)| *cur) {
        None
    } else {
        ult.store
            .load_for_track(local_path, header_md5_hex.as_deref())
    };
    let cached_s1_current = cached_s1_entry.as_ref().is_some_and(|e| {
        e.is_results_current(
            DETECTOR_VERSION,
            header_md5_hex.as_deref(),
            local_path.map(|_| file_size),
            local_path.map(|_| file_mtime),
        )
    });

    let cached_current = cached_s2_track.as_ref().is_some_and(|(_, cur)| *cur) || cached_s1_current;
    let base_cached_wt = if let Some((ref t, cur)) = cached_s2_track {
        CachedWholeTrackEvidence::from_album_track(t, cur)
    } else if let Some(ref e) = cached_s1_entry {
        CachedWholeTrackEvidence::from_track_db_entry(e, cached_s1_current)
    } else {
        CachedWholeTrackEvidence::default()
    };

    let cached_wt = match effective_filter_mode {
        FilterMode::Suggested => base_cached_wt.clone().suggested_only(),
        FilterMode::Mine | FilterMode::Auto => {
            if let Some(ref ov) = resolved_user_overrides {
                base_cached_wt.clone().with_user_overrides(ov)
            } else {
                base_cached_wt.clone()
            }
        }
    };

    let early_digital = track_meta.is_early_digital_era();
    let orig_below = track_meta
        .original_sample_rate_hz
        .is_some_and(|r| r < spec.sample_rate_hz);
    let w30_snapshot = analyzer.snapshot(
        hit_eof_in_w0,
        early_digital,
        orig_below,
        track_meta.is_dsd_hint,
    );

    let fused = fuse_track_evidence(
        spec.sample_rate_hz,
        spec.bits_per_sample,
        &raw_peak_meta,
        &track_meta,
        Some(&w30_snapshot),
        Some(&cached_wt),
    );

    let policy = pipeline.policy();
    let mut selection = match select_ultimate_plan(
        spec,
        is_streaming,
        total_duration_s,
        &fused,
        &policy,
        ult.config.genre_rules,
        cached_current,
    ) {
        Ok(sel) => sel,
        Err(err) => {
            if !flush_prev_on_incompatible(
                buf,
                incoming_carryover,
                panic_backup_carry,
                had_pending_prev_end,
            ) {
                return UltimateStreamResult::Aborted;
            }
            return UltimateStreamResult::Failed(format!(
                "Ultimate selection error on '{}': {err}",
                display_path.display()
            ));
        }
    };
    selection.warnings.extend(user_warnings.iter().cloned());

    // If user overrode `apodizing` (attribute-only override), compute pure machine suggestion too.
    if fused.apodizing.source == EvidenceSource::User {
        let sug_wt = base_cached_wt.clone().suggested_only();
        let sug_fused = fuse_track_evidence(
            spec.sample_rate_hz,
            spec.bits_per_sample,
            &raw_peak_meta,
            &track_meta,
            Some(&w30_snapshot),
            Some(&sug_wt),
        );
        if let Ok(sug_sel) = select_ultimate_plan(
            spec,
            is_streaming,
            total_duration_s,
            &sug_fused,
            &UpsamplerPolicy {
                filter_override: None,
                ..policy
            },
            ult.config.genre_rules,
            cached_current,
        ) {
            selection.suggested_filter = sug_sel.plan.filter;
            selection.suggested_rule = sug_sel.winning_rule;
        }
    }

    // --- W2: Single-Pass Unity-Gain Render of W0 + Startup-Aware Throughput Self-Check (F9/F-B1) ---
    let placeholder_hr = HeadroomDecision {
        mode: HeadroomMode::ZeroDb,
        peak_dbfs: None,
        peak_kind: None,
        convolution_scale_db: 0.0,
        reason: "ultimate W2 unity-gain measurement".to_string(),
    };

    let w0_audio_s = w0_frames as f64 / f64::from(spec.sample_rate_hz.max(1));
    let buffered_before_w2_s = buf.buffered_duration_seconds();
    let buffered_opt = (buffered_before_w2_s > 0.0).then_some(buffered_before_w2_s);

    enum W2RenderError {
        Aborted,
        SlowProbe(ap_dsp::selector::ThroughputCheckResult),
        Dsp(String),
    }

    let render_w2 = |plan: &ap_dsp::selector::SelectedDspPlan,
                     allow_early_abort: bool|
     -> Result<
        (
            ap_dsp::pipeline::StreamingPipelineSession,
            Vec<StereoF64Buffer>,
            f64,
            f64,
        ),
        W2RenderError,
    > {
        let mut sess = pipeline
            .start_track_with_plan_and_headroom(spec, plan.clone(), placeholder_hr.clone())
            .map_err(W2RenderError::Dsp)?;
        let started = Instant::now();
        let mut rendered = Vec::with_capacity(w0_blocks.len());
        let mut peak = 0.0_f64;
        let mut rendered_src_frames: u64 = 0;
        let probe_min_s = 2.5_f64.min(w0_audio_s * 0.5);
        for blk in &w0_blocks {
            buf.drain_deferred_drops();
            if is_aborted() || buf.current_epoch() != epoch {
                return Err(W2RenderError::Aborted);
            }
            #[cfg(test)]
            {
                TEST_W2_BLOCKS_RENDERED.with(|c| c.set(c.get() + 1));
                if allow_early_abort {
                    let delay_ms = TEST_W2_BLOCK_DELAY_MS.with(|c| c.get());
                    if delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                    }
                }
            }
            let blk_frames = blk.left.len() as u64;
            let out = sess
                .process_block_unity_f64(blk.clone())
                .map_err(W2RenderError::Dsp)?;
            if !out.is_empty() {
                peak = peak.max(peak_linear_of_buffer(&out));
                rendered.push(out);
            }
            rendered_src_frames += blk_frames;

            // Review #7 F9: Early-abort probe after ~2.5 s of W0 so a too-slow mega-tap filter
            // switches to its lossless fallback immediately instead of spending 20-30 s stalling!
            if allow_early_abort && rendered_src_frames < w0_frames {
                let rendered_src_s =
                    rendered_src_frames as f64 / f64::from(spec.sample_rate_hz.max(1));
                if rendered_src_s >= probe_min_s {
                    let probe = check_ultimate_throughput_with_buffer(
                        plan.filter,
                        spec.sample_rate_hz,
                        rendered_src_s,
                        started.elapsed().as_secs_f64(),
                        total_duration_s,
                        w0_audio_s,
                        buffered_opt,
                        // Review #7 re-review R2: `buffered_before_w2_s` was sampled AFTER W0
                        // decode finished, so decode time is already reflected in `buffered_opt`!
                        0.0,
                    );
                    if !probe.passed
                        && probe.fallback_filter.is_some_and(|fb| fb != plan.filter)
                    {
                        return Err(W2RenderError::SlowProbe(probe));
                    }
                }
            }
        }
        let wall_s = started.elapsed().as_secs_f64();
        Ok((sess, rendered, peak, wall_s))
    };

    let (initial_result, early_slow_probe) = match render_w2(&selection.plan, true) {
        Ok(v) => (Some(v), None),
        Err(W2RenderError::Aborted) => return UltimateStreamResult::Aborted,
        Err(W2RenderError::SlowProbe(probe)) => (None, Some(probe)),
        Err(W2RenderError::Dsp(err)) => {
            if !flush_prev_on_incompatible(
                buf,
                incoming_carryover,
                panic_backup_carry,
                had_pending_prev_end,
            ) {
                return UltimateStreamResult::Aborted;
            }
            return UltimateStreamResult::Failed(format!(
                "Ultimate W2 render error on '{}': {err}",
                display_path.display()
            ));
        }
    };

    let tp_check = if let Some(probe) = early_slow_probe {
        probe
    } else {
        let (_, _, _, w2_wall_s) = initial_result.as_ref().unwrap();
        check_ultimate_throughput_with_buffer(
            selection.plan.filter,
            spec.sample_rate_hz,
            w0_audio_s,
            *w2_wall_s,
            total_duration_s,
            w0_audio_s,
            buffered_opt,
            0.0,
        )
    };

    let (mut session, w2_rendered, m30_linear, final_speed_x_rt) = if tp_check.passed {
        let (s, r, p, _) = initial_result.unwrap();
        (s, r, p, tp_check.measured_x_rt)
    } else {
        let mut fallback_applied = None;
        let mut fallback_failure_reason: Option<String> = None;

        if let Some(fb_arch) = tp_check.fallback_filter {
            if fb_arch == selection.plan.filter {
                fallback_failure_reason = Some(format!(
                    "already on fallback filter {fb_arch}"
                ));
            } else {
                eprintln!("  [ultimate throughput] {}", tp_check.detail);
                selection.warnings.push(tp_check.detail.clone());
                let fb_policy = UpsamplerPolicy {
                    filter_override: Some(fb_arch),
                    ..policy
                };
                match select_ultimate_plan(
                    spec,
                    is_streaming,
                    total_duration_s,
                    &fused,
                    &fb_policy,
                    ult.config.genre_rules,
                    cached_current,
                ) {
                    Ok(fb_sel) => {
                        // Review #8 C6: re-sample remaining buffer cushion right before fallback W2
                        // render so we can warn if the fallback still exceeds remaining cushion.
                        let fb_buffered_s = buf.buffered_duration_seconds();
                        let fb_buffered_opt = (fb_buffered_s > 0.0).then_some(fb_buffered_s);
                        match render_w2(&fb_sel.plan, false) {
                            Ok((s2, r2, p2, w2)) => {
                                let fb_check = check_ultimate_throughput_with_buffer(
                                    fb_sel.plan.filter,
                                    spec.sample_rate_hz,
                                    w0_audio_s,
                                    w2,
                                    total_duration_s,
                                    w0_audio_s,
                                    fb_buffered_opt,
                                    0.0,
                                );
                                if !fb_check.passed {
                                    let warn = format!(
                                        "Fallback {} throughput {:.2}x RT still below {:.2}x RT post-probe cushion (underruns likely)",
                                        fb_sel.plan.filter,
                                        fb_check.measured_x_rt,
                                        fb_check.required_x_rt
                                    );
                                    eprintln!("  [ultimate throughput warn] {warn}");
                                    selection.warnings.push(warn);
                                }
                                let spd = w0_audio_s / w2.max(1e-9);
                                selection.plan = fb_sel.plan;
                                selection.winning_rule = format!("throughput_fallback({fb_arch})");
                                fallback_applied = Some((s2, r2, p2, spd));
                            }
                            Err(W2RenderError::Aborted) => return UltimateStreamResult::Aborted,
                            Err(W2RenderError::Dsp(err)) => {
                                fallback_failure_reason = Some(err);
                            }
                            Err(W2RenderError::SlowProbe(_)) => {}
                        }
                    }
                    Err(err) => {
                        fallback_failure_reason = Some(err);
                    }
                }
            }
        } else {
            fallback_failure_reason = Some("no faster lossless fallback available".to_string());
        }

        if let Some(v) = fallback_applied {
            v
        } else {
            // Review #7 F-B1: NEVER silently swallow fallback unavailability/errors!
            let reason = fallback_failure_reason.unwrap_or_else(|| "unknown error".to_string());
            let warn_msg = format!(
                "fallback unavailable: {reason}; continuing at {:.2}x RT, underruns likely",
                tp_check.measured_x_rt
            );
            eprintln!("  [ultimate throughput warn] {warn_msg}");
            selection.warnings.push(warn_msg);

            if let Some((s, r, p, _)) = initial_result {
                (s, r, p, tp_check.measured_x_rt)
            } else {
                // Early probe aborted initial render and fallback failed -> render original plan
                // cleanly (without early abort) or fail if it errors, never emit truncated audio.
                match render_w2(&selection.plan, false) {
                    Ok((s, r, p, w)) => (s, r, p, w0_audio_s / w.max(1e-9)),
                    Err(W2RenderError::Aborted) => return UltimateStreamResult::Aborted,
                    Err(W2RenderError::Dsp(err)) => {
                        if !flush_prev_on_incompatible(
                            buf,
                            incoming_carryover,
                            panic_backup_carry,
                            had_pending_prev_end,
                        ) {
                            return UltimateStreamResult::Aborted;
                        }
                        return UltimateStreamResult::Failed(format!(
                            "Ultimate W2 render error on '{}': {err}",
                            display_path.display()
                        ));
                    }
                    Err(W2RenderError::SlowProbe(_)) => unreachable!(),
                }
            }
        }
    };
    drop(w0_blocks);

    // Build exact PostChainKey matching this active linear chain (shared with `scanner.rs`, F6).
    let post_key = build_post_chain_key(
        &selection.plan,
        spec.sample_rate_hz,
        session.report().output_rate_hz,
        session.report().convolver_content_hash.as_deref(),
    );

    // Look up cached exact whole-track M (local files only; NEVER used for streams per OQ-33!).
    let cached_exact_m_db = if is_streaming || !cached_current {
        None
    } else {
        cached_s2_track
            .as_ref()
            .filter(|(_, cur)| *cur)
            .and_then(|(t, _)| t.find_post_chain_peak(&post_key))
            .or_else(|| {
                cached_s1_entry
                    .as_ref()
                    .filter(|_| cached_s1_current)
                    .and_then(|e| e.find_post_chain_peak(&post_key))
            })
    };

    let m30_dbfs = linear_to_dbfs(m30_linear);
    let conv_boost_db = session.convolver_max_boost_db();

    // Compute MANDATORY FIXED WHOLE-TRACK GAIN `G` (OQ-33) — locked for the entire track!
    // Review #7 F2: If W2 built a bit-perfect `Bypass` quantizer (for `Null` filter with placeholder
    // 0 dB headroom), a bit-perfect 0 dB pass needs 0.0 dB quantizer margin; if the resulting fixed
    // gain is negative (`G < 0`), immediately reconfigure the quantizer to its dithered/shaped
    // profile at `dac_bits` and recompute `m_q_db` & `G` BEFORE quantizing any block!
    let mut m_q_db = if session.report().effective_shaper == NoiseShaperProfile::Bypass {
        0.0
    } else {
        quantizer_margin_db(session.report().output_bits, session.shaper_l1_sum())
    };
    let mut fixed_headroom = select_ultimate_headroom(
        &fused.peak_meta,
        spec.bits_per_sample,
        is_streaming,
        Some(conv_boost_db),
        cached_exact_m_db,
        Some(m30_dbfs),
        m_q_db,
    );
    if fixed_headroom.total_gain_db() < -1e-9
        && session.report().effective_shaper == NoiseShaperProfile::Bypass
    {
        if let Err(err) = session.reconfigure_quantizer_for_headroom(&fixed_headroom) {
            if !flush_prev_on_incompatible(
                buf,
                incoming_carryover,
                panic_backup_carry,
                had_pending_prev_end,
            ) {
                return UltimateStreamResult::Aborted;
            }
            return UltimateStreamResult::Failed(format!(
                "Ultimate quantizer reconfigure error on '{}': {err}",
                display_path.display()
            ));
        }
        m_q_db = quantizer_margin_db(session.report().output_bits, session.shaper_l1_sum());
        fixed_headroom = select_ultimate_headroom(
            &fused.peak_meta,
            spec.bits_per_sample,
            is_streaming,
            Some(conv_boost_db),
            cached_exact_m_db,
            Some(m30_dbfs),
            m_q_db,
        );
    }
    let fixed_gain_db = fixed_headroom.total_gain_db();
    session.set_fixed_headroom(fixed_headroom);

    // Attach Showcase Decision Report (U13 + Schema-2 dual `playing | suggested` display).
    let override_label = if policy.filter_override.is_some() {
        Some("cli".to_string())
    } else {
        selection
            .user_provenance
            .map(|p| p.short_label().to_string())
    };
    let decision_report = UltimateDecisionReport {
        mode: if cached_exact_m_db.is_some() {
            "ultimate: cached".to_string()
        } else {
            selection.mode_label.clone()
        },
        winning_rule: selection.winning_rule.clone(),
        active_filter: Some(selection.plan.filter.to_string()),
        override_label,
        suggested_filter: Some(selection.suggested_filter.to_string()),
        suggested_rule: Some(selection.suggested_rule.clone()),
        fired_rules: selection.fired_rules.clone(),
        cache_hit: cached_current || cached_exact_m_db.is_some(),
        m30_dbfs: Some(m30_dbfs),
        cached_m_dbfs: cached_exact_m_db,
        fixed_gain_db,
        quantizer_margin_db: m_q_db,
        throughput_x_rt: Some(final_speed_x_rt),
        warnings: selection.warnings.clone(),
    };
    session.set_ultimate_report(decision_report);

    // --- W3: Strict Additive Gapless Tail Separation (`OQ-40`) + Quantize W2 & Emit TrackStart ---
    let mut additive_tail: Option<(ConvolutionTail, usize)> = None;
    let mut had_gapless_carryover = false;

    if let Some(carry) = incoming_carryover.take() {
        // Review #7 F8: Preserve backup of Track A's carryover across the window until Track B's
        // first quantized PCM block (carrying `additive_tail`) is successfully pushed.
        *panic_backup_carry = Some(carry.clone());
        #[cfg(test)]
        if take_test_panic_after_carry_take(display_path) {
            panic!("injected F8 test panic after incoming_carryover.take()");
        }
        let rate = session.report().output_rate_hz;
        if carry.output_rate_hz == rate && carry.upsampler.is_none() {
            // Track A already flushed its upsampler and pre-scaled `conv_tail` by G_A.
            if let Some(q_st) = carry.quantizer_state {
                session.restore_quantizer_state_if_compatible(
                    q_st,
                    carry.effective_shaper,
                    carry.output_bits,
                );
            }
            if let Some(ct) = carry.convolver {
                if !ct.left.is_empty() {
                    additive_tail = Some((ct, 0));
                    had_gapless_carryover = true;
                }
            }
            if *had_pending_prev_end {
                *had_pending_prev_end = false;
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    *panic_backup_carry = None;
                    return UltimateStreamResult::Aborted;
                }
            }
        } else {
            // Incompatible rate or normal-mode carryover with un-flushed upsampler history:
            // drain cleanly onto Track A first so Track B stays 100% independent.
            if let Ok(pcm) = pipeline.drain_gapless_carryover(carry) {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    *panic_backup_carry = None;
                    return UltimateStreamResult::Aborted;
                }
            }
            *panic_backup_carry = None;
            if *had_pending_prev_end {
                *had_pending_prev_end = false;
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
            }
        }
    } else if *had_pending_prev_end {
        *had_pending_prev_end = false;
        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
            return UltimateStreamResult::Aborted;
        }
    }

    // Emit TrackStart carrying full UltimateDecisionReport.
    if !buf.push_for_epoch(
        epoch,
        StreamEvent::TrackStart {
            path: display_path.to_path_buf(),
            upnp_uri,
            upnp_next_gen,
            mode_label,
            report: session.report().clone(),
            total_duration_s,
            had_gapless_carryover,
        },
    ) {
        *panic_backup_carry = None;
        return UltimateStreamResult::Aborted;
    }

    // Quantize and push the already-rendered unity-gain W2 blocks (ZERO double-rendering!).
    // Review #7 F7 & F-B2: After `TrackStart` has been emitted, any decode/DSP/quantize error logs
    // a warning, sets `had_error = true` (disabling YAML learn-on-play and gapless carryover), and
    // breaks cleanly into EOF flush + `TrackEnd` so single-file sessions and playlists stay intact.
    let mut had_error = false;
    let mut skip_session_flush = false;
    let mut error_warning: Option<String> = None;
    let mut whole_track_m_linear = m30_linear;
    let mut cleared_panic_backup = false;

    for (w3_idx, unity_blk) in w2_rendered.into_iter().enumerate() {
        let _ = w3_idx;
        if is_aborted() || buf.current_epoch() != epoch {
            *panic_backup_carry = None;
            return UltimateStreamResult::Aborted;
        }
        let q_res = {
            #[cfg(test)]
            {
                if TEST_FAIL_W3_QUANTIZE_AT_BLOCK.with(|c| c.get()) == Some(w3_idx) {
                    TEST_FAIL_W3_QUANTIZE_AT_BLOCK.with(|c| c.set(None));
                    Err(format!("injected W3 quantize error at block {w3_idx}"))
                } else {
                    session.quantize_scaled_block(&unity_blk, fixed_gain_db, &mut additive_tail)
                }
            }
            #[cfg(not(test))]
            {
                session.quantize_scaled_block(&unity_blk, fixed_gain_db, &mut additive_tail)
            }
        };
        match q_res {
            Ok(pcm) if !pcm.is_empty() => {
                if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    *panic_backup_carry = None;
                    return UltimateStreamResult::Aborted;
                }
                if !cleared_panic_backup {
                    *panic_backup_carry = None;
                    cleared_panic_backup = true;
                }
            }
            Ok(_) => {}
            Err(err) => {
                let msg = format!(
                    "Ultimate W3 quantize error on '{}': {err}",
                    display_path.display()
                );
                eprintln!("  [ultimate err] {msg}");
                had_error = true;
                skip_session_flush = true;
                error_warning = Some(msg);
                break;
            }
        }
    }
    if !cleared_panic_backup {
        *panic_backup_carry = None;
    }

    // --- W4: Stream Remainder through EOF (same fixed G, continuous TrackAnalyzer + exact M) ---
    if !hit_eof_in_w0 && !had_error {
        let mut w4_block_idx: usize = 0;
        loop {
            if is_aborted() || buf.current_epoch() != epoch {
                // Mid-track skip: discard cleanly WITHOUT writing YAML!
                return UltimateStreamResult::Aborted;
            }
            match source.read_next_block(src_block_frames) {
                Ok(Some(blk)) if blk.is_empty() => continue,
                Ok(Some(blk)) => {
                    let cur_w4_idx = w4_block_idx;
                    w4_block_idx += 1;
                    let _ = cur_w4_idx;
                    analyzer.push_block(&blk);
                    let unity = match session.process_block_unity_f64(blk) {
                        Ok(u) => u,
                        Err(err) => {
                            let msg = format!(
                                "Ultimate W4 DSP error on '{}': {err}",
                                display_path.display()
                            );
                            eprintln!("  [ultimate err] {msg}");
                            had_error = true;
                            skip_session_flush = true;
                            error_warning = Some(msg);
                            break;
                        }
                    };
                    if !unity.is_empty() {
                        whole_track_m_linear =
                            whole_track_m_linear.max(peak_linear_of_buffer(&unity));
                        let q_res = {
                            #[cfg(test)]
                            {
                                if TEST_FAIL_W4_QUANTIZE_AT_BLOCK.with(|c| c.get())
                                    == Some(cur_w4_idx)
                                {
                                    TEST_FAIL_W4_QUANTIZE_AT_BLOCK.with(|c| c.set(None));
                                    Err(format!("injected W4 quantize error at block {cur_w4_idx}"))
                                } else {
                                    session.quantize_scaled_block(
                                        &unity,
                                        fixed_gain_db,
                                        &mut additive_tail,
                                    )
                                }
                            }
                            #[cfg(not(test))]
                            {
                                session.quantize_scaled_block(
                                    &unity,
                                    fixed_gain_db,
                                    &mut additive_tail,
                                )
                            }
                        };
                        match q_res {
                            Ok(pcm) if !pcm.is_empty() => {
                                if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                                    return UltimateStreamResult::Aborted;
                                }
                            }
                            Ok(_) => {}
                            Err(err) => {
                                let msg = format!(
                                    "Ultimate W4 quantize error on '{}': {err}",
                                    display_path.display()
                                );
                                eprintln!("  [ultimate err] {msg}");
                                had_error = true;
                                skip_session_flush = true;
                                error_warning = Some(msg);
                                break;
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    let msg = format!(
                        "FLAC/stream decode error during W4 on '{}': {err}",
                        display_path.display()
                    );
                    eprintln!("  [ultimate err] {msg}");
                    had_error = true;
                    error_warning = Some(msg);
                    break;
                }
            }
        }
    }

    // Helper: quantize and push any residual `additive_tail` samples onto silence if the track
    // ended (or errored) before Track A's carried REW tail finished decaying (Review #7 F7).
    let drain_remaining_additive_tail =
        |sess: &mut ap_dsp::pipeline::StreamingPipelineSession,
         tail_opt: &mut Option<(ConvolutionTail, usize)>,
         had_err: &mut bool,
         err_warn: &mut Option<String>|
         -> bool {
            while let Some((ref tail, off)) = *tail_opt {
                let rem = tail.left.len().saturating_sub(off);
                if rem == 0 {
                    *tail_opt = None;
                    break;
                }
                let chunk = rem.min(65_536);
                let silence = StereoF64Buffer {
                    sample_rate_hz: sess.report().output_rate_hz,
                    left: vec![0.0; chunk],
                    right: vec![0.0; chunk],
                };
                match sess.quantize_scaled_block(&silence, 0.0, tail_opt) {
                    Ok(pcm) if !pcm.is_empty() => {
                        if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                            return false;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        let msg = format!(
                            "Ultimate additive-tail quantize error on '{}': {err}",
                            display_path.display()
                        );
                        eprintln!("  [ultimate err] {msg}");
                        *had_err = true;
                        if err_warn.is_none() {
                            *err_warn = Some(msg);
                        }
                        *tail_opt = None;
                        break;
                    }
                }
            }
            true
        };

    // Clean EOF flush (unity-gain + pre-scaled gapless tail export; forced non-gapless on error).
    let use_gapless_eof = gapless_carryover && !had_error;
    let (next_carryover, next_pending_end) = if use_gapless_eof {
        match session.flush_for_gapless_unity_f64(fixed_gain_db) {
            Ok((ending_unity, tail_peak_unity, carry)) => {
                whole_track_m_linear = whole_track_m_linear.max(tail_peak_unity);
                if !ending_unity.is_empty() {
                    let q_res = {
                        #[cfg(test)]
                        {
                            if TEST_FAIL_EOF_FLUSH_QUANTIZE.with(|c| c.replace(false)) {
                                Err("injected EOF flush quantize error".to_string())
                            } else {
                                session.quantize_scaled_block(
                                    &ending_unity,
                                    fixed_gain_db,
                                    &mut additive_tail,
                                )
                            }
                        }
                        #[cfg(not(test))]
                        {
                            session.quantize_scaled_block(
                                &ending_unity,
                                fixed_gain_db,
                                &mut additive_tail,
                            )
                        }
                    };
                    match q_res {
                        Ok(pcm) if !pcm.is_empty() => {
                            if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                                return UltimateStreamResult::Aborted;
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let msg = format!(
                                "Ultimate EOF flush quantize error on '{}': {err}",
                                display_path.display()
                            );
                            eprintln!("  [ultimate err] {msg}");
                            had_error = true;
                            if error_warning.is_none() {
                                error_warning = Some(msg);
                            }
                        }
                    }
                }
                if !drain_remaining_additive_tail(
                    &mut session,
                    &mut additive_tail,
                    &mut had_error,
                    &mut error_warning,
                ) {
                    return UltimateStreamResult::Aborted;
                }
                if had_error {
                    if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                        return UltimateStreamResult::Aborted;
                    }
                    (None, false)
                } else {
                    (carry, true)
                }
            }
            Err(err) => {
                let msg = format!(
                    "Ultimate flush error on '{}': {err}",
                    display_path.display()
                );
                eprintln!("  [ultimate err] {msg}");
                had_error = true;
                if error_warning.is_none() {
                    error_warning = Some(msg);
                }
                if !drain_remaining_additive_tail(
                    &mut session,
                    &mut additive_tail,
                    &mut had_error,
                    &mut error_warning,
                ) {
                    return UltimateStreamResult::Aborted;
                }
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                (None, false)
            }
        }
    } else if skip_session_flush {
        // Review #7 re-review F7 + Review #8 C4: On a W3 quantize error, W4 quantize error, or W4
        // DSP error, `session` has advanced past the last emitted PCM block (or is in an undefined
        // DSP state), so flushing `session` would append a discontinuous tail (audible click).
        // Drain only any remaining incoming `additive_tail` and emit `TrackEnd` cleanly.
        if !drain_remaining_additive_tail(
            &mut session,
            &mut additive_tail,
            &mut had_error,
            &mut error_warning,
        ) {
            return UltimateStreamResult::Aborted;
        }
        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
            return UltimateStreamResult::Aborted;
        }
        (None, false)
    } else {
        match session.flush_unity_f64() {
            Ok(ending_unity) => {
                whole_track_m_linear =
                    whole_track_m_linear.max(peak_linear_of_buffer(&ending_unity));
                if !ending_unity.is_empty() {
                    let q_res = {
                        #[cfg(test)]
                        {
                            if TEST_FAIL_EOF_FLUSH_QUANTIZE.with(|c| c.replace(false)) {
                                Err("injected EOF flush quantize error".to_string())
                            } else {
                                session.quantize_scaled_block(
                                    &ending_unity,
                                    fixed_gain_db,
                                    &mut additive_tail,
                                )
                            }
                        }
                        #[cfg(not(test))]
                        {
                            session.quantize_scaled_block(
                                &ending_unity,
                                fixed_gain_db,
                                &mut additive_tail,
                            )
                        }
                    };
                    match q_res {
                        Ok(pcm) if !pcm.is_empty() => {
                            if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                                return UltimateStreamResult::Aborted;
                            }
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let msg = format!(
                                "Ultimate EOF flush quantize error on '{}': {err}",
                                display_path.display()
                            );
                            eprintln!("  [ultimate err] {msg}");
                            had_error = true;
                            if error_warning.is_none() {
                                error_warning = Some(msg);
                            }
                        }
                    }
                }
                if !drain_remaining_additive_tail(
                    &mut session,
                    &mut additive_tail,
                    &mut had_error,
                    &mut error_warning,
                ) {
                    return UltimateStreamResult::Aborted;
                }
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                (None, false)
            }
            Err(err) => {
                let msg = format!(
                    "Ultimate flush error on '{}': {err}",
                    display_path.display()
                );
                eprintln!("  [ultimate err] {msg}");
                had_error = true;
                if error_warning.is_none() {
                    error_warning = Some(msg);
                }
                if !drain_remaining_additive_tail(
                    &mut session,
                    &mut additive_tail,
                    &mut had_error,
                    &mut error_warning,
                ) {
                    return UltimateStreamResult::Aborted;
                }
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                (None, false)
            }
        }
    };

    // --- U5b Learn-on-Play: Persist Verified Whole-Track Results Atomically ONCE at Clean EOF ---
    // Review #7 F7 & F-B2: Strictly gated on `!had_error` so partial/errored plays NEVER write YAML!
    let mut wrote_yaml = false;
    if !had_error && ult.config.learn_on_play && !is_streaming && local_path.is_some() {
        let computed_md5 = source.computed_md5();
        let md5_ok = match (header_md5, computed_md5) {
            (Some(expected), Some(actual)) => {
                if expected != actual {
                    eprintln!(
                        "  [ultimate learn-on-play] MD5 mismatch on '{}' (header={} computed={}); refusing to write YAML",
                        display_path.display(),
                        md5_to_hex(&expected),
                        md5_to_hex(&actual),
                    );
                    false
                } else {
                    true
                }
            }
            // If STREAMINFO.md5 is unset (0), allow persistence if computed_md5 succeeded cleanly.
            (None, Some(_)) => true,
            // Stream did not reach clean EOF with zero decode errors -> never write.
            (_, None) => false,
        };

        if md5_ok {
            let wt_analysis =
                analyzer.snapshot(true, early_digital, orig_below, track_meta.is_dsd_hint);
            let final_md5_hex = header_md5_hex.or_else(|| computed_md5.as_ref().map(md5_to_hex));
            // Compute pure machine-suggested filter/rule from whole-track analysis so Schema-2
            // album YAML always records the machine suggestion (never a user choice).
            let wt_sug_ev =
                CachedWholeTrackEvidence::from_whole_track_analysis(&wt_analysis, None)
                    .suggested_only();
            let wt_sug_fused = fuse_track_evidence(
                spec.sample_rate_hz,
                spec.bits_per_sample,
                &raw_peak_meta,
                &track_meta,
                Some(&wt_analysis),
                Some(&wt_sug_ev),
            );
            let (wt_sug_filter, wt_sug_rule) = select_ultimate_plan(
                spec,
                false,
                Some(wt_analysis.duration_s),
                &wt_sug_fused,
                &UpsamplerPolicy {
                    filter_override: None,
                    ..policy
                },
                ult.config.genre_rules,
                false,
            )
            .map(|s| (s.plan.filter.to_string(), s.winning_rule))
            .unwrap_or_else(|_| {
                (
                    selection.suggested_filter.to_string(),
                    selection.suggested_rule.clone(),
                )
            });

            let mut entry = build_track_db_entry(
                display_path.to_string_lossy().into_owned(),
                file_size,
                file_mtime,
                final_md5_hex,
                &wt_analysis,
                selection.plan.deemphasis,
                wt_sug_filter,
                wt_sug_rule,
            );
            let whole_m_dbfs = linear_to_dbfs(whole_track_m_linear);
            entry.upsert_post_chain_peak(post_key, whole_m_dbfs, false);

            if let Some((ref root, ref rel_folder, ref file_name)) = track_loc {
                let head_hash = local_path.and_then(file_head_4k_hash);
                let album_track = AlbumTrackAnalysis::from_schema1(&entry, head_hash);
                match ult.filter_db.upsert_track_nonblocking(
                    root,
                    rel_folder,
                    file_name,
                    DETECTOR_VERSION,
                    album_track,
                ) {
                    Ok(Some(merged_track)) => {
                        ult.update_album_cache(root, rel_folder, file_name, merged_track);
                        wrote_yaml = true;
                    }
                    Ok(None) => {
                        // Another writer (e.g. `scan`) holds this folder lock; skip cleanly!
                    }
                    Err(err) => {
                        eprintln!(
                            "  [ultimate learn-on-play warn] failed to save Schema-2 album YAML for '{}': {err}",
                            display_path.display()
                        );
                    }
                }
                if ult.store.sidecar_enabled() {
                    let _ = ult.store.save_preserving_user(local_path, entry);
                }
            } else {
                match ult.store.save_preserving_user(local_path, entry) {
                    Ok(_) => wrote_yaml = true,
                    Err(err) => {
                        eprintln!(
                            "  [ultimate learn-on-play warn] failed to save YAML for '{}': {err}",
                            display_path.display()
                        );
                    }
                }
            }
        }
    }

    UltimateStreamResult::Played {
        next_carryover,
        next_pending_end,
        wrote_yaml,
        warning: error_warning,
    }
}

/// Mandate M3: play tracks `< 30 s` through normal-mode defaults and label as `"normal (<30 s)"`,
/// while still honoring explicit user choices from `my-filters.yaml` / `AP_*` tags in `mine` mode.
#[allow(clippy::too_many_arguments)]
fn stream_short_track_normal<S, F>(
    pipeline: &AudioPipeline,
    buf: &BoundedBlockStreamBuffer,
    epoch: u64,
    is_aborted: &F,
    source: &mut S,
    pre_read_blocks: Vec<StereoF64Buffer>,
    read_remaining_from_source: bool,
    display_path: &Path,
    upnp_uri: Option<String>,
    upnp_next_gen: Option<u64>,
    mode_label: PlayModeLabel,
    total_duration_s: Option<f64>,
    incoming_carryover: &mut Option<GaplessCarryover>,
    panic_backup_carry: &mut Option<GaplessCarryover>,
    had_pending_prev_end: &mut bool,
    gapless_carryover: bool,
    src_block_frames: usize,
    effective_filter_mode: FilterMode,
    user_overrides: Option<&ResolvedUserOverrides>,
    user_warnings: &[String],
) -> UltimateStreamResult
where
    S: AudioBlockSource,
    F: Fn() -> bool,
{
    let spec = source.spec();
    let peak_meta = source.peak_meta();
    let track_meta = source.track_meta();
    let is_streaming = source.is_streaming();

    let mut src_info = ap_dsp::selector::TrackSourceInfo::from_stream_spec(spec);
    src_info.is_streaming_source = is_streaming;
    src_info.deemphasis = if effective_filter_mode == FilterMode::Suggested {
        track_meta.cuesheet_pre_emphasis
    } else {
        user_overrides
            .and_then(|u| u.deemphasis.as_ref().map(|(d, _)| *d))
            .or(track_meta.ap_deemphasis)
            .unwrap_or(track_meta.cuesheet_pre_emphasis)
    };

    let mut warnings: Vec<String> = user_warnings.to_vec();
    if let Some(ov) = user_overrides {
        warnings.extend(ov.warnings.iter().cloned());
    }

    let base_policy = pipeline.policy();
    let mut override_label: Option<String> = None;
    let mut winning_rule = "short_track_bypass".to_string();
    let mut user_filter_override: Option<FilterArchetype> = None;

    if base_policy.filter_override.is_some() {
        override_label = Some("cli".to_string());
        winning_rule = "cli_override".to_string();
    } else if effective_filter_mode == FilterMode::Mine {
        let candidate_filter = user_overrides
            .and_then(|u| {
                u.filter
                    .as_ref()
                    .map(|(f, prov, _)| (f.as_str(), *prov))
            })
            .or_else(|| {
                track_meta
                    .ap_filter
                    .as_deref()
                    .map(|f| (f, UserFilterProvenance::Tag))
            });
        if let Some((fname, prov)) = candidate_filter {
            match FilterArchetype::from_str(fname) {
                Ok(arch) => {
                    if is_streaming && !arch.is_streaming_safe() {
                        warnings.push(format!(
                            "M4 streaming guard: user override '{}' is local-only on short track; using normal default",
                            arch
                        ));
                    } else {
                        user_filter_override = Some(arch);
                        override_label = Some(prov.short_label().to_string());
                        winning_rule = prov.rule_name().to_string();
                    }
                }
                Err(err) => {
                    warnings.push(format!("ignoring unrecognized user filter override '{fname}': {err}"));
                }
            }
        } else {
            let phase_choice = user_overrides
                .and_then(|u| u.phase.as_ref().map(|(p, _)| *p))
                .or(track_meta.ap_phase);
            match phase_choice {
                Some(PhaseTagOverride::Minimum) => {
                    user_filter_override = Some(FilterArchetype::MinphaseLong);
                    override_label = Some(UserFilterProvenance::Track.short_label().to_string());
                    winning_rule = "user_phase_min".to_string();
                }
                Some(PhaseTagOverride::Intermediate) => {
                    user_filter_override = Some(FilterArchetype::IntermediateLong);
                    override_label = Some(UserFilterProvenance::Track.short_label().to_string());
                    winning_rule = "user_phase_ip".to_string();
                }
                _ => {}
            }
        }
    }

    let mut session = if let Some(user_arch) = user_filter_override {
        let mut eff_policy = base_policy;
        eff_policy.filter_override = Some(user_arch);
        let picker = match UpsamplerPicker::new(eff_policy) {
            Ok(p) => p,
            Err(err) => {
                return UltimateStreamResult::Failed(format!(
                    "DSP picker error on short track '{}': {err}",
                    display_path.display()
                ));
            }
        };
        let mut plan = picker.select(&src_info);
        plan.reason = format!("short track (<30 s) user override -> {user_arch}");
        let placeholder_hr = HeadroomDecision {
            mode: HeadroomMode::ZeroDb,
            peak_dbfs: None,
            peak_kind: None,
            convolution_scale_db: 0.0,
            reason: "short-track user override headroom pending".to_string(),
        };
        let mut s = match pipeline.start_track_with_plan_and_headroom(spec, plan, placeholder_hr) {
            Ok(s) => s,
            Err(err) => {
                return UltimateStreamResult::Failed(format!(
                    "DSP pipeline error on short track '{}': {err}",
                    display_path.display()
                ));
            }
        };
        let conv_boost_db = s.convolver_max_boost_db();
        let hr = ap_core::headroom::select_headroom_for_bits(
            &peak_meta,
            spec.bits_per_sample,
            is_streaming,
            Some(conv_boost_db),
            None,
        );
        if let Err(err) = s.reconfigure_quantizer_for_headroom(&hr) {
            return UltimateStreamResult::Failed(format!(
                "DSP quantizer error on short track '{}': {err}",
                display_path.display()
            ));
        }
        s.set_fixed_headroom(hr);
        s
    } else {
        match pipeline.start_track_with_source_info(spec, src_info, &peak_meta) {
            Ok(s) => s,
            Err(err) => {
                return UltimateStreamResult::Failed(format!(
                    "DSP pipeline error on short track '{}': {err}",
                    display_path.display()
                ));
            }
        }
    };

    let fixed_g = session.report().headroom.total_gain_db();
    let m_q = quantizer_margin_db(session.report().output_bits, session.shaper_l1_sum());
    let active_filter_name = session.report().dsp_plan.filter.to_string();
    let suggested_filter_name = session.report().dsp_plan.target_audiophile_filter.to_string();
    session.set_ultimate_report(UltimateDecisionReport {
        mode: "normal (<30 s)".to_string(),
        winning_rule,
        active_filter: Some(active_filter_name),
        override_label,
        suggested_filter: Some(suggested_filter_name),
        suggested_rule: Some("short_track_bypass".to_string()),
        fired_rules: vec!["M3(<30 s)".to_string()],
        cache_hit: false,
        m30_dbfs: None,
        cached_m_dbfs: None,
        fixed_gain_db: fixed_g,
        quantizer_margin_db: m_q,
        throughput_x_rt: None,
        warnings,
    });

    if let Some(carry) = incoming_carryover.take() {
        *panic_backup_carry = Some(carry.clone());
        if let Ok(pcm) = pipeline.drain_gapless_carryover(carry) {
            if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                *panic_backup_carry = None;
                return UltimateStreamResult::Aborted;
            }
        }
        *panic_backup_carry = None;
    }
    if *had_pending_prev_end {
        *had_pending_prev_end = false;
        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
            return UltimateStreamResult::Aborted;
        }
    }

    if !buf.push_for_epoch(
        epoch,
        StreamEvent::TrackStart {
            path: display_path.to_path_buf(),
            upnp_uri,
            upnp_next_gen,
            mode_label,
            report: session.report().clone(),
            total_duration_s,
            had_gapless_carryover: false,
        },
    ) {
        return UltimateStreamResult::Aborted;
    }

    let mut had_error = false;
    let mut error_warning: Option<String> = None;

    let mut push_blk = |blk: StereoF64Buffer| -> Result<bool, String> {
        let pcm = session.process_block(blk)?;
        if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
            return Ok(false);
        }
        Ok(true)
    };

    for blk in pre_read_blocks {
        if is_aborted() || buf.current_epoch() != epoch {
            return UltimateStreamResult::Aborted;
        }
        match push_blk(blk) {
            Ok(true) => {}
            Ok(false) => return UltimateStreamResult::Aborted,
            Err(err) => {
                eprintln!("  [short track err] {err}");
                had_error = true;
                error_warning = Some(err);
                break;
            }
        }
    }

    if read_remaining_from_source && !had_error {
        loop {
            if is_aborted() || buf.current_epoch() != epoch {
                return UltimateStreamResult::Aborted;
            }
            match source.read_next_block(src_block_frames) {
                Ok(Some(blk)) if blk.is_empty() => continue,
                Ok(Some(blk)) => match push_blk(blk) {
                    Ok(true) => {}
                    Ok(false) => return UltimateStreamResult::Aborted,
                    Err(err) => {
                        eprintln!("  [short track err] {err}");
                        had_error = true;
                        error_warning = Some(err);
                        break;
                    }
                },
                Ok(None) => break,
                Err(err) => {
                    eprintln!("  [short track err] {err}");
                    had_error = true;
                    error_warning = Some(err);
                    break;
                }
            }
        }
    }

    if gapless_carryover && !had_error {
        match session.flush_for_gapless() {
            Ok((pcm, carry)) => {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    return UltimateStreamResult::Aborted;
                }
                UltimateStreamResult::Played {
                    next_carryover: carry,
                    next_pending_end: true,
                    wrote_yaml: false,
                    warning: None,
                }
            }
            Err(err) => {
                eprintln!("  [short track flush err] {err}");
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                UltimateStreamResult::Played {
                    next_carryover: None,
                    next_pending_end: false,
                    wrote_yaml: false,
                    warning: Some(err),
                }
            }
        }
    } else {
        match session.flush() {
            Ok(pcm) => {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    return UltimateStreamResult::Aborted;
                }
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                UltimateStreamResult::Played {
                    next_carryover: None,
                    next_pending_end: false,
                    wrote_yaml: false,
                    warning: error_warning,
                }
            }
            Err(err) => {
                eprintln!("  [short track flush err] {err}");
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                UltimateStreamResult::Played {
                    next_carryover: None,
                    next_pending_end: false,
                    wrote_yaml: false,
                    warning: error_warning.or(Some(err)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use ap_core::config::ConvolutionConfig;
    use ap_core::format::StreamSpec;
    use ap_core::headroom::TrackPeakMetadata;
    use ap_core::metadata::TrackMetadata;
    use ap_dsp::selector::FilterArchetype;

    use crate::prefetch::{ULTIMATE_MAX_BUFFERED_FRAMES, ULTIMATE_MAX_BUFFERED_SECONDS};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ap_ult_test_{label}_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Synthetic in-memory `AudioBlockSource` supporting local & streaming fixtures, custom tags,
    /// unknown `total_frames = None`, and forced MD5 mismatch for §7 & §5.5 TDD gates.
    struct SyntheticSource {
        spec: StreamSpec,
        peak_meta: TrackPeakMetadata,
        track_meta: TrackMetadata,
        is_streaming: bool,
        claimed_total_frames: Option<u64>,
        streaminfo_md5: Option<[u8; 16]>,
        computed_md5_at_eof: Option<[u8; 16]>,
        total_frames: usize,
        cursor: usize,
        amp: f64,
        freq_hz: f64,
        clipped: bool,
        fail_at_frame: Option<usize>,
    }

    impl SyntheticSource {
        fn sine(rate: u32, duration_s: f64, amp: f64, is_streaming: bool) -> Self {
            let total_frames = (rate as f64 * duration_s).round() as usize;
            Self {
                spec: StreamSpec {
                    sample_rate_hz: rate,
                    bits_per_sample: 16,
                    channels: 2,
                },
                peak_meta: TrackPeakMetadata::default(),
                track_meta: TrackMetadata::default(),
                is_streaming,
                claimed_total_frames: Some(total_frames as u64),
                streaminfo_md5: Some([0x11; 16]),
                computed_md5_at_eof: Some([0x11; 16]),
                total_frames,
                cursor: 0,
                amp,
                freq_hz: 1_000.0,
                clipped: false,
                fail_at_frame: None,
            }
        }
    }

    impl AudioBlockSource for SyntheticSource {
        fn spec(&self) -> StreamSpec {
            self.spec
        }
        fn peak_meta(&self) -> TrackPeakMetadata {
            self.peak_meta
        }
        fn is_streaming(&self) -> bool {
            self.is_streaming
        }
        fn total_frames(&self) -> Option<u64> {
            self.claimed_total_frames
        }
        fn track_meta(&self) -> TrackMetadata {
            self.track_meta.clone()
        }
        fn streaminfo_md5(&self) -> Option<[u8; 16]> {
            self.streaminfo_md5
        }
        fn computed_md5(&self) -> Option<[u8; 16]> {
            if self.cursor >= self.total_frames {
                self.computed_md5_at_eof
            } else {
                None
            }
        }
        fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
            if let Some(fail_at) = self.fail_at_frame {
                if self.cursor >= fail_at {
                    return Err(format!("injected synthetic decode error at frame {fail_at}"));
                }
            }
            if self.cursor >= self.total_frames {
                return Ok(None);
            }
            let limit = self
                .fail_at_frame
                .map_or(self.total_frames, |f| f.min(self.total_frames));
            let take = max_frames.min(limit.saturating_sub(self.cursor));
            if take == 0 {
                return Err("injected synthetic decode error".to_string());
            }
            let mut left = Vec::with_capacity(take);
            let mut right = Vec::with_capacity(take);
            let rate = self.spec.sample_rate_hz as f64;
            for i in 0..take {
                let n = self.cursor + i;
                let mut v = (2.0 * PI * self.freq_hz * n as f64 / rate).sin() * self.amp;
                if self.clipped {
                    v = v.clamp(-0.9999, 0.9999);
                }
                left.push(v);
                right.push(v);
            }
            self.cursor += take;
            Ok(Some(StereoF64Buffer {
                sample_rate_hz: self.spec.sample_rate_hz,
                left,
                right,
            }))
        }
    }

    fn drain_track_events(
        buf: &BoundedBlockStreamBuffer,
        epoch: u64,
    ) -> (ap_dsp::pipeline::PipelineReport, usize, i32) {
        let mut report = None;
        let mut frames = 0usize;
        let mut max_abs_i32 = 0i32;
        loop {
            match buf.pop_for_epoch_timeout(epoch, Duration::from_secs(60)) {
                Some(StreamEvent::TrackStart { report: r, .. }) => report = Some(r),
                Some(StreamEvent::PcmBlock(b)) => {
                    frames += b.frames.len();
                    for &[l, r] in &b.frames {
                        max_abs_i32 = max_abs_i32.max(l.saturating_abs()).max(r.saturating_abs());
                    }
                }
                Some(StreamEvent::TrackEnd) => break,
                other => {
                    buf.reset_and_flush();
                    panic!("unexpected stream event: {other:?}");
                }
            }
        }
        (report.expect("missing TrackStart"), frames, max_abs_i32)
    }

    fn make_fast_ultimate_pipeline() -> AudioPipeline {
        // Use Null filter at 352.8k or simple-test at 44.1k for fast debug/release TDD runs while
        // still exercising the full W0..W4 ultimate state machine.
        AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy::default(),
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap()
    }

    #[test]
    fn gate_m3_short_track_header_and_zero_total_samples_use_normal_mode() {
        let db_root = temp_dir("m3");
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        let pipeline = make_fast_ultimate_pipeline();

        // Case A: header explicitly claims 10 s (< 30 s) -> normal (<30 s), no YAML written.
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();
        let mut src_a = SyntheticSource::sine(352_800, 1.0, 0.5, false);
        let dummy_path = db_root.join("short_a.flac");
        let mut carry = None;
        let mut backup = None;
        let mut pend = false;
        let res = stream_track_ultimate(
            &pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_a,
            &dummy_path,
            Some(&dummy_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(1.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            65_536,
        );
        assert!(matches!(res, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let (rep_a, frames_a, _) = drain_track_events(&buf, ep);
        assert_eq!(frames_a, 352_800);
        assert_eq!(rep_a.ultimate_report.unwrap().mode, "normal (<30 s)");

        // Case B (§5.5): total_samples = 0 / None header with 1 s body -> EOF inside W0 -> normal (<30 s)!
        let mut src_b = SyntheticSource::sine(352_800, 1.0, 0.5, false);
        src_b.claimed_total_frames = None;
        let res_b = stream_track_ultimate(
            &pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_b,
            &dummy_path,
            Some(&dummy_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            None,
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            65_536,
        );
        assert!(matches!(res_b, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let (rep_b, frames_b, _) = drain_track_events(&buf, ep);
        assert_eq!(frames_b, 352_800);
        assert_eq!(rep_b.ultimate_report.unwrap().mode, "normal (<30 s)");

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn gate_learn_on_play_exact_m_zero_wasted_headroom_and_md5_mismatch_refusal() {
        let db_root = temp_dir("exact_m");
        let track_path = db_root.join("track_32s.flac");
        fs::write(&track_path, b"dummy").unwrap();
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        let pipeline = make_fast_ultimate_pipeline();

        // Use native 8fS (352.8 kHz) at 32 s? No, 8 kHz upsampled or 44.1 kHz with --filter null?
        // Using 44.1 kHz with --filter null keeps 31 s tiny (~1.37M frames -> < 30 ms in test!)
        // while exercising full W0..W4, exact M measurement, learn-on-play YAML, and cached replay!
        let fast_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let fast_pipeline = AudioPipeline::with_rew_config_full_tail(
            fast_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();

        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();

        // Track amplitude = 0.85 (-1.41 dBFS, untagged):
        // - Play 1 (uncached): G <= G_HR (-3.0 dB), writes YAML ONCE with exact M ~= -1.41 dBFS.
        let mut src1 = SyntheticSource::sine(44_100, 31.0, 0.85, false);
        let mut carry = None;
        let mut backup = None;
        let mut pend = false;
        let buf_c = Arc::clone(&buf);
        let drain_h = std::thread::spawn(move || drain_track_events(&buf_c, ep));
        let r1 = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src1,
            &track_path,
            Some(&track_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r1, UltimateStreamResult::Played { wrote_yaml: true, .. }));
        let (rep1, frames1, _) = drain_h.join().unwrap();
        assert_eq!(frames1, 44_100 * 31);
        assert!(
            rep1.headroom.total_gain_db() <= -3.0 + 1e-6,
            "uncached untagged track must apply G <= G_HR (-3 dB), got {}",
            rep1.headroom.total_gain_db()
        );
        assert_eq!(rep1.ultimate_report.as_ref().unwrap().mode, "ultimate: live");

        // - Play 2 (cached exact M = -1.41 dBFS): G = 0.0 dB (zero wasted headroom!), 0 overs!
        let mut src2 = SyntheticSource::sine(44_100, 31.0, 0.85, false);
        let buf_c2 = Arc::clone(&buf);
        let drain_h2 = std::thread::spawn(move || drain_track_events(&buf_c2, ep));
        let r2 = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src2,
            &track_path,
            Some(&track_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r2, UltimateStreamResult::Played { wrote_yaml: true, .. }));
        let (rep2, _, _) = drain_h2.join().unwrap();
        let u2 = rep2.ultimate_report.as_ref().unwrap();
        assert_eq!(u2.mode, "ultimate: cached");
        assert!(u2.cache_hit);
        assert!(
            rep2.headroom.total_gain_db().abs() < 1e-6,
            "cached exact M=-1.41 dBFS must apply 0.0 dB (no wasted -3 dB!), got {}",
            rep2.headroom.total_gain_db()
        );

        // - MD5 mismatch fixture (§5.5): refuses to write YAML!
        let corrupt_path = db_root.join("corrupt.flac");
        fs::write(&corrupt_path, b"corrupt").unwrap();
        let mut src_bad = SyntheticSource::sine(44_100, 31.0, 0.5, false);
        src_bad.streaminfo_md5 = Some([0xAA; 16]);
        src_bad.computed_md5_at_eof = Some([0xBB; 16]);
        let buf_c3 = Arc::clone(&buf);
        let drain_h3 = std::thread::spawn(move || drain_track_events(&buf_c3, ep));
        let r3 = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_bad,
            &corrupt_path,
            Some(&corrupt_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r3, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let _ = drain_h3.join().unwrap();
        assert!(ult.store.load_for_track(Some(&corrupt_path), None).is_none());

        // - Mid-window skip discards cleanly with ZERO YAML write!
        let skip_path = db_root.join("skip.flac");
        fs::write(&skip_path, b"skip").unwrap();
        let mut src_skip = SyntheticSource::sine(44_100, 31.0, 0.5, false);
        src_skip.streaminfo_md5 = Some([0xCC; 16]);
        let abort_flag = AtomicBool::new(true);
        let r_skip = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || abort_flag.load(Ordering::Relaxed),
            &mut src_skip,
            &skip_path,
            Some(&skip_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r_skip, UltimateStreamResult::Aborted));
        assert!(ult.store.load_for_track(Some(&skip_path), None).is_none());

        let _ = pipeline;
        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn gate_streaming_m4_oq33_stale_tag_disproof_and_strict_per_track_independence_oq40() {
        let db_root = temp_dir("stream_oq40");
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        let fast_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let fast_pipeline = AudioPipeline::with_rew_config_full_tail(
            fast_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();

        // 1. Streaming source with quiet tag MUST STILL enforce -3 dB safety (OQ-33) & write 0 YAML!
        let mut src_stream = SyntheticSource::sine(44_100, 31.0, 0.25, true);
        src_stream.peak_meta.true_peak_linear = Some(0.25);
        let stream_label = PathBuf::from("tidal://track/1");
        let mut carry = None;
        let mut backup = None;
        let mut pend = false;
        let buf_c = Arc::clone(&buf);
        let h1 = std::thread::spawn(move || drain_track_events(&buf_c, ep));
        let rs = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_stream,
            &stream_label,
            None,
            Some("http://127.0.0.1/stream.flac".to_string()),
            None,
            PlayModeLabel::UpnpRenderer,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(rs, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let (rep_s, _, _) = h1.join().unwrap();
        assert_eq!(rep_s.headroom.mode, HeadroomMode::Minus3Db);
        assert!((rep_s.headroom.total_gain_db() - (-3.0)).abs() < 1e-6);

        // 2. Stale peak tag disproof (§5.5): tag claims 0.25 (-12 dBFS) but W0 peaks at 0.90!
        let stale_path = db_root.join("stale.flac");
        fs::write(&stale_path, b"stale").unwrap();
        let mut src_stale = SyntheticSource::sine(44_100, 31.0, 0.90, false);
        src_stale.peak_meta.replaygain_track_peak = Some(0.25);
        src_stale.streaminfo_md5 = Some([0xDD; 16]);
        src_stale.computed_md5_at_eof = Some([0xDD; 16]);
        let buf_c2 = Arc::clone(&buf);
        let h2 = std::thread::spawn(move || drain_track_events(&buf_c2, ep));
        let _ = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_stale,
            &stale_path,
            Some(&stale_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        let (rep_stale, _, _) = h2.join().unwrap();
        assert_eq!(rep_stale.headroom.mode, HeadroomMode::Minus3Db);
        assert!(
            rep_stale
                .ultimate_report
                .as_ref()
                .unwrap()
                .warnings
                .iter()
                .any(|w| w.contains("stale peak tag")),
            "expected stale peak tag warning"
        );

        // 3. Strict per-track independence (OQ-40): Track B's fixed gain G_B and M30 are 100%
        // identical whether or not Track A carried a loud gapless ConvolutionTail!
        let indep_path = db_root.join("indep_b.flac");
        fs::write(&indep_path, b"b").unwrap();
        let run_b = |with_tail: bool| -> (f64, f64) {
            let mut src_b = SyntheticSource::sine(44_100, 31.0, 0.40, false);
            src_b.peak_meta.true_peak_linear = Some(0.40);
            src_b.streaminfo_md5 = Some([0xEE; 16]);
            src_b.computed_md5_at_eof = Some([0xEE; 16]);
            let mut inc = if with_tail {
                Some(GaplessCarryover {
                    internal_rate_hz: 44_100,
                    output_rate_hz: 44_100,
                    output_bits: 16,
                    effective_shaper: ap_dsp::selector::NoiseShaperProfile::Bypass,
                    upsampler: None,
                    deemphasis: None,
                    convolver: Some(ConvolutionTail {
                        sample_rate_hz: 44_100,
                        left: vec![0.95; 512],
                        right: vec![0.95; 512],
                    }),
                    downsampler: None,
                    quantizer_state: None,
                })
            } else {
                None
            };
            let mut p = false;
            let mut bkp = None;
            let buf_x = Arc::clone(&buf);
            let hx = std::thread::spawn(move || drain_track_events(&buf_x, ep));
            let _ = stream_track_ultimate(
                &fast_pipeline,
                &ult,
                &buf,
                ep,
                || false,
                &mut src_b,
                &indep_path,
                Some(&indep_path),
                None,
                None,
                PlayModeLabel::SingleFile,
                Some(31.0),
                &mut inc,
                &mut bkp,
                &mut p,
                false,
                16_384,
            );
            let (rep, _, _) = hx.join().unwrap();
            let u = rep.ultimate_report.unwrap();
            (u.fixed_gain_db, u.m30_dbfs.unwrap())
        };
        let (g_alone, m30_alone) = run_b(false);
        let (g_after_a, m30_after_a) = run_b(true);
        assert_eq!(g_alone, g_after_a, "OQ-40: Track A must NEVER alter Track B's fixed gain!");
        assert!(
            (m30_alone - m30_after_a).abs() < 1e-12,
            "OQ-40: Track A must NEVER alter Track B's M30!"
        );

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn ultimate_null_filter_negative_gain_never_bypass_shaper_and_zero_gain_stays_bit_perfect() {
        let db_root = temp_dir("f2_quantizer");
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        let fast_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let fast_pipeline = AudioPipeline::with_rew_config_full_tail(
            fast_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();

        // Case 1 (F2): 44.1k/16-bit sine at 0.999, 31 s, filter_override=Null, uncached ->
        // G < 0 (-3 dB) MUST reconfigure away from Bypass to TPDF dither at >= 24 bits!
        let track_hot = db_root.join("hot_null.flac");
        fs::write(&track_hot, b"hot").unwrap();
        let mut src_hot = SyntheticSource::sine(44_100, 31.0, 0.999, false);
        let mut carry = None;
        let mut backup = None;
        let mut pend = false;
        let buf_c = Arc::clone(&buf);
        let h1 = std::thread::spawn(move || drain_track_events(&buf_c, ep));
        let r1 = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_hot,
            &track_hot,
            Some(&track_hot),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r1, UltimateStreamResult::Played { .. }));
        let (rep_hot, _, _) = h1.join().unwrap();
        assert!(rep_hot.headroom.total_gain_db() < 0.0);
        assert_ne!(
            rep_hot.effective_shaper,
            NoiseShaperProfile::Bypass,
            "negative fixed gain must NEVER requantize through Bypass without dither (F2)"
        );
        assert!(
            rep_hot.output_bits >= 24,
            "dithered negative-gain Null path must use >= 24-bit output, got {}",
            rep_hot.output_bits
        );

        // Case 2 (F2): 44.1k/16-bit sine at 0.25 with cached M -> G == 0.0 stays bit-perfect Bypass!
        let track_quiet = db_root.join("quiet_null.flac");
        fs::write(&track_quiet, b"quiet").unwrap();
        let mut src_q1 = SyntheticSource::sine(44_100, 31.0, 0.25, false);
        src_q1.streaminfo_md5 = Some([0x12; 16]);
        src_q1.computed_md5_at_eof = Some([0x12; 16]);
        let buf_c2 = Arc::clone(&buf);
        let h2 = std::thread::spawn(move || drain_track_events(&buf_c2, ep));
        let _ = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_q1,
            &track_quiet,
            Some(&track_quiet),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        let _ = h2.join().unwrap();

        let mut src_q2 = SyntheticSource::sine(44_100, 31.0, 0.25, false);
        src_q2.streaminfo_md5 = Some([0x12; 16]);
        src_q2.computed_md5_at_eof = Some([0x12; 16]);
        let buf_c3 = Arc::clone(&buf);
        let h3 = std::thread::spawn(move || {
            let mut report = None;
            let mut first_frames: Vec<[i32; 2]> = Vec::new();
            loop {
                match buf_c3.pop_for_epoch_timeout(ep, Duration::from_secs(2)) {
                    Some(StreamEvent::TrackStart { report: r, .. }) => report = Some(r),
                    Some(StreamEvent::PcmBlock(b)) => {
                        if first_frames.len() < 1_024 {
                            let need = 1_024 - first_frames.len();
                            first_frames.extend_from_slice(&b.frames[..b.frames.len().min(need)]);
                        }
                    }
                    Some(StreamEvent::TrackEnd) => break,
                    other => panic!("unexpected event in Case 2: {other:?}"),
                }
            }
            (report.expect("missing TrackStart"), first_frames)
        });
        let _ = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_q2,
            &track_quiet,
            Some(&track_quiet),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        let (rep_q2, pcm_q2) = h3.join().unwrap();
        assert!(rep_q2.headroom.total_gain_db().abs() < 1e-9);
        assert_eq!(
            rep_q2.effective_shaper,
            NoiseShaperProfile::Bypass,
            "0 dB Null pass-through must stay bit-perfect Bypass"
        );
        assert_eq!(rep_q2.output_bits, 16);
        // Verify output PCM matches source 16-bit samples bit-for-bit (F2 zero-gain bit-perfect check).
        assert_eq!(pcm_q2.len(), 1_024);
        let mut src_ref = SyntheticSource::sine(44_100, 31.0, 0.25, false);
        let ref_blk = src_ref.read_next_block(1_024).unwrap().unwrap();
        let mut ref_q =
            ap_dsp::quantize::StreamingQuantizer::new(NoiseShaperProfile::Bypass, 16).unwrap();
        let ref_pcm = ref_q.quantize_block(&ref_blk);
        assert_eq!(
            pcm_q2, ref_pcm.frames,
            "F2: 0 dB Null pass-through PCM must match source 16-bit samples bit-for-bit"
        );

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn w4_decode_error_emits_trackend_drains_tail_and_skips_yaml_f7_fb2_f8() {
        let db_root = temp_dir("f7_f8");
        let track_path = db_root.join("corrupt_w4.flac");
        fs::write(&track_path, b"corrupt_w4").unwrap();
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        let fast_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let fast_pipeline = AudioPipeline::with_rew_config_full_tail(
            fast_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();

        // 1. F7: 40 s source failing at 35 s -> TrackStart + ~35 s PCM + TrackEnd, Played (not Failed), wrote_yaml=false!
        let mut src = SyntheticSource::sine(44_100, 40.0, 0.5, false);
        src.fail_at_frame = Some(44_100 * 35);
        let mut carry = Some(GaplessCarryover {
            internal_rate_hz: 44_100,
            output_rate_hz: 44_100,
            output_bits: 24,
            effective_shaper: NoiseShaperProfile::TpdfDither,
            upsampler: None,
            deemphasis: None,
            convolver: Some(ConvolutionTail {
                sample_rate_hz: 44_100,
                left: vec![0.4; 256],
                right: vec![0.4; 256],
            }),
            downsampler: None,
            quantizer_state: None,
        });
        let mut backup = None;
        let mut pend = false;
        let buf_c = Arc::clone(&buf);
        let h1 = std::thread::spawn(move || drain_track_events(&buf_c, ep));
        let res = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src,
            &track_path,
            Some(&track_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(40.0),
            &mut carry,
            &mut backup,
            &mut pend,
            true,
            16_384,
        );
        match res {
            UltimateStreamResult::Played {
                next_carryover,
                next_pending_end,
                wrote_yaml,
                warning,
            } => {
                assert!(next_carryover.is_none(), "errored track must not export carryover");
                assert!(!next_pending_end);
                assert!(!wrote_yaml, "F7: errored track must NEVER write YAML");
                assert!(warning.is_some());
            }
            other => panic!("F7: expected Played with warning, got {other:?}"),
        }
        assert!(backup.is_none(), "F8: panic_backup_carry must be cleared after W3 push");
        let (_, frames, max_abs) = h1.join().unwrap();
        assert_eq!(frames, 44_100 * 35);
        assert!(max_abs > 0);
        assert!(ult.store.load_for_track(Some(&track_path), None).is_none());

        // 2. F8: Panic injected right after incoming_carryover.take() preserves `panic_backup_carry`!
        let mut src_p = SyntheticSource::sine(44_100, 31.0, 0.5, false);
        let mut carry_p = Some(GaplessCarryover {
            internal_rate_hz: 44_100,
            output_rate_hz: 44_100,
            output_bits: 24,
            effective_shaper: NoiseShaperProfile::TpdfDither,
            upsampler: None,
            deemphasis: None,
            convolver: Some(ConvolutionTail {
                sample_rate_hz: 44_100,
                left: vec![0.3; 128],
                right: vec![0.3; 128],
            }),
            downsampler: None,
            quantizer_state: None,
        });
        let mut backup_p = None;
        let mut pend_p = true;
        let panic_target = track_path.to_string_lossy().into_owned();
        arm_test_panic_after_carry_take(Some(&panic_target));
        let panic_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stream_track_ultimate(
                &fast_pipeline,
                &ult,
                &buf,
                ep,
                || false,
                &mut src_p,
                &track_path,
                Some(&track_path),
                None,
                None,
                PlayModeLabel::SingleFile,
                Some(31.0),
                &mut carry_p,
                &mut backup_p,
                &mut pend_p,
                false,
                16_384,
            )
        }));
        disarm_test_panic_after_carry_take(&panic_target);
        assert!(panic_res.is_err(), "expected injected panic");
        assert!(carry_p.is_none());
        assert!(
            backup_p.is_some(),
            "F8: panic_backup_carry MUST still hold Track A's carryover after panic!"
        );

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn fallback_unavailable_warning_is_reported_f9_fb1() {
        let db_root = temp_dir("f9_fb1");
        let track_path = db_root.join("slow_probe.flac");
        fs::write(&track_path, b"slow").unwrap();
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: false,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        // Use a slightly larger buffer ceiling in this test so the pre-populated 1 s cushion
        // stays buffered throughout W2 without racing a concurrent drainer thread.
        let buf = BoundedBlockStreamBuffer::with_limits(
            352_800 * 35,
            Duration::from_secs(35),
        );
        let ep = buf.current_epoch();

        // 1. F-B1: Already on `GaussHiresLp` (same as hi-res fallback, 2x 176.4 -> 352.8 kHz is fast
        //    in both debug & release) with a 1 s buffered cushion (burst requires >= 40x RT) +
        //    120 ms delay per 3 s block (<= 25x RT) -> MUST record "fallback unavailable"!
        let lp_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::GaussHiresLp),
            ..UpsamplerPolicy::default()
        };
        let lp_pipeline = AudioPipeline::with_rew_config_full_tail(
            lp_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        assert!(buf.push_for_epoch(
            ep,
            StreamEvent::PcmBlock(ap_core::buffer::InterleavedInt32Buffer {
                sample_rate_hz: 352_800,
                frames: vec![[0, 0]; 352_800],
            })
        ));
        let mut src_fb1 = SyntheticSource::sine(176_400, 30.1, 0.5, false);
        let mut carry = None;
        let mut backup = None;
        let mut pend = false;
        TEST_W2_BLOCK_DELAY_MS.with(|c| c.set(120));
        let r1 = stream_track_ultimate(
            &lp_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_fb1,
            &track_path,
            Some(&track_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(30.1),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            176_400 * 3,
        );
        TEST_W2_BLOCK_DELAY_MS.with(|c| c.set(0));
        assert!(matches!(r1, UltimateStreamResult::Played { .. }));
        let mut rep_fb1 = None;
        loop {
            match buf.pop_for_epoch_timeout(ep, Duration::from_secs(2)) {
                Some(StreamEvent::TrackStart { report: r, .. }) => rep_fb1 = Some(r),
                Some(StreamEvent::PcmBlock(_)) => {}
                Some(StreamEvent::TrackEnd) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        let urep = rep_fb1.unwrap().ultimate_report.expect("missing ultimate_report");
        assert!(
            urep.warnings.iter().any(|w| w.contains("fallback unavailable")),
            "F-B1: expected 'fallback unavailable' warning, got {:?}",
            urep.warnings
        );

        // 2. F9 early-abort probe: `ShortLp` with 1 s buffer cushion + 180 ms/block delay on the
        //    slow primary filter (10 W0 blocks would take 10 + 10 = 20 renders without early abort).
        //    Probe aborts after 1 block (3.0 s >= 2.5 s) and switches to `GaussHiresLp` fallback,
        //    so total W2 blocks rendered across primary + fallback is strictly 11!
        assert!(buf.push_for_epoch(
            ep,
            StreamEvent::PcmBlock(ap_core::buffer::InterleavedInt32Buffer {
                sample_rate_hz: 352_800,
                frames: vec![[0, 0]; 352_800],
            })
        ));
        let short_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::ShortLp),
            ..UpsamplerPolicy::default()
        };
        let short_pipeline = AudioPipeline::with_rew_config_full_tail(
            short_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let mut src_f9 = SyntheticSource::sine(176_400, 30.1, 0.5, false);
        TEST_W2_BLOCKS_RENDERED.with(|c| c.set(0));
        TEST_W2_BLOCK_DELAY_MS.with(|c| c.set(180));
        let r2 = stream_track_ultimate(
            &short_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_f9,
            &track_path,
            Some(&track_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(30.1),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            176_400 * 3,
        );
        TEST_W2_BLOCK_DELAY_MS.with(|c| c.set(0));
        let blocks_rendered = TEST_W2_BLOCKS_RENDERED.with(|c| c.get());
        assert!(matches!(r2, UltimateStreamResult::Played { .. }));
        let mut rep_f9 = None;
        loop {
            match buf.pop_for_epoch_timeout(ep, Duration::from_secs(2)) {
                Some(StreamEvent::TrackStart { report: r, .. }) => rep_f9 = Some(r),
                Some(StreamEvent::PcmBlock(_)) => {}
                Some(StreamEvent::TrackEnd) => break,
                other => panic!("unexpected event: {other:?}"),
            }
        }
        let rep_f9 = rep_f9.unwrap();
        assert_eq!(rep_f9.dsp_plan.filter, FilterArchetype::GaussHiresLp);
        assert_eq!(
            blocks_rendered, 11,
            "F9: early probe abort must stop after 1 block (1 + 10 fallback = 11 vs 20 without abort)"
        );

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn w4_tail_energy_and_eof_flush_quantize_error_f7_fb2() {
        let db_root = temp_dir("f7_fb2_tail");
        let track_path = db_root.join("tail_and_eof.flac");
        fs::write(&track_path, b"tail_eof").unwrap();
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        let fast_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let fast_pipeline = AudioPipeline::with_rew_config_full_tail(
            fast_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();

        // 1. F7 tail-energy check: compare first 256 samples of W4-failing track WITH vs WITHOUT
        //    incoming ConvolutionTail([0.35; 256]) -> carried tail energy MUST appear in PCM!
        let run_capture_prefix = |with_tail: bool| -> Vec<[i32; 2]> {
            let mut src = SyntheticSource::sine(44_100, 35.0, 0.1, false);
            src.fail_at_frame = Some(44_100 * 31);
            let mut carry = if with_tail {
                Some(GaplessCarryover {
                    internal_rate_hz: 44_100,
                    output_rate_hz: 44_100,
                    output_bits: 24,
                    effective_shaper: NoiseShaperProfile::TpdfDither,
                    upsampler: None,
                    deemphasis: None,
                    convolver: Some(ConvolutionTail {
                        sample_rate_hz: 44_100,
                        left: vec![0.35; 256],
                        right: vec![0.35; 256],
                    }),
                    downsampler: None,
                    quantizer_state: None,
                })
            } else {
                None
            };
            let mut backup = None;
            let mut pend = false;
            let buf_c = Arc::clone(&buf);
            let h = std::thread::spawn(move || {
                let mut prefix = Vec::new();
                loop {
                    match buf_c.pop_for_epoch_timeout(ep, Duration::from_secs(30)) {
                        Some(StreamEvent::TrackStart { .. }) => {}
                        Some(StreamEvent::PcmBlock(b)) => {
                            if prefix.len() < 256 {
                                let need = 256 - prefix.len();
                                prefix.extend_from_slice(&b.frames[..b.frames.len().min(need)]);
                            }
                        }
                        Some(StreamEvent::TrackEnd) => break,
                        other => {
                            buf_c.reset_and_flush();
                            panic!("unexpected event: {other:?}");
                        }
                    }
                }
                prefix
            });
            let r = stream_track_ultimate(
                &fast_pipeline,
                &ult,
                &buf,
                ep,
                || false,
                &mut src,
                &track_path,
                Some(&track_path),
                None,
                None,
                PlayModeLabel::SingleFile,
                Some(35.0),
                &mut carry,
                &mut backup,
                &mut pend,
                false,
                16_384,
            );
            assert!(matches!(r, UltimateStreamResult::Played { wrote_yaml: false, .. }));
            h.join().unwrap()
        };

        let no_tail_pcm = run_capture_prefix(false);
        let with_tail_pcm = run_capture_prefix(true);
        assert_eq!(no_tail_pcm.len(), 256);
        assert_eq!(with_tail_pcm.len(), 256);
        let diff_energy: i128 = with_tail_pcm
            .iter()
            .zip(no_tail_pcm.iter())
            .map(|(a, b)| (i128::from(a[0]) - i128::from(b[0])).abs())
            .sum();
        assert!(
            diff_energy > 256 * 100_000_000_i128,
            "F7: carried ConvolutionTail energy must appear in quantized output (diff_energy={diff_energy})"
        );

        // 2. F-B2: Inject quantize error during EOF flush -> MUST log warning, emit TrackEnd,
        //    and suppress YAML learn-on-play (`wrote_yaml == false`)!
        //    Use `ShortLp` so `flush_unity_f64()` produces a non-empty filter tail block at EOF.
        let lp_pipeline = AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy {
                filter_override: Some(FilterArchetype::ShortLp),
                ..UpsamplerPolicy::default()
            },
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let eof_track = db_root.join("fb2_eof_err.flac");
        fs::write(&eof_track, b"fb2").unwrap();
        let mut src_eof = SyntheticSource::sine(176_400, 30.2, 0.5, false);
        let mut carry = None;
        let mut backup = None;
        let mut pend = false;
        let buf_c2 = Arc::clone(&buf);
        let h2 = std::thread::spawn(move || drain_track_events(&buf_c2, ep));
        TEST_FAIL_EOF_FLUSH_QUANTIZE.with(|c| c.set(true));
        let r_eof = stream_track_ultimate(
            &lp_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_eof,
            &eof_track,
            Some(&eof_track),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(30.2),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            176_400 * 3,
        );
        TEST_FAIL_EOF_FLUSH_QUANTIZE.with(|c| c.set(false));
        let _ = h2.join().unwrap();
        match r_eof {
            UltimateStreamResult::Played {
                wrote_yaml,
                warning,
                ..
            } => {
                assert!(!wrote_yaml, "F-B2: EOF flush quantize error must suppress YAML write");
                let w = warning.expect("F-B2: EOF flush quantize error must surface warning");
                assert!(
                    w.contains("EOF flush quantize error"),
                    "unexpected F-B2 warning: {w}"
                );
            }
            other => panic!("F-B2: expected Played with warning, got {other:?}"),
        }
        assert!(
            ult.store.load_for_track(Some(&eof_track), None).is_none(),
            "F-B2: YAML file must not be persisted after EOF flush quantize error"
        );

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn w3_and_w4_quantize_errors_skip_discontinuous_session_flush_c4() {
        // Review #8 C4: On W3 or W4 quantize error, `session.flush_unity_f64()` MUST be skipped
        // (no discontinuous filter tail appended after the last good block); only any remaining
        // incoming `additive_tail` is drained, followed by `TrackEnd`, and `wrote_yaml == false`.
        let db_root = temp_dir("c4_skip_flush");
        let ult = UltimateRuntimeContext::from_legacy(
            UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            TrackDbStore::new(db_root.clone(), false),
        );
        // ShortLp at 44.1 -> 352.8 kHz produces a non-empty filter flush tail if flushed!
        let lp_pipeline = AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy {
                filter_override: Some(FilterArchetype::ShortLp),
                ..UpsamplerPolicy::default()
            },
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );

        // 1. W3 quantize error at block 0 with a 256-sample carried additive tail:
        //    ZERO session blocks should follow; ONLY the 256-sample additive-tail drain + TrackEnd!
        {
            let ep = buf.reset_and_flush();
            let p_w3 = db_root.join("c4_w3_err.flac");
            fs::write(&p_w3, b"w3").unwrap();
            let mut src = SyntheticSource::sine(44_100, 32.0, 0.3, false);
            let mut carry = Some(GaplessCarryover {
                internal_rate_hz: 352_800,
                output_rate_hz: 352_800,
                output_bits: 24,
                effective_shaper: NoiseShaperProfile::TpdfDither,
                upsampler: None,
                deemphasis: None,
                convolver: Some(ConvolutionTail {
                    sample_rate_hz: 352_800,
                    left: vec![0.2; 256],
                    right: vec![-0.2; 256],
                }),
                downsampler: None,
                quantizer_state: None,
            });
            let mut backup = None;
            let mut pend = false;
            let buf_c = Arc::clone(&buf);
            let h = std::thread::spawn(move || drain_track_events(&buf_c, ep));
            TEST_FAIL_W3_QUANTIZE_AT_BLOCK.with(|c| c.set(Some(0)));
            let res = stream_track_ultimate(
                &lp_pipeline,
                &ult,
                &buf,
                ep,
                || false,
                &mut src,
                &p_w3,
                Some(&p_w3),
                None,
                None,
                PlayModeLabel::SingleFile,
                Some(32.0),
                &mut carry,
                &mut backup,
                &mut pend,
                false,
                44_100 * 5,
            );
            TEST_FAIL_W3_QUANTIZE_AT_BLOCK.with(|c| c.set(None));
            let (_, total_frames, _) = h.join().unwrap();
            assert!(matches!(
                res,
                UltimateStreamResult::Played {
                    wrote_yaml: false,
                    warning: Some(_),
                    ..
                }
            ));
            assert_eq!(
                total_frames, 256,
                "C4 (W3): only the 256-sample additive tail may be pushed (no discontinuous session flush)"
            );
        }

        // 2. W4 quantize error at block 0 (right after W3):
        //    Exact frame count MUST equal W0 (30 s * 352,800) with ZERO extra session flush frames!
        {
            let ep = buf.reset_and_flush();
            let p_w4 = db_root.join("c4_w4_err.flac");
            fs::write(&p_w4, b"w4").unwrap();
            let mut src = SyntheticSource::sine(44_100, 34.0, 0.3, false);
            let mut carry = None;
            let mut backup = None;
            let mut pend = false;
            let buf_c = Arc::clone(&buf);
            let h = std::thread::spawn(move || drain_track_events(&buf_c, ep));
            TEST_FAIL_W4_QUANTIZE_AT_BLOCK.with(|c| c.set(Some(0)));
            let res = stream_track_ultimate(
                &lp_pipeline,
                &ult,
                &buf,
                ep,
                || false,
                &mut src,
                &p_w4,
                Some(&p_w4),
                None,
                None,
                PlayModeLabel::SingleFile,
                Some(34.0),
                &mut carry,
                &mut backup,
                &mut pend,
                false,
                44_100 * 5,
            );
            TEST_FAIL_W4_QUANTIZE_AT_BLOCK.with(|c| c.set(None));
            let (_, total_frames, _) = h.join().unwrap();
            assert!(matches!(
                res,
                UltimateStreamResult::Played {
                    wrote_yaml: false,
                    warning: Some(_),
                    ..
                }
            ));
            // ShortLp 8x upsampler has a non-empty flush tail (~thousands of frames). Verify
            // output stopped strictly at W0 (no W4 block and no discontinuous EOF flush tail).
            assert!(
                total_frames <= 30 * 352_800,
                "C4 (W4): expected <= W0 frames ({}), got {total_frames} (discontinuous flush leaked!)",
                30 * 352_800
            );
        }

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn schema2_mine_vs_suggested_short_track_override_hot_reload_and_malformed_fallback() {
        let root = temp_dir("schema2_playback");
        let music_root = root.join("music");
        let album_dir = music_root.join("Jazz").join("Miles Davis - Kind of Blue");
        let filterdb_dir = root.join("filterdb");
        let choices_path = root.join("my-filters.yaml");
        fs::create_dir_all(&album_dir).unwrap();

        let track_long = album_dir.join("01 So What.flac");
        let track_short = album_dir.join("02 Interlude.flac");
        fs::write(&track_long, b"dummy long flac").unwrap();
        fs::write(&track_short, b"dummy short flac").unwrap();

        fs::write(
            &choices_path,
            "schema: 2\n\nfolders:\n  \"main:Jazz/Miles Davis - Kind of Blue\":\n    filter: short-lp\n    note: \"prefer short-lp on this album\"\n",
        )
        .unwrap();

        let mut manifest = LibraryManifest::default();
        manifest.upsert_root("main", &music_root);

        let make_ctx = |mode: FilterMode| {
            UltimateRuntimeContext::new(
                UltimateConfig {
                    enabled: true,
                    genre_rules: true,
                    learn_on_play: true,
                },
                TrackDbStore::new(root.join("legacy_trackdb"), false),
                FilterDbStore::new(filterdb_dir.clone()),
                manifest.clone(),
                choices_path.clone(),
                mode,
            )
        };

        let pipeline = make_fast_ultimate_pipeline();

        // 1. Short track (< 30 s) in Auto/Mine mode honors folder override from my-filters.yaml!
        let ult_auto = make_ctx(FilterMode::Auto);
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();
        let mut src_short = SyntheticSource::sine(44_100, 1.0, 0.5, false);
        let mut carry = None;
        let mut backup = None;
        let mut pend = false;
        let r_short = stream_track_ultimate(
            &pipeline,
            &ult_auto,
            &buf,
            ep,
            || false,
            &mut src_short,
            &track_short,
            Some(&track_short),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(1.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            8_192,
        );
        assert!(matches!(r_short, UltimateStreamResult::Played { .. }));
        let (rep_short, _, _) = drain_track_events(&buf, ep);
        assert_eq!(rep_short.dsp_plan.filter, FilterArchetype::ShortLp);
        let ult_rep_short = rep_short.ultimate_report.unwrap();
        assert_eq!(ult_rep_short.override_label.as_deref(), Some("mine: folder"));
        assert!(ult_rep_short.summary().contains("playing: short-lp (mine: folder)"));

        // 2. Same short track in Suggested mode ignores my-filters.yaml and plays default gauss-long-lp!
        let ult_sug = make_ctx(FilterMode::Suggested);
        let buf2 = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep2 = buf2.current_epoch();
        let mut src_short2 = SyntheticSource::sine(44_100, 1.0, 0.5, false);
        let _ = stream_track_ultimate(
            &pipeline,
            &ult_sug,
            &buf2,
            ep2,
            || false,
            &mut src_short2,
            &track_short,
            Some(&track_short),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(1.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            8_192,
        );
        let (rep_sug, _, _) = drain_track_events(&buf2, ep2);
        assert_eq!(rep_sug.dsp_plan.filter, FilterArchetype::GaussLongLp);
        assert_eq!(rep_sug.ultimate_report.unwrap().override_label, None);

        // 3. Long track (>= 30 s) in Auto/Mine mode uses folder override (`short-lp`), reports
        //    dual `playing: short-lp (mine: folder) | suggested: ...`, AND writes Schema-2 album
        //    YAML with pure machine `suggested` (never overwriting my-filters.yaml!).
        let buf3 = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep3 = buf3.current_epoch();
        let mut src_long = SyntheticSource::sine(44_100, 31.0, 0.5, false);
        let buf3_c = Arc::clone(&buf3);
        let drain_h = std::thread::spawn(move || drain_track_events(&buf3_c, ep3));
        let r_long = stream_track_ultimate(
            &pipeline,
            &ult_auto,
            &buf3,
            ep3,
            || false,
            &mut src_long,
            &track_long,
            Some(&track_long),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r_long, UltimateStreamResult::Played { wrote_yaml: true, .. }));
        let (rep_long, _, _) = drain_h.join().unwrap();
        assert_eq!(rep_long.dsp_plan.filter, FilterArchetype::ShortLp);
        let u_long = rep_long.ultimate_report.unwrap();
        assert_eq!(u_long.override_label.as_deref(), Some("mine: folder"));
        assert_eq!(u_long.suggested_filter.as_deref(), Some("sinc-steep-4m"));

        // Verify Schema-2 album YAML exists and stored machine suggestion (not user choice!),
        // with post-chain peak keyed to actually played `short-lp`.
        let album = ult_auto
            .filter_db
            .load_album("main", "Jazz/Miles Davis - Kind of Blue")
            .expect("Schema-2 album YAML should be created on learn-on-play");
        let tr = album.tracks.get("01 So What.flac").unwrap();
        assert_eq!(tr.suggested, "sinc-steep-4m");
        assert_eq!(tr.post_chain_peaks.len(), 1);
        assert_eq!(tr.post_chain_peaks[0].key.upsampler, "short-lp");

        // 4. Malformed my-filters.yaml falls back cleanly with line-numbered warning, never fails playback!
        fs::write(&choices_path, "schema: 2\n\tbad_tab: true\n").unwrap();
        let buf4 = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep4 = buf4.current_epoch();
        let mut src_bad = SyntheticSource::sine(44_100, 1.0, 0.5, false);
        let r_bad = stream_track_ultimate(
            &pipeline,
            &ult_auto,
            &buf4,
            ep4,
            || false,
            &mut src_bad,
            &track_short,
            Some(&track_short),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(1.0),
            &mut carry,
            &mut backup,
            &mut pend,
            false,
            8_192,
        );
        assert!(matches!(r_bad, UltimateStreamResult::Played { .. }));
        let (rep_bad, _, _) = drain_track_events(&buf4, ep4);
        assert_eq!(rep_bad.dsp_plan.filter, FilterArchetype::GaussLongLp);
        let u_bad = rep_bad.ultimate_report.unwrap();
        assert!(
            u_bad.warnings.iter().any(|w| w.contains("line 2")),
            "expected line-numbered parse warning, got {:?}",
            u_bad.warnings
        );

        let _ = fs::remove_dir_all(&root);
    }
}
