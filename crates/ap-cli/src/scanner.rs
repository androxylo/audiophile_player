// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Offline parallel, folder-sharded, incremental, resumable FLAC library scanner
//! (`audiophile-player scan`, Schema 2 `FILTER_DATABASE_RESEARCH.md` §§4 & 7).
//!
//! Guarantees:
//! - Strictly read-only on music files (NEVER modifies FLAC audio, metadata, or timestamps).
//! - Strictly per-track (`OQ-40`): every track's detector analysis depends solely on that track.
//! - Folder-sharded parallelism: each worker owns one album folder at a time (zero lock contention
//!   on album YAML files!).
//! - Fast-skip unchanged tracks (`size` + `mtime ±2 s` + current `detector_version`).
//! - Metadata-only retag fast path: when `STREAMINFO.md5` is unchanged, reuses detector results
//!   and only re-fuses metadata/tags (e.g. edited `GENRE`) without re-decoding audio.
//! - Move detection (`md5` + `(size, seconds, head_hash)` fallback): reuses existing analysis for
//!   moved files without re-decoding, never confuses duplicates with moves, and writes
//!   `filterdb/relink-report.yaml` whenever a moved file is referenced in `my-filters.yaml`.
//! - Never writes `my-filters.yaml` (user-owned file).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ap_core::config::{ConvolutionConfig, PlayerConfig};
use ap_core::filter_db::{
    AlbumAnalysisFile, AlbumTrackAnalysis, DbEnv, FilterDbStore, LibraryManifest, RelinkEntry,
    RelinkReport, file_head_4k_hash,
};
use ap_core::track_db::{PostChainKey, file_size_and_mtime, now_unix_secs};
use ap_core::user_filters::{
    FilterMode, UserFiltersCache, UserFiltersFile, format_root_rel_key,
};
use ap_dsp::analysis::{DETECTOR_VERSION, TrackAnalyzer};
use ap_dsp::convolution::{RewConvolverFactory, StereoConvolverFactory};
use ap_dsp::deemphasis::StereoDeemphasis;
use ap_dsp::fusion::{CachedWholeTrackEvidence, build_track_db_entry};
use ap_dsp::selector::{
    SelectedDspPlan, UpsamplerPolicy, build_post_chain_key, resolve_active_selection,
    resolve_machine_suggestion,
};
use ap_dsp::upsampler::build_upsampler;
use ap_source::flac::{AudioBlockSource, FlacBlockReader};
use ap_source::library::{default_user_music_dir, discover_flac_files};
use ap_source::md5::md5_to_hex;

use crate::signals::ShutdownController;

const SCAN_BLOCK_FRAMES: usize = 8_192;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Target folder path, named root (from `config.toml` / `library.yaml`), or `None` to scan
    /// all configured roots.
    pub folder: Option<PathBuf>,
    pub report: bool,
    pub post_chain: bool,
    pub force: bool,
    pub jobs: Option<usize>,
    pub trackdb_dir: Option<PathBuf>,
    pub filterdb_dir: Option<PathBuf>,
    pub choices_file: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub rew_override: Option<ConvolutionConfig>,
    pub upsampler_policy: UpsamplerPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub total_files: usize,
    pub scanned: usize,
    pub skipped_current: usize,
    pub retagged_fast: usize,
    pub moved_reused: usize,
    pub failed: usize,
    pub suggested_changed: usize,
    pub relink_candidates: usize,
    pub report_lines: Vec<String>,
}

/// Entrypoint for `audiophile-player scan [FOLDER|ROOT] ...`.
pub fn run_scan_cli(
    opts: ScanOptions,
    controller: &ShutdownController,
) -> Result<(), String> {
    let summary = scan_folder(&opts, Some(controller))?;
    println!(
        "Scan complete: total={} scanned={} skipped(current)={} retagged={} moved={} failed={} suggested_changed={}",
        summary.total_files,
        summary.scanned,
        summary.skipped_current,
        summary.retagged_fast,
        summary.moved_reused,
        summary.failed,
        summary.suggested_changed,
    );
    if summary.relink_candidates > 0 {
        println!(
            "  Note: {} track(s) referenced in my-filters.yaml moved. Run `audiophile-player filters relink` to preview.",
            summary.relink_candidates
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct OldTrackSnapshot {
    root: String,
    rel_folder: String,
    file_name: String,
    track: AlbumTrackAnalysis,
}

impl OldTrackSnapshot {
    fn root_rel_key(&self) -> String {
        format_root_rel_key(&self.root, &self.rel_folder, Some(&self.file_name))
    }

    fn canonical_tuple(&self) -> (String, String, String) {
        (
            self.root.to_ascii_lowercase(),
            self.rel_folder.to_ascii_lowercase(),
            self.file_name.to_ascii_lowercase(),
        )
    }
}

struct MoveIndex {
    by_md5: HashMap<String, Vec<OldTrackSnapshot>>,
    by_fallback: HashMap<(u64, i64, String), Vec<OldTrackSnapshot>>,
    present_keys: HashSet<(String, String, String)>,
}

impl MoveIndex {
    fn find_unique_vanished_by_md5(&self, md5: &str) -> Option<&OldTrackSnapshot> {
        let list = self.by_md5.get(md5)?;
        let mut vanished = list
            .iter()
            .filter(|s| !self.present_keys.contains(&s.canonical_tuple()));
        let first = vanished.next()?;
        if vanished.next().is_some() {
            return None; // ambiguous multiple vanished copies
        }
        // Duplicate safety: if ANY existing copy of this MD5 is still present at its old location,
        // treat this as a duplicate rather than a move!
        let any_still_present = list
            .iter()
            .any(|s| self.present_keys.contains(&s.canonical_tuple()));
        if any_still_present {
            return None;
        }
        Some(first)
    }

    fn find_unique_vanished_by_fallback(
        &self,
        size: u64,
        seconds_rounded: i64,
        head_hash: &str,
    ) -> Option<&OldTrackSnapshot> {
        let key = (size, seconds_rounded, head_hash.to_string());
        let list = self.by_fallback.get(&key)?;
        let any_still_present = list
            .iter()
            .any(|s| self.present_keys.contains(&s.canonical_tuple()));
        if any_still_present {
            return None;
        }
        let mut vanished = list
            .iter()
            .filter(|s| !self.present_keys.contains(&s.canonical_tuple()));
        let first = vanished.next()?;
        if vanished.next().is_some() {
            return None;
        }
        Some(first)
    }
}

struct AlbumWorkUnit {
    root: String,
    rel_folder: String,
    files: Vec<PathBuf>,
}

#[derive(Default)]
struct AlbumWorkOutcome {
    scanned: usize,
    skipped_current: usize,
    retagged_fast: usize,
    moved_reused: usize,
    failed: usize,
    suggested_changed: usize,
    detected_moves: Vec<RelinkEntry>,
    report_lines: Vec<(bool, String)>,
}

fn canonical_album_key(root: &str, rel_folder: &str) -> (String, String) {
    (
        root.to_ascii_lowercase(),
        rel_folder.to_ascii_lowercase(),
    )
}

fn ensure_root_for_folder(manifest: &mut LibraryManifest, folder: &Path) -> String {
    if let Some((root, _)) = manifest.resolve_folder_path(folder) {
        return root;
    }
    let name = if !manifest.roots.contains_key("main") {
        "main".to_string()
    } else {
        let mut idx = 2usize;
        loop {
            let cand = format!("root{idx}");
            if !manifest.roots.contains_key(&cand) {
                break cand;
            }
            idx += 1;
        }
    };
    manifest.upsert_root(&name, folder);
    name
}

/// Run the offline folder-sharded batch scanner, updating Schema-2 album YAMLs, `library.yaml`,
/// and `relink-report.yaml` (plus opt-in `--sidecar` legacy files when requested).
pub fn scan_folder(
    opts: &ScanOptions,
    controller: Option<&ShutdownController>,
) -> Result<ScanSummary, String> {
    let loaded_cfg = PlayerConfig::discover_and_load(opts.config_path.as_deref())?;
    let conv_cfg = opts
        .rew_override
        .clone()
        .unwrap_or_else(|| loaded_cfg.config.convolution.clone());
    let genre_rules = loaded_cfg.config.ultimate.genre_rules;
    let rew_factory = RewConvolverFactory::new(conv_cfg).with_full_tail(true);

    // Review #9 U6: resolve filterdb, choices_path, and named library roots via the single shared
    // `DbEnv::resolve` helper. When `--filterdb-dir` is unset, `music_dir` / `~/Music` anchors
    // `"main"` so scanning a subfolder never redefines `"main"`. When `--filterdb-dir` isolates a
    // brand-new DB and an explicit folder is given, let `ensure_root_for_folder` register that
    // folder as `"main"` rather than pulling in `~/Music`.
    let default_music = if opts.filterdb_dir.is_some() && opts.folder.is_some() {
        None
    } else {
        default_user_music_dir().filter(|p| p.is_dir())
    };
    let db_env = DbEnv::resolve(
        &loaded_cfg.config,
        opts.filterdb_dir.as_deref(),
        opts.choices_file.as_deref(),
        opts.trackdb_dir.as_deref(),
        default_music.as_deref(),
        DETECTOR_VERSION,
    );
    let filter_db = db_env.filter_db;
    let (_, user_filters, _) = UserFiltersCache::new(db_env.choices_path).load_fresh();
    let scan_started_unix_s = now_unix_secs();
    let mut manifest = db_env.manifest;

    // Resolve target folder(s) to scan.
    let target_dirs: Vec<PathBuf> = match &opts.folder {
        Some(target) => {
            if !target.is_dir() {
                if let Some(root_str) = target.to_str() {
                    if let Some(root_path) = manifest.roots.get(root_str) {
                        vec![PathBuf::from(root_path)]
                    } else {
                        return Err(format!(
                            "scan target '{}' is neither an existing folder nor a configured root name",
                            target.display()
                        ));
                    }
                } else {
                    return Err(format!(
                        "scan target '{}' does not exist or is not a folder",
                        target.display()
                    ));
                }
            } else {
                ensure_root_for_folder(&mut manifest, target);
                vec![target.clone()]
            }
        }
        None => {
            if manifest.roots.is_empty() {
                return Err(
                    "scan requires a folder path or at least one configured root in [filters.roots] / library.yaml"
                        .to_string(),
                );
            }
            manifest.roots.values().map(PathBuf::from).collect()
        }
    };

    let mut all_files: Vec<PathBuf> = Vec::new();
    for dir in &target_dirs {
        let mut discovered = discover_flac_files(dir)?;
        all_files.append(&mut discovered);
    }
    let total_files = all_files.len();
    if total_files == 0 {
        let _ = filter_db.save_manifest(&manifest);
        return Ok(ScanSummary::default());
    }

    // Pre-index all existing Schema-2 albums for fast skip, retag, and move detection.
    let existing_list = filter_db.load_all_albums();
    let mut existing_albums: HashMap<(String, String), AlbumAnalysisFile> = HashMap::new();
    let mut by_md5: HashMap<String, Vec<OldTrackSnapshot>> = HashMap::new();
    let mut by_fallback: HashMap<(u64, i64, String), Vec<OldTrackSnapshot>> = HashMap::new();

    for (_, album) in existing_list {
        for (fname, track) in &album.tracks {
            let snap = OldTrackSnapshot {
                root: album.root.clone(),
                rel_folder: album.folder.clone(),
                file_name: fname.clone(),
                track: track.clone(),
            };
            if let Some(ref md5) = track.md5 {
                by_md5.entry(md5.clone()).or_default().push(snap.clone());
            }
            if let Some(ref hh) = track.head_hash {
                let key = (track.size, track.seconds.round() as i64, hh.clone());
                by_fallback.entry(key).or_default().push(snap);
            }
        }
        existing_albums.insert(canonical_album_key(&album.root, &album.folder), album);
    }

    // Group discovered FLAC files by (root, rel_folder) -> one work unit per album folder!
    let mut grouped: BTreeMap<(String, String), Vec<PathBuf>> = BTreeMap::new();
    let mut present_keys: HashSet<(String, String, String)> = HashSet::with_capacity(total_files);

    for file in all_files {
        let (root, rel_folder, file_name) = match manifest.resolve_track_path(&file) {
            Some(loc) => loc,
            None => {
                let parent = file.parent().unwrap_or_else(|| Path::new("."));
                let r = ensure_root_for_folder(&mut manifest, parent);
                let fname = file
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (r, String::new(), fname)
            }
        };
        present_keys.insert((
            root.to_ascii_lowercase(),
            rel_folder.to_ascii_lowercase(),
            file_name.to_ascii_lowercase(),
        ));
        grouped.entry((root, rel_folder)).or_default().push(file);
    }

    let move_index = MoveIndex {
        by_md5,
        by_fallback,
        present_keys,
    };

    let work_units: Vec<AlbumWorkUnit> = grouped
        .into_iter()
        .map(|((root, rel_folder), mut files)| {
            files.sort();
            AlbumWorkUnit {
                root,
                rel_folder,
                files,
            }
        })
        .collect();

    let default_workers = std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 4))
        .unwrap_or(2);
    let workers = opts
        .jobs
        .unwrap_or(default_workers)
        .clamp(1, 32)
        .min(work_units.len().max(1));

    let next_unit = AtomicUsize::new(0);
    let unit_outcomes: Mutex<Vec<(usize, AlbumWorkOutcome)>> =
        Mutex::new(Vec::with_capacity(work_units.len()));

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if controller.is_some_and(|c| c.current_signal().is_some()) {
                    break;
                }
                let idx = next_unit.fetch_add(1, Ordering::Relaxed);
                if idx >= work_units.len() {
                    break;
                }
                let unit = &work_units[idx];
                let existing_album =
                    existing_albums.get(&canonical_album_key(&unit.root, &unit.rel_folder));
                let outcome = scan_album_folder(
                    unit,
                    existing_album,
                    &move_index,
                    &filter_db,
                    user_filters.as_ref(),
                    &opts.upsampler_policy,
                    &rew_factory,
                    genre_rules,
                    opts.post_chain,
                    opts.force,
                    controller,
                );
                if let Ok(mut guard) = unit_outcomes.lock() {
                    guard.push((idx, outcome));
                }
            });
        }
    });

    let mut collected = unit_outcomes
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    collected.sort_by_key(|(idx, _)| *idx);

    let mut summary = ScanSummary {
        total_files,
        ..ScanSummary::default()
    };
    let mut all_moves: Vec<RelinkEntry> = Vec::new();

    for (_, outcome) in collected {
        summary.scanned += outcome.scanned;
        summary.skipped_current += outcome.skipped_current;
        summary.retagged_fast += outcome.retagged_fast;
        summary.moved_reused += outcome.moved_reused;
        summary.failed += outcome.failed;
        summary.suggested_changed += outcome.suggested_changed;
        all_moves.extend(outcome.detected_moves);
        for (is_err, line) in outcome.report_lines {
            if is_err {
                eprintln!("{line}");
            } else if opts.report {
                println!("{line}");
            }
            summary.report_lines.push(line);
        }
    }

    // Prune tracks that vanished from previously scanned albums that are now empty of those tracks
    // inside our scanned target dirs (e.g. when a whole track moved out of AlbumA into AlbumB).
    let touched_albums: HashSet<(String, String)> = work_units
        .iter()
        .map(|u| canonical_album_key(&u.root, &u.rel_folder))
        .collect();
    for (album_key, old_album) in &existing_albums {
        if touched_albums.contains(album_key) {
            continue;
        }
        // Only prune albums under one of the target directories we scanned.
        let album_key_str = format_root_rel_key(&old_album.root, &old_album.folder, None);
        let album_abs = manifest.to_abs_path(&album_key_str);
        let under_target = album_abs.as_ref().is_some_and(|abs| {
            target_dirs
                .iter()
                .any(|t| path_is_prefix_of(t, abs))
        });
        if !under_target {
            continue;
        }
        let mut pruned = old_album.clone();
        let before = pruned.tracks.len();
        pruned.tracks.retain(|fname, _| {
            move_index.present_keys.contains(&(
                old_album.root.to_ascii_lowercase(),
                old_album.folder.to_ascii_lowercase(),
                fname.to_ascii_lowercase(),
            ))
        });
        if pruned.tracks.len() != before {
            let _ = filter_db.save_album_nonblocking(&pruned);
        }
    }

    // Filter detected moves to those actually referenced in `my-filters.yaml` (if present),
    // and write `filterdb/relink-report.yaml` so `filters relink` can preview & apply them.
    if !all_moves.is_empty() {
        let relevant_moves: Vec<RelinkEntry> = if let Some(ref uf) = user_filters {
            all_moves
                .into_iter()
                .filter(|m| is_key_referenced_in_user_filters(uf, &m.old_key))
                .collect()
        } else {
            Vec::new()
        };
        if !relevant_moves.is_empty() {
            summary.relink_candidates = relevant_moves.len();
            let mut report = filter_db.load_relink_report().unwrap_or_else(|| RelinkReport {
                schema: ap_core::filter_db::FILTER_DB_SCHEMA_VERSION,
                generated_unix_s: now_unix_secs(),
                moves: Vec::new(),
            });
            report.generated_unix_s = now_unix_secs();
            for mv in relevant_moves {
                if !report
                    .moves
                    .iter()
                    .any(|e| e.old_key.eq_ignore_ascii_case(&mv.old_key))
                {
                    report.moves.push(mv);
                }
            }
            let _ = filter_db.save_relink_report(&report);
        }
    }

    // Refresh manifest counters from all albums on disk.
    let all_albums_after = filter_db.load_all_albums();
    manifest.last_scan_started_unix_s = scan_started_unix_s;
    manifest.last_scan_finished_unix_s = now_unix_secs();
    manifest.detector_version = DETECTOR_VERSION;
    manifest.last_scan_folders = all_albums_after.len() as u64;
    manifest.last_scan_files = all_albums_after
        .iter()
        .map(|(_, a)| a.tracks.len() as u64)
        .sum();
    manifest.suggested_changed_since_previous = summary.suggested_changed as u64;
    let _ = filter_db.save_manifest(&manifest);

    Ok(summary)
}

fn path_is_prefix_of(prefix: &Path, candidate: &Path) -> bool {
    let p_norm = ap_core::filter_db::clean_root_path_string(prefix);
    let c_norm = ap_core::filter_db::clean_root_path_string(candidate);
    #[cfg(windows)]
    {
        let p_lc = p_norm.to_ascii_lowercase();
        let c_lc = c_norm.to_ascii_lowercase();
        c_lc == p_lc || c_lc.starts_with(&format!("{p_lc}\\"))
    }
    #[cfg(not(windows))]
    {
        c_norm == p_norm || c_norm.starts_with(&format!("{p_norm}/"))
    }
}

fn is_key_referenced_in_user_filters(uf: &UserFiltersFile, root_rel_track_key: &str) -> bool {
    #[cfg(windows)]
    let needle = root_rel_track_key.to_lowercase();
    #[cfg(not(windows))]
    let needle = root_rel_track_key.to_string();
    uf.tracks.contains_key(&needle)
}

#[allow(clippy::too_many_arguments)]
fn scan_album_folder(
    unit: &AlbumWorkUnit,
    existing_album: Option<&AlbumAnalysisFile>,
    move_index: &MoveIndex,
    filter_db: &FilterDbStore,
    user_filters: Option<&UserFiltersFile>,
    policy: &UpsamplerPolicy,
    rew_factory: &RewConvolverFactory,
    genre_rules: bool,
    measure_post_chain: bool,
    force: bool,
    controller: Option<&ShutdownController>,
) -> AlbumWorkOutcome {
    let mut out = AlbumWorkOutcome::default();

    let lock = match filter_db.try_lock_album(&unit.root, &unit.rel_folder) {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            out.report_lines.push((
                true,
                format!(
                    "SKIP  {}:{} (folder locked by another writer)",
                    unit.root, unit.rel_folder
                ),
            ));
            return out;
        }
        Err(err) => {
            out.report_lines.push((
                true,
                format!(
                    "FAIL  {}:{}: lock error: {err}",
                    unit.root, unit.rel_folder
                ),
            ));
            out.failed += unit.files.len();
            return out;
        }
    };

    // Review #9 U5 + U8: reload the album YAML INSIDE the folder lock so concurrent learn-on-play
    // writes are never clobbered by a pre-lock snapshot, and never overwrite a corrupt existing
    // album file with partial data.
    let fresh_existing = match filter_db.try_load_album(&unit.root, &unit.rel_folder) {
        Ok(loaded) => loaded,
        Err(err) => {
            out.report_lines.push((
                true,
                format!(
                    "FAIL  {}:{}: refusing to overwrite unreadable album YAML: {err}",
                    unit.root, unit.rel_folder
                ),
            ));
            out.failed += unit.files.len();
            return out;
        }
    };
    let effective_existing = fresh_existing.as_ref().or(existing_album);
    let mut album = effective_existing.cloned().unwrap_or_else(|| {
        AlbumAnalysisFile::new(unit.root.clone(), unit.rel_folder.clone(), DETECTOR_VERSION)
    });
    let mut album_dirty = effective_existing.is_none();
    let mut present_in_album: HashSet<String> = HashSet::with_capacity(unit.files.len());

    for file_path in &unit.files {
        if controller.is_some_and(|c| c.current_signal().is_some()) {
            break;
        }
        if let Some(ref lk) = lock {
            lk.refresh();
        }
        let file_name = file_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| file_path.to_string_lossy().into_owned());
        present_in_album.insert(file_name.clone());

        let old_track = effective_existing.and_then(|a| a.get_track(&file_name)).cloned();
        let res = catch_unwind(AssertUnwindSafe(|| {
            scan_single_track_schema2(
                file_path,
                &unit.root,
                &unit.rel_folder,
                &file_name,
                old_track.as_ref(),
                move_index,
                user_filters,
                policy,
                rew_factory,
                genre_rules,
                measure_post_chain,
                force,
            )
        }));

        match res {
            Ok(SingleTrackResult::Skipped(line)) => {
                out.skipped_current += 1;
                out.report_lines.push((false, line));
            }
            Ok(SingleTrackResult::Retagged {
                track,
                suggested_changed,
                line,
            }) => {
                album.tracks.insert(file_name, track);
                album_dirty = true;
                out.retagged_fast += 1;
                if suggested_changed {
                    out.suggested_changed += 1;
                }
                out.report_lines.push((false, line));
            }
            Ok(SingleTrackResult::Moved {
                track,
                move_entry,
                suggested_changed,
                line,
            }) => {
                album.tracks.insert(file_name, track);
                album_dirty = true;
                out.moved_reused += 1;
                if suggested_changed {
                    out.suggested_changed += 1;
                }
                out.detected_moves.push(move_entry);
                out.report_lines.push((false, line));
            }
            Ok(SingleTrackResult::Scanned {
                track,
                suggested_changed,
                move_entry,
                line,
            }) => {
                album.tracks.insert(file_name, track);
                album_dirty = true;
                out.scanned += 1;
                if suggested_changed {
                    out.suggested_changed += 1;
                }
                if let Some(mv) = move_entry {
                    out.detected_moves.push(mv);
                }
                out.report_lines.push((false, line));
            }
            Ok(SingleTrackResult::Failed(line)) => {
                out.failed += 1;
                out.report_lines.push((true, line));
            }
            Err(payload) => {
                let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic payload".to_string()
                };
                out.failed += 1;
                out.report_lines
                    .push((true, format!("FAIL  {file_name}: panic: {msg}")));
            }
        }
    }

    // Prune tracks that disappeared from this album folder (only when scan was not interrupted).
    if !controller.is_some_and(|c| c.current_signal().is_some()) {
        let before = album.tracks.len();
        album.tracks.retain(|k, _| present_in_album.contains(k));
        if album.tracks.len() != before {
            album_dirty = true;
        }
    }

    if album_dirty {
        album.recompute_header_detector_version(DETECTOR_VERSION);
        album.scanned_unix_s = now_unix_secs();
        if let Some(ref lk) = lock {
            if let Err(err) = filter_db.save_album_under_lock(&album, lk) {
                out.report_lines.push((
                    true,
                    format!(
                        "FAIL  {}:{}: failed saving album YAML: {err}",
                        unit.root, unit.rel_folder
                    ),
                ));
            }
        }
    }
    drop(lock);
    out
}

enum SingleTrackResult {
    Skipped(String),
    Retagged {
        track: AlbumTrackAnalysis,
        suggested_changed: bool,
        line: String,
    },
    Moved {
        track: AlbumTrackAnalysis,
        move_entry: RelinkEntry,
        suggested_changed: bool,
        line: String,
    },
    Scanned {
        track: AlbumTrackAnalysis,
        suggested_changed: bool,
        move_entry: Option<RelinkEntry>,
        line: String,
    },
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
fn scan_single_track_schema2(
    path: &Path,
    root: &str,
    rel_folder: &str,
    file_name: &str,
    old_track: Option<&AlbumTrackAnalysis>,
    move_index: &MoveIndex,
    user_filters: Option<&UserFiltersFile>,
    policy: &UpsamplerPolicy,
    rew_factory: &RewConvolverFactory,
    genre_rules: bool,
    measure_post_chain: bool,
    force: bool,
) -> SingleTrackResult {
    #[cfg(test)]
    if file_name.contains("FORCE_PANIC_TEST") {
        panic!("injected scanner test panic");
    }

    let mut reader = match FlacBlockReader::open(path) {
        Ok(r) => r,
        Err(err) => return SingleTrackResult::Failed(format!("FAIL  {file_name}: {err}")),
    };

    let spec = reader.spec();
    let raw_peak_meta = reader.peak_meta();
    let track_meta = reader.track_meta();
    let header_md5 = reader.streaminfo_md5();
    let header_md5_hex = header_md5.as_ref().map(md5_to_hex);
    let (file_size, file_mtime) = file_size_and_mtime(path).unwrap_or((0, 0));
    let header_duration_s = reader
        .total_frames()
        .filter(|&f| f > 0)
        .map(|f| f as f64 / f64::from(spec.sample_rate_hz.max(1)));

    // --- Fast Path 1: Unchanged (size + mtime ±2 s + per-track current detector version, U1) ---
    if !force && !measure_post_chain {
        if let Some(prev) = old_track.filter(|p| p.detector_version == DETECTOR_VERSION) {
            let md5_matches = match (prev.md5.as_deref(), header_md5_hex.as_deref()) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                // Zero-MD5 FLACs store computed MD5 in `prev.md5` while header MD5 is None:
                // accept on matching `(size, mtime ±2 s)`.
                (_, None) => true,
                (None, Some(_)) => false,
            };
            if md5_matches && prev.matches_size_mtime(file_size, file_mtime) {
                return SingleTrackResult::Skipped(format!(
                    "SKIP  {file_name} (current v{DETECTOR_VERSION})"
                ));
            }

            // --- Fast Path 2: Metadata-only edit (MD5 unchanged, size/mtime changed) ---
            if let (Some(a), Some(b)) = (prev.md5.as_deref(), header_md5_hex.as_deref()) {
                if a.eq_ignore_ascii_case(b) {
                    let mut updated = prev.clone();
                    updated.size = file_size;
                    updated.mtime_unix_s = file_mtime;
                    updated.head_hash = file_head_4k_hash(path);
                    let base_wt = CachedWholeTrackEvidence::from_album_track(&updated, true);
                    if let Ok(sel) = resolve_machine_suggestion(
                        spec,
                        false,
                        Some(updated.seconds),
                        &raw_peak_meta,
                        &track_meta,
                        None,
                        &base_wt,
                        policy,
                        genre_rules,
                        true,
                    ) {
                        let new_sug = sel.plan.filter.to_string();
                        let changed = new_sug != prev.suggested;
                        updated.suggested = new_sug;
                        updated.rule = sel.winning_rule;
                        updated.why = ap_core::filter_db::why_for_rule(&updated.rule).to_string();
                        updated.deemphasis = sel.plan.deemphasis;
                        return SingleTrackResult::Retagged {
                            line: format!(
                                "TAG   {file_name} (metadata updated, audio MD5 unchanged -> {} [{}])",
                                updated.suggested, updated.rule
                            ),
                            suggested_changed: changed,
                            track: updated,
                        };
                    }
                }
            }
        }
    }

    // --- Fast Path 3: Moved File Detection (vanished old entry with identical MD5 or fallback) ---
    let mut detected_move_entry: Option<RelinkEntry> = None;
    if old_track.is_none() {
        let head_hash_opt = file_head_4k_hash(path);
        let moved_snap = header_md5_hex
            .as_deref()
            .and_then(|m| move_index.find_unique_vanished_by_md5(m))
            .or_else(|| {
                let dur = header_duration_s?.round() as i64;
                let hh = head_hash_opt.as_deref()?;
                move_index.find_unique_vanished_by_fallback(file_size, dur, hh)
            });

        if let Some(snap) = moved_snap {
            let new_key = format_root_rel_key(root, rel_folder, Some(file_name));
            let mv = RelinkEntry {
                old_key: snap.root_rel_key(),
                new_key,
                md5: header_md5_hex.clone().or_else(|| snap.track.md5.clone()),
            };
            if !force
                && !measure_post_chain
                && snap.track.detector_version == DETECTOR_VERSION
            {
                let mut updated = snap.track.clone();
                updated.size = file_size;
                updated.mtime_unix_s = file_mtime;
                updated.head_hash = head_hash_opt;
                let base_wt = CachedWholeTrackEvidence::from_album_track(&updated, true);
                let mut changed = false;
                if let Ok(sel) = resolve_machine_suggestion(
                    spec,
                    false,
                    Some(updated.seconds),
                    &raw_peak_meta,
                    &track_meta,
                    None,
                    &base_wt,
                    policy,
                    genre_rules,
                    true,
                ) {
                    let new_sug = sel.plan.filter.to_string();
                    changed = new_sug != snap.track.suggested;
                    updated.suggested = new_sug;
                    updated.rule = sel.winning_rule;
                    updated.why = ap_core::filter_db::why_for_rule(&updated.rule).to_string();
                    updated.deemphasis = sel.plan.deemphasis;
                }
                return SingleTrackResult::Moved {
                    line: format!(
                        "MOVE  {file_name} (from {} -> {} [{}])",
                        mv.old_key, updated.suggested, updated.rule
                    ),
                    track: updated,
                    move_entry: mv,
                    suggested_changed: changed,
                };
            } else {
                detected_move_entry = Some(mv);
            }
        }
    }

    // --- Full Scan Pass 1: Stream entire track once through TrackAnalyzer + MD5 verifier ---
    let mut analyzer = TrackAnalyzer::new(spec.sample_rate_hz, spec.bits_per_sample);
    loop {
        match reader.read_next_block(SCAN_BLOCK_FRAMES) {
            Ok(Some(block)) => analyzer.push_block(&block),
            Ok(None) => break,
            Err(err) => {
                return SingleTrackResult::Failed(format!(
                    "FAIL  {file_name}: decode error: {err}"
                ));
            }
        }
    }

    // Verify RFC 9639 STREAMINFO.md5 before persisting whole-track results.
    let computed_md5 = reader.computed_md5();
    if let (Some(expected), Some(actual)) = (header_md5, computed_md5) {
        if expected != actual {
            return SingleTrackResult::Failed(format!(
                "FAIL  {file_name}: MD5 mismatch (header={} computed={}); refusing to cache",
                md5_to_hex(&expected),
                md5_to_hex(&actual)
            ));
        }
    }
    let final_md5_hex = header_md5_hex.or_else(|| computed_md5.as_ref().map(md5_to_hex));

    let early_digital = track_meta.is_early_digital_era();
    let orig_below = track_meta
        .original_sample_rate_hz
        .is_some_and(|r| r < spec.sample_rate_hz);
    let analysis = analyzer.snapshot(true, early_digital, orig_below, track_meta.is_dsd_hint);

    // 1. Always compute pure machine-suggested filter & rule via shared helper (U10).
    let base_wt = CachedWholeTrackEvidence::from_whole_track_analysis(&analysis, None);
    let sug_selection = match resolve_machine_suggestion(
        spec,
        false,
        Some(analysis.duration_s),
        &raw_peak_meta,
        &track_meta,
        Some(&analysis),
        &base_wt,
        policy,
        genre_rules,
        false,
    ) {
        Ok(sel) => sel,
        Err(err) => {
            return SingleTrackResult::Failed(format!(
                "FAIL  {file_name}: ultimate selection error: {err}"
            ));
        }
    };
    let suggested_filter = sug_selection.plan.filter.to_string();
    let suggested_rule = sug_selection.winning_rule.clone();
    let suggested_changed = old_track.is_some_and(|p| p.suggested != suggested_filter);

    // 2. Also compute active user chain (`my-filters.yaml` + `AP_*` tags, U10/U12) so
    //    `--post-chain` measures the active user chain peak as well!
    let active_selection = {
        let default_uf = UserFiltersFile::default();
        let uf_ref = user_filters.unwrap_or(&default_uf);
        let ov = uf_ref.resolve_for_track(root, rel_folder, file_name, &track_meta);
        resolve_active_selection(
            spec,
            false,
            Some(analysis.duration_s),
            &raw_peak_meta,
            &track_meta,
            Some(&analysis),
            &base_wt,
            Some(&ov),
            FilterMode::Mine,
            policy,
            genre_rules,
            false,
        )
        .unwrap_or_else(|_| sug_selection.clone())
    };

    let s1_entry = build_track_db_entry(
        path.to_string_lossy().into_owned(),
        file_size,
        file_mtime,
        final_md5_hex.clone(),
        &analysis,
        active_selection.plan.deemphasis,
        active_selection.plan.filter.to_string(),
        active_selection.winning_rule.clone(),
    );

    // Build Schema-2 track entry (always stores pure machine `suggested` / `rule` / `why`!).
    let head_hash = file_head_4k_hash(path);
    let mut s2_track = AlbumTrackAnalysis::from_schema1(&s1_entry, head_hash);
    s2_track.suggested = suggested_filter.clone();
    s2_track.rule = suggested_rule.clone();
    s2_track.why = ap_core::filter_db::why_for_rule(&suggested_rule).to_string();
    s2_track.deemphasis = sug_selection.plan.deemphasis;

    // Preserve any existing `post_chain_peaks` when audio MD5 is unchanged.
    if let Some(prev) = old_track {
        if prev.md5.is_some() && prev.md5 == s2_track.md5 {
            s2_track.post_chain_peaks = prev.post_chain_peaks.clone();
        }
    }

    // Pass 2 (`--post-chain`): measure unity-gain whole-track peak M through the exact chain!
    let mut peak_note = String::new();
    if measure_post_chain {
        let mut plans_to_measure: Vec<&SelectedDspPlan> = vec![&active_selection.plan];
        if sug_selection.plan.filter != active_selection.plan.filter
            || sug_selection.plan.deemphasis != active_selection.plan.deemphasis
        {
            plans_to_measure.push(&sug_selection.plan);
        }
        for (idx, plan_ref) in plans_to_measure.into_iter().enumerate() {
            match run_post_chain_pass(path, spec, plan_ref, rew_factory) {
                Ok(Some((key, peak_dbfs))) => {
                    if idx == 0 {
                        peak_note = format!(" M={peak_dbfs:+.2}dBFS");
                    }
                    s2_track.upsert_post_chain_peak(key, peak_dbfs, false);
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("WARN  {file_name}: post-chain failed ({err}); skipping M");
                    if idx == 0 {
                        peak_note = format!(" WARN(post-chain: {err})");
                    }
                }
            }
        }
    }

    let lufs_str = analysis
        .integrated_lufs
        .map(|l| format!("{l:+.1}LUFS"))
        .unwrap_or_else(|| "n/a".to_string());
    let line = format!(
        "SCAN  {file_name} ({}/{}b {:.1}s) TP={:+.2}dBTP {lufs_str} B={}b H={} A={} L={} HF={} -> {suggested_filter} [{suggested_rule}]{peak_note}",
        spec.sample_rate_hz,
        spec.bits_per_sample,
        analysis.duration_s,
        analysis.true_peak_dbtp,
        analysis.effective_bits,
        analysis.upsampled_from_1x.as_str(),
        analysis.apodizing_needed.as_str(),
        analysis.loud_and_clipped.as_str(),
        analysis.ultrasonic_noise_detected.as_str(),
    );

    SingleTrackResult::Scanned {
        track: s2_track,
        suggested_changed,
        move_entry: detected_move_entry,
        line,
    }
}

fn run_post_chain_pass(
    path: &Path,
    spec: ap_core::format::StreamSpec,
    plan: &SelectedDspPlan,
    rew_factory: &RewConvolverFactory,
) -> Result<Option<(PostChainKey, f64)>, String> {
    let mut runner = prepare_post_chain_runner(spec, plan, rew_factory)?;
    let mut reader2 = FlacBlockReader::open(path)?;
    loop {
        match reader2.read_next_block(SCAN_BLOCK_FRAMES)? {
            Some(block) => runner.process_block(&block)?,
            None => break,
        }
    }
    let (key, peak_dbfs) = runner.finish()?;
    if peak_dbfs.is_finite() {
        Ok(Some((key, peak_dbfs)))
    } else {
        Ok(None)
    }
}

struct PostChainRunner {
    key: PostChainKey,
    upsampler: Box<dyn ap_dsp::upsampler::Upsampler>,
    deemphasis: Option<StereoDeemphasis>,
    convolver: Box<dyn ap_dsp::convolution::StereoConvolver>,
    peak_linear: f64,
}

impl PostChainRunner {
    fn process_block(&mut self, input: &ap_core::buffer::StereoF64Buffer) -> Result<(), String> {
        let mut buf = self.upsampler.process_block(input.clone())?;
        if let Some(ref mut deemph) = self.deemphasis {
            buf = deemph.process_block(buf)?;
        }
        buf = self.convolver.process_block(buf)?;
        self.observe(&buf);
        Ok(())
    }

    fn finish(mut self) -> Result<(PostChainKey, f64), String> {
        let mut tail = self.upsampler.flush()?;
        if !tail.left.is_empty() {
            if let Some(ref mut deemph) = self.deemphasis {
                tail = deemph.process_block(tail)?;
            }
            let c = self.convolver.process_block(tail)?;
            self.observe(&c);
        }
        let c_tail = self.convolver.flush()?;
        self.observe(&c_tail);

        let peak_dbfs = if self.peak_linear > 0.0 {
            20.0 * self.peak_linear.log10()
        } else {
            -120.0
        };
        Ok((self.key, peak_dbfs))
    }

    fn observe(&mut self, buf: &ap_core::buffer::StereoF64Buffer) {
        for &s in buf.left.iter().chain(buf.right.iter()) {
            let a = s.abs();
            if a > self.peak_linear {
                self.peak_linear = a;
            }
        }
    }
}

fn prepare_post_chain_runner(
    spec: ap_core::format::StreamSpec,
    plan: &SelectedDspPlan,
    rew_factory: &RewConvolverFactory,
) -> Result<PostChainRunner, String> {
    let upsampler = build_upsampler(plan)?;
    let deemphasis = if plan.deemphasis {
        Some(StereoDeemphasis::new_50_15us(plan.target_rate_hz)?)
    } else {
        None
    };
    let convolver = rew_factory.create_for_rate(plan.target_rate_hz)?;
    let key = build_post_chain_key(
        plan,
        spec.sample_rate_hz,
        plan.target_rate_hz,
        convolver.content_hash(),
    );

    Ok(PostChainRunner {
        key,
        upsampler,
        deemphasis,
        convolver,
        peak_linear: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use flac_codec::encode::{FlacSampleWriter, Options};

    fn make_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ap_scan_test_{label}_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_test_flac(path: &Path, rate: u32, frames: usize, amp: f64) {
        let mut interleaved = Vec::with_capacity(frames * 2);
        let scale = 32767.0 * amp;
        for n in 0..frames {
            let v = ((2.0 * PI * 1000.0 * n as f64 / rate as f64).sin() * scale).round() as i32;
            interleaved.push(v);
            interleaved.push(v);
        }
        let mut writer = FlacSampleWriter::create(
            path,
            Options::default(),
            rate,
            16,
            2,
            Some(interleaved.len() as u64),
        )
        .unwrap();
        writer.write(&interleaved).unwrap();
        writer.finalize().unwrap();
    }

    #[test]
    fn offline_scan_writes_yaml_preserves_user_overrides_is_resumable_and_never_touches_flac() {
        let root = make_temp_dir("basic");
        let music_dir = root.join("music");
        let db_dir = root.join("trackdb");
        let filterdb_dir = root.join("filterdb");
        let choices_path = root.join("my-filters.yaml");
        fs::create_dir_all(&music_dir).unwrap();

        let flac_path = music_dir.join("01_sine.flac");
        write_test_flac(&flac_path, 44_100, 44_100, 0.5);
        let flac_bytes_before = fs::read(&flac_path).unwrap();

        let opts = ScanOptions {
            folder: Some(music_dir.clone()),
            report: false,
            post_chain: true,
            force: false,
            jobs: None,
            trackdb_dir: Some(db_dir.clone()),
            filterdb_dir: Some(filterdb_dir.clone()),
            choices_file: Some(choices_path.clone()),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy::default(),
        };
        let s1 = scan_folder(&opts, None).expect("scan 1 failed");
        assert_eq!(s1.total_files, 1);
        assert_eq!(s1.scanned, 1);
        assert_eq!(s1.failed, 0);
        assert_eq!(fs::read(&flac_path).unwrap(), flac_bytes_before);

        let store = FilterDbStore::new(&filterdb_dir);
        let alb = store.load_album("main", "").expect("Schema-2 album YAML must exist");
        let t1 = &alb.tracks["01_sine.flac"];
        assert_eq!(t1.post_chain_peaks.len(), 1);
        let measured_m = t1.post_chain_peaks[0].peak_dbfs;
        assert!((measured_m - (-6.02)).abs() < 0.2);

        // Write user choice in `my-filters.yaml` — scanner must NEVER modify it!
        let user_yaml = "schema: 2\n\ntracks:\n  \"main:01_sine.flac\":\n    filter: short-lp\n    note: \"keep my custom filter\"\n";
        fs::write(&choices_path, user_yaml).unwrap();

        let mut opts_rescan = opts.clone();
        opts_rescan.post_chain = false;
        opts_rescan.force = false;
        let s2 = scan_folder(&opts_rescan, None).expect("resumable skip scan failed");
        assert_eq!(s2.skipped_current, 1);
        assert_eq!(s2.scanned, 0);

        opts_rescan.force = true;
        let s3 = scan_folder(&opts_rescan, None).expect("forced rescan failed");
        assert_eq!(s3.scanned, 1);
        assert_eq!(fs::read_to_string(&choices_path).unwrap(), user_yaml);

        let alb2 = store.load_album("main", "").unwrap();
        assert_eq!(alb2.tracks["01_sine.flac"].post_chain_peaks.len(), 1);
        assert_eq!(
            alb2.tracks["01_sine.flac"].suggested,
            alb2.tracks["01_sine.flac"].post_chain_peaks[0].key.upsampler
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn post_chain_key_matches_ultimate_key_and_respects_user_deemphasis_override() {
        let root = make_temp_dir("f6_keys");
        let music_dir = root.join("music");
        let db_dir = root.join("trackdb");
        let filterdb_dir = root.join("filterdb");
        let choices_path = root.join("my-filters.yaml");
        fs::create_dir_all(&music_dir).unwrap();

        let flac_path = music_dir.join("01_override.flac");
        write_test_flac(&flac_path, 44_100, 44_100, 0.4);

        // User overrides filter & deemphasis in `my-filters.yaml` (Schema-2).
        fs::write(
            &choices_path,
            "schema: 2\n\ntracks:\n  \"main:01_override.flac\":\n    filter: short-lp\n    deemphasis: true\n",
        )
        .unwrap();

        let opts = ScanOptions {
            folder: Some(music_dir.clone()),
            report: false,
            post_chain: true,
            force: true,
            jobs: None,
            trackdb_dir: Some(db_dir.clone()),
            filterdb_dir: Some(filterdb_dir.clone()),
            choices_file: Some(choices_path),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy::default(),
        };
        let s = scan_folder(&opts, None).unwrap();
        assert_eq!(s.scanned, 1);
        assert_eq!(s.failed, 0);

        let store = FilterDbStore::new(&filterdb_dir);
        let alb = store.load_album("main", "").unwrap();
        let tr = &alb.tracks["01_override.flac"];
        assert!(
            tr.post_chain_peaks
                .iter()
                .any(|p| p.key.upsampler == "short-lp" && p.key.deemphasis),
            "expected post-chain peak for user override short-lp + deemphasis=true, got {:?}",
            tr.post_chain_peaks
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_folder_continues_after_track_panic() {
        let root = make_temp_dir("fc1_panic");
        let music_dir = root.join("music");
        let db_dir = root.join("trackdb");
        let filterdb_dir = root.join("filterdb");
        fs::create_dir_all(&music_dir).unwrap();

        let bad_path = music_dir.join("01_FORCE_PANIC_TEST.flac");
        let good_path = music_dir.join("02_good.flac");
        write_test_flac(&bad_path, 44_100, 44_100, 0.3);
        write_test_flac(&good_path, 44_100, 44_100, 0.3);

        let opts = ScanOptions {
            folder: Some(music_dir.clone()),
            report: false,
            post_chain: false,
            force: true,
            jobs: Some(1),
            trackdb_dir: Some(db_dir.clone()),
            filterdb_dir: Some(filterdb_dir.clone()),
            choices_file: Some(root.join("my-filters.yaml")),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy::default(),
        };
        let summary = scan_folder(&opts, None).expect("scan_folder must catch per-track panic");
        assert_eq!(summary.total_files, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.scanned, 1);
        assert!(summary.report_lines.iter().any(|l| l.contains("panic:")));
        let store = FilterDbStore::new(&filterdb_dir);
        let alb = store.load_album("main", "").unwrap();
        assert!(alb.tracks.contains_key("02_good.flac"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_zero_md5_flac_then_second_scan_skips_f4() {
        let root = make_temp_dir("f4_zero_md5");
        let music_dir = root.join("music");
        let db_dir = root.join("trackdb");
        let filterdb_dir = root.join("filterdb");
        fs::create_dir_all(&music_dir).unwrap();

        let flac_path = music_dir.join("01_zero_md5.flac");
        write_test_flac(&flac_path, 44_100, 44_100, 0.4);

        let mut raw = fs::read(&flac_path).unwrap();
        assert_eq!(&raw[0..4], b"fLaC");
        for b in &mut raw[26..42] {
            *b = 0;
        }
        fs::write(&flac_path, &raw).unwrap();

        let opts = ScanOptions {
            folder: Some(music_dir.clone()),
            report: false,
            post_chain: false,
            force: false,
            jobs: Some(2),
            trackdb_dir: Some(db_dir.clone()),
            filterdb_dir: Some(filterdb_dir.clone()),
            choices_file: Some(root.join("my-filters.yaml")),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy::default(),
        };
        let s1 = scan_folder(&opts, None).unwrap();
        assert_eq!(s1.scanned, 1);
        assert_eq!(s1.skipped_current, 0);

        let s2 = scan_folder(&opts, None).unwrap();
        assert_eq!(s2.skipped_current, 1);
        assert_eq!(s2.scanned, 0);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn schema2_sharded_scan_detects_moves_writes_relink_report_and_ignores_duplicates() {
        let root = make_temp_dir("schema2_move");
        let music_dir = root.join("music");
        let album_a = music_dir.join("AlbumA");
        let album_b = music_dir.join("AlbumB");
        let filterdb_dir = root.join("filterdb");
        let choices_path = root.join("my-filters.yaml");
        fs::create_dir_all(&album_a).unwrap();
        fs::create_dir_all(&album_b).unwrap();

        let track_a1 = album_a.join("01_alpha.flac");
        let track_a2 = album_a.join("02_beta.flac");
        let track_b1 = album_b.join("01_gamma.flac");
        write_test_flac(&track_a1, 44_100, 44_100, 0.35);
        write_test_flac(&track_a2, 44_100, 44_100, 0.65);
        write_test_flac(&track_b1, 48_000, 48_000, 0.45);

        fs::write(
            &choices_path,
            "schema: 2\n\ntracks:\n  \"main:AlbumA/01_alpha.flac\":\n    filter: gauss-long-lp\n",
        )
        .unwrap();

        let opts = ScanOptions {
            folder: Some(music_dir.clone()),
            report: false,
            post_chain: false,
            force: false,
            jobs: Some(2),
            trackdb_dir: Some(root.join("trackdb")),
            filterdb_dir: Some(filterdb_dir.clone()),
            choices_file: Some(choices_path.clone()),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy::default(),
        };

        // 1. Initial parallel sharded scan writes Schema-2 album YAMLs + library.yaml.
        let s1 = scan_folder(&opts, None).unwrap();
        assert_eq!(s1.total_files, 3);
        assert_eq!(s1.scanned, 3);
        let store = FilterDbStore::new(filterdb_dir.clone());
        let mf = store.load_manifest().expect("library.yaml should exist");
        assert_eq!(mf.last_scan_folders, 2);
        assert_eq!(mf.last_scan_files, 3);
        assert!(store.load_album("main", "AlbumA").is_some());
        assert!(store.load_album("main", "AlbumB").is_some());

        // 2. Second scan fast-skips all 3 tracks.
        let s2 = scan_folder(&opts, None).unwrap();
        assert_eq!(s2.skipped_current, 3);
        assert_eq!(s2.scanned, 0);

        // 3. Move 01_alpha.flac from AlbumA -> AlbumB/02_alpha_moved.flac, and COPY 02_beta.flac
        //    to AlbumB/03_beta_copy.flac (duplicate, original still present!).
        let moved_dest = album_b.join("02_alpha_moved.flac");
        fs::rename(&track_a1, &moved_dest).unwrap();
        fs::copy(&track_a2, album_b.join("03_beta_copy.flac")).unwrap();

        let s3 = scan_folder(&opts, None).unwrap();
        assert_eq!(s3.moved_reused, 1, "moved track must be reused without re-decoding");
        assert_eq!(s3.scanned, 1, "duplicate copy must be scanned as new, never treated as a move");
        assert_eq!(s3.relink_candidates, 1, "my-filters.yaml track move must be reported");

        let report = store
            .load_relink_report()
            .expect("relink-report.yaml should exist");
        assert_eq!(report.moves.len(), 1);
        assert_eq!(report.moves[0].old_key, "main:AlbumA/01_alpha.flac");
        assert_eq!(report.moves[0].new_key, "main:AlbumB/02_alpha_moved.flac");

        // Verify vanished 01_alpha.flac was pruned from AlbumA.yaml.
        let alb_a = store.load_album("main", "AlbumA").unwrap();
        assert!(!alb_a.tracks.contains_key("01_alpha.flac"));
        assert!(alb_a.tracks.contains_key("02_beta.flac"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn review9_u1_per_track_detector_version_and_u2_cli_filter_does_not_overwrite_suggested() {
        let root = std::env::temp_dir().join(format!(
            "ap_scan_u1_u2_{}_{}",
            std::process::id(),
            ap_core::track_db::now_unix_secs()
        ));
        let music_dir = root.join("Music");
        let album_dir = music_dir.join("Album1");
        let filterdb_dir = root.join("filterdb");
        fs::create_dir_all(&album_dir).unwrap();

        let t1 = album_dir.join("01_one.flac");
        let t2 = album_dir.join("02_two.flac");
        write_test_flac(&t1, 44_100, 44_100, 0.35);
        write_test_flac(&t2, 44_100, 44_100, 0.50);

        // U2: Running `scan --filter short-mp` MUST NOT store `short-mp` / `cli_override` as `suggested`!
        let opts_with_override = ScanOptions {
            folder: Some(music_dir.clone()),
            report: false,
            post_chain: false,
            force: false,
            jobs: Some(1),
            trackdb_dir: Some(root.join("trackdb")),
            filterdb_dir: Some(filterdb_dir.clone()),
            choices_file: Some(root.join("my-filters.yaml")),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy {
                filter_override: Some(ap_dsp::selector::FilterArchetype::ShortMp),
                ..UpsamplerPolicy::default()
            },
        };
        let s1 = scan_folder(&opts_with_override, None).unwrap();
        assert_eq!(s1.scanned, 2);

        let store = FilterDbStore::new(&filterdb_dir);
        let mut alb = store.load_album("main", "Album1").unwrap();
        for (fname, tr) in &alb.tracks {
            assert_ne!(
                tr.rule, "cli_override",
                "U2: track {fname} stored cli_override as machine suggested!"
            );
            assert_ne!(
                tr.suggested, "short-mp",
                "U2: track {fname} stored CLI --filter override as suggested!"
            );
            assert_eq!(tr.detector_version, DETECTOR_VERSION);
        }

        // U1: Simulate an album where track 1 has an old detector_version (e.g. DETECTOR_VERSION - 1)
        // while track 2 was already updated to DETECTOR_VERSION (e.g. learn-on-play or partial scan).
        // Even if the album header was bumped, rescanning MUST re-scan track 1 and skip track 2!
        alb.detector_version = DETECTOR_VERSION;
        alb.tracks.get_mut("01_one.flac").unwrap().detector_version = DETECTOR_VERSION - 1;
        alb.tracks.get_mut("02_two.flac").unwrap().detector_version = DETECTOR_VERSION;
        assert!(store.save_album_nonblocking(&alb).unwrap());

        let s2 = scan_folder(&opts_with_override, None).unwrap();
        assert_eq!(s2.scanned, 1, "U1: stale-detector track 1 must be re-scanned");
        assert_eq!(s2.skipped_current, 1, "U1: current-detector track 2 must be skipped");
        let alb2 = store.load_album("main", "Album1").unwrap();
        assert_eq!(alb2.tracks["01_one.flac"].detector_version, DETECTOR_VERSION);
        assert_eq!(alb2.tracks["02_two.flac"].detector_version, DETECTOR_VERSION);

        let _ = fs::remove_dir_all(&root);
    }
}
