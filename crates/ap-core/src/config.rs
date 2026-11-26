// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Engine configuration schema & zero-dependency TOML config file loader.
//!
//! Configures **REW stereo convolution** (`[convolution]`), **quantizer / noise shaping**
//! (`[quantizer]`), the built-in **UPnP AV MediaRenderer** (`[upnp]`), opt-in **exclusive stream
//! capture** (`[capture]`), **Ultimate Mode** (`[ultimate]`), and the Schema-1 YAML **Track
//! Database** (`[trackdb]`).
//!
//! ## Config Discovery Order
//! 1. Explicit `--config <PATH>` (must exist and parse cleanly).
//! 2. `./audiophile-player.toml` (next to working directory, if present).
//! 3. `%APPDATA%\audiophile-player\config.toml` on Windows (or `$HOME/.config/audiophile-player/config.toml`).
//! 4. If no config file is found: safe default with convolution **disabled** (null pass-through),
//!    quantizer `dac_bits = 24` (`shaper = auto`), and UPnP enabled.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::track_db::atomic_write_yaml;
use crate::user_filters::{FilterMode, is_known_filter_name};

/// Standard local config filename (portable / current working directory).
pub const LOCAL_CONFIG_FILENAME: &str = "audiophile-player.toml";

/// Standard per-user subdirectory + filename under `%APPDATA%`.
pub const USER_CONFIG_DIR_NAME: &str = "audiophile-player";
pub const USER_CONFIG_FILENAME: &str = "config.toml";

/// Allowed explicit DAC quantization bit depths (`docs/research/04-noise-shapers.md` §6.5).
pub const ALLOWED_DAC_BITS: [u8; 4] = [16, 18, 20, 24];

/// Default DAC quantization bit depth (`24-bit` TPDF on 32-bit WDM-KS wire).
pub const DEFAULT_DAC_BITS: u8 = 24;

/// Returns `true` iff `bits` is one of the allowed DAC quantization bit depths (`16, 18, 20, 24`).
pub const fn is_valid_dac_bits(bits: u8) -> bool {
    matches!(bits, 16 | 18 | 20 | 24)
}

/// Allowed `[quantizer] shaper` string values (case-insensitive).
pub const ALLOWED_SHAPER_NAMES: [&str; 10] = [
    "auto",
    "bypass",
    "tpdf",
    "ns-gentle-2",
    "ns5",
    "ns5-class",
    "lns15",
    "lns15-class",
    "pggb-optimal",
    "wide40-9",
];

/// Returns `true` iff `name` (trimmed, ASCII case-insensitive) is a valid `[quantizer] shaper`.
pub fn is_valid_shaper_name(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    ALLOWED_SHAPER_NAMES.iter().any(|&a| a == lower)
}

/// Strictly mixer-free Windows output transport selection.
/// Shared Mode is intentionally unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTransport {
    /// Direct Windows Kernel Streaming (`WDM-KS`, Audirvana-style WaveRT pin).
    WdmKs,
    /// Event-driven WASAPI Exclusive (`AUDCLNT_SHAREMODE_EXCLUSIVE`).
    WasapiExclusive,
}

/// REW stereo convolution configuration loaded from `[convolution]` in `config.toml`
/// or overridden via CLI flags.
///
/// Resolution order for an active `8fS` sample rate (`352_800` or `384_000 Hz`):
/// 1. If `!enabled` -> disabled (`NullConvolver`).
/// 2. Explicit per-rate WAV (`file_352800` for `352_800 Hz`, `file_384000` for `384_000 Hz`).
/// 3. Explicit single WAV (`file`, used when its sample rate matches; otherwise falls through).
/// 4. Auto-discovery directory (`dir`: newest 2-channel `.wav` by modification date matching rate).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConvolutionConfig {
    pub enabled: bool,
    pub dir: Option<PathBuf>,
    pub file: Option<PathBuf>,
    pub file_352800: Option<PathBuf>,
    pub file_384000: Option<PathBuf>,
}

impl ConvolutionConfig {
    /// Safe default: convolution disabled until explicitly configured by the user.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Enable folder auto-discovery (newest rate-matched 2-channel `.wav` by modification date).
    pub fn auto_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            enabled: true,
            dir: Some(dir.into()),
            file: None,
            file_352800: None,
            file_384000: None,
        }
    }

    /// Enable one explicit stereo WAV file.
    pub fn explicit_file(path: impl Into<PathBuf>) -> Self {
        Self {
            enabled: true,
            dir: None,
            file: Some(path.into()),
            file_352800: None,
            file_384000: None,
        }
    }

    /// Returns true if at least one directory or explicit WAV path is configured.
    pub fn has_any_source(&self) -> bool {
        self.dir.is_some()
            || self.file.is_some()
            || self.file_352800.is_some()
            || self.file_384000.is_some()
    }

    /// Validate logical consistency (e.g., `enabled = true` must specify a dir or file).
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && !self.has_any_source() {
            return Err(
                "[convolution] has enabled = true, but none of 'dir', 'file', 'file_352800', or 'file_384000' is set"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Validate that configured paths exist on disk when `enabled = true`.
    ///
    /// Called at runtime startup so mistyped `dir` or `file` paths fail loudly instead of
    /// silently falling back to `NullConvolver`.
    pub fn validate_paths(&self) -> Result<(), String> {
        self.validate()?;
        if !self.enabled {
            if self.has_any_source() {
                eprintln!(
                    "Notice: [convolution] has enabled = false while filter sources are configured; \
                     REW convolution is bypassed. Remove `enabled = false` or set `enabled = true` to enable."
                );
            }
            return Ok(());
        }
        if let Some(ref dir) = self.dir {
            if !dir.is_dir() {
                return Err(format!(
                    "[convolution] configured 'dir' does not exist or is not a directory: '{}'",
                    dir.display()
                ));
            }
        }
        for (label, opt) in [
            ("file", &self.file),
            ("file_352800", &self.file_352800),
            ("file_384000", &self.file_384000),
        ] {
            if let Some(path) = opt {
                if !path.is_file() {
                    return Err(format!(
                        "[convolution] configured '{label}' does not exist or is not a file: '{}'",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    }
}

/// UPnP AV MediaRenderer configuration loaded from `[upnp]` in `config.toml` or CLI overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpnpConfig {
    pub enabled: bool,
    pub friendly_name: String,
    pub port: Option<u16>,
    pub bind_ip: Option<String>,
}

impl Default for UpnpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            friendly_name: "Audiophile Player".to_string(),
            port: None,
            bind_ip: None,
        }
    }
}

/// Quantizer & `8fS` noise-shaper configuration loaded from `[quantizer]` in `config.toml`
/// or overridden via `--dac-bits` / `--shaper` CLI flags (`docs/research/04-noise-shapers.md` §6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantizerConfig {
    /// Target DAC quantization bit depth (`16`, `18`, `20`, or `24`; default `24`).
    pub dac_bits: u8,
    /// Optional explicit shaper profile override (`None` = `"auto"` -> `ns-gentle-2` at 8fS,
    /// `bypass` for untouched native 32-bit 8fS, `tpdf` at non-8fS).
    pub shaper: Option<String>,
}

impl Default for QuantizerConfig {
    fn default() -> Self {
        Self {
            dac_bits: DEFAULT_DAC_BITS,
            shaper: None,
        }
    }
}

impl QuantizerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_dac_bits(self.dac_bits) {
            return Err(format!(
                "[quantizer] invalid 'dac_bits' ({}); expected one of 16, 18, 20, or 24",
                self.dac_bits
            ));
        }
        if let Some(ref shaper) = self.shaper {
            if !is_valid_shaper_name(shaper) {
                return Err(format!(
                    "[quantizer] unknown 'shaper' '{shaper}'; expected auto, bypass, tpdf, \
                     ns-gentle-2, ns5 (alias ns5-class), lns15 (alias lns15-class), \
                     or pggb-optimal (alias wide40-9)"
                ));
            }
        }
        Ok(())
    }
}

/// Ultimate Mode configuration loaded from `[ultimate]` in `config.toml` or CLI `--ultimate`
/// (`docs/research/07-ultimate-mode.md` §6, `OQ-35`, `OQ-36`, `OQ-40`).
///
/// Notes:
/// - The 30 s look-ahead window (`ULTIMATE_LOOKAHEAD_SECONDS = 30`) is a hard constant, not a setting.
/// - Runtime execution is pure `f64` (`OQ-36`; U8 one-time double-double kernel synthesis is always on).
/// - Strict per-track independence (`OQ-40`): zero album/folder context or sibling priors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UltimateConfig {
    /// Master switch (`false` by default — normal mode stays 100% unchanged).
    pub enabled: bool,
    /// Rule G genre-aware filter defaults (`true` by default in ultimate mode, `OQ-35`).
    pub genre_rules: bool,
    /// Persist verified whole-track analysis + exact post-chain peak `M` to YAML on clean EOF (`U5b`).
    pub learn_on_play: bool,
}

impl Default for UltimateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            genre_rules: true,
            learn_on_play: true,
        }
    }
}

/// Opt-in exclusive kernel stream capture configuration (`\\.\ApExclusiveAudio`, e.g. Tidal Desktop
/// Exclusive Mode) loaded from `[capture]` in `config.toml` or overridden via `--no-capture`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Watch for Audiophile Player's opt-in ACX virtual render driver (`true` by default; quietly
    /// parks with zero audio impact when the driver is not installed).
    pub enabled: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Persistent Schema-1 YAML Track Database configuration loaded from `[trackdb]` in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackDbConfig {
    /// Central trackdb directory (`None` -> default `%APPDATA%\audiophile-player\trackdb`).
    pub dir: Option<PathBuf>,
    /// Opt-in sidecar persistence (`<track.flac>.ap.yaml` next to local music files; default `false`).
    pub sidecar: bool,
}

/// Schema-2 Filter Database & User Choices configuration loaded from `[filters]` and
/// `[filters.roots]` in `config.toml` (`FILTER_DATABASE_RESEARCH.md` §§4–5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FiltersConfig {
    /// Filter resolution mode: `Auto` (default: `mine` when `my-filters.yaml` exists, else
    /// `suggested`), `Suggested` (analyser suggestions only), or `Mine` (user choices + fallback).
    pub mode: FilterMode,
    /// Machine-managed Schema-2 database directory (`None` -> `%APPDATA%\audiophile-player\filterdb`).
    pub dir: Option<PathBuf>,
    /// User-owned filter choices file (`None` -> `%APPDATA%\audiophile-player\my-filters.yaml`).
    pub choices_file: Option<PathBuf>,
    /// Named library roots (e.g. `main = 'D:\Music'`, `classical = 'E:\Classical'`).
    pub roots: BTreeMap<String, PathBuf>,
}

/// Allowed `[player] opt` priority tier names.
pub const ALLOWED_OPT_LEVELS: [&str; 3] = ["normal", "high", "extreme"];

/// Default/saved player runtime preferences loaded from `[player]` in `config.toml`
/// (`device`, `music_dir`, `filter`, `max_taps`, `port`, `opt`). CLI flags always override these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerSectionConfig {
    /// Preferred WDM-KS DAC name substring (e.g. `"Gungnir 2"`).
    pub device: Option<String>,
    /// Default local music library folder (`None` -> `~/Music`).
    pub music_dir: Option<PathBuf>,
    /// Optional forced upsampling filter (`None` = `"auto"`).
    pub filter: Option<String>,
    /// Optional upsampling tap ceiling (`1..=4_000_000`; `None` = `4_000_000`).
    pub max_taps: Option<usize>,
    /// Optional DAC output sample-rate ceiling in Hz (`44100|48000|88200|96000|176400|192000|352800|384000`;
    /// `None` = auto-probe highest supported rate on the DAC).
    pub max_rate: Option<u32>,
    /// Optional local HTTP control UI port (`1..=65535`; `None` = auto `8085..=8199`).
    pub port: Option<u16>,
    /// Optional KS priority tier (`normal | high | extreme`).
    pub opt: Option<String>,
}

impl PlayerSectionConfig {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(ref f) = self.filter {
            if !f.eq_ignore_ascii_case("auto") && !is_known_filter_name(f) {
                return Err(format!(
                    "[player] unknown 'filter' '{f}'; expected auto or a known filter name"
                ));
            }
        }
        if let Some(t) = self.max_taps {
            if t == 0 || t > 4_000_000 {
                return Err(format!(
                    "[player] invalid 'max_taps' ({t}); expected 1..=4000000"
                ));
            }
        }
        if let Some(r) = self.max_rate {
            if !crate::format::is_valid_max_rate(r) {
                return Err(format!(
                    "[player] invalid 'max_rate' ({r}); expected one of 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000"
                ));
            }
        }
        if let Some(p) = self.port {
            if p == 0 {
                return Err("[player] 'port' must be in 1..=65535".to_string());
            }
        }
        if let Some(ref o) = self.opt {
            if !ALLOWED_OPT_LEVELS.iter().any(|a| a.eq_ignore_ascii_case(o)) {
                return Err(format!(
                    "[player] invalid 'opt' '{o}'; expected normal, high, or extreme"
                ));
            }
        }
        Ok(())
    }
}

/// Top-level user configuration file (`config.toml` / `audiophile-player.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerConfig {
    pub player: PlayerSectionConfig,
    pub convolution: ConvolutionConfig,
    pub quantizer: QuantizerConfig,
    pub upnp: UpnpConfig,
    pub capture: CaptureConfig,
    pub ultimate: UltimateConfig,
    pub trackdb: TrackDbConfig,
    pub filters: FiltersConfig,
}

/// Canonical dotted TOML keys supported by `PlayerConfig` (verified against `cli_spec.rs` in tests).
pub const ALL_KNOWN_CONFIG_KEYS: &[&str] = &[
    "player.device",
    "player.music_dir",
    "player.filter",
    "player.max_taps",
    "player.max_rate",
    "player.port",
    "player.opt",
    "convolution.enabled",
    "convolution.dir",
    "convolution.file",
    "convolution.file_352800",
    "convolution.file_384000",
    "quantizer.dac_bits",
    "quantizer.shaper",
    "upnp.enabled",
    "upnp.friendly_name",
    "upnp.port",
    "upnp.bind_ip",
    "capture.enabled",
    "ultimate.enabled",
    "ultimate.genre_rules",
    "ultimate.learn_on_play",
    "trackdb.dir",
    "trackdb.sidecar",
    "filters.mode",
    "filters.dir",
    "filters.choices_file",
];

/// Map a canonical dotted key or convenient shorthand (e.g. `device`, `music_dir`, `filter`)
/// to its canonical `section.key` name.
pub fn canonicalize_config_key(raw: &str) -> Option<&'static str> {
    let lower = raw.trim().to_ascii_lowercase();
    for &k in ALL_KNOWN_CONFIG_KEYS {
        if k == lower {
            return Some(k);
        }
    }
    match lower.as_str() {
        "device" => Some("player.device"),
        "music_dir" | "music-dir" => Some("player.music_dir"),
        "filter" => Some("player.filter"),
        "max_taps" | "max-taps" => Some("player.max_taps"),
        "max_rate" | "max-rate" => Some("player.max_rate"),
        "port" => Some("player.port"),
        "opt" => Some("player.opt"),
        "dac_bits" | "dac-bits" => Some("quantizer.dac_bits"),
        "shaper" => Some("quantizer.shaper"),
        "ultimate" => Some("ultimate.enabled"),
        "genre_rules" | "genre-rules" => Some("ultimate.genre_rules"),
        "learn_on_play" | "learn-on-play" => Some("ultimate.learn_on_play"),
        "filters" => Some("filters.mode"),
        _ => None,
    }
}

/// Result of config file discovery + loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPlayerConfig {
    pub config: PlayerConfig,
    /// Resolved path of the loaded config file, or `None` if no config file existed (defaults used).
    pub source_path: Option<PathBuf>,
}

impl PlayerConfig {
    /// Default per-user Windows config path (`%APPDATA%\audiophile-player\config.toml`).
    pub fn default_user_config_path() -> Option<PathBuf> {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let trimmed = appdata.trim();
            if !trimmed.is_empty() {
                return Some(
                    PathBuf::from(trimmed)
                        .join(USER_CONFIG_DIR_NAME)
                        .join(USER_CONFIG_FILENAME),
                );
            }
        }
        if let Ok(profile) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            let trimmed = profile.trim();
            if !trimmed.is_empty() {
                return Some(
                    PathBuf::from(trimmed)
                        .join(".config")
                        .join(USER_CONFIG_DIR_NAME)
                        .join(USER_CONFIG_FILENAME),
                );
            }
        }
        None
    }

    /// Parse a strict TOML config string (`[convolution]`, `[quantizer]`, `[upnp]`, `[capture]`,
    /// `[ultimate]`, and `[trackdb]` sections).
    pub fn from_toml_str(input: &str) -> Result<Self, String> {
        // Strip optional UTF-8 BOM (Windows editors such as Notepad may emit it).
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Section {
            None,
            Player,
            Convolution,
            Quantizer,
            Upnp,
            Capture,
            Ultimate,
            TrackDb,
            Filters,
            FiltersRoots,
        }

        let mut active_section = Section::None;
        let mut seen_player_section = false;
        let mut seen_convolution_section = false;
        let mut seen_quantizer_section = false;
        let mut seen_upnp_section = false;
        let mut seen_capture_section = false;
        let mut seen_ultimate_section = false;
        let mut seen_trackdb_section = false;
        let mut seen_filters_section = false;
        let mut seen_filters_roots_section = false;
        let mut seen_player_keys: HashSet<&str> = HashSet::new();
        let mut seen_conv_keys: HashSet<&str> = HashSet::new();
        let mut seen_quant_keys: HashSet<&str> = HashSet::new();
        let mut seen_upnp_keys: HashSet<&str> = HashSet::new();
        let mut seen_capture_keys: HashSet<&str> = HashSet::new();
        let mut seen_ultimate_keys: HashSet<&str> = HashSet::new();
        let mut seen_trackdb_keys: HashSet<&str> = HashSet::new();
        let mut seen_filters_keys: HashSet<String> = HashSet::new();
        let mut seen_filter_root_names: HashSet<String> = HashSet::new();
        let mut explicit_conv_enabled: Option<bool> = None;
        let mut player = PlayerSectionConfig::default();
        let mut conv = ConvolutionConfig::default();
        let mut quantizer = QuantizerConfig::default();
        let mut upnp = UpnpConfig::default();
        let mut capture = CaptureConfig::default();
        let mut ultimate = UltimateConfig::default();
        let mut trackdb = TrackDbConfig::default();
        let mut filters = FiltersConfig::default();

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let stripped = strip_toml_comment(raw_line).trim();
            if stripped.is_empty() {
                continue;
            }

            if stripped.starts_with('[') {
                if !stripped.ends_with(']') {
                    return Err(format!(
                        "config line {line_no}: unclosed section header '{stripped}'"
                    ));
                }
                let section = stripped[1..stripped.len() - 1].trim();
                match section {
                    "player" => {
                        if seen_player_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[player]'"
                            ));
                        }
                        seen_player_section = true;
                        active_section = Section::Player;
                    }
                    "convolution" => {
                        if seen_convolution_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[convolution]'"
                            ));
                        }
                        seen_convolution_section = true;
                        active_section = Section::Convolution;
                    }
                    "quantizer" => {
                        if seen_quantizer_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[quantizer]'"
                            ));
                        }
                        seen_quantizer_section = true;
                        active_section = Section::Quantizer;
                    }
                    "upnp" => {
                        if seen_upnp_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[upnp]'"
                            ));
                        }
                        seen_upnp_section = true;
                        active_section = Section::Upnp;
                    }
                    "capture" => {
                        if seen_capture_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[capture]'"
                            ));
                        }
                        seen_capture_section = true;
                        active_section = Section::Capture;
                    }
                    "ultimate" => {
                        if seen_ultimate_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[ultimate]'"
                            ));
                        }
                        seen_ultimate_section = true;
                        active_section = Section::Ultimate;
                    }
                    "trackdb" => {
                        if seen_trackdb_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[trackdb]'"
                            ));
                        }
                        seen_trackdb_section = true;
                        active_section = Section::TrackDb;
                    }
                    "filters" => {
                        if seen_filters_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[filters]'"
                            ));
                        }
                        seen_filters_section = true;
                        active_section = Section::Filters;
                    }
                    "filters.roots" => {
                        if seen_filters_roots_section {
                            return Err(format!(
                                "config line {line_no}: duplicate section '[filters.roots]'"
                            ));
                        }
                        seen_filters_roots_section = true;
                        active_section = Section::FiltersRoots;
                    }
                    other => {
                        return Err(format!(
                            "config line {line_no}: unknown section '[{other}]' (supported: [convolution], [quantizer], [upnp], [capture], [ultimate], [trackdb], [filters], [filters.roots])"
                        ));
                    }
                }
                continue;
            }

            if active_section == Section::None {
                return Err(format!(
                    "config line {line_no}: key-value entry outside a section (expected [convolution], [quantizer], [upnp], [capture], [ultimate], or [trackdb])"
                ));
            }

            let Some((raw_key, raw_val)) = stripped.split_once('=') else {
                return Err(format!(
                    "config line {line_no}: expected 'key = value', got '{stripped}'"
                ));
            };
            let key = raw_key.trim();
            let val = raw_val.trim();

            match active_section {
                Section::None => unreachable!(),
                Section::Player => {
                    if !seen_player_keys.insert(key) {
                        return Err(format!(
                            "config line {line_no}: duplicate key '{key}' in [player]"
                        ));
                    }
                    match key {
                        "device" => {
                            let s = parse_toml_string(val, line_no, "device")?;
                            let trimmed = s.trim();
                            player.device = (!trimmed.is_empty()).then(|| trimmed.to_string());
                        }
                        "music_dir" => {
                            let s = parse_toml_string(val, line_no, "music_dir")?;
                            player.music_dir = non_empty_path(s);
                        }
                        "filter" => {
                            let s = parse_toml_string(val, line_no, "filter")?;
                            let trimmed = s.trim();
                            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
                                player.filter = None;
                            } else if is_known_filter_name(trimmed) {
                                player.filter = Some(trimmed.to_ascii_lowercase());
                            } else {
                                return Err(format!(
                                    "config line {line_no}: unknown 'filter' '{trimmed}' in [player]"
                                ));
                            }
                        }
                        "max_taps" => {
                            let taps = val.parse::<usize>().map_err(|_| {
                                format!(
                                    "config line {line_no}: invalid integer for 'max_taps' in [player]: '{val}'"
                                )
                            })?;
                            if taps == 0 || taps > 4_000_000 {
                                return Err(format!(
                                    "config line {line_no}: 'max_taps' in [player] must be in 1..=4000000"
                                ));
                            }
                            player.max_taps = Some(taps);
                        }
                        "max_rate" => {
                            if val.starts_with('"') || val.starts_with('\'') {
                                let s = parse_toml_string(val, line_no, "max_rate")?;
                                if s.trim().is_empty() || s.trim().eq_ignore_ascii_case("auto") {
                                    player.max_rate = None;
                                } else {
                                    let rate = s.trim().parse::<u32>().map_err(|_| {
                                        format!(
                                            "config line {line_no}: invalid 'max_rate' in [player]: '{s}'"
                                        )
                                    })?;
                                    if !crate::format::is_valid_max_rate(rate) {
                                        return Err(format!(
                                            "config line {line_no}: unsupported 'max_rate' ({rate}) in [player]"
                                        ));
                                    }
                                    player.max_rate = Some(rate);
                                }
                            } else {
                                let rate = val.parse::<u32>().map_err(|_| {
                                    format!(
                                        "config line {line_no}: invalid integer for 'max_rate' in [player]: '{val}'"
                                    )
                                })?;
                                if !crate::format::is_valid_max_rate(rate) {
                                    return Err(format!(
                                        "config line {line_no}: unsupported 'max_rate' ({rate}) in [player] (expected 44100|48000|88200|96000|176400|192000|352800|384000)"
                                    ));
                                }
                                player.max_rate = Some(rate);
                            }
                        }
                        "port" => {
                            let port = val.parse::<u16>().map_err(|_| {
                                format!(
                                    "config line {line_no}: invalid integer for 'port' in [player]: '{val}'"
                                )
                            })?;
                            if port == 0 {
                                return Err(format!(
                                    "config line {line_no}: 'port' in [player] must be in 1..=65535"
                                ));
                            }
                            player.port = Some(port);
                        }
                        "opt" => {
                            let s = parse_toml_string(val, line_no, "opt")?;
                            let trimmed = s.trim().to_ascii_lowercase();
                            if trimmed.is_empty() {
                                player.opt = None;
                            } else if ALLOWED_OPT_LEVELS.contains(&trimmed.as_str()) {
                                player.opt = Some(trimmed);
                            } else {
                                return Err(format!(
                                    "config line {line_no}: invalid 'opt' '{trimmed}' in [player] (expected normal, high, or extreme)"
                                ));
                            }
                        }
                        other => {
                            return Err(format!(
                                "config line {line_no}: unknown key '{other}' in [player] (allowed: device, music_dir, filter, max_taps, port, opt)"
                            ));
                        }
                    }
                }
                Section::Convolution => {
                    if !seen_conv_keys.insert(key) {
                        return Err(format!(
                            "config line {line_no}: duplicate key '{key}' in [convolution]"
                        ));
                    }
                    match key {
                        "enabled" => {
                            let b = parse_toml_bool(val, line_no, "enabled")?;
                            explicit_conv_enabled = Some(b);
                        }
                        "dir" => {
                            let s = parse_toml_string(val, line_no, "dir")?;
                            conv.dir = non_empty_path(s);
                        }
                        "file" => {
                            let s = parse_toml_string(val, line_no, "file")?;
                            conv.file = non_empty_path(s);
                        }
                        "file_352800" => {
                            let s = parse_toml_string(val, line_no, "file_352800")?;
                            conv.file_352800 = non_empty_path(s);
                        }
                        "file_384000" => {
                            let s = parse_toml_string(val, line_no, "file_384000")?;
                            conv.file_384000 = non_empty_path(s);
                        }
                        other => {
                            return Err(format!(
                                "config line {line_no}: unknown key '{other}' in [convolution] (allowed: enabled, dir, file, file_352800, file_384000)"
                            ));
                        }
                    }
                }
                Section::Quantizer => {
                    if !seen_quant_keys.insert(key) {
                        return Err(format!(
                            "config line {line_no}: duplicate key '{key}' in [quantizer]"
                        ));
                    }
                    match key {
                        "dac_bits" => {
                            let bits = val.parse::<u8>().map_err(|_| {
                                format!(
                                    "config line {line_no}: invalid integer for 'dac_bits' in [quantizer]: '{val}' (expected 16, 18, 20, or 24)"
                                )
                            })?;
                            if !is_valid_dac_bits(bits) {
                                return Err(format!(
                                    "config line {line_no}: invalid 'dac_bits' ({bits}) in [quantizer]; expected 16, 18, 20, or 24"
                                ));
                            }
                            quantizer.dac_bits = bits;
                        }
                        "shaper" => {
                            let s = parse_toml_string(val, line_no, "shaper")?;
                            let trimmed = s.trim();
                            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
                                quantizer.shaper = None;
                            } else if is_valid_shaper_name(trimmed) {
                                quantizer.shaper = Some(trimmed.to_ascii_lowercase());
                            } else {
                                return Err(format!(
                                    "config line {line_no}: unknown 'shaper' '{trimmed}' in [quantizer] \
                                     (allowed: auto, bypass, tpdf, ns-gentle-2, ns5, lns15, pggb-optimal)"
                                ));
                            }
                        }
                        other => {
                            return Err(format!(
                                "config line {line_no}: unknown key '{other}' in [quantizer] (allowed: dac_bits, shaper)"
                            ));
                        }
                    }
                }
                Section::Upnp => {
                    if !seen_upnp_keys.insert(key) {
                        return Err(format!(
                            "config line {line_no}: duplicate key '{key}' in [upnp]"
                        ));
                    }
                    match key {
                        "enabled" => {
                            upnp.enabled = parse_toml_bool(val, line_no, "enabled")?;
                        }
                        "friendly_name" => {
                            let s = parse_toml_string(val, line_no, "friendly_name")?;
                            let trimmed = s.trim();
                            if trimmed.is_empty() {
                                return Err(format!(
                                    "config line {line_no}: 'friendly_name' in [upnp] cannot be empty"
                                ));
                            }
                            upnp.friendly_name = trimmed.to_string();
                        }
                        "port" => {
                            let port = val.parse::<u16>().map_err(|_| {
                                format!(
                                    "config line {line_no}: invalid integer for 'port' in [upnp]: '{val}'"
                                )
                            })?;
                            if port == 0 {
                                return Err(format!(
                                    "config line {line_no}: 'port' in [upnp] must be in 1..=65535"
                                ));
                            }
                            upnp.port = Some(port);
                        }
                        "bind_ip" => {
                            let s = parse_toml_string(val, line_no, "bind_ip")?;
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                trimmed.parse::<std::net::Ipv4Addr>().map_err(|_| {
                                    format!(
                                        "config line {line_no}: invalid IPv4 address for 'bind_ip' in [upnp]: '{trimmed}'"
                                    )
                                })?;
                                upnp.bind_ip = Some(trimmed.to_string());
                            }
                        }
                        other => {
                            return Err(format!(
                                "config line {line_no}: unknown key '{other}' in [upnp] (allowed: enabled, friendly_name, port, bind_ip)"
                            ));
                        }
                    }
                }
                Section::Capture => {
                    if !seen_capture_keys.insert(key) {
                        return Err(format!(
                            "config line {line_no}: duplicate key '{key}' in [capture]"
                        ));
                    }
                    match key {
                        "enabled" => {
                            capture.enabled = parse_toml_bool(val, line_no, "enabled")?;
                        }
                        other => {
                            return Err(format!(
                                "config line {line_no}: unknown key '{other}' in [capture] (allowed: enabled)"
                            ));
                        }
                    }
                }
                Section::Ultimate => {
                    if !seen_ultimate_keys.insert(key) {
                        return Err(format!(
                            "config line {line_no}: duplicate key '{key}' in [ultimate]"
                        ));
                    }
                    match key {
                        "enabled" => {
                            ultimate.enabled = parse_toml_bool(val, line_no, "enabled")?;
                        }
                        "genre_rules" => {
                            ultimate.genre_rules = parse_toml_bool(val, line_no, "genre_rules")?;
                        }
                        "learn_on_play" => {
                            ultimate.learn_on_play =
                                parse_toml_bool(val, line_no, "learn_on_play")?;
                        }
                        other => {
                            return Err(format!(
                                "config line {line_no}: unknown key '{other}' in [ultimate] (allowed: enabled, genre_rules, learn_on_play)"
                            ));
                        }
                    }
                }
                Section::TrackDb => {
                    if !seen_trackdb_keys.insert(key) {
                        return Err(format!(
                            "config line {line_no}: duplicate key '{key}' in [trackdb]"
                        ));
                    }
                    match key {
                        "dir" => {
                            let s = parse_toml_string(val, line_no, "dir")?;
                            trackdb.dir = non_empty_path(s);
                        }
                        "sidecar" => {
                            trackdb.sidecar = parse_toml_bool(val, line_no, "sidecar")?;
                        }
                        other => {
                            return Err(format!(
                                "config line {line_no}: unknown key '{other}' in [trackdb] (allowed: dir, sidecar)"
                            ));
                        }
                    }
                }
                Section::Filters => {
                    if let Some(root_name_raw) = key.strip_prefix("root.") {
                        let root_name = root_name_raw.trim();
                        if root_name.is_empty()
                            || !root_name
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                        {
                            return Err(format!(
                                "config line {line_no}: invalid root name '{root_name}' in [filters]"
                            ));
                        }
                        let norm = root_name.to_ascii_lowercase();
                        if !seen_filter_root_names.insert(norm) {
                            return Err(format!(
                                "config line {line_no}: duplicate filter root '{root_name}'"
                            ));
                        }
                        let s = parse_toml_string(val, line_no, key)?;
                        let Some(path) = non_empty_path(s) else {
                            return Err(format!(
                                "config line {line_no}: filter root '{root_name}' path cannot be empty"
                            ));
                        };
                        filters.roots.insert(root_name.to_string(), path);
                    } else {
                        if !seen_filters_keys.insert(key.to_string()) {
                            return Err(format!(
                                "config line {line_no}: duplicate key '{key}' in [filters]"
                            ));
                        }
                        match key {
                            "mode" => {
                                let s = parse_toml_string(val, line_no, "mode")?;
                                filters.mode = FilterMode::parse(&s).map_err(|e| {
                                    format!("config line {line_no}: [filters] {e}")
                                })?;
                            }
                            "dir" => {
                                let s = parse_toml_string(val, line_no, "dir")?;
                                filters.dir = non_empty_path(s);
                            }
                            "choices_file" => {
                                let s = parse_toml_string(val, line_no, "choices_file")?;
                                filters.choices_file = non_empty_path(s);
                            }
                            other => {
                                return Err(format!(
                                    "config line {line_no}: unknown key '{other}' in [filters] (allowed: mode, dir, choices_file, root.<name>)"
                                ));
                            }
                        }
                    }
                }
                Section::FiltersRoots => {
                    let root_name = key.trim();
                    if root_name.is_empty()
                        || !root_name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    {
                        return Err(format!(
                            "config line {line_no}: invalid root name '{root_name}' in [filters.roots]"
                        ));
                    }
                    let norm = root_name.to_ascii_lowercase();
                    if !seen_filter_root_names.insert(norm) {
                        return Err(format!(
                            "config line {line_no}: duplicate filter root '{root_name}'"
                        ));
                    }
                    let s = parse_toml_string(val, line_no, root_name)?;
                    let Some(path) = non_empty_path(s) else {
                        return Err(format!(
                            "config line {line_no}: filter root '{root_name}' path cannot be empty"
                        ));
                    };
                    filters.roots.insert(root_name.to_string(), path);
                }
            }
        }

        // If `enabled` was omitted in [convolution], enable automatically iff at least one filter source was set.
        conv.enabled = explicit_conv_enabled.unwrap_or_else(|| conv.has_any_source());
        player.validate()?;
        conv.validate()?;
        quantizer.validate()?;

        Ok(Self {
            player,
            convolution: conv,
            quantizer,
            upnp,
            capture,
            ultimate,
            trackdb,
            filters,
        })
    }

    /// Read and parse an explicit TOML config file.
    pub fn load_from_file(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("failed to read config file '{}': {e}", path.display()))?;
        Self::from_toml_str(&content)
            .map_err(|e| format!("invalid config file '{}': {e}", path.display()))
    }

    /// Discover and load config using the standard search order:
    /// 1. `explicit_path` (if `Some`, file MUST exist)
    /// 2. `./audiophile-player.toml` (if present in `cwd`)
    /// 3. `user_config_path` (`%APPDATA%\audiophile-player\config.toml`, if present)
    /// 4. Otherwise default config (convolution disabled).
    pub fn discover_and_load_with_paths(
        explicit_path: Option<&Path>,
        local_path: Option<&Path>,
        user_path: Option<&Path>,
    ) -> Result<LoadedPlayerConfig, String> {
        if let Some(path) = explicit_path {
            let config = Self::load_from_file(path)?;
            return Ok(LoadedPlayerConfig {
                config,
                source_path: Some(path.to_path_buf()),
            });
        }

        if let Some(path) = local_path {
            if path.is_file() {
                let config = Self::load_from_file(path)?;
                return Ok(LoadedPlayerConfig {
                    config,
                    source_path: Some(path.to_path_buf()),
                });
            }
        }

        if let Some(path) = user_path {
            if path.is_file() {
                let config = Self::load_from_file(path)?;
                return Ok(LoadedPlayerConfig {
                    config,
                    source_path: Some(path.to_path_buf()),
                });
            }
        }

        Ok(LoadedPlayerConfig {
            config: Self::default(),
            source_path: None,
        })
    }

    /// Discover and load config using the live process working directory and `%APPDATA%`.
    pub fn discover_and_load(explicit_path: Option<&Path>) -> Result<LoadedPlayerConfig, String> {
        let local_path = PathBuf::from(LOCAL_CONFIG_FILENAME);
        let user_path = Self::default_user_config_path();
        Self::discover_and_load_with_paths(
            explicit_path,
            Some(local_path.as_path()),
            user_path.as_deref(),
        )
    }
}

fn parse_toml_bool(val: &str, line_no: usize, key: &str) -> Result<bool, String> {
    match val {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!(
            "config line {line_no}: invalid boolean for '{key}': '{other}' (expected true or false)"
        )),
    }
}

fn non_empty_path(s: String) -> Option<PathBuf> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Strip `# ...` comments while respecting single- and double-quoted TOML strings.
fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (byte_idx, ch) in line.char_indices() {
        if in_double {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_double = false;
            }
        } else if in_single {
            if ch == '\'' {
                in_single = false;
            }
        } else {
            match ch {
                '\'' => in_single = true,
                '"' => in_double = true,
                '#' => return &line[..byte_idx],
                _ => {}
            }
        }
    }
    line
}

/// Parse a TOML literal string (`'...'`, ideal for Windows paths) or basic string (`"..."`).
fn parse_toml_string(val: &str, line_no: usize, key: &str) -> Result<String, String> {
    if let Some(inner) = val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        if inner.contains('\'') {
            return Err(format!(
                "config line {line_no}: unexpected single quote inside literal string for '{key}'"
            ));
        }
        return Ok(inner.to_string());
    }

    if let Some(inner) = val.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                let esc = chars.next().ok_or_else(|| {
                    format!("config line {line_no}: trailing backslash in '{key}'")
                })?;
                match esc {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    other => {
                        return Err(format!(
                            "config line {line_no}: invalid escape '\\{other}' in '{key}' (hint: use single quotes 'C:\\path' for Windows paths)"
                        ));
                    }
                }
            } else if c == '"' {
                return Err(format!(
                    "config line {line_no}: unescaped quote inside string for '{key}'"
                ));
            } else {
                out.push(c);
            }
        }
        return Ok(out);
    }

    Err(format!(
        "config line {line_no}: expected quoted string for '{key}', got '{val}'"
    ))
}

impl PlayerConfig {
    /// Read the current effective string value of any canonical dotted key in [`ALL_KNOWN_CONFIG_KEYS`].
    pub fn get_value_str(&self, canonical_key: &str) -> Option<String> {
        match canonical_key {
            "player.device" => self.player.device.clone(),
            "player.music_dir" => self
                .player
                .music_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            "player.filter" => self.player.filter.clone(),
            "player.max_taps" => self.player.max_taps.map(|n| n.to_string()),
            "player.max_rate" => self.player.max_rate.map(|r| r.to_string()),
            "player.port" => self.player.port.map(|p| p.to_string()),
            "player.opt" => self.player.opt.clone(),
            "convolution.enabled" => Some(self.convolution.enabled.to_string()),
            "convolution.dir" => self
                .convolution
                .dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            "convolution.file" => self
                .convolution
                .file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            "convolution.file_352800" => self
                .convolution
                .file_352800
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            "convolution.file_384000" => self
                .convolution
                .file_384000
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            "quantizer.dac_bits" => Some(self.quantizer.dac_bits.to_string()),
            "quantizer.shaper" => Some(
                self.quantizer
                    .shaper
                    .clone()
                    .unwrap_or_else(|| "auto".to_string()),
            ),
            "upnp.enabled" => Some(self.upnp.enabled.to_string()),
            "upnp.friendly_name" => Some(self.upnp.friendly_name.clone()),
            "upnp.port" => self.upnp.port.map(|p| p.to_string()),
            "upnp.bind_ip" => self.upnp.bind_ip.clone(),
            "capture.enabled" => Some(self.capture.enabled.to_string()),
            "ultimate.enabled" => Some(self.ultimate.enabled.to_string()),
            "ultimate.genre_rules" => Some(self.ultimate.genre_rules.to_string()),
            "ultimate.learn_on_play" => Some(self.ultimate.learn_on_play.to_string()),
            "trackdb.dir" => self
                .trackdb
                .dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            "trackdb.sidecar" => Some(self.trackdb.sidecar.to_string()),
            "filters.mode" => Some(self.filters.mode.as_str().to_string()),
            "filters.dir" => self
                .filters
                .dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            "filters.choices_file" => self
                .filters
                .choices_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            _ => None,
        }
    }
}

/// Format a raw CLI/MCP string value as a valid TOML literal for `canonical_key`, validating type/range.
pub fn format_toml_value_for_key(canonical_key: &str, raw_value: &str) -> Result<String, String> {
    let val = raw_value.trim();
    match canonical_key {
        // Boolean keys
        "convolution.enabled"
        | "upnp.enabled"
        | "capture.enabled"
        | "ultimate.enabled"
        | "ultimate.genre_rules"
        | "ultimate.learn_on_play"
        | "trackdb.sidecar" => match val.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok("true".to_string()),
            "false" | "0" | "no" | "off" => Ok("false".to_string()),
            _ => Err(format!(
                "invalid boolean '{raw_value}' for '{canonical_key}' (expected true or false)"
            )),
        },
        // Integer keys
        "quantizer.dac_bits" => {
            let bits = val.parse::<u8>().map_err(|_| {
                format!("invalid dac_bits '{raw_value}' (expected 16, 18, 20, or 24)")
            })?;
            if !is_valid_dac_bits(bits) {
                return Err(format!(
                    "invalid dac_bits {bits} (expected 16, 18, 20, or 24)"
                ));
            }
            Ok(bits.to_string())
        }
        "player.max_taps" => {
            let taps = val
                .parse::<usize>()
                .map_err(|_| format!("invalid max_taps '{raw_value}' (expected 1..=4000000)"))?;
            if taps == 0 || taps > 4_000_000 {
                return Err(format!("max_taps must be in 1..=4000000, got {taps}"));
            }
            Ok(taps.to_string())
        }
        "player.max_rate" => {
            if val.eq_ignore_ascii_case("auto") {
                return Ok("\"auto\"".to_string());
            }
            let rate = val.parse::<u32>().map_err(|_| {
                format!(
                    "invalid max_rate '{raw_value}' (expected auto or one of 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000)"
                )
            })?;
            if !crate::format::is_valid_max_rate(rate) {
                return Err(format!(
                    "unsupported max_rate {rate} (expected auto or one of 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000)"
                ));
            }
            Ok(rate.to_string())
        }
        "player.port" | "upnp.port" => {
            let port = val
                .parse::<u16>()
                .map_err(|_| format!("invalid port '{raw_value}' (expected 1..=65535)"))?;
            if port == 0 {
                return Err("port must be in 1..=65535".to_string());
            }
            Ok(port.to_string())
        }
        // Validated enum strings
        "player.filter" => {
            if !val.eq_ignore_ascii_case("auto") && !is_known_filter_name(val) {
                return Err(format!(
                    "unknown filter '{val}'; expected auto or one of: {}",
                    crate::user_filters::KNOWN_FILTER_NAMES.join(", ")
                ));
            }
            Ok(toml_quote_string(&val.to_ascii_lowercase()))
        }
        "player.opt" => {
            let lower = val.to_ascii_lowercase();
            if !ALLOWED_OPT_LEVELS.contains(&lower.as_str()) {
                return Err(format!(
                    "invalid opt '{val}'; expected normal, high, or extreme"
                ));
            }
            Ok(toml_quote_string(&lower))
        }
        "quantizer.shaper" => {
            if !is_valid_shaper_name(val) {
                return Err(format!(
                    "unknown shaper '{val}'; expected one of: {}",
                    ALLOWED_SHAPER_NAMES.join(", ")
                ));
            }
            Ok(toml_quote_string(&val.to_ascii_lowercase()))
        }
        "filters.mode" => {
            let mode = FilterMode::parse(val)?;
            Ok(toml_quote_string(mode.as_str()))
        }
        "upnp.bind_ip" => {
            val.parse::<std::net::Ipv4Addr>()
                .map_err(|_| format!("invalid IPv4 address '{val}' for upnp.bind_ip"))?;
            Ok(toml_quote_string(val))
        }
        "upnp.friendly_name" => {
            if val.is_empty() {
                return Err("upnp.friendly_name cannot be empty".to_string());
            }
            Ok(toml_quote_string(val))
        }
        // General strings & filesystem paths
        "player.device"
        | "player.music_dir"
        | "convolution.dir"
        | "convolution.file"
        | "convolution.file_352800"
        | "convolution.file_384000"
        | "trackdb.dir"
        | "filters.dir"
        | "filters.choices_file" => Ok(toml_quote_string(val)),
        other => Err(format!("unknown config key '{other}'")),
    }
}

/// Quote a TOML string cleanly: prefer single-quoted literal `'...'` when it contains no single
/// quotes or control chars (ideal for Windows `C:\...` paths), otherwise double-quoted escaped.
pub fn toml_quote_string(s: &str) -> String {
    if !s.contains('\'') && !s.chars().any(|c| c.is_control()) {
        format!("'{s}'")
    } else {
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
}

/// Comment-preserving line-level TOML upsert for `section.key = <toml_value>`.
/// Preserves all existing comments, blank lines, and ordering outside the replaced line.
pub fn upsert_toml_key_preserving_comments(
    existing_text: Option<&str>,
    canonical_key: &str,
    toml_value: &str,
) -> Result<String, String> {
    let Some((target_sec, target_key)) = canonical_key.split_once('.') else {
        return Err(format!("invalid dotted config key '{canonical_key}'"));
    };
    let new_line = format!("{target_key} = {toml_value}");
    let base = existing_text.unwrap_or("");
    let mut lines: Vec<String> = if base.is_empty() {
        vec![
            "# Audiophile Player configuration (config.toml)".to_string(),
            String::new(),
        ]
    } else {
        base.lines().map(str::to_string).collect()
    };

    let mut sec_start: Option<usize> = None;
    let mut sec_end: usize = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let stripped = strip_toml_comment(line).trim();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            let sec_name = stripped[1..stripped.len() - 1].trim();
            if sec_start.is_some() {
                sec_end = i;
                break;
            }
            if sec_name == target_sec {
                sec_start = Some(i);
            }
        }
    }

    if let Some(s_idx) = sec_start {
        let mut replaced = false;
        for line in &mut lines[s_idx + 1..sec_end] {
            let stripped = strip_toml_comment(line);
            let trimmed = stripped.trim();
            if let Some((raw_k, _)) = trimmed.split_once('=') {
                if raw_k.trim() == target_key {
                    let trailing_comment = &line[stripped.len()..];
                    *line = if trailing_comment.is_empty() {
                        new_line.clone()
                    } else {
                        format!("{new_line} {trailing_comment}")
                    };
                    replaced = true;
                    break;
                }
            }
        }
        if !replaced {
            let mut insert_at = sec_end;
            while insert_at > s_idx + 1 && lines[insert_at - 1].trim().is_empty() {
                insert_at -= 1;
            }
            lines.insert(insert_at, new_line);
        }
    } else {
        if !lines.last().is_some_and(|l| l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("[{target_sec}]"));
        lines.push(new_line);
    }

    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

/// Comment-preserving line-level TOML removal for `section.key`. Returns `(updated, removed)`.
pub fn unset_toml_key_preserving_comments(
    existing_text: &str,
    canonical_key: &str,
) -> Result<(String, bool), String> {
    let Some((target_sec, target_key)) = canonical_key.split_once('.') else {
        return Err(format!("invalid dotted config key '{canonical_key}'"));
    };
    let mut lines: Vec<String> = existing_text.lines().map(str::to_string).collect();
    let mut in_sec = false;
    let mut remove_idx: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        let stripped = strip_toml_comment(line).trim();
        if stripped.starts_with('[') && stripped.ends_with(']') {
            let sec_name = stripped[1..stripped.len() - 1].trim();
            in_sec = sec_name == target_sec;
            continue;
        }
        if in_sec {
            if let Some((raw_k, _)) = stripped.split_once('=') {
                if raw_k.trim() == target_key {
                    remove_idx = Some(i);
                    break;
                }
            }
        }
    }

    if let Some(idx) = remove_idx {
        lines.remove(idx);
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        Ok((out, true))
    } else {
        Ok((existing_text.to_string(), false))
    }
}

/// Save `config.toml` atomically, creating `config.toml.bak` first if the file exists.
pub fn write_config_with_backup(path: &Path, content: &str) -> Result<(), String> {
    // Always validate before touching disk!
    PlayerConfig::from_toml_str(content)?;
    if path.exists() {
        let mut bak_os = path.as_os_str().to_os_string();
        bak_os.push(".bak");
        let bak_path = PathBuf::from(bak_os);
        fs::copy(path, &bak_path).map_err(|e| {
            format!(
                "failed creating config backup '{}': {e}",
                bak_path.display()
            )
        })?;
    }
    atomic_write_yaml(path, content)
}
