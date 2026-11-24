//! Engine configuration schema & zero-dependency TOML config file loader.
//!
//! Currently configures **REW stereo convolution** (`[convolution]`), since upsampling
//! and noise-shaping have strong automatic defaults whereas room-correction filters are
//! strictly user- and room-specific.
//!
//! ## Config Discovery Order
//! 1. Explicit `--config <PATH>` (must exist and parse cleanly).
//! 2. `./audiophile-player.toml` (next to working directory, if present).
//! 3. `%APPDATA%\audiophile-player\config.toml` on Windows (or `$HOME/.config/audiophile-player/config.toml`).
//! 4. If no config file is found: safe default with convolution **disabled** (null pass-through).

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Standard local config filename (portable / current working directory).
pub const LOCAL_CONFIG_FILENAME: &str = "audiophile-player.toml";

/// Standard per-user subdirectory + filename under `%APPDATA%`.
pub const USER_CONFIG_DIR_NAME: &str = "audiophile-player";
pub const USER_CONFIG_FILENAME: &str = "config.toml";

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

/// Top-level user configuration file (`config.toml` / `audiophile-player.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlayerConfig {
    pub convolution: ConvolutionConfig,
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

    /// Parse a strict TOML config string (`[convolution]` section only for now).
    pub fn from_toml_str(input: &str) -> Result<Self, String> {
        // Strip optional UTF-8 BOM (Windows editors such as Notepad may emit it).
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);

        let mut in_convolution = false;
        let mut seen_convolution_section = false;
        let mut seen_keys: HashSet<&str> = HashSet::new();
        let mut explicit_enabled: Option<bool> = None;
        let mut conv = ConvolutionConfig::default();

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
                        in_convolution = true;
                    }
                    other => {
                        return Err(format!(
                            "config line {line_no}: unknown section '[{other}]' (currently only [convolution] is supported)"
                        ));
                    }
                }
                continue;
            }

            if !in_convolution {
                return Err(format!(
                    "config line {line_no}: key-value entry outside a section (expected [convolution])"
                ));
            }

            let Some((raw_key, raw_val)) = stripped.split_once('=') else {
                return Err(format!(
                    "config line {line_no}: expected 'key = value', got '{stripped}'"
                ));
            };
            let key = raw_key.trim();
            let val = raw_val.trim();

            if !seen_keys.insert(key) {
                return Err(format!(
                    "config line {line_no}: duplicate key '{key}' in [convolution]"
                ));
            }

            match key {
                "enabled" => {
                    let b = match val {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(format!(
                                "config line {line_no}: invalid boolean for 'enabled': '{other}' (expected true or false)"
                            ));
                        }
                    };
                    explicit_enabled = Some(b);
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

        // If `enabled` was omitted, enable automatically iff at least one filter source was set.
        conv.enabled = explicit_enabled.unwrap_or_else(|| conv.has_any_source());
        conv.validate()?;

        Ok(Self { convolution: conv })
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
