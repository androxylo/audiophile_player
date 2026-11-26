// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Shared zero-dependency YAML helpers, safety checks, atomic file persistence, identity matching,
//! and post-chain peak helpers used by `filter_db`, `user_filters`, and legacy `track_db` migration
//! (Review #9 U11).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::track_db::{PostChainKey, PostChainPeakEntry};

/// Filesystem mtime tolerance (±2 s) for FAT/SMB/NTFS staleness checks when MD5 is unset.
pub const MTIME_TOLERANCE_SECS: u64 = 2;

/// Return current Unix epoch timestamp in whole seconds.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Read `(file_size_bytes, mtime_unix_s)` for a local path.
pub fn file_size_and_mtime(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some((size, mtime))
}

/// Normalize a path string for cross-format equality comparisons (strips Windows verbatim
/// `\\?\` / `\\?\UNC\` prefixes and folds case on Windows).
pub fn normalize_path_for_eq(s: &str) -> String {
    let trimmed = s.trim();
    #[cfg(windows)]
    {
        let mut norm = trimmed.replace('/', "\\");
        if let Some(rest) = norm.strip_prefix(r"\\?\UNC\") {
            norm = format!(r"\\{rest}");
        } else if let Some(rest) = norm.strip_prefix(r"\\?\unc\") {
            norm = format!(r"\\{rest}");
        } else if let Some(rest) = norm.strip_prefix(r"\\?\") {
            norm = rest.to_string();
        }
        norm.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        trimmed.to_string()
    }
}

/// Write `yaml` atomically to `target` (temp file -> `sync_all()` -> atomic rename, with
/// non-destructive sibling staging on Windows so an existing file is never lost on rename failure).
pub fn atomic_write_yaml(target: &Path, yaml: &str) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create YAML directory '{}': {e}",
                parent.display()
            )
        })?;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let mut tmp_name = target
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "entry.yaml".into());
    tmp_name.push(format!(".tmp.{pid}.{nanos}"));
    let tmp_path = target.with_file_name(tmp_name);

    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| format!("failed to create temp YAML '{}': {e}", tmp_path.display()))?;
        f.write_all(yaml.as_bytes())
            .map_err(|e| format!("failed to write temp YAML '{}': {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("failed to fsync temp YAML '{}': {e}", tmp_path.display()))?;
    }

    if let Err(first_err) = fs::rename(&tmp_path, target) {
        if target.exists() {
            let mut old_name = target
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_else(|| "entry.yaml".into());
            old_name.push(format!(".old.{pid}.{nanos}"));
            let old_path = target.with_file_name(old_name);
            if let Err(move_err) = fs::rename(target, &old_path) {
                let _ = fs::remove_file(&tmp_path);
                return Err(format!(
                    "failed to stage existing YAML '{}' for replacement: {move_err} (initial: {first_err})",
                    target.display()
                ));
            }
            if let Err(e2) = fs::rename(&tmp_path, target) {
                let _ = fs::rename(&old_path, target);
                let _ = fs::remove_file(&tmp_path);
                return Err(format!(
                    "failed to atomically replace YAML '{}': {e2} (initial: {first_err})",
                    target.display()
                ));
            }
            let _ = fs::remove_file(&old_path);
        } else {
            let _ = fs::remove_file(&tmp_path);
            return Err(format!(
                "failed to rename temp YAML '{}' -> '{}': {first_err}",
                tmp_path.display(),
                target.display()
            ));
        }
    }
    Ok(())
}

/// 128-bit FNV-1a lowercase hex digest.
pub fn fnv1a_128_hex(bytes: &[u8]) -> String {
    const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= u128::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:032x}")
}

/// Double-quote and escape a string for deterministic YAML output.
pub fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Strip an unquoted trailing `# ...` comment from a YAML line.
pub fn strip_yaml_comment(line: &str) -> &str {
    let mut in_double = false;
    let mut in_single = false;
    let mut escape = false;
    for (i, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_double {
            escape = true;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '#' && !in_double && !in_single {
            return &line[..i];
        }
    }
    line
}

/// Returns `true` if `line` contains unquoted YAML tags (`!`), anchors (`&`), or aliases (`*`).
pub fn contains_unquoted_yaml_special(line: &str) -> bool {
    let mut in_double = false;
    let mut in_single = false;
    let mut escape = false;
    let bytes = line.as_bytes();
    for (i, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_double {
            escape = true;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if !in_double && !in_single && matches!(ch, '!' | '&' | '*') {
            if i == 0 || matches!(bytes[i - 1], b' ' | b':' | b',' | b'{' | b'[') {
                return true;
            }
        }
    }
    false
}

/// Single shared safety gate for untrusted YAML lines (replaces 5 copies of tab/directive/tag checks).
pub fn check_yaml_line_safety(
    raw_line: &str,
    trimmed: &str,
    line_no: usize,
    label: &str,
) -> Result<(), String> {
    if raw_line.starts_with('\t') {
        return Err(format!("{label} line {line_no}: tabs are forbidden"));
    }
    if trimmed.starts_with('%')
        || trimmed.starts_with("<<:")
        || contains_unquoted_yaml_special(trimmed)
    {
        return Err(format!(
            "{label} line {line_no}: unsafe YAML tags, anchors, aliases, or directives are forbidden"
        ));
    }
    Ok(())
}

/// Split a YAML mapping line on the FIRST unquoted `:`, returning `(key_raw, val_raw)`.
pub fn split_yaml_key_val(line: &str) -> Option<(&str, &str)> {
    let mut in_double = false;
    let mut in_single = false;
    let mut escape = false;
    for (i, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_double {
            escape = true;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == ':' && !in_double && !in_single {
            return Some((&line[..i], &line[i + 1..]));
        }
    }
    None
}

/// Parse a double-quoted, single-quoted, or bare YAML scalar string.
pub fn parse_yaml_string(val: &str) -> Result<String, String> {
    let trimmed = val.trim();
    if let Some(inner) = trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('\\') => out.push('\\'),
                    Some('"') => out.push('"'),
                    Some('n') => out.push('\n'),
                    Some('r') => out.push('\r'),
                    Some('t') => out.push('\t'),
                    Some(other) => return Err(format!("invalid escape \\{other} in YAML string")),
                    None => return Err("unterminated escape in YAML string".to_string()),
                }
            } else {
                out.push(c);
            }
        }
        Ok(out)
    } else if let Some(inner) = trimmed.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        Ok(inner.replace("''", "'"))
    } else {
        Ok(trimmed.to_string())
    }
}

pub fn parse_opt_str(val: &str) -> Result<Option<String>, String> {
    let t = val.trim();
    if t.is_empty() || t == "null" || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_yaml_string(t)?))
    }
}

pub fn parse_u32(val: &str, line_no: usize) -> Result<u32, String> {
    val.trim()
        .parse::<u32>()
        .map_err(|_| format!("YAML line {line_no}: invalid integer '{val}'"))
}

pub fn parse_u64(val: &str, line_no: usize) -> Result<u64, String> {
    val.trim()
        .parse::<u64>()
        .map_err(|_| format!("YAML line {line_no}: invalid integer '{val}'"))
}

pub fn parse_opt_u32(val: &str, line_no: usize) -> Result<Option<u32>, String> {
    let t = val.trim();
    if t.is_empty() || t == "null" || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_u32(t, line_no)?))
    }
}

pub fn parse_f64(val: &str, line_no: usize) -> Result<f64, String> {
    let v = val
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("YAML line {line_no}: invalid float '{val}'"))?;
    if !v.is_finite() {
        return Err(format!("YAML line {line_no}: non-finite float '{val}'"));
    }
    Ok(v)
}

pub fn parse_opt_f64(val: &str, line_no: usize) -> Result<Option<f64>, String> {
    let t = val.trim();
    if t.is_empty() || t == "null" || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_f64(t, line_no)?))
    }
}

pub fn parse_bool(val: &str, line_no: usize) -> Result<bool, String> {
    match val.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("YAML line {line_no}: invalid boolean '{other}'")),
    }
}

pub fn parse_opt_bool(val: &str, line_no: usize) -> Result<Option<bool>, String> {
    let t = val.trim();
    if t.is_empty() || t == "null" || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_bool(t, line_no)?))
    }
}

/// Shared track identity comparison (MD5 match when both present, else `(size, mtime ±2 s)`).
pub fn matches_track_identity(
    stored_md5: Option<&str>,
    stored_size: u64,
    stored_mtime_unix_s: u64,
    cur_md5_hex: Option<&str>,
    cur_size: Option<u64>,
    cur_mtime_unix_s: Option<u64>,
) -> bool {
    if let (Some(a), Some(b)) = (stored_md5, cur_md5_hex) {
        return a.eq_ignore_ascii_case(b);
    }
    if let (Some(sz), Some(mt)) = (cur_size, cur_mtime_unix_s) {
        return stored_size == sz && stored_mtime_unix_s.abs_diff(mt) <= MTIME_TOLERANCE_SECS;
    }
    false
}

/// Look up a finite post-chain peak `M` (dBFS) matching `key`.
pub fn find_post_chain_peak(peaks: &[PostChainPeakEntry], key: &PostChainKey) -> Option<f64> {
    peaks
        .iter()
        .find(|e| &e.key == key && e.peak_dbfs.is_finite())
        .map(|e| e.peak_dbfs)
}

/// Monotonically insert or raise (or overwrite when `monotonic = false`) `peak_dbfs` for `key`.
pub fn upsert_post_chain_peak(
    peaks: &mut Vec<PostChainPeakEntry>,
    key: PostChainKey,
    peak_dbfs: f64,
    monotonic: bool,
) {
    if !peak_dbfs.is_finite() {
        return;
    }
    if let Some(existing) = peaks.iter_mut().find(|e| e.key == key) {
        if monotonic {
            existing.peak_dbfs = existing.peak_dbfs.max(peak_dbfs);
        } else {
            existing.peak_dbfs = peak_dbfs;
        }
    } else {
        peaks.push(PostChainPeakEntry { key, peak_dbfs });
    }
}
