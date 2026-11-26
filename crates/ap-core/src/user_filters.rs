// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! User-owned `my-filters.yaml` parser, hot-reloading cache, specificity resolver, linter, and
//! comment-preserving line-level editor (`FILTER_DATABASE_RESEARCH.md` §§4.3, 5, 6, 7).
//!
//! Safety guarantees:
//! - The player and scanner NEVER silently write `my-filters.yaml` (only explicit `filters set`,
//!   `filters relink --apply`, and `filters migrate` touch it, always keeping `.bak`).
//! - A parse error in `my-filters.yaml` NEVER stops playback: it reports the exact line number
//!   and falls back cleanly as if the file were empty (`suggested` mode).
//! - Specificity order in `mine` mode:
//!   `CLI --filter` > `tracks:` > deepest `folders:` > shallower `folders:` > `AP_*` FLAC tags >
//!   `defaults:` > `suggested` analysis.
//! - Attribute-only entries (`phase`, `apodizing`, `deemphasis` without `filter`) merge cleanly
//!   without terminating the filter lookup.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::filter_db::{
    FILTER_DB_SCHEMA_VERSION, LibraryManifest, RelinkEntry, normalize_rel_path, split_root_key,
    split_yaml_key_val,
};
use crate::metadata::{PhaseTagOverride, TrackMetadata};
use crate::track_db::{
    atomic_write_yaml, contains_unquoted_yaml_special, file_size_and_mtime, parse_yaml_string,
    strip_yaml_comment, yaml_quote,
};

/// Hard maximum size for `my-filters.yaml` (512 KiB — thousands of hand-written overrides).
pub const MAX_USER_FILTERS_BYTES: usize = 512 * 1024;

/// Allowed canonical filter archetype names and aliases (mirrors `ap-dsp::selector::FilterArchetype`).
pub const KNOWN_FILTER_NAMES: &[&str] = &[
    "null",
    "simple-test",
    "gauss-long-lp",
    "gauss-hires-lp",
    "gauss-hires-mp",
    "gauss-hires-ip",
    "gauss-apod-4m",
    "sinc-steep-4m",
    "ext-sharp-lp",
    "minphase-long",
    "intermediate-long",
    "short-lp",
    "short-mp",
];

/// Returns `true` when `name` matches a recognized Audiophile Player filter archetype name/alias.
pub fn is_known_filter_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    KNOWN_FILTER_NAMES.iter().any(|&k| k == lower)
}

/// Global filter resolution mode (`--filters suggested | mine`, `[filters] mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FilterMode {
    /// Auto: `Mine` when `my-filters.yaml` exists on disk, `Suggested` otherwise.
    #[default]
    Auto,
    /// Play machine-suggested filters only (ignores `my-filters.yaml` and `AP_*` filter choices —
    /// ideal for A/B comparison against the analyser).
    Suggested,
    /// Play user choices (`my-filters.yaml` + `AP_*` tags) wherever defined, falling back to
    /// machine suggestions elsewhere.
    Mine,
}

impl FilterMode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "suggested" => Ok(Self::Suggested),
            "mine" => Ok(Self::Mine),
            other => Err(format!(
                "invalid filter mode '{other}' (expected: auto | suggested | mine)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Suggested => "suggested",
            Self::Mine => "mine",
        }
    }

    /// Resolve `Auto` to concrete `Mine` or `Suggested` based on whether `my-filters.yaml` exists.
    pub fn resolve(self, choices_file_exists: bool) -> Self {
        match self {
            Self::Auto => {
                if choices_file_exists {
                    Self::Mine
                } else {
                    Self::Suggested
                }
            }
            concrete => concrete,
        }
    }
}

/// One user choice block (`defaults:`, `folders.<key>:`, or `tracks.<key>:`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserFilterEntry {
    pub filter: Option<String>,
    pub phase: Option<PhaseTagOverride>,
    pub apodizing: Option<bool>,
    pub deemphasis: Option<bool>,
    pub md5: Option<String>,
    pub note: Option<String>,
    pub line_no: usize,
}

impl UserFilterEntry {
    pub fn is_empty(&self) -> bool {
        self.filter.is_none()
            && self.phase.is_none()
            && self.apodizing.is_none()
            && self.deemphasis.is_none()
    }
}

/// Provenance of a winning user filter override (`§5.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserFilterProvenance {
    /// Track entry in `my-filters.yaml` (`rule: user_filter`).
    Track,
    /// Folder entry in `my-filters.yaml` (`rule: user_folder`).
    Folder,
    /// `AP_FILTER` FLAC Vorbis tag (`rule: user_tag`).
    Tag,
    /// `defaults:` in `my-filters.yaml` (`rule: user_default`).
    Default,
}

impl UserFilterProvenance {
    pub fn rule_name(self) -> &'static str {
        match self {
            Self::Track => "user_filter",
            Self::Folder => "user_folder",
            Self::Tag => "user_tag",
            Self::Default => "user_default",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Track => "mine: track",
            Self::Folder => "mine: folder",
            Self::Tag => "mine: tag",
            Self::Default => "mine: default",
        }
    }
}

/// Merged effective user overrides for one track after walking the full specificity chain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedUserOverrides {
    pub filter: Option<(String, UserFilterProvenance, String)>,
    pub phase: Option<(PhaseTagOverride, String)>,
    pub apodizing: Option<(bool, String)>,
    pub deemphasis: Option<(bool, String)>,
    pub warnings: Vec<String>,
}

/// Parsed contents of user-owned `my-filters.yaml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserFiltersFile {
    pub schema: u32,
    pub defaults: UserFilterEntry,
    /// Normalized keys (`"root:rel/folder"` lowercase-matched on Windows) -> `(original_key, entry)`.
    pub folders: BTreeMap<String, (String, UserFilterEntry)>,
    /// Normalized keys (`"root:rel/folder/track.flac"`) -> `(original_key, entry)`.
    pub tracks: BTreeMap<String, (String, UserFilterEntry)>,
    /// Non-fatal warnings detected while parsing (e.g., unknown filter names or phase values).
    pub parse_warnings: Vec<String>,
}

fn canonical_lookup_key(raw_key: &str) -> String {
    let (root, rel) = split_root_key(raw_key);
    #[cfg(windows)]
    {
        format!("{}:{}", root.to_lowercase(), rel.to_lowercase())
    }
    #[cfg(not(windows))]
    {
        format!("{root}:{rel}")
    }
}

/// Format canonical `"root:rel/path"` display key (preserving original case).
/// Root-level keys use `"root:"` so lookup canonicalization matches `resolve_for_track` (Review #9 U4).
pub fn format_root_rel_key(root: &str, rel_folder: &str, file_name: Option<&str>) -> String {
    let norm_folder = normalize_rel_path(rel_folder);
    match (norm_folder.is_empty(), file_name) {
        (true, None) => format!("{root}:"),
        (true, Some(f)) => format!("{root}:{f}"),
        (false, None) => format!("{root}:{norm_folder}"),
        (false, Some(f)) => format!("{root}:{norm_folder}/{f}"),
    }
}

impl UserFiltersFile {
    /// Parse untrusted user-edited `my-filters.yaml` with line-numbered error diagnostics.
    pub fn from_yaml_str(input: &str) -> Result<Self, String> {
        if input.len() > MAX_USER_FILTERS_BYTES {
            return Err(format!(
                "my-filters.yaml exceeds {MAX_USER_FILTERS_BYTES}-byte limit ({} bytes)",
                input.len()
            ));
        }
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let mut out = UserFiltersFile::default();
        let mut saw_schema = false;
        let mut section = "";
        let mut cur_entry_key: Option<String> = None;
        let mut cur_entry = UserFilterEntry::default();

        let flush_entry = |sec: &str,
                           key: &mut Option<String>,
                           entry: &mut UserFilterEntry,
                           dst: &mut UserFiltersFile| {
            if let Some(orig_key) = key.take() {
                let norm = canonical_lookup_key(&orig_key);
                let taken = std::mem::take(entry);
                match sec {
                    "folders" => {
                        dst.folders.insert(norm, (orig_key, taken));
                    }
                    "tracks" => {
                        dst.tracks.insert(norm, (orig_key, taken));
                    }
                    _ => {}
                }
            }
        };

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let stripped = strip_yaml_comment(raw_line);
            let trimmed = stripped.trim();
            if trimmed.is_empty() {
                continue;
            }
            if raw_line.starts_with('\t') {
                return Err(format!("my-filters.yaml line {line_no}: tabs are forbidden"));
            }
            if trimmed.starts_with('%')
                || trimmed.starts_with("<<:")
                || contains_unquoted_yaml_special(trimmed)
            {
                return Err(format!(
                    "my-filters.yaml line {line_no}: unsafe YAML tags, anchors, aliases, or directives are forbidden"
                ));
            }

            let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();
            let Some((k_raw, v_raw)) = split_yaml_key_val(trimmed) else {
                return Err(format!(
                    "my-filters.yaml line {line_no}: expected 'key: value'"
                ));
            };
            let key = parse_yaml_string(k_raw)?;
            let val = v_raw.trim();

            if indent == 0 {
                flush_entry(section, &mut cur_entry_key, &mut cur_entry, &mut out);
                match key.as_str() {
                    "schema" => {
                        let ver = val.parse::<u32>().map_err(|_| {
                            format!("my-filters.yaml line {line_no}: invalid schema '{val}'")
                        })?;
                        if ver != FILTER_DB_SCHEMA_VERSION {
                            return Err(format!(
                                "my-filters.yaml line {line_no}: unsupported schema {ver} (expected {FILTER_DB_SCHEMA_VERSION})"
                            ));
                        }
                        out.schema = ver;
                        saw_schema = true;
                        section = "";
                    }
                    "defaults" => {
                        if !val.is_empty() && val != "{}" {
                            return Err(format!(
                                "my-filters.yaml line {line_no}: inline value '{val}' on 'defaults:' is not supported; put fields on indented lines below"
                            ));
                        }
                        section = if val == "{}" { "" } else { "defaults" };
                        out.defaults.line_no = line_no;
                    }
                    "folders" => {
                        if !val.is_empty() && val != "{}" {
                            return Err(format!(
                                "my-filters.yaml line {line_no}: inline value '{val}' on 'folders:' is not supported; put entries on indented lines below"
                            ));
                        }
                        section = if val == "{}" { "" } else { "folders" };
                    }
                    "tracks" => {
                        if !val.is_empty() && val != "{}" {
                            return Err(format!(
                                "my-filters.yaml line {line_no}: inline value '{val}' on 'tracks:' is not supported; put entries on indented lines below"
                            ));
                        }
                        section = if val == "{}" { "" } else { "tracks" };
                    }
                    other => {
                        return Err(format!(
                            "my-filters.yaml line {line_no}: unknown top-level section '{other}' (expected schema, defaults, folders, tracks)"
                        ));
                    }
                }
                continue;
            }

            if section == "defaults" {
                if indent < 2 {
                    return Err(format!(
                        "my-filters.yaml line {line_no}: invalid indentation ({indent} spaces) under defaults; expected 2 spaces"
                    ));
                }
                apply_entry_field(
                    &mut out.defaults,
                    &key,
                    val,
                    line_no,
                    &mut out.parse_warnings,
                )?;
                continue;
            }

            if (section == "folders" || section == "tracks") && indent == 2 {
                if !val.is_empty() {
                    return Err(format!(
                        "my-filters.yaml line {line_no}: inline value '{val}' on key '{key}:' is not supported; put fields (filter, phase, apodizing, deemphasis, note) on 4-space indented lines below"
                    ));
                }
                flush_entry(section, &mut cur_entry_key, &mut cur_entry, &mut out);
                cur_entry_key = Some(key);
                cur_entry = UserFilterEntry {
                    line_no,
                    ..UserFilterEntry::default()
                };
                continue;
            }

            if (section == "folders" || section == "tracks") && indent >= 4 {
                if cur_entry_key.is_none() {
                    return Err(format!(
                        "my-filters.yaml line {line_no}: field '{key}' without a parent path key"
                    ));
                }
                apply_entry_field(&mut cur_entry, &key, val, line_no, &mut out.parse_warnings)?;
                continue;
            }

            return Err(format!(
                "my-filters.yaml line {line_no}: unexpected indentation ({indent} spaces) for '{key}'"
            ));
        }

        flush_entry(section, &mut cur_entry_key, &mut cur_entry, &mut out);
        if !saw_schema {
            return Err("my-filters.yaml missing required 'schema: 2' header".to_string());
        }
        Ok(out)
    }

    /// Resolve effective overrides for a track in `mine` mode (`§5.1`):
    /// `tracks:` > deepest `folders:` > shallower `folders:` > `AP_*` FLAC tags > `defaults:`.
    /// Attribute-only entries (`phase`, `apodizing`, `deemphasis` without `filter`) merge without
    /// terminating the search for `filter`.
    pub fn resolve_for_track(
        &self,
        root_name: &str,
        rel_folder: &str,
        file_name: &str,
        track_meta: &TrackMetadata,
    ) -> ResolvedUserOverrides {
        let mut res = ResolvedUserOverrides::default();
        let norm_folder = normalize_rel_path(rel_folder);
        let track_disp = format_root_rel_key(root_name, &norm_folder, Some(file_name));

        let merge_entry = |res: &mut ResolvedUserOverrides,
                           entry: &UserFilterEntry,
                           prov: UserFilterProvenance,
                           label: &str| {
            if res.filter.is_none() {
                if let Some(ref f) = entry.filter {
                    if is_known_filter_name(f) {
                        res.filter = Some((f.clone(), prov, format!("{label}: filter={f}")));
                    } else {
                        res.warnings.push(format!(
                            "my-filters.yaml line {}: ignoring unrecognized filter '{f}' ({label})",
                            entry.line_no
                        ));
                    }
                }
            }
            if res.phase.is_none() {
                if let Some(p) = entry.phase {
                    res.phase = Some((p, format!("{label}: phase={p:?}")));
                }
            }
            if res.apodizing.is_none() {
                if let Some(a) = entry.apodizing {
                    res.apodizing = Some((a, format!("{label}: apodizing={a}")));
                }
            }
            if res.deemphasis.is_none() {
                if let Some(d) = entry.deemphasis {
                    res.deemphasis = Some((d, format!("{label}: deemphasis={d}")));
                }
            }
        };

        // 1. Track entry in `my-filters.yaml`
        let track_key = canonical_lookup_key(&track_disp);
        if let Some((orig, entry)) = self.tracks.get(&track_key) {
            merge_entry(
                &mut res,
                entry,
                UserFilterProvenance::Track,
                &format!("my-filters track '{orig}'"),
            );
        }

        // 2. Folder entries in `my-filters.yaml` (deepest folder -> shallower -> root)
        let mut folder_candidates: Vec<String> = Vec::new();
        if !norm_folder.is_empty() {
            let segs: Vec<&str> = norm_folder.split('/').collect();
            for depth in (1..=segs.len()).rev() {
                folder_candidates.push(format!("{root_name}:{}", segs[..depth].join("/")));
            }
        }
        folder_candidates.push(format!("{root_name}:"));

        for cand in folder_candidates {
            let ck = canonical_lookup_key(&cand);
            if let Some((orig, entry)) = self.folders.get(&ck) {
                merge_entry(
                    &mut res,
                    entry,
                    UserFilterProvenance::Folder,
                    &format!("my-filters folder '{orig}'"),
                );
            }
        }

        // 3. `AP_*` Vorbis tags in the FLAC file (rank below `my-filters.yaml` track/folder, above `defaults:`)
        if res.filter.is_none() {
            if let Some(ref tag_f) = track_meta.ap_filter {
                if is_known_filter_name(tag_f) {
                    res.filter = Some((
                        tag_f.clone(),
                        UserFilterProvenance::Tag,
                        format!("AP_FILTER={tag_f}"),
                    ));
                } else {
                    res.warnings
                        .push(format!("ignoring unrecognized AP_FILTER tag '{tag_f}'"));
                }
            }
        }
        if res.phase.is_none() {
            if let Some(p) = track_meta.ap_phase {
                res.phase = Some((p, format!("AP_PHASE={p:?}")));
            }
        }
        if res.apodizing.is_none() {
            if let Some(a) = track_meta.ap_apodizing {
                res.apodizing = Some((a, format!("AP_APODIZING={a}")));
            }
        }
        if res.deemphasis.is_none() {
            if let Some(d) = track_meta.ap_deemphasis {
                res.deemphasis = Some((d, format!("AP_DEEMPHASIS={d}")));
            }
        }

        // 4. `defaults:` in `my-filters.yaml` (lowest user tier, still beats machine suggestion)
        if !self.defaults.is_empty() {
            merge_entry(
                &mut res,
                &self.defaults,
                UserFilterProvenance::Default,
                "my-filters defaults",
            );
        }

        res
    }

    /// Lint `my-filters.yaml` against `manifest` (used by `audiophile-player filters check`):
    /// checks unknown filter names, missing paths, and shadowed track entries identical to their
    /// nearest enclosing folder entry.
    pub fn lint(&self, manifest: Option<&LibraryManifest>) -> Vec<String> {
        let mut issues = self.parse_warnings.clone();

        let check_entry = |label: &str, e: &UserFilterEntry, out: &mut Vec<String>| {
            if let Some(ref f) = e.filter {
                if !is_known_filter_name(f) {
                    out.push(format!(
                        "line {}: unknown filter '{f}' in {label}",
                        e.line_no
                    ));
                }
            }
        };

        check_entry("defaults", &self.defaults, &mut issues);

        for (orig, entry) in self.folders.values() {
            check_entry(&format!("folder '{orig}'"), entry, &mut issues);
            if let Some(m) = manifest {
                match m.to_abs_path(orig) {
                    Some(abs) if !abs.is_dir() => issues.push(format!(
                        "line {}: folder '{}' does not exist on disk ({})",
                        entry.line_no,
                        orig,
                        abs.display()
                    )),
                    None => issues.push(format!(
                        "line {}: folder '{}' references unknown root (not in library.yaml)",
                        entry.line_no, orig
                    )),
                    _ => {}
                }
            }
        }

        for (orig, entry) in self.tracks.values() {
            check_entry(&format!("track '{orig}'"), entry, &mut issues);
            if let Some(m) = manifest {
                match m.to_abs_path(orig) {
                    Some(abs) if !abs.is_file() => issues.push(format!(
                        "line {}: track '{}' does not exist on disk ({})",
                        entry.line_no,
                        orig,
                        abs.display()
                    )),
                    None => issues.push(format!(
                        "line {}: track '{}' references unknown root (not in library.yaml)",
                        entry.line_no, orig
                    )),
                    _ => {}
                }
            }
            // Shadowed check: track specifies the exact same filter/phase/apodizing/deemphasis
            // already inherited from its deepest enclosing folder!
            let (root, rel) = split_root_key(orig);
            if let Some((folder_rel, _fname)) = rel.rsplit_once('/') {
                let segs: Vec<&str> = folder_rel.split('/').collect();
                for depth in (1..=segs.len()).rev() {
                    let cand = format!("{root}:{}", segs[..depth].join("/"));
                    let ck = canonical_lookup_key(&cand);
                    if let Some((folder_orig, fe)) = self.folders.get(&ck) {
                        if entry.filter.is_some()
                            && entry.filter == fe.filter
                            && entry.phase == fe.phase
                            && entry.apodizing == fe.apodizing
                            && entry.deemphasis == fe.deemphasis
                        {
                            issues.push(format!(
                                "line {}: track '{}' duplicates enclosing folder '{}' (shadowed/redundant)",
                                entry.line_no, orig, folder_orig
                            ));
                        }
                        break;
                    }
                }
            }
        }

        issues
    }
}

fn apply_entry_field(
    entry: &mut UserFilterEntry,
    key: &str,
    val: &str,
    line_no: usize,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let t = val.trim();
    let is_null = t.is_empty() || t.eq_ignore_ascii_case("null") || t == "~";
    match key {
        "filter" => {
            entry.filter = if is_null {
                None
            } else {
                let s = parse_yaml_string(t)?;
                if !is_known_filter_name(&s) {
                    warnings.push(format!(
                        "my-filters.yaml line {line_no}: unknown filter name '{s}'"
                    ));
                }
                Some(s)
            };
        }
        "phase" => {
            entry.phase = if is_null {
                None
            } else {
                let s = parse_yaml_string(t)?;
                match PhaseTagOverride::parse(&s) {
                    Some(p) => Some(p),
                    None => {
                        warnings.push(format!(
                            "my-filters.yaml line {line_no}: invalid phase '{s}' (expected min | ip | lin)"
                        ));
                        None
                    }
                }
            };
        }
        "apodizing" => {
            entry.apodizing = if is_null {
                None
            } else {
                match t.to_ascii_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => {
                        return Err(format!(
                            "my-filters.yaml line {line_no}: invalid bool '{t}' for apodizing"
                        ));
                    }
                }
            };
        }
        "deemphasis" => {
            entry.deemphasis = if is_null {
                None
            } else {
                match t.to_ascii_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => {
                        return Err(format!(
                            "my-filters.yaml line {line_no}: invalid bool '{t}' for deemphasis"
                        ));
                    }
                }
            };
        }
        "md5" => {
            entry.md5 = if is_null {
                None
            } else {
                Some(parse_yaml_string(t)?)
            };
        }
        "note" => {
            entry.note = if is_null {
                None
            } else {
                Some(parse_yaml_string(t)?)
            };
        }
        other => {
            warnings.push(format!(
                "my-filters.yaml line {line_no}: ignoring unknown field '{other}'"
            ));
        }
    }
    Ok(())
}

/// Hot-reloading, playback-safe cache for `my-filters.yaml`.
/// Reloads automatically whenever `(size, mtime)` changes; parse errors never stop playback.
#[derive(Debug)]
pub struct UserFiltersCache {
    path: PathBuf,
    inner: Mutex<CachedUserFiltersState>,
}

#[derive(Debug, Default)]
struct CachedUserFiltersState {
    stamp: Option<(u64, u64)>,
    file: Option<UserFiltersFile>,
    last_error: Option<String>,
}

impl UserFiltersCache {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            inner: Mutex::new(CachedUserFiltersState::default()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns `(exists_on_disk, parsed_file_if_valid, parse_error_warning_if_any)`.
    pub fn load_fresh(&self) -> (bool, Option<UserFiltersFile>, Option<String>) {
        let Some(stamp) = file_size_and_mtime(&self.path) else {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.stamp = None;
            st.file = None;
            st.last_error = None;
            return (false, None, None);
        };
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if st.stamp == Some(stamp) {
            return (true, st.file.clone(), st.last_error.clone());
        }
        st.stamp = Some(stamp);
        if stamp.0 > MAX_USER_FILTERS_BYTES as u64 {
            let msg = format!(
                "my-filters.yaml exceeds {MAX_USER_FILTERS_BYTES}-byte limit ({} bytes); falling back to suggested mode",
                stamp.0
            );
            eprintln!("  [filters warn] {msg}");
            st.file = None;
            st.last_error = Some(msg.clone());
            return (true, None, Some(msg));
        }
        match fs::read_to_string(&self.path) {
            Ok(text) => match UserFiltersFile::from_yaml_str(&text) {
                Ok(parsed) => {
                    st.file = Some(parsed.clone());
                    st.last_error = None;
                    (true, Some(parsed), None)
                }
                Err(err) => {
                    let msg = format!(
                        "my-filters.yaml parse error (falling back to suggested mode): {err}"
                    );
                    eprintln!("  [filters warn] {msg}");
                    st.file = None;
                    st.last_error = Some(msg.clone());
                    (true, None, Some(msg))
                }
            },
            Err(err) => {
                let msg = format!("could not read '{}': {err}", self.path.display());
                st.file = None;
                st.last_error = Some(msg.clone());
                (true, None, Some(msg))
            }
        }
    }
}

/// Comment-preserving line-level editor: append or replace a single entry in `my-filters.yaml`
/// (`folders:` or `tracks:`), keeping all existing comments, blank lines, and ordering intact.
pub fn upsert_entry_preserving_comments(
    existing_text: Option<&str>,
    section: &str,
    entry_key: &str,
    filter: Option<&str>,
    apodizing: Option<bool>,
    deemphasis: Option<bool>,
    md5: Option<&str>,
    note: Option<&str>,
) -> String {
    let base = match existing_text {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => "# My filter choices. Audiophile Player only READS this file.\n\
              # Paths are relative to a root in filterdb/library.yaml.\n\
              # Most specific entry wins: track > deeper folder > shallower folder > AP_FILTER tag > defaults.\n\
              schema: 2\n\n\
              folders:\n\n\
              tracks:\n"
            .to_string(),
    };

    let mut new_block = Vec::new();
    new_block.push(format!("  {}:", yaml_quote(entry_key)));
    if let Some(f) = filter {
        new_block.push(format!("    filter: {f}"));
    }
    if let Some(a) = apodizing {
        new_block.push(format!("    apodizing: {a}"));
    }
    if let Some(d) = deemphasis {
        new_block.push(format!("    deemphasis: {d}"));
    }
    if let Some(m) = md5 {
        new_block.push(format!("    md5: {m}"));
    }
    if let Some(n) = note {
        if !n.is_empty() {
            new_block.push(format!("    note: {}", yaml_quote(n)));
        }
    }

    let lines: Vec<String> = base.lines().map(str::to_string).collect();
    let target_norm = canonical_lookup_key(entry_key);

    // Find section span (`folders:` or `tracks:`) and any existing matching key block inside it.
    let mut sec_start: Option<usize> = None;
    let mut sec_end: usize = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let stripped = strip_yaml_comment(line);
        let trimmed = stripped.trim();
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent == 0 && !trimmed.is_empty() {
            if let Some((k, _)) = split_yaml_key_val(trimmed) {
                let k_clean = parse_yaml_string(k).unwrap_or_default();
                if sec_start.is_some() {
                    sec_end = i;
                    break;
                }
                if k_clean == section {
                    sec_start = Some(i);
                }
            }
        }
    }

    let mut out_lines = lines.clone();
    if let Some(s_idx) = sec_start {
        // Normalize `folders: {}` -> `folders:` when inserting into an empty flow section.
        if let Some((_, v)) = split_yaml_key_val(strip_yaml_comment(&out_lines[s_idx]).trim()) {
            if v.trim() == "{}" {
                out_lines[s_idx] = format!("{section}:");
            }
        }
        // Scan inside `s_idx + 1 .. sec_end` for an existing block matching `entry_key`.
        let mut match_range: Option<(usize, usize)> = None;
        let mut i = s_idx + 1;
        while i < sec_end {
            let stripped = strip_yaml_comment(&out_lines[i]);
            let trimmed = stripped.trim();
            let indent = out_lines[i].len() - out_lines[i].trim_start_matches(' ').len();
            if indent == 2 && !trimmed.is_empty() {
                if let Some((k, _)) = split_yaml_key_val(trimmed) {
                    if let Ok(k_parsed) = parse_yaml_string(k) {
                        if canonical_lookup_key(&k_parsed) == target_norm {
                            let start = i;
                            let mut end = i + 1;
                            while end < sec_end {
                                let t2 = strip_yaml_comment(&out_lines[end]).trim();
                                let ind2 =
                                    out_lines[end].len() - out_lines[end].trim_start_matches(' ').len();
                                if !t2.is_empty() && ind2 <= 2 {
                                    break;
                                }
                                end += 1;
                            }
                            match_range = Some((start, end));
                            break;
                        }
                    }
                }
            }
            i += 1;
        }

        if let Some((start, end)) = match_range {
            // Review #9 U3: update existing block IN PLACE so unspecified fields (`phase`,
            // `apodizing`, `deemphasis`, `md5`, `note`) and inline/standalone comments stay intact!
            let mut updates: Vec<(&str, String)> = Vec::new();
            if let Some(f) = filter {
                updates.push(("filter", format!("    filter: {f}")));
            }
            if let Some(a) = apodizing {
                updates.push(("apodizing", format!("    apodizing: {a}")));
            }
            if let Some(d) = deemphasis {
                updates.push(("deemphasis", format!("    deemphasis: {d}")));
            }
            if let Some(m) = md5 {
                updates.push(("md5", format!("    md5: {m}")));
            }
            if let Some(n) = note {
                if !n.is_empty() {
                    updates.push(("note", format!("    note: {}", yaml_quote(n))));
                }
            }

            let mut block_lines: Vec<String> = out_lines[start..end].to_vec();
            for (field_name, new_line) in updates {
                let mut replaced = false;
                for bl in block_lines.iter_mut().skip(1) {
                    let ind = bl.len() - bl.trim_start_matches(' ').len();
                    let stripped = strip_yaml_comment(bl);
                    let tr = stripped.trim();
                    if ind >= 4 && !tr.is_empty() {
                        if let Some((k_raw, _)) = split_yaml_key_val(tr) {
                            if parse_yaml_string(k_raw).ok().as_deref() == Some(field_name) {
                                // Preserve any trailing inline comment on this field line.
                                let suffix = &bl[stripped.len()..];
                                *bl = if suffix.is_empty() {
                                    new_line.clone()
                                } else {
                                    format!("{new_line} {}", suffix.trim_start())
                                };
                                replaced = true;
                                break;
                            }
                        }
                    }
                }
                if !replaced {
                    // Insert right before any trailing blank lines in the block.
                    let mut ins = block_lines.len();
                    while ins > 1 && block_lines[ins - 1].trim().is_empty() {
                        ins -= 1;
                    }
                    block_lines.insert(ins, new_line);
                }
            }
            out_lines.splice(start..end, block_lines);
        } else {
            // Insert before trailing blank lines at `sec_end`.
            let mut insert_at = sec_end;
            while insert_at > s_idx + 1 && out_lines[insert_at - 1].trim().is_empty() {
                insert_at -= 1;
            }
            out_lines.splice(insert_at..insert_at, new_block);
        }
    } else {
        if !out_lines.last().is_some_and(|l| l.trim().is_empty()) {
            out_lines.push(String::new());
        }
        out_lines.push(format!("{section}:"));
        out_lines.extend(new_block);
    }

    let mut joined = out_lines.join("\n");
    joined.push('\n');
    joined
}

/// Apply `RelinkEntry` path rewrites to `my-filters.yaml` via line-level edits so comments and
/// entry ordering are 100% preserved. Returns `(updated_text, replaced_count)`.
pub fn apply_relink_preserving_comments(text: &str, moves: &[RelinkEntry]) -> (String, usize) {
    let mut replaced = 0usize;
    let mut out_lines = Vec::new();
    for line in text.lines() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let stripped = strip_yaml_comment(line);
        let trimmed = stripped.trim();
        let mut updated = None;
        if indent == 2 && !trimmed.is_empty() {
            if let Some((k_raw, rest)) = split_yaml_key_val(trimmed) {
                if let Ok(k_parsed) = parse_yaml_string(k_raw) {
                    let norm_k = canonical_lookup_key(&k_parsed);
                    if let Some(m) = moves
                        .iter()
                        .find(|m| canonical_lookup_key(&m.old_key) == norm_k)
                    {
                        let comment_part = &line[stripped.len()..];
                        let rebuilt = format!(
                            "  {}:{}{}",
                            yaml_quote(&m.new_key),
                            rest,
                            comment_part
                        );
                        updated = Some(rebuilt);
                        replaced += 1;
                    }
                }
            }
        }
        out_lines.push(updated.unwrap_or_else(|| line.to_string()));
    }
    let mut joined = out_lines.join("\n");
    joined.push('\n');
    (joined, replaced)
}

/// Save `my-filters.yaml` atomically, creating `my-filters.yaml.bak` first if the file exists.
pub fn write_user_filters_with_backup(path: &Path, content: &str) -> Result<(), String> {
    if path.exists() {
        let mut bak_os = path.as_os_str().to_os_string();
        bak_os.push(".bak");
        let bak_path = PathBuf::from(bak_os);
        fs::copy(path, &bak_path).map_err(|e| {
            format!(
                "failed creating backup '{}': {e}",
                bak_path.display()
            )
        })?;
    }
    atomic_write_yaml(path, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_CHOICES: &str = r#"# My filter choices
schema: 2

defaults:
  filter: gauss-long-lp

folders:
  "main:Classical":
    filter: gauss-hires-mp
  "main:Jazz/Miles Davis":
    apodizing: false
    phase: min
  "main:Jazz/Miles Davis/Kind of Blue (1959)":
    filter: sinc-steep-4m
    note: "prefer steep here"

tracks:
  "main:Jazz/Miles Davis/Kind of Blue (1959)/04 All Blues.flac":
    filter: gauss-apod-4m
    md5: 7c41d2e8f09b3a56c1d4e7f8a9b0c1d2
    note: "this one benefits from apodizing"
  "main:Jazz/Miles Davis/Kind of Blue (1959)/01 So What.flac":
    filter: sinc-steep-4m
"#;

    #[test]
    fn resolves_full_specificity_precedence_and_attribute_merging() {
        let parsed = UserFiltersFile::from_yaml_str(SAMPLE_CHOICES).unwrap();
        let empty_meta = TrackMetadata::default();

        // 1. Track entry beats album folder & parent folder, while inheriting parent's `apodizing: false`
        //    and `phase: min`!
        let r_track = parsed.resolve_for_track(
            "main",
            "Jazz/Miles Davis/Kind of Blue (1959)",
            "04 All Blues.flac",
            &empty_meta,
        );
        assert_eq!(
            r_track.filter.as_ref().map(|(f, p, _)| (f.as_str(), *p)),
            Some(("gauss-apod-4m", UserFilterProvenance::Track))
        );
        assert_eq!(r_track.apodizing.as_ref().map(|(a, _)| *a), Some(false));
        assert_eq!(
            r_track.phase.as_ref().map(|(p, _)| *p),
            Some(PhaseTagOverride::Minimum)
        );

        // 2. Deeper folder beats shallower folder & AP_FILTER tag!
        let mut tagged_meta = TrackMetadata::default();
        tagged_meta.ap_filter = Some("ext-sharp-lp".to_string());
        let r_folder = parsed.resolve_for_track(
            "main",
            "Jazz/Miles Davis/Kind of Blue (1959)",
            "02 Freddie Freeloader.flac",
            &tagged_meta,
        );
        assert_eq!(
            r_folder.filter.as_ref().map(|(f, p, _)| (f.as_str(), *p)),
            Some(("sinc-steep-4m", UserFilterProvenance::Folder))
        );

        // 3. Attribute-only folder (`Jazz/Miles Davis`: apodizing=false, phase=min) does NOT stop
        //    search: AP_FILTER tag wins for filter while folder's apodizing/phase still apply!
        let r_attr_tag = parsed.resolve_for_track(
            "main",
            "Jazz/Miles Davis/Bitches Brew",
            "01 Pharaoh's Dance.flac",
            &tagged_meta,
        );
        assert_eq!(
            r_attr_tag.filter.as_ref().map(|(f, p, _)| (f.as_str(), *p)),
            Some(("ext-sharp-lp", UserFilterProvenance::Tag))
        );
        assert_eq!(r_attr_tag.apodizing.as_ref().map(|(a, _)| *a), Some(false));
        assert_eq!(
            r_attr_tag.phase.as_ref().map(|(p, _)| *p),
            Some(PhaseTagOverride::Minimum)
        );

        // 4. `defaults:` applies when no track, folder, or AP_FILTER tag matches!
        let r_def = parsed.resolve_for_track("main", "Rock/Pink Floyd", "01.flac", &empty_meta);
        assert_eq!(
            r_def.filter.as_ref().map(|(f, p, _)| (f.as_str(), *p)),
            Some(("gauss-long-lp", UserFilterProvenance::Default))
        );

        // 5. Lint catches shadowed track (`01 So What.flac` duplicates enclosing folder)!
        let lint = parsed.lint(None);
        assert!(
            lint.iter().any(|w| w.contains("01 So What.flac") && w.contains("shadowed")),
            "expected shadowed lint warning, got: {lint:?}"
        );
    }

    #[test]
    fn line_level_upsert_and_relink_preserve_comments_and_order() {
        let updated = upsert_entry_preserving_comments(
            Some(SAMPLE_CHOICES),
            "folders",
            "main:Rock/Pink Floyd",
            Some("ext-sharp-lp"),
            None,
            None,
            None,
            Some("punchy rock"),
        );
        assert!(updated.contains("# My filter choices"));
        assert!(updated.contains("\"main:Rock/Pink Floyd\":"));
        let reparsed = UserFiltersFile::from_yaml_str(&updated).unwrap();
        assert_eq!(
            reparsed.folders[&canonical_lookup_key("main:Rock/Pink Floyd")]
                .1
                .filter
                .as_deref(),
            Some("ext-sharp-lp")
        );

        let (relinked, count) = apply_relink_preserving_comments(
            &updated,
            &[RelinkEntry {
                old_key: "main:Jazz/Miles Davis/Kind of Blue (1959)/04 All Blues.flac".to_string(),
                new_key: "main:Jazz/Miles Davis/Kind of Blue (1959) [Remaster]/04 All Blues.flac"
                    .to_string(),
                md5: Some("7c41d2e8f09b3a56c1d4e7f8a9b0c1d2".to_string()),
            }],
        );
        assert_eq!(count, 1);
        assert!(relinked.contains("# My filter choices"));
        assert!(relinked.contains("Kind of Blue (1959) [Remaster]/04 All Blues.flac"));
        assert!(UserFiltersFile::from_yaml_str(&relinked).is_ok());
    }

    #[test]
    fn review9_u3_u4_u9_inplace_upsert_root_key_and_strict_parser() {
        // U3: Updating `filter:` on an existing entry MUST preserve `phase`, `apodizing`,
        //     `deemphasis`, and comments inside the block!
        let initial = "\
schema: 2

tracks:
  \"main:Jazz/01.flac\":
    # User listening comment
    filter: gauss-long-lp # old inline comment
    phase: min
    apodizing: false
    deemphasis: true
    note: \"keep me\"
";
        let updated = upsert_entry_preserving_comments(
            Some(initial),
            "tracks",
            "main:Jazz/01.flac",
            Some("sinc-steep-4m"),
            None,
            None,
            None,
            None,
        );
        assert!(updated.contains("# User listening comment"));
        let parsed = UserFiltersFile::from_yaml_str(&updated).unwrap();
        let entry = &parsed.tracks[&canonical_lookup_key("main:Jazz/01.flac")].1;
        assert_eq!(entry.filter.as_deref(), Some("sinc-steep-4m"));
        assert_eq!(entry.phase, Some(PhaseTagOverride::Minimum));
        assert_eq!(entry.apodizing, Some(false));
        assert_eq!(entry.deemphasis, Some(true));
        assert_eq!(entry.note.as_deref(), Some("keep me"));

        // U4: Root-level key formats as `"main:"` and matches tracks anywhere under `main`.
        let root_key = format_root_rel_key("main", "", None);
        assert_eq!(root_key, "main:");
        let with_root = upsert_entry_preserving_comments(
            None,
            "folders",
            &root_key,
            Some("ext-sharp-lp"),
            None,
            None,
            None,
            None,
        );
        let parsed_root = UserFiltersFile::from_yaml_str(&with_root).unwrap();
        let res = parsed_root.resolve_for_track("main", "Sub/Album", "01.flac", &TrackMetadata::default());
        assert_eq!(
            res.filter.as_ref().map(|(f, p, _)| (f.as_str(), *p)),
            Some(("ext-sharp-lp", UserFilterProvenance::Folder))
        );

        // U9: Inline value on key line and malformed indentation (1 or 3 spaces) MUST error!
        let inline_val = "schema: 2\ntracks:\n  \"main:A/b.flac\": { filter: gauss-long-lp }\n";
        assert!(UserFiltersFile::from_yaml_str(inline_val).is_err());
        let bad_indent = "schema: 2\ntracks:\n  \"main:A/b.flac\":\n   filter: gauss-long-lp\n";
        assert!(UserFiltersFile::from_yaml_str(bad_indent).is_err());
    }
}
