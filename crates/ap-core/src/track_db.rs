//! Zero-dependency strict Schema-1 YAML Track Database (`FILTER_SELECTION_RESEARCH.md` §§7.9–7.10,
//! `HEADROOM_DESIGN.md` §4, `docs/research/07-ultimate-mode.md` U5b/U5c).
//!
//! Security & integrity guarantees:
//! - Treats every YAML file as untrusted user-editable input (`MAX_YAML_BYTES = 64 KiB`).
//! - Strictly rejects YAML tags (`!`), anchors (`&`), aliases (`*`), merge keys (`<<:`), and
//!   unsupported schema versions (`schema != 1`).
//! - Atomic persistence: write temp file -> `sync_all()` -> atomic rename (never half-written).
//! - Always preserves the `user:` section on rescan or learn-on-play update.
//! - Strictly per-track (`OQ-40`): lookups are keyed ONLY by this track's own `STREAMINFO.md5`
//!   or `(path, size, mtime)`. Never inspects sibling tracks or folder neighbors.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Hard maximum size for a Schema-1 YAML track entry (64 KiB).
pub const MAX_YAML_BYTES: usize = 64 * 1024;

/// Supported YAML schema version.
pub const TRACK_DB_SCHEMA_VERSION: u32 = 1;

/// Filesystem mtime tolerance (±2 s) for FAT/SMB/NTFS staleness checks when MD5 is unset.
pub const MTIME_TOLERANCE_SECS: u64 = 2;

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

/// Hand-editable user overrides — ALWAYS preserved across rescans and learn-on-play writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackDbUserOverrides {
    pub filter: Option<String>,
    pub deemphasis: Option<bool>,
    pub apodizing: Option<bool>,
    pub note: String,
}

/// Complete Schema-1 YAML track entry.
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

impl TrackDbEntry {
    /// True when `results` is valid for the current file identity and `detector_version`.
    pub fn is_results_current(
        &self,
        expected_detector_version: u32,
        cur_md5_hex: Option<&str>,
        cur_size: Option<u64>,
        cur_mtime_unix_s: Option<u64>,
    ) -> bool {
        if self.schema != TRACK_DB_SCHEMA_VERSION
            || self.scan.detector_version != expected_detector_version
        {
            return false;
        }
        if let (Some(stored_md5), Some(cur_md5)) = (self.file.streaminfo_md5.as_deref(), cur_md5_hex)
        {
            return stored_md5.eq_ignore_ascii_case(cur_md5);
        }
        if let (Some(sz), Some(mt)) = (cur_size, cur_mtime_unix_s) {
            return self.file.size == sz && self.file.mtime_unix_s.abs_diff(mt) <= MTIME_TOLERANCE_SECS;
        }
        false
    }

    /// Look up the measured unity-gain post-chain peak `M` (dBFS) matching `key`.
    pub fn find_post_chain_peak(&self, key: &PostChainKey) -> Option<f64> {
        self.results
            .post_chain_peaks
            .iter()
            .find(|e| &e.key == key && e.peak_dbfs.is_finite())
            .map(|e| e.peak_dbfs)
    }

    /// Monotonically insert or raise `peak_dbfs` for `key` (or set exact value when `monotonic = false`).
    pub fn upsert_post_chain_peak(&mut self, key: PostChainKey, peak_dbfs: f64, monotonic: bool) {
        if !peak_dbfs.is_finite() {
            return;
        }
        if let Some(existing) = self
            .results
            .post_chain_peaks
            .iter_mut()
            .find(|e| e.key == key)
        {
            if monotonic {
                existing.peak_dbfs = existing.peak_dbfs.max(peak_dbfs);
            } else {
                existing.peak_dbfs = peak_dbfs;
            }
        } else {
            self.results
                .post_chain_peaks
                .push(PostChainPeakEntry { key, peak_dbfs });
        }
    }

    /// Serialize cleanly into deterministic Schema-1 YAML.
    pub fn to_yaml_string(&self) -> String {
        let mut s = String::with_capacity(1024);
        s.push_str(&format!("schema: {}\n", self.schema));

        s.push_str("file:\n");
        s.push_str(&format!("  path: {}\n", yaml_quote(&self.file.path)));
        s.push_str(&format!("  size: {}\n", self.file.size));
        s.push_str(&format!("  mtime_unix_s: {}\n", self.file.mtime_unix_s));
        s.push_str(&format!(
            "  streaminfo_md5: {}\n",
            yaml_opt_str(self.file.streaminfo_md5.as_deref())
        ));
        s.push_str(&format!("  sample_rate: {}\n", self.file.sample_rate));
        s.push_str(&format!("  bits: {}\n", self.file.bits));

        s.push_str("scan:\n");
        s.push_str(&format!("  detector_version: {}\n", self.scan.detector_version));
        s.push_str(&format!("  scanned_at_unix_s: {}\n", self.scan.scanned_at_unix_s));
        s.push_str(&format!("  duration_s: {:.3}\n", self.scan.duration_s));

        s.push_str("results:\n");
        s.push_str(&format!(
            "  effective_bandwidth_hz: {}\n",
            self.results.effective_bandwidth_hz
        ));
        s.push_str(&format!(
            "  upsampled_from_1x: {}\n",
            yaml_quote(&self.results.upsampled_from_1x)
        ));
        s.push_str(&format!(
            "  effective_nyquist_hz: {}\n",
            yaml_opt_u32(self.results.effective_nyquist_hz)
        ));
        s.push_str(&format!(
            "  ultrasonic_noise_detected: {}\n",
            yaml_quote(&self.results.ultrasonic_noise_detected)
        ));
        s.push_str(&format!(
            "  lossy_origin_suspected: {}\n",
            self.results.lossy_origin_suspected
        ));
        s.push_str(&format!("  apod_events: {}\n", self.results.apod_events));
        s.push_str(&format!(
            "  apodizing_needed: {}\n",
            yaml_quote(&self.results.apodizing_needed)
        ));
        s.push_str(&format!(
            "  integrated_lufs: {}\n",
            yaml_opt_f64(self.results.integrated_lufs, 3)
        ));
        s.push_str(&format!("  sample_peak: {:.6}\n", self.results.sample_peak));
        s.push_str(&format!(
            "  true_peak_dbtp: {}\n",
            yaml_opt_f64(self.results.true_peak_dbtp, 4)
        ));
        s.push_str(&format!("  clipped_runs: {}\n", self.results.clipped_runs));
        s.push_str(&format!(
            "  loud_and_clipped: {}\n",
            yaml_quote(&self.results.loud_and_clipped)
        ));
        s.push_str(&format!("  effective_bits: {}\n", self.results.effective_bits));
        s.push_str(&format!(
            "  deemphasis_flag: {}\n",
            self.results.deemphasis_flag
        ));
        if self.results.post_chain_peaks.is_empty() {
            s.push_str("  post_chain_peaks: []\n");
        } else {
            s.push_str("  post_chain_peaks:\n");
            for p in &self.results.post_chain_peaks {
                s.push_str(&format!(
                    "    - upsampler: {}\n",
                    yaml_quote(&p.key.upsampler)
                ));
                s.push_str(&format!("      f_eff_hz: {}\n", p.key.f_eff_hz));
                s.push_str(&format!("      output_rate_hz: {}\n", p.key.output_rate_hz));
                s.push_str(&format!("      deemphasis: {}\n", p.key.deemphasis));
                s.push_str(&format!(
                    "      hf_noise_filter: {}\n",
                    p.key.hf_noise_filter
                ));
                s.push_str(&format!(
                    "      rew_filter_hash: {}\n",
                    yaml_quote(&p.key.rew_filter_hash)
                ));
                s.push_str(&format!("      peak_dbfs: {:.6}\n", p.peak_dbfs));
            }
        }

        s.push_str("decision:\n");
        s.push_str(&format!("  filter: {}\n", yaml_quote(&self.decision.filter)));
        s.push_str(&format!("  rule: {}\n", yaml_quote(&self.decision.rule)));

        s.push_str("user:\n");
        s.push_str(&format!(
            "  filter: {}\n",
            yaml_opt_str(self.user.filter.as_deref())
        ));
        s.push_str(&format!(
            "  deemphasis: {}\n",
            yaml_opt_bool(self.user.deemphasis)
        ));
        s.push_str(&format!(
            "  apodizing: {}\n",
            yaml_opt_bool(self.user.apodizing)
        ));
        s.push_str(&format!("  note: {}\n", yaml_quote(&self.user.note)));

        s
    }

    /// Parse a strict untrusted Schema-1 YAML string.
    /// Rejects oversized inputs (`> 64 KiB`), YAML tags (`!`), anchors (`&`), aliases (`*`),
    /// merge keys (`<<:`), and unsupported schema versions.
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
            // Reject unsafe YAML directives/tags/anchors/aliases/merge keys anywhere outside quotes.
            if trimmed.starts_with('%')
                || trimmed.starts_with("<<:")
                || contains_unquoted_yaml_special(trimmed)
            {
                return Err(format!(
                    "trackdb YAML line {line_no}: unsafe YAML tags, anchors, aliases, or directives are forbidden"
                ));
            }

            let indent = raw_line.len() - raw_line.trim_start_matches(' ').len();
            if raw_line.starts_with('\t') {
                return Err(format!("trackdb YAML line {line_no}: tabs are forbidden in YAML indentation"));
            }

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

            // Leaving peaks list if indentation returned to level 2.
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
                ("file", "streaminfo_md5") => entry.file.streaminfo_md5 = parse_opt_string(val)?,
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

                ("user", "filter") => entry.user.filter = parse_opt_string(val)?,
                ("user", "deemphasis") => entry.user.deemphasis = parse_opt_bool(val, line_no)?,
                ("user", "apodizing") => entry.user.apodizing = parse_opt_bool(val, line_no)?,
                ("user", "note") => entry.user.note = parse_yaml_string(val)?,
                _ => {
                    // Unknown keys inside valid sections are ignored gracefully per FSR §7.10.
                }
            }
        }

        flush_peak(&mut cur_peak, &mut entry.results.post_chain_peaks);
        if !saw_schema {
            return Err("trackdb YAML missing required 'schema: 1' header".to_string());
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

/// Persistent Schema-1 Track DB store (central directory + opt-in sidecars, strictly per-track).
#[derive(Debug, Clone)]
pub struct TrackDbStore {
    root_dir: PathBuf,
    sidecar_enabled: bool,
}

fn db_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

impl TrackDbStore {
    pub fn new(root_dir: impl Into<PathBuf>, sidecar_enabled: bool) -> Self {
        Self {
            root_dir: root_dir.into(),
            sidecar_enabled,
        }
    }

    /// Default per-user central trackdb folder (`%APPDATA%\audiophile-player\trackdb`).
    pub fn default_central_dir() -> PathBuf {
        if let Some(cfg) = crate::config::PlayerConfig::default_user_config_path() {
            if let Some(parent) = cfg.parent() {
                return parent.join("trackdb");
            }
        }
        PathBuf::from("trackdb")
    }

    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Sidecar path `<track.flac>.ap.yaml` next to a local music file.
    pub fn sidecar_path_for(track_path: &Path) -> PathBuf {
        let mut os = track_path.as_os_str().to_os_string();
        os.push(".ap.yaml");
        PathBuf::from(os)
    }

    fn central_path_for_md5(&self, md5_hex: &str) -> PathBuf {
        let clean: String = md5_hex
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let prefix = if clean.len() >= 2 { &clean[..2] } else { "00" };
        self.root_dir
            .join("md5")
            .join(prefix)
            .join(format!("{clean}.yaml"))
    }

    fn central_path_for_file(&self, track_path: &Path) -> PathBuf {
        let h = fnv1a_128_hex(track_path.to_string_lossy().as_bytes());
        self.root_dir
            .join("path")
            .join(&h[..2])
            .join(format!("{h}.yaml"))
    }

    /// Strictly per-track lookup order (`FILTER_SELECTION_RESEARCH.md` §7.9):
    /// 1. Sidecar `<track>.ap.yaml` (if `track_path` is local and sidecar exists).
    /// 2. Central DB by `track_path`.
    /// 3. Central DB by `STREAMINFO.md5` (if non-null). Note: MD5-matched entry from another path
    ///    clears `user:` so per-path user overrides never leak onto a different file!
    pub fn load_for_track(
        &self,
        track_path: Option<&Path>,
        md5_hex: Option<&str>,
    ) -> Option<TrackDbEntry> {
        if let Some(p) = track_path {
            let sidecar = Self::sidecar_path_for(p);
            if let Some(e) = read_entry_if_exists(&sidecar) {
                return Some(e);
            }
            let by_path = self.central_path_for_file(p);
            if let Some(e) = read_entry_if_exists(&by_path) {
                return Some(e);
            }
        }
        if let Some(md5) = md5_hex {
            let by_md5 = self.central_path_for_md5(md5);
            if let Some(mut e) = read_entry_if_exists(&by_md5) {
                // FSR §7.9: an MD5 match never carries `user` overrides across different paths.
                if track_path.is_some_and(|p| p.to_string_lossy() != e.file.path) {
                    e.user = TrackDbUserOverrides::default();
                }
                return Some(e);
            }
        }
        None
    }

    /// Atomically persist `entry`, preserving any existing hand-edited `user:` section on disk.
    pub fn save_preserving_user(
        &self,
        track_path: Option<&Path>,
        mut entry: TrackDbEntry,
    ) -> Result<PathBuf, String> {
        let _guard = db_write_lock().lock().unwrap_or_else(|e| e.into_inner());

        let target = if self.sidecar_enabled {
            if let Some(p) = track_path {
                Self::sidecar_path_for(p)
            } else if let Some(ref md5) = entry.file.streaminfo_md5 {
                self.central_path_for_md5(md5)
            } else {
                return Err("cannot determine trackdb save path (no path or MD5)".to_string());
            }
        } else if let Some(ref md5) = entry.file.streaminfo_md5 {
            self.central_path_for_md5(md5)
        } else if let Some(p) = track_path {
            self.central_path_for_file(p)
        } else {
            return Err("cannot determine trackdb save path (no path or MD5)".to_string());
        };

        // Preserve any hand-edited `user:` section already on disk.
        if let Some(existing) = read_entry_if_exists(&target) {
            if entry.user == TrackDbUserOverrides::default() {
                entry.user = existing.user;
            }
            // Also keep any existing post_chain_peaks for other DSP keys.
            for old_peak in existing.results.post_chain_peaks {
                if !entry
                    .results
                    .post_chain_peaks
                    .iter()
                    .any(|p| p.key == old_peak.key)
                {
                    entry.results.post_chain_peaks.push(old_peak);
                }
            }
        }

        atomic_write_yaml(&target, &entry.to_yaml_string())?;
        Ok(target)
    }
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn file_size_and_mtime(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((meta.len(), mtime))
}

fn read_entry_if_exists(path: &Path) -> Option<TrackDbEntry> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_YAML_BYTES as u64 {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    match TrackDbEntry::from_yaml_str(&text) {
        Ok(entry) => Some(entry),
        Err(err) => {
            eprintln!("  [trackdb warn] ignoring invalid YAML '{}': {err}", path.display());
            None
        }
    }
}

fn atomic_write_yaml(target: &Path, yaml: &str) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "failed to create trackdb directory '{}': {e}",
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
        // On Windows, fs::rename may fail if `target` already exists; replace cleanly.
        if target.exists() {
            let _ = fs::remove_file(target);
            if let Err(e2) = fs::rename(&tmp_path, target) {
                let _ = fs::remove_file(&tmp_path);
                return Err(format!(
                    "failed to atomically replace trackdb YAML '{}': {e2} (initial: {first_err})",
                    target.display()
                ));
            }
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

fn fnv1a_128_hex(bytes: &[u8]) -> String {
    const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= u128::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:032x}")
}

fn yaml_quote(s: &str) -> String {
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

fn yaml_opt_str(v: Option<&str>) -> String {
    match v {
        Some(s) => yaml_quote(s),
        None => "null".to_string(),
    }
}

fn yaml_opt_u32(v: Option<u32>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn yaml_opt_bool(v: Option<bool>) -> String {
    match v {
        Some(b) => b.to_string(),
        None => "null".to_string(),
    }
}

fn yaml_opt_f64(v: Option<f64>, prec: usize) -> String {
    match v {
        Some(x) if x.is_finite() => format!("{x:.prec$}"),
        _ => "null".to_string(),
    }
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let mut escape = false;
    for (i, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_quotes {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if ch == '#' && !in_quotes {
            return &line[..i];
        }
    }
    line
}

fn contains_unquoted_yaml_special(line: &str) -> bool {
    let mut in_quotes = false;
    let mut escape = false;
    let bytes = line.as_bytes();
    for (i, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_quotes {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_quotes = !in_quotes;
            continue;
        }
        if !in_quotes && matches!(ch, '!' | '&' | '*') {
            // Only flag when at start of value token (after space or colon or line start).
            if i == 0 || matches!(bytes[i - 1], b' ' | b':') {
                return true;
            }
        }
    }
    false
}

fn parse_yaml_string(val: &str) -> Result<String, String> {
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

fn parse_opt_string(val: &str) -> Result<Option<String>, String> {
    let t = val.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_yaml_string(t)?))
    }
}

fn parse_u32(val: &str, line_no: usize) -> Result<u32, String> {
    val.trim()
        .parse::<u32>()
        .map_err(|_| format!("trackdb YAML line {line_no}: invalid u32 '{val}'"))
}

fn parse_u64(val: &str, line_no: usize) -> Result<u64, String> {
    val.trim()
        .parse::<u64>()
        .map_err(|_| format!("trackdb YAML line {line_no}: invalid u64 '{val}'"))
}

fn parse_f64(val: &str, line_no: usize) -> Result<f64, String> {
    let v = val
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("trackdb YAML line {line_no}: invalid f64 '{val}'"))?;
    if !v.is_finite() {
        return Err(format!("trackdb YAML line {line_no}: non-finite float '{val}'"));
    }
    Ok(v)
}

fn parse_bool(val: &str, line_no: usize) -> Result<bool, String> {
    match val.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("trackdb YAML line {line_no}: invalid bool '{val}'")),
    }
}

fn parse_opt_u32(val: &str, line_no: usize) -> Result<Option<u32>, String> {
    let t = val.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_u32(t, line_no)?))
    }
}

fn parse_opt_f64(val: &str, line_no: usize) -> Result<Option<f64>, String> {
    let t = val.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_f64(t, line_no)?))
    }
}

fn parse_opt_bool(val: &str, line_no: usize) -> Result<Option<bool>, String> {
    let t = val.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") || t == "~" {
        Ok(None)
    } else {
        Ok(Some(parse_bool(t, line_no)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema1_yaml_roundtrips_preserves_user_section_and_rejects_unsafe_input() {
        let mut entry = TrackDbEntry::default();
        entry.file = TrackDbFileInfo {
            path: r"C:\Music\Album\01 Track.flac".to_string(),
            size: 31_457_280,
            mtime_unix_s: 1_764_000_000,
            streaminfo_md5: Some("0123456789abcdef0123456789abcdef".to_string()),
            sample_rate: 44_100,
            bits: 16,
        };
        entry.scan = TrackDbScanInfo {
            detector_version: 1,
            scanned_at_unix_s: 1_764_000_100,
            duration_s: 254.3,
        };
        entry.results.apod_events = 17;
        entry.results.apodizing_needed = "positive".to_string();
        entry.results.true_peak_dbtp = Some(0.85);
        entry.upsert_post_chain_peak(
            PostChainKey {
                upsampler: "gauss-apod-4m".to_string(),
                f_eff_hz: 22_050,
                output_rate_hz: 352_800,
                deemphasis: false,
                hf_noise_filter: false,
                rew_filter_hash: "fnv1a128:deadbeef".to_string(),
            },
            4.25,
            true,
        );
        entry.user = TrackDbUserOverrides {
            filter: Some("minphase-long".to_string()),
            deemphasis: Some(false),
            apodizing: None,
            note: "user hand edit".to_string(),
        };

        let yaml = entry.to_yaml_string();
        let parsed = TrackDbEntry::from_yaml_str(&yaml).expect("roundtrip parse");
        assert_eq!(parsed, entry);
        assert!(parsed.is_results_current(1, Some("0123456789abcdef0123456789abcdef"), None, None));
        assert!(!parsed.is_results_current(2, Some("0123456789abcdef0123456789abcdef"), None, None));

        // Reject YAML tags, anchors, aliases, unknown schema, and oversized payloads.
        assert!(TrackDbEntry::from_yaml_str("schema: 2\n").is_err());
        assert!(TrackDbEntry::from_yaml_str("schema: 1\nuser:\n  filter: !ruby/object foo\n").is_err());
        assert!(TrackDbEntry::from_yaml_str("schema: 1\nuser:\n  filter: &anchor foo\n").is_err());
        assert!(TrackDbEntry::from_yaml_str("schema: 1\nuser:\n  filter: *alias\n").is_err());
        let huge = format!("schema: 1\nuser:\n  note: \"{}\"\n", "a".repeat(MAX_YAML_BYTES));
        assert!(TrackDbEntry::from_yaml_str(&huge).is_err());

        // Atomic store roundtrip preserves `user:` section on rescan.
        let tmp_dir = std::env::temp_dir().join(format!("ap_trackdb_test_{}", now_unix_secs()));
        let store = TrackDbStore::new(&tmp_dir, false);
        let track_p = Path::new(r"C:\Music\Album\01 Track.flac");
        let saved = store.save_preserving_user(Some(track_p), entry.clone()).expect("save 1");
        assert!(saved.is_file());

        let mut rescan = entry.clone();
        rescan.results.apod_events = 22;
        rescan.user = TrackDbUserOverrides::default();
        store.save_preserving_user(Some(track_p), rescan).expect("save 2");

        let loaded = store
            .load_for_track(Some(track_p), Some("0123456789abcdef0123456789abcdef"))
            .expect("load after rescan");
        assert_eq!(loaded.results.apod_events, 22);
        assert_eq!(loaded.user.filter.as_deref(), Some("minphase-long"));
        assert_eq!(loaded.user.note, "user hand edit");
        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
