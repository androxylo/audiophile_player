// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Legacy Schema-1 YAML Track Database parser (migrate-only since Review #9 U7).
//!
//! Runtime persistence uses Schema-2 per-album files (`filter_db.rs`) plus user-owned
//! `my-filters.yaml` (`user_filters.rs`). This module retains strictly the data structures,
//! hardened Schema-1 parser (`TrackDbEntry::from_yaml_str`), and default legacy directory helper
//! (`default_legacy_trackdb_dir`) needed by `audiophile-player filters migrate`.

use std::path::PathBuf;

pub use crate::yaml_lite::{
    MTIME_TOLERANCE_SECS, atomic_write_yaml, file_size_and_mtime, find_post_chain_peak,
    matches_track_identity, now_unix_secs, upsert_post_chain_peak,
};
use crate::yaml_lite::{
    check_yaml_line_safety, parse_bool, parse_f64, parse_opt_bool, parse_opt_f64, parse_opt_str,
    parse_opt_u32, parse_u32, parse_u64, parse_yaml_string, strip_yaml_comment,
};

/// Hard maximum size for a Schema-1 YAML track entry (64 KiB).
pub const MAX_YAML_BYTES: usize = 64 * 1024;

/// Supported legacy Schema-1 YAML schema version.
pub const TRACK_DB_SCHEMA_VERSION: u32 = 1;

/// Key identifying a specific DSP chain configuration for `post_chain_peaks` (`HEADROOM_DESIGN.md` §4).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PostChainKey {
    pub upsampler: String,
    pub f_eff_hz: u32,
    pub output_rate_hz: u32,
    pub deemphasis: bool,
    pub hf_noise_filter: bool,
    pub rew_filter_hash: String,
}

/// Measured unity-gain post-chain peak (`M` in dBFS before `G_up` / `S`) for one DSP chain key.
#[derive(Debug, Clone, PartialEq)]
pub struct PostChainPeakEntry {
    pub key: PostChainKey,
    pub peak_dbfs: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackDbFileInfo {
    pub path: String,
    pub size: u64,
    pub mtime_unix_s: u64,
    pub streaminfo_md5: Option<String>,
    pub sample_rate: u32,
    pub bits: u16,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackDbScanInfo {
    pub detector_version: u32,
    pub scanned_at_unix_s: u64,
    pub duration_s: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackDbResults {
    pub effective_bandwidth_hz: u32,
    pub upsampled_from_1x: String,
    pub effective_nyquist_hz: Option<u32>,
    pub ultrasonic_noise_detected: String,
    pub lossy_origin_suspected: bool,
    pub apod_events: u32,
    pub apodizing_needed: String,
    pub integrated_lufs: Option<f64>,
    pub sample_peak: f64,
    pub true_peak_dbtp: Option<f64>,
    pub clipped_runs: u32,
    pub loud_and_clipped: String,
    pub effective_bits: u16,
    pub deemphasis_flag: bool,
    pub post_chain_peaks: Vec<PostChainPeakEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackDbDecision {
    pub filter: String,
    pub rule: String,
}

/// Legacy Schema-1 `user:` override fields (migrated into `my-filters.yaml` by `filters migrate`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackDbUserOverrides {
    pub filter: Option<String>,
    pub deemphasis: Option<bool>,
    pub apodizing: Option<bool>,
    pub note: String,
}

/// Legacy Schema-1 YAML track entry (parsed only by `audiophile-player filters migrate`).
#[derive(Debug, Clone, PartialEq)]
pub struct TrackDbEntry {
    pub schema: u32,
    pub file: TrackDbFileInfo,
    pub scan: TrackDbScanInfo,
    pub results: TrackDbResults,
    pub decision: TrackDbDecision,
    pub user: TrackDbUserOverrides,
}

impl Default for TrackDbEntry {
    fn default() -> Self {
        Self {
            schema: TRACK_DB_SCHEMA_VERSION,
            file: TrackDbFileInfo::default(),
            scan: TrackDbScanInfo::default(),
            results: TrackDbResults {
                upsampled_from_1x: "inconclusive".to_string(),
                ultrasonic_noise_detected: "negative".to_string(),
                apodizing_needed: "inconclusive".to_string(),
                loud_and_clipped: "inconclusive".to_string(),
                ..TrackDbResults::default()
            },
            decision: TrackDbDecision::default(),
            user: TrackDbUserOverrides::default(),
        }
    }
}

/// Default per-user legacy Schema-1 central trackdb directory (`%APPDATA%\audiophile-player\trackdb`),
/// used strictly by `audiophile-player filters migrate`.
pub fn default_legacy_trackdb_dir() -> PathBuf {
    if let Some(cfg) = crate::config::PlayerConfig::default_user_config_path() {
        if let Some(parent) = cfg.parent() {
            return parent.join("trackdb");
        }
    }
    PathBuf::from("trackdb")
}

impl TrackDbEntry {
    /// Look up the measured unity-gain post-chain peak `M` (dBFS) matching `key`.
    pub fn find_post_chain_peak(&self, key: &PostChainKey) -> Option<f64> {
        find_post_chain_peak(&self.results.post_chain_peaks, key)
    }

    /// Monotonically insert or raise `peak_dbfs` for `key` (or set exact value when `monotonic = false`).
    pub fn upsert_post_chain_peak(&mut self, key: PostChainKey, peak_dbfs: f64, monotonic: bool) {
        upsert_post_chain_peak(&mut self.results.post_chain_peaks, key, peak_dbfs, monotonic);
    }

    /// Parse a strict untrusted Schema-1 YAML string (`filters migrate`).
    /// Rejects oversized inputs (`> 64 KiB`), YAML tags (`!`), anchors (`&`), aliases (`*`),
    /// merge keys (`<<:`), tabs, and unsupported schema versions.
    pub fn from_yaml_str(input: &str) -> Result<Self, String> {
        if input.len() > MAX_YAML_BYTES {
            return Err(format!(
                "trackdb YAML exceeds {MAX_YAML_BYTES}-byte safety limit ({} bytes)",
                input.len()
            ));
        }
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let mut entry = TrackDbEntry::default();
        let mut saw_schema = false;
        let mut section = "";
        let mut in_peaks_list = false;
        let mut cur_peak: Option<PostChainPeakEntry> = None;

        let flush_peak = |cur: &mut Option<PostChainPeakEntry>, dst: &mut Vec<PostChainPeakEntry>| {
            if let Some(p) = cur.take() {
                if !p.key.upsampler.is_empty() && p.peak_dbfs.is_finite() {
                    dst.push(p);
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
            check_yaml_line_safety(raw_line, trimmed, line_no, "trackdb YAML")?;

            let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();

            if indent == 0 {
                flush_peak(&mut cur_peak, &mut entry.results.post_chain_peaks);
                in_peaks_list = false;
                let Some((k, v)) = trimmed.split_once(':') else {
                    return Err(format!("trackdb YAML line {line_no}: expected 'key: value'"));
                };
                let key = k.trim();
                let val = v.trim();
                match key {
                    "schema" => {
                        let ver = val.parse::<u32>().map_err(|_| {
                            format!("trackdb YAML line {line_no}: invalid schema '{val}'")
                        })?;
                        if ver != TRACK_DB_SCHEMA_VERSION {
                            return Err(format!(
                                "trackdb YAML unsupported schema version {ver} (expected {TRACK_DB_SCHEMA_VERSION})"
                            ));
                        }
                        entry.schema = ver;
                        saw_schema = true;
                        section = "";
                    }
                    "file" | "scan" | "results" | "decision" | "user" => {
                        section = key;
                    }
                    _ => {
                        section = "_unknown";
                    }
                }
                continue;
            }

            if section == "results" && in_peaks_list && indent >= 4 {
                if let Some(rest) = trimmed.strip_prefix("- ") {
                    flush_peak(&mut cur_peak, &mut entry.results.post_chain_peaks);
                    cur_peak = Some(PostChainPeakEntry {
                        key: PostChainKey {
                            upsampler: String::new(),
                            f_eff_hz: 0,
                            output_rate_hz: 0,
                            deemphasis: false,
                            hf_noise_filter: false,
                            rew_filter_hash: String::new(),
                        },
                        peak_dbfs: f64::NAN,
                    });
                    if let Some((pk, pv)) = rest.split_once(':') {
                        apply_peak_field(cur_peak.as_mut().unwrap(), pk.trim(), pv.trim(), line_no)?;
                    }
                    continue;
                } else if indent >= 6 {
                    if let (Some(p), Some((pk, pv))) = (cur_peak.as_mut(), trimmed.split_once(':')) {
                        apply_peak_field(p, pk.trim(), pv.trim(), line_no)?;
                        continue;
                    }
                }
            }

            if indent <= 2 && in_peaks_list {
                flush_peak(&mut cur_peak, &mut entry.results.post_chain_peaks);
                in_peaks_list = false;
            }

            if section == "_unknown" {
                continue;
            }

            let Some((k, v)) = trimmed.split_once(':') else {
                return Err(format!("trackdb YAML line {line_no}: expected 'key: value'"));
            };
            let key = k.trim();
            let val = v.trim();

            match (section, key) {
                ("file", "path") => entry.file.path = parse_yaml_string(val)?,
                ("file", "size") => entry.file.size = parse_u64(val, line_no)?,
                ("file", "mtime_unix_s") => entry.file.mtime_unix_s = parse_u64(val, line_no)?,
                ("file", "streaminfo_md5") => entry.file.streaminfo_md5 = parse_opt_str(val)?,
                ("file", "sample_rate") => entry.file.sample_rate = parse_u32(val, line_no)?,
                ("file", "bits") => entry.file.bits = parse_u32(val, line_no)? as u16,

                ("scan", "detector_version") => entry.scan.detector_version = parse_u32(val, line_no)?,
                ("scan", "scanned_at_unix_s") => entry.scan.scanned_at_unix_s = parse_u64(val, line_no)?,
                ("scan", "duration_s") => entry.scan.duration_s = parse_f64(val, line_no)?,

                ("results", "effective_bandwidth_hz") => {
                    entry.results.effective_bandwidth_hz = parse_u32(val, line_no)?;
                }
                ("results", "upsampled_from_1x") => {
                    entry.results.upsampled_from_1x = parse_yaml_string(val)?;
                }
                ("results", "effective_nyquist_hz") => {
                    entry.results.effective_nyquist_hz = parse_opt_u32(val, line_no)?;
                }
                ("results", "ultrasonic_noise_detected") => {
                    entry.results.ultrasonic_noise_detected = parse_yaml_string(val)?;
                }
                ("results", "lossy_origin_suspected") => {
                    entry.results.lossy_origin_suspected = parse_bool(val, line_no)?;
                }
                ("results", "apod_events") => entry.results.apod_events = parse_u32(val, line_no)?,
                ("results", "apodizing_needed") => {
                    entry.results.apodizing_needed = parse_yaml_string(val)?;
                }
                ("results", "integrated_lufs") => {
                    entry.results.integrated_lufs = parse_opt_f64(val, line_no)?;
                }
                ("results", "sample_peak") => entry.results.sample_peak = parse_f64(val, line_no)?,
                ("results", "true_peak_dbtp") => {
                    entry.results.true_peak_dbtp = parse_opt_f64(val, line_no)?;
                }
                ("results", "clipped_runs") => entry.results.clipped_runs = parse_u32(val, line_no)?,
                ("results", "loud_and_clipped") => {
                    entry.results.loud_and_clipped = parse_yaml_string(val)?;
                }
                ("results", "effective_bits") => {
                    entry.results.effective_bits = parse_u32(val, line_no)? as u16;
                }
                ("results", "deemphasis_flag") => {
                    entry.results.deemphasis_flag = parse_bool(val, line_no)?;
                }
                ("results", "post_chain_peaks") => {
                    if val == "[]" {
                        in_peaks_list = false;
                    } else if val.is_empty() {
                        in_peaks_list = true;
                    } else {
                        return Err(format!(
                            "trackdb YAML line {line_no}: invalid post_chain_peaks value '{val}'"
                        ));
                    }
                }

                ("decision", "filter") => entry.decision.filter = parse_yaml_string(val)?,
                ("decision", "rule") => entry.decision.rule = parse_yaml_string(val)?,

                ("user", "filter") => entry.user.filter = parse_opt_str(val)?,
                ("user", "deemphasis") => entry.user.deemphasis = parse_opt_bool(val, line_no)?,
                ("user", "apodizing") => entry.user.apodizing = parse_opt_bool(val, line_no)?,
                ("user", "note") => entry.user.note = parse_yaml_string(val)?,
                _ => {}
            }
        }

        flush_peak(&mut cur_peak, &mut entry.results.post_chain_peaks);
        if !saw_schema {
            return Err("trackdb YAML missing required 'schema: 1' header".to_string());
        }
        if let Some(feff) = entry.results.effective_nyquist_hz {
            const MIN_VALID_FEFF_HZ: u32 = 19_380;
            let max_valid = if entry.file.sample_rate > 0 {
                entry.file.sample_rate / 2
            } else {
                384_000 / 2
            };
            if feff <= MIN_VALID_FEFF_HZ || feff > max_valid {
                entry.results.effective_nyquist_hz = None;
            }
        }
        Ok(entry)
    }
}

fn apply_peak_field(
    entry: &mut PostChainPeakEntry,
    key: &str,
    val: &str,
    line_no: usize,
) -> Result<(), String> {
    match key {
        "upsampler" => entry.key.upsampler = parse_yaml_string(val)?,
        "f_eff_hz" => entry.key.f_eff_hz = parse_u32(val, line_no)?,
        "output_rate_hz" => entry.key.output_rate_hz = parse_u32(val, line_no)?,
        "deemphasis" => entry.key.deemphasis = parse_bool(val, line_no)?,
        "hf_noise_filter" => entry.key.hf_noise_filter = parse_bool(val, line_no)?,
        "rew_filter_hash" | "rew_filter_sha256" => entry.key.rew_filter_hash = parse_yaml_string(val)?,
        "peak_dbfs" => entry.peak_dbfs = parse_f64(val, line_no)?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema1_migrate_parser_handles_valid_entries_and_rejects_unsafe_input() {
        let sample = "\
schema: 1
file:
  path: \"C:\\\\Music\\\\Album\\\\01 Track.flac\"
  size: 12345678
  mtime_unix_s: 1700000000
  streaminfo_md5: \"0123456789abcdef0123456789abcdef\"
  sample_rate: 44100
  bits: 16
scan:
  detector_version: 1
  scanned_at_unix_s: 1700000100
  duration_s: 245.500
results:
  effective_bandwidth_hz: 21400
  upsampled_from_1x: \"negative\"
  effective_nyquist_hz: null
  ultrasonic_noise_detected: \"negative\"
  lossy_origin_suspected: false
  apod_events: 12
  apodizing_needed: \"positive\"
  integrated_lufs: -12.345
  sample_peak: 0.999123
  true_peak_dbtp: 0.4123
  clipped_runs: 3
  loud_and_clipped: \"negative\"
  effective_bits: 16
  deemphasis_flag: false
  post_chain_peaks:
    - upsampler: \"gauss-apod-4m\"
      f_eff_hz: 22050
      output_rate_hz: 352800
      deemphasis: false
      hf_noise_filter: false
      rew_filter_hash: \"fnv1a128:deadbeef\"
      peak_dbfs: 4.250000
decision:
  filter: \"gauss-apod-4m\"
  rule: \"rule_A_apodizing\"
user:
  filter: \"minphase-long\"
  deemphasis: false
  apodizing: true
  note: \"user hand edit\"
";
        let parsed = TrackDbEntry::from_yaml_str(sample).expect("parse schema 1");
        assert_eq!(parsed.file.sample_rate, 44_100);
        assert_eq!(parsed.user.filter.as_deref(), Some("minphase-long"));
        assert_eq!(parsed.user.deemphasis, Some(false));
        assert_eq!(parsed.user.apodizing, Some(true));
        assert_eq!(parsed.results.post_chain_peaks.len(), 1);

        assert!(TrackDbEntry::from_yaml_str("schema: 2\n").is_err());
        assert!(TrackDbEntry::from_yaml_str("schema: 1\nuser:\n  filter: !ruby/object foo\n").is_err());
        assert!(TrackDbEntry::from_yaml_str("schema: 1\nuser:\n  filter: &anchor foo\n").is_err());
        assert!(TrackDbEntry::from_yaml_str("schema: 1\nuser:\n  filter: *alias\n").is_err());
        let huge = format!("schema: 1\nuser:\n  note: \"{}\"\n", "a".repeat(MAX_YAML_BYTES));
        assert!(TrackDbEntry::from_yaml_str(&huge).is_err());
    }
}
