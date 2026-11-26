// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! `audiophile-player filters <show|diff|set|check|relink|migrate>` subcommands
//! (`FILTER_DATABASE_RESEARCH.md` §§5–7).
//!
//! Safety guarantees:
//! - Strictly read-only on music library FLAC files.
//! - Every modification to `my-filters.yaml` preserves hand-written comments/order via line-level
//!   patching and always writes a `.bak` backup before atomic rename.
//! - `filters migrate` is 100% non-destructive (leaves legacy Schema-1 `trackdb/` files untouched).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ap_core::config::PlayerConfig;
use ap_core::filter_db::{
    AlbumAnalysisFile, AlbumTrackAnalysis, FilterDbStore, LibraryManifest, file_head_4k_hash,
    split_root_key, why_for_rule,
};
use ap_core::metadata::TrackMetadata;
use ap_core::track_db::{TrackDbEntry, TrackDbStore, now_unix_secs};
use ap_core::user_filters::{
    UserFiltersFile, apply_relink_preserving_comments, format_root_rel_key, is_known_filter_name,
    upsert_entry_preserving_comments, write_user_filters_with_backup,
};
use ap_dsp::analysis::DETECTOR_VERSION;
use ap_source::flac::{AudioBlockSource, FlacBlockReader};
use ap_source::library::{default_user_music_dir, discover_flac_files};

use crate::cli_args::FiltersCommand;

struct FiltersEnv {
    filter_db: FilterDbStore,
    choices_path: PathBuf,
    manifest: LibraryManifest,
    legacy_trackdb_dir: PathBuf,
}

fn load_env(
    config_path: Option<&Path>,
    filterdb_dir: Option<&Path>,
    choices_file: Option<&Path>,
    trackdb_dir: Option<&Path>,
) -> Result<FiltersEnv, String> {
    let loaded = PlayerConfig::discover_and_load(config_path)?;
    let fdb_dir = filterdb_dir
        .map(Path::to_path_buf)
        .or_else(|| loaded.config.filters.dir.clone())
        .unwrap_or_else(FilterDbStore::default_dir);
    let c_path = choices_file
        .map(Path::to_path_buf)
        .or_else(|| loaded.config.filters.choices_file.clone())
        .unwrap_or_else(FilterDbStore::default_choices_path);
    let legacy_dir = trackdb_dir
        .map(Path::to_path_buf)
        .or_else(|| loaded.config.trackdb.dir.clone())
        .unwrap_or_else(TrackDbStore::default_central_dir);

    let filter_db = FilterDbStore::new(fdb_dir);
    let mut manifest = filter_db
        .load_manifest()
        .unwrap_or_else(|| LibraryManifest::new(DETECTOR_VERSION));
    for (name, path) in &loaded.config.filters.roots {
        manifest.upsert_root(name, path);
    }
    if manifest.roots.is_empty() {
        if let Some(def_music) = default_user_music_dir() {
            if def_music.is_dir() {
                manifest.upsert_root("main", &def_music);
            }
        }
    }

    Ok(FiltersEnv {
        filter_db,
        choices_path: c_path,
        manifest,
        legacy_trackdb_dir: legacy_dir,
    })
}

fn load_user_filters_if_present(path: &Path) -> Result<Option<UserFiltersFile>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)
        .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
    let parsed = UserFiltersFile::from_yaml_str(&text)?;
    Ok(Some(parsed))
}

/// Dispatch entrypoint for `audiophile-player filters <SUBCOMMAND>`.
pub fn run_filters_cli(cmd: FiltersCommand) -> Result<(), String> {
    let out = run_filters_capture(cmd)?;
    print!("{out}");
    Ok(())
}

/// Run a `filters` subcommand and capture its human-readable output (shared by CLI & MCP).
pub fn run_filters_capture(cmd: FiltersCommand) -> Result<String, String> {
    match cmd {
        FiltersCommand::Show {
            target,
            config_path,
            filterdb_dir,
            choices_file,
        } => {
            let env = load_env(
                config_path.as_deref(),
                filterdb_dir.as_deref(),
                choices_file.as_deref(),
                None,
            )?;
            cmd_show(&env, &target)
        }
        FiltersCommand::Diff {
            config_path,
            filterdb_dir,
            choices_file,
        } => {
            let env = load_env(
                config_path.as_deref(),
                filterdb_dir.as_deref(),
                choices_file.as_deref(),
                None,
            )?;
            cmd_diff(&env)
        }
        FiltersCommand::Set {
            target,
            filter,
            note,
            config_path,
            filterdb_dir,
            choices_file,
        } => {
            let env = load_env(
                config_path.as_deref(),
                filterdb_dir.as_deref(),
                choices_file.as_deref(),
                None,
            )?;
            cmd_set(&env, &target, &filter, note.as_deref())
        }
        FiltersCommand::Check {
            config_path,
            filterdb_dir,
            choices_file,
        } => {
            let env = load_env(
                config_path.as_deref(),
                filterdb_dir.as_deref(),
                choices_file.as_deref(),
                None,
            )?;
            let (out, issues) = cmd_check(&env)?;
            if issues > 0 {
                return Err(format!("{out}my-filters.yaml check found {issues} issue(s)"));
            }
            Ok(out)
        }
        FiltersCommand::Relink {
            apply,
            config_path,
            filterdb_dir,
            choices_file,
        } => {
            let env = load_env(
                config_path.as_deref(),
                filterdb_dir.as_deref(),
                choices_file.as_deref(),
                None,
            )?;
            cmd_relink(&env, apply)
        }
        FiltersCommand::Migrate {
            config_path,
            trackdb_dir,
            filterdb_dir,
            choices_file,
            sidecar_root,
        } => {
            let mut env = load_env(
                config_path.as_deref(),
                filterdb_dir.as_deref(),
                choices_file.as_deref(),
                trackdb_dir.as_deref(),
            )?;
            cmd_migrate(&mut env, sidecar_root.as_deref())
        }
    }
}

fn cmd_show(env: &FiltersEnv, target: &Path) -> Result<String, String> {
    let uf = load_user_filters_if_present(&env.choices_path)?;
    let mut out = String::new();

    // Support both filesystem paths and explicit `"root:rel"` keys.
    let files_to_show: Vec<(String, String, String, Option<PathBuf>)> = if target.is_file() {
        let (root, rel_folder, fname) = env
            .manifest
            .resolve_track_path(target)
            .unwrap_or_else(|| {
                let f = target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| target.to_string_lossy().into_owned());
                ("main".to_string(), String::new(), f)
            });
        vec![(root, rel_folder, fname, Some(target.to_path_buf()))]
    } else if target.is_dir() {
        let (root, rel_folder) = env
            .manifest
            .resolve_folder_path(target)
            .unwrap_or_else(|| ("main".to_string(), String::new()));
        let flacs = discover_flac_files(target)?;
        if flacs.is_empty() {
            // Maybe album exists in filterdb even if folder has no direct FLACs?
            if let Some(alb) = env.filter_db.load_album(&root, &rel_folder) {
                alb.tracks
                    .keys()
                    .map(|k| (root.clone(), rel_folder.clone(), k.clone(), None))
                    .collect()
            } else {
                return Err(format!(
                    "no FLAC files or cached analysis found for '{}'",
                    target.display()
                ));
            }
        } else {
            flacs
                .into_iter()
                .map(|p| {
                    let (r, rf, fnm) = env
                        .manifest
                        .resolve_track_path(&p)
                        .unwrap_or_else(|| {
                            let f = p
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            (root.clone(), rel_folder.clone(), f)
                        });
                    (r, rf, fnm, Some(p))
                })
                .collect()
        }
    } else if let Some(raw) = target.to_str() {
        if raw.contains(':') {
            let (root, rel) = split_root_key(raw);
            if rel.to_ascii_lowercase().ends_with(".flac") {
                let (rf, fnm) = match rel.rsplit_once('/') {
                    Some((d, f)) => (d.to_string(), f.to_string()),
                    None => (String::new(), rel.clone()),
                };
                let abs = env.manifest.to_abs_path(raw);
                vec![(root.to_string(), rf, fnm, abs)]
            } else if let Some(alb) = env.filter_db.load_album(root, &rel) {
                alb.tracks
                    .keys()
                    .map(|k| {
                        let key_k = format_root_rel_key(root, &rel, Some(k));
                        let abs = env.manifest.to_abs_path(&key_k);
                        (root.to_string(), rel.clone(), k.clone(), abs)
                    })
                    .collect()
            } else {
                return Err(format!("target '{raw}' not found on disk or in filterdb"));
            }
        } else {
            return Err(format!("target '{}' does not exist", target.display()));
        }
    } else {
        return Err(format!("target '{}' does not exist", target.display()));
    };

    for (root, rel_folder, file_name, abs_opt) in files_to_show {
        let key = format_root_rel_key(&root, &rel_folder, Some(&file_name));
        let cached = env
            .filter_db
            .load_album(&root, &rel_folder)
            .and_then(|alb| alb.tracks.get(&file_name).cloned());
        let meta = abs_opt
            .as_deref()
            .filter(|p| p.is_file())
            .and_then(|p| FlacBlockReader::open(p).ok())
            .map(|r| r.track_meta())
            .unwrap_or_default();

        let (sug_filter, sug_rule, sug_why) = match cached {
            Some(ref t) => (t.suggested.clone(), t.rule.clone(), t.why.clone()),
            None => (
                "(unscanned)".to_string(),
                "unscanned".to_string(),
                "run `audiophile-player scan` or play track in Ultimate Mode".to_string(),
            ),
        };

        let resolved = uf
            .as_ref()
            .map(|u| u.resolve_for_track(&root, &rel_folder, &file_name, &meta));

        out.push_str(&format!("{key}\n"));
        if let Some(ref ov) = resolved {
            if let Some((ref f, prov, ref detail)) = ov.filter {
                out.push_str(&format!(
                    "  playing (mine): {f} ({}) [{detail}]\n",
                    prov.short_label()
                ));
            } else if let Some((p, ref detail)) = ov.phase {
                out.push_str(&format!(
                    "  playing (mine): phase={p:?} [{detail}]\n"
                ));
            } else {
                out.push_str(&format!(
                    "  playing (mine): {sug_filter} (same as suggested)\n"
                ));
            }
        } else if let Some(ref ap_f) = meta.ap_filter {
            out.push_str(&format!("  playing (mine): {ap_f} (mine: tag) [AP_FILTER]\n"));
        } else {
            out.push_str(&format!(
                "  playing (mine): {sug_filter} (no my-filters.yaml override)\n"
            ));
        }
        out.push_str(&format!(
            "  suggested:      {sug_filter} [{sug_rule}] — {sug_why}\n"
        ));
    }

    Ok(out)
}

fn cmd_diff(env: &FiltersEnv) -> Result<String, String> {
    let uf = load_user_filters_if_present(&env.choices_path)?;
    let albums = env.filter_db.load_all_albums();
    let mut out = String::new();
    let mut diff_count = 0usize;

    for (_, album) in &albums {
        let mut album_lines = Vec::new();
        for (fname, track) in &album.tracks {
            let meta = TrackMetadata::default();
            let resolved = uf
                .as_ref()
                .map(|u| u.resolve_for_track(&album.root, &album.folder, fname, &meta));
            if let Some(ref ov) = resolved {
                if let Some((ref f, prov, _)) = ov.filter {
                    if !f.eq_ignore_ascii_case(&track.suggested) {
                        album_lines.push(format!(
                            "  {fname}: mine={f} ({}) | suggested={} [{}]",
                            prov.short_label(),
                            track.suggested,
                            track.rule
                        ));
                        diff_count += 1;
                    }
                } else if ov.phase.is_some() || ov.apodizing.is_some() || ov.deemphasis.is_some() {
                    let mut attrs = Vec::new();
                    if let Some((p, _)) = ov.phase {
                        attrs.push(format!("phase={p:?}"));
                    }
                    if let Some((a, _)) = ov.apodizing {
                        attrs.push(format!("apodizing={a}"));
                    }
                    if let Some((d, _)) = ov.deemphasis {
                        attrs.push(format!("deemphasis={d}"));
                    }
                    album_lines.push(format!(
                        "  {fname}: mine={} | suggested={} [{}]",
                        attrs.join(", "),
                        track.suggested,
                        track.rule
                    ));
                    diff_count += 1;
                }
            }
        }
        if !album_lines.is_empty() {
            let folder_label = format_root_rel_key(&album.root, &album.folder, None);
            out.push_str(&format!("[{folder_label}]\n"));
            for line in album_lines {
                out.push_str(&line);
                out.push('\n');
            }
        }
    }

    if diff_count == 0 {
        out.push_str("No differences: all scanned tracks match machine suggestions.\n");
    } else {
        out.push_str(&format!(
            "Total: {diff_count} track(s) differ from machine suggestions.\n"
        ));
    }
    Ok(out)
}

fn cmd_set(
    env: &FiltersEnv,
    target: &Path,
    filter: &str,
    note: Option<&str>,
) -> Result<String, String> {
    let filter_trimmed = filter.trim();
    if !is_known_filter_name(filter_trimmed) {
        return Err(format!(
            "unknown filter '{filter_trimmed}'; valid filters: {}",
            ap_core::user_filters::KNOWN_FILTER_NAMES.join(", ")
        ));
    }

    let (section, key, md5_opt) = if target.is_dir() {
        let (root, rel) = env.manifest.resolve_folder_path(target).ok_or_else(|| {
            format!(
                "folder '{}' is not under any configured library root (run `audiophile-player scan` or add [filters.roots] in config.toml)",
                target.display()
            )
        })?;
        ("folders", format_root_rel_key(&root, &rel, None), None)
    } else if target.is_file() {
        let (root, rel_folder, fname) =
            env.manifest.resolve_track_path(target).ok_or_else(|| {
                format!(
                    "file '{}' is not under any configured library root (run `audiophile-player scan` or add [filters.roots] in config.toml)",
                    target.display()
                )
            })?;
        let md5 = FlacBlockReader::open(target)
            .ok()
            .and_then(|r| r.streaminfo_md5())
            .map(|m| ap_source::md5::md5_to_hex(&m));
        (
            "tracks",
            format_root_rel_key(&root, &rel_folder, Some(&fname)),
            md5,
        )
    } else if let Some(raw) = target.to_str() {
        if raw.contains(':') {
            let (root, rel) = split_root_key(raw);
            let is_track = rel.to_ascii_lowercase().ends_with(".flac");
            let sec = if is_track { "tracks" } else { "folders" };
            let key = if rel.is_empty() {
                root.to_string()
            } else {
                format!("{root}:{rel}")
            };
            (sec, key, None)
        } else {
            return Err(format!(
                "target '{}' does not exist on disk and is not a 'root:rel/path' key",
                target.display()
            ));
        }
    } else {
        return Err(format!("target '{}' does not exist", target.display()));
    };

    let existing = if env.choices_path.is_file() {
        Some(
            fs::read_to_string(&env.choices_path)
                .map_err(|e| format!("failed to read '{}': {e}", env.choices_path.display()))?,
        )
    } else {
        None
    };

    let updated = upsert_entry_preserving_comments(
        existing.as_deref(),
        section,
        &key,
        Some(filter_trimmed),
        None,
        None,
        md5_opt.as_deref(),
        note,
    );
    // Sanity-parse before touching disk!
    UserFiltersFile::from_yaml_str(&updated)?;
    write_user_filters_with_backup(&env.choices_path, &updated)?;

    Ok(format!(
        "Set {section}.\"{key}\" -> filter={filter_trimmed} in {}\n",
        env.choices_path.display()
    ))
}

fn cmd_check(env: &FiltersEnv) -> Result<(String, usize), String> {
    if !env.choices_path.is_file() {
        return Ok((
            format!(
                "No my-filters.yaml found at '{}' (nothing to check).\n",
                env.choices_path.display()
            ),
            0,
        ));
    }
    let text = fs::read_to_string(&env.choices_path)
        .map_err(|e| format!("failed to read '{}': {e}", env.choices_path.display()))?;
    let uf = UserFiltersFile::from_yaml_str(&text)?;
    let issues = uf.lint(Some(&env.manifest));

    let mut out = String::new();
    for issue in &issues {
        out.push_str(&format!("WARN  {issue}\n"));
    }
    if issues.is_empty() {
        out.push_str(&format!(
            "OK: '{}' is valid ({} folder rule(s), {} track rule(s)).\n",
            env.choices_path.display(),
            uf.folders.len(),
            uf.tracks.len()
        ));
    }
    Ok((out, issues.len()))
}

fn cmd_relink(env: &FiltersEnv, apply: bool) -> Result<String, String> {
    let Some(mut report) = env.filter_db.load_relink_report() else {
        return Ok("No pending moved files to relink (relink-report.yaml not present).\n".to_string());
    };
    if report.moves.is_empty() {
        return Ok("No pending moved files to relink.\n".to_string());
    }

    let mut out = String::new();
    out.push_str(&format!(
        "Pending relinks ({}):\n",
        report.moves.len()
    ));
    for mv in &report.moves {
        out.push_str(&format!("  {}  ->  {}\n", mv.old_key, mv.new_key));
    }

    if !apply {
        out.push_str(
            "Dry run only. Run `audiophile-player filters relink --apply` to update my-filters.yaml (creates .bak backup).\n",
        );
        return Ok(out);
    }

    if !env.choices_path.is_file() {
        return Err(format!(
            "cannot apply relink: '{}' does not exist",
            env.choices_path.display()
        ));
    }
    let existing = fs::read_to_string(&env.choices_path)
        .map_err(|e| format!("failed to read '{}': {e}", env.choices_path.display()))?;
    let (updated, applied_count) = apply_relink_preserving_comments(&existing, &report.moves);
    UserFiltersFile::from_yaml_str(&updated)?;
    write_user_filters_with_backup(&env.choices_path, &updated)?;

    report.moves.clear();
    report.generated_unix_s = now_unix_secs();
    let _ = env.filter_db.save_relink_report(&report);

    out.push_str(&format!(
        "Applied {applied_count} relink(s) to '{}' (backup saved as .bak).\n",
        env.choices_path.display()
    ));
    Ok(out)
}

fn cmd_migrate(env: &mut FiltersEnv, sidecar_root: Option<&Path>) -> Result<String, String> {
    let mut entries: Vec<TrackDbEntry> = Vec::new();

    // 1. Collect legacy Schema-1 YAML files from `trackdb/{md5,path,move}`.
    for sub in ["md5", "path", "move"] {
        let dir = env.legacy_trackdb_dir.join(sub);
        if dir.is_dir() {
            collect_schema1_yamls_in_dir(&dir, &mut entries);
        }
    }

    // 2. Optionally collect `<track.flac>.ap.yaml` sidecars under `sidecar_root`.
    if let Some(root_dir) = sidecar_root {
        if root_dir.is_dir() {
            collect_sidecar_yamls_recursive(root_dir, &mut entries);
        }
    }

    if entries.is_empty() {
        return Ok(format!(
            "No legacy Schema-1 entries found in '{}' (nothing to migrate).\n",
            env.legacy_trackdb_dir.display()
        ));
    }

    // Deduplicate by normalized path (latest scanned_at wins).
    let mut by_path: BTreeMap<String, TrackDbEntry> = BTreeMap::new();
    for entry in entries {
        if entry.file.path.trim().is_empty() {
            continue;
        }
        let norm_key = entry.file.path.to_ascii_lowercase();
        let replace = by_path
            .get(&norm_key)
            .is_none_or(|old| entry.scan.scanned_at_unix_s >= old.scan.scanned_at_unix_s);
        if replace {
            by_path.insert(norm_key, entry);
        }
    }

    // Group migrated tracks by (root, rel_folder) -> AlbumAnalysisFile, and collect user choices.
    let mut albums: BTreeMap<(String, String), AlbumAnalysisFile> = BTreeMap::new();
    let mut user_choices: Vec<(
        String,
        Option<String>,
        Option<bool>,
        Option<bool>,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    let mut migrated_tracks = 0usize;

    for entry in by_path.values() {
        let path = Path::new(&entry.file.path);
        let (root, rel_folder, file_name) = match env.manifest.resolve_track_path(path) {
            Some(loc) => loc,
            None => {
                // Auto-register parent directory (or root drive / common parent) if manifest has no match.
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let root_name = if !env.manifest.roots.contains_key("main") {
                    env.manifest.upsert_root("main", parent);
                    "main".to_string()
                } else {
                    let mut idx = 2usize;
                    loop {
                        let cand = format!("migrated{idx}");
                        if !env.manifest.roots.contains_key(&cand) {
                            env.manifest.upsert_root(&cand, parent);
                            break cand;
                        }
                        idx += 1;
                    }
                };
                let fname = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.file.path.clone());
                (root_name, String::new(), fname)
            }
        };

        let head_hash = path.is_file().then(|| file_head_4k_hash(path)).flatten();
        let mut s2 = AlbumTrackAnalysis::from_schema1(entry, head_hash);
        if s2.why.is_empty() {
            s2.why = why_for_rule(&s2.rule).to_string();
        }

        let alb = albums
            .entry((root.clone(), rel_folder.clone()))
            .or_insert_with(|| {
                env.filter_db
                    .load_album(&root, &rel_folder)
                    .unwrap_or_else(|| {
                        AlbumAnalysisFile::new(
                            root.clone(),
                            rel_folder.clone(),
                            entry.scan.detector_version.max(DETECTOR_VERSION),
                        )
                    })
            });
        alb.tracks.insert(file_name.clone(), s2);
        migrated_tracks += 1;

        let uf_opt = entry
            .user
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if uf_opt.is_some() || entry.user.apodizing.is_some() || entry.user.deemphasis.is_some() {
            let note = if entry.user.note.trim().is_empty() {
                Some("migrated from schema 1".to_string())
            } else {
                Some(entry.user.note.trim().to_string())
            };
            let key = format_root_rel_key(&root, &rel_folder, Some(&file_name));
            user_choices.push((
                key,
                uf_opt,
                entry.user.apodizing,
                entry.user.deemphasis,
                note,
                entry.file.streaminfo_md5.clone(),
            ));
        }
    }

    let mut written_albums = 0usize;
    for (_, alb) in &albums {
        let _ = env.filter_db.save_album_nonblocking(alb)?;
        written_albums += 1;
    }

    let mut migrated_user_choices = 0usize;
    if !user_choices.is_empty() {
        let mut text = if env.choices_path.is_file() {
            fs::read_to_string(&env.choices_path)
                .map_err(|e| format!("failed to read '{}': {e}", env.choices_path.display()))?
        } else {
            String::new()
        };
        for (key, filter, apodizing, deemphasis, note, md5) in user_choices {
            let prev = if text.is_empty() {
                None
            } else {
                Some(text.as_str())
            };
            text = upsert_entry_preserving_comments(
                prev,
                "tracks",
                &key,
                filter.as_deref(),
                apodizing,
                deemphasis,
                md5.as_deref(),
                note.as_deref(),
            );
            migrated_user_choices += 1;
        }
        UserFiltersFile::from_yaml_str(&text)?;
        write_user_filters_with_backup(&env.choices_path, &text)?;
    }

    let all_albums = env.filter_db.load_all_albums();
    env.manifest.last_scan_finished_unix_s = now_unix_secs();
    env.manifest.detector_version = DETECTOR_VERSION;
    env.manifest.last_scan_folders = all_albums.len() as u64;
    env.manifest.last_scan_files = all_albums
        .iter()
        .map(|(_, a)| a.tracks.len() as u64)
        .sum();
    env.filter_db.save_manifest(&env.manifest)?;

    Ok(format!(
        "Migration complete (non-destructive; legacy Schema-1 files untouched):\n  migrated tracks: {migrated_tracks}\n  album files written: {written_albums}\n  user choices migrated to my-filters.yaml: {migrated_user_choices}\n"
    ))
}

fn collect_schema1_yamls_in_dir(dir: &Path, dst: &mut Vec<TrackDbEntry>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            collect_schema1_yamls_in_dir(&p, dst);
        } else if p
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml"))
        {
            if let Ok(text) = fs::read_to_string(&p) {
                if let Ok(parsed) = TrackDbEntry::from_yaml_str(&text) {
                    dst.push(parsed);
                }
            }
        }
    }
}

fn collect_sidecar_yamls_recursive(dir: &Path, dst: &mut Vec<TrackDbEntry>) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            collect_sidecar_yamls_recursive(&p, dst);
        } else if p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().ends_with(".ap.yaml"))
        {
            if let Ok(text) = fs::read_to_string(&p) {
                if let Ok(parsed) = TrackDbEntry::from_yaml_str(&text) {
                    dst.push(parsed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::time::{SystemTime, UNIX_EPOCH};

    use ap_core::config::ConvolutionConfig;
    use ap_core::track_db::{
        TRACK_DB_SCHEMA_VERSION, TrackDbDecision, TrackDbFileInfo, TrackDbResults, TrackDbScanInfo,
        TrackDbUserOverrides,
    };
    use ap_dsp::selector::UpsamplerPolicy;
    use flac_codec::encode::{FlacSampleWriter, Options};

    use crate::scanner::{ScanOptions, scan_folder};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ap_filters_cmd_{label}_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_flac(path: &Path, rate: u32, frames: usize, amp: f64) {
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
    fn filters_set_show_diff_check_relink_and_migrate_work_end_to_end() {
        let root = temp_dir("e2e");
        let music_dir = root.join("music");
        let album_dir = music_dir.join("Jazz").join("Album1");
        let album2_dir = music_dir.join("Jazz").join("Album2");
        let filterdb_dir = root.join("filterdb");
        let choices_path = root.join("my-filters.yaml");
        let legacy_dir = root.join("legacy_trackdb");
        fs::create_dir_all(&album_dir).unwrap();
        fs::create_dir_all(&album2_dir).unwrap();

        let track1 = album_dir.join("01_tune.flac");
        let track2 = album_dir.join("02_ballad.flac");
        write_flac(&track1, 44_100, 44_100, 0.4);
        write_flac(&track2, 44_100, 44_100, 0.5);

        // 1. Scan library into Schema 2.
        let opts = ScanOptions {
            folder: Some(music_dir.clone()),
            report: false,
            sidecar: false,
            post_chain: false,
            force: false,
            jobs: Some(1),
            trackdb_dir: Some(legacy_dir.clone()),
            filterdb_dir: Some(filterdb_dir.clone()),
            choices_file: Some(choices_path.clone()),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy::default(),
        };
        let s = scan_folder(&opts, None).unwrap();
        assert_eq!(s.scanned, 2);

        // 2. `filters set` rejects unknown filter names.
        let env = load_env(None, Some(&filterdb_dir), Some(&choices_path), Some(&legacy_dir)).unwrap();
        assert!(cmd_set(&env, &track1, "bogus-filter", None).is_err());

        // 3. `filters set` sets track override cleanly and creates my-filters.yaml.
        let set_out = cmd_set(&env, &track1, "short-lp", Some("warmer snare")).unwrap();
        assert!(set_out.contains("tracks.\"main:Jazz/Album1/01_tune.flac\""));
        assert!(choices_path.is_file());

        // 4. `filters show` shows both active (mine) and suggested side-by-side.
        let show_out = cmd_show(&env, &track1).unwrap();
        assert!(show_out.contains("playing (mine): short-lp (mine: track)"));
        assert!(show_out.contains("suggested:"));

        // 5. `filters diff` reports the overridden track.
        let diff_out = cmd_diff(&env).unwrap();
        assert!(diff_out.contains("01_tune.flac: mine=short-lp"));
        assert!(diff_out.contains("Total: 1 track(s)"));

        // 6. `filters check` passes with 0 issues.
        let (check_out, issues) = cmd_check(&env).unwrap();
        assert_eq!(issues, 0, "check_out: {check_out}");

        // 7. Move track1 -> Album2/01_tune_moved.flac, rescan, then `filters relink --apply`.
        let moved = album2_dir.join("01_tune_moved.flac");
        fs::rename(&track1, &moved).unwrap();
        let s2 = scan_folder(&opts, None).unwrap();
        assert_eq!(s2.moved_reused, 1);
        assert_eq!(s2.relink_candidates, 1);

        // Dry run preview leaves my-filters.yaml untouched.
        let dry_out = cmd_relink(&env, false).unwrap();
        assert!(dry_out.contains("Dry run only"));
        assert!(fs::read_to_string(&choices_path).unwrap().contains("Album1/01_tune.flac"));

        // Apply updates my-filters.yaml with .bak backup and clears relink report!
        let apply_out = cmd_relink(&env, true).unwrap();
        assert!(apply_out.contains("Applied 1 relink(s)"));
        assert!(choices_path.with_extension("yaml.bak").is_file());
        assert!(
            fs::read_to_string(&choices_path)
                .unwrap()
                .contains("main:Jazz/Album2/01_tune_moved.flac")
        );
        let (_, issues_after) = cmd_check(&env).unwrap();
        assert_eq!(issues_after, 0);

        // 8. `filters migrate` imports legacy Schema-1 trackdb entries + user overrides non-destructively.
        let leg_track = album2_dir.join("09_legacy.flac");
        write_flac(&leg_track, 44_100, 44_100, 0.3);
        let legacy_store = TrackDbStore::new(legacy_dir.clone(), false);
        let leg_entry = TrackDbEntry {
            schema: TRACK_DB_SCHEMA_VERSION,
            file: TrackDbFileInfo {
                path: leg_track.to_string_lossy().into_owned(),
                size: 1000,
                mtime_unix_s: 1234,
                streaminfo_md5: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                sample_rate: 44_100,
                bits: 16,
            },
            scan: TrackDbScanInfo {
                detector_version: DETECTOR_VERSION,
                scanned_at_unix_s: 1234,
                duration_s: 120.0,
            },
            results: TrackDbResults::default(),
            decision: TrackDbDecision {
                filter: "sinc-steep-4m".to_string(),
                rule: "rule_G_classical".to_string(),
            },
            user: TrackDbUserOverrides {
                filter: Some("gauss-long-lp".to_string()),
                deemphasis: None,
                apodizing: None,
                note: "legacy note".to_string(),
            },
        };
        let saved_legacy = legacy_store.save_preserving_user(Some(&leg_track), leg_entry).unwrap();
        assert!(saved_legacy.is_file());

        let mut env_mut = load_env(None, Some(&filterdb_dir), Some(&choices_path), Some(&legacy_dir)).unwrap();
        let mig_out = cmd_migrate(&mut env_mut, None).unwrap();
        assert!(mig_out.contains("migrated tracks: 1"));
        assert!(mig_out.contains("user choices migrated to my-filters.yaml: 1"));
        // Legacy file untouched!
        assert!(saved_legacy.is_file());
        // Schema-2 album & my-filters.yaml updated!
        let alb2 = env_mut.filter_db.load_album("main", "Jazz/Album2").unwrap();
        assert!(alb2.tracks.contains_key("09_legacy.flac"));
        let uf_after = UserFiltersFile::from_yaml_str(&fs::read_to_string(&choices_path).unwrap()).unwrap();
        assert!(uf_after.tracks.values().any(|(k, e)| k == "main:Jazz/Album2/09_legacy.flac" && e.filter.as_deref() == Some("gauss-long-lp")));

        let _ = fs::remove_dir_all(&root);
    }
}
