//! Engine configuration schema & zero-dependency TOML config file loader.
//!
//! Configures **REW stereo convolution** (`[convolution]`), **quantizer / noise shaping**
//! (`[quantizer]`), and the built-in **UPnP AV MediaRenderer** (`[upnp]`).
//!
//! ## Config Discovery Order
//! 1. Explicit `--config <PATH>` (must exist and parse cleanly).
//! 2. `./audiophile-player.toml` (next to working directory, if present).
//! 3. `%APPDATA%\audiophile-player\config.toml` on Windows (or `$HOME/.config/audiophile-player/config.toml`).
//! 4. If no config file is found: safe default with convolution **disabled** (null pass-through),
//!    quantizer `dac_bits = 24` (`shaper = auto`), and UPnP enabled.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

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

/// Top-level user configuration file (`config.toml` / `audiophile-player.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerConfig {
    pub convolution: ConvolutionConfig,
    pub quantizer: QuantizerConfig,
    pub upnp: UpnpConfig,
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

    /// Parse a strict TOML config string (`[convolution]`, `[quantizer]`, and `[upnp]` sections).
    pub fn from_toml_str(input: &str) -> Result<Self, String> {
        // Strip optional UTF-8 BOM (Windows editors such as Notepad may emit it).
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Section {
            None,
            Convolution,
            Quantizer,
            Upnp,
        }

        let mut active_section = Section::None;
        let mut seen_convolution_section = false;
        let mut seen_quantizer_section = false;
        let mut seen_upnp_section = false;
        let mut seen_conv_keys: HashSet<&str> = HashSet::new();
        let mut seen_quant_keys: HashSet<&str> = HashSet::new();
        let mut seen_upnp_keys: HashSet<&str> = HashSet::new();
        let mut explicit_conv_enabled: Option<bool> = None;
        let mut conv = ConvolutionConfig::default();
        let mut quantizer = QuantizerConfig::default();
        let mut upnp = UpnpConfig::default();

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
                    other => {
                        return Err(format!(
                            "config line {line_no}: unknown section '[{other}]' (supported: [convolution], [quantizer], [upnp])"
                        ));
                    }
                }
                continue;
            }

            if active_section == Section::None {
                return Err(format!(
                    "config line {line_no}: key-value entry outside a section (expected [convolution], [quantizer], or [upnp])"
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
            }
        }

        // If `enabled` was omitted in [convolution], enable automatically iff at least one filter source was set.
        conv.enabled = explicit_conv_enabled.unwrap_or_else(|| conv.has_any_source());
        conv.validate()?;
        quantizer.validate()?;

        Ok(Self {
            convolution: conv,
            quantizer,
            upnp,
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
