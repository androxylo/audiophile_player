// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Minimal zero-dependency CLI argument parser.
//!
//! Default behavior with **zero arguments**: immediately plays random FLAC files from
//! `~/Music` in highest-quality Windows Kernel Streaming (`WDM-KS` WaveRT) mode on the
//! auto-selected best DAC, with automatic per-track upsampler selection (`UpsamplerPicker`)
//! and the local HTTP UI starting at port `8085`. Any non-default mode requires explicit flags.

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;

use ap_core::config::{ConvolutionConfig, is_valid_dac_bits};
use ap_core::user_filters::FilterMode;
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};
use ap_output_win::hygiene::OptimizationLevel;

use crate::cli_spec;

/// Help output variants (`help`, `help <topic>`, `help --json`, `help --markdown`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpMode {
    Summary,
    Detail(String),
    Json,
    Markdown,
}

/// Subcommands under `audiophile-player config <show|set|unset>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigCommand {
    Show {
        config_path: Option<PathBuf>,
        effective: bool,
        as_json: bool,
    },
    Set {
        key: String,
        value: String,
        config_path: Option<PathBuf>,
        as_json: bool,
    },
    Unset {
        key: String,
        config_path: Option<PathBuf>,
        as_json: bool,
    },
}

/// Explicit CLI overrides for `[quantizer]` in `config.toml` (`--dac-bits`, `--shaper`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuantizerCliOverride {
    /// `Some(bits)` when `--dac-bits` was explicitly passed on the CLI.
    pub dac_bits: Option<u8>,
    /// `Some(None)` = explicit `--shaper auto`; `Some(Some(p))` = explicit `--shaper <p>`;
    /// `None` = not passed on CLI (defer to `[quantizer] shaper` in `config.toml`).
    pub shaper: Option<Option<NoiseShaperProfile>>,
}

/// Explicit CLI overrides for the built-in UPnP AV MediaRenderer (`--upnp-*` / `--no-upnp`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpnpCliOverride {
    pub disabled: bool,
    pub friendly_name: Option<String>,
    pub port: Option<u16>,
    pub bind_ip: Option<Ipv4Addr>,
}

/// Explicit CLI overrides for `[ultimate]`, `[trackdb]`, and `[filters]` (`--ultimate`, `--filters`, etc.).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UltimateCliOverride {
    pub enabled: Option<bool>,
    pub genre_rules: Option<bool>,
    pub learn_on_play: Option<bool>,
    pub trackdb_dir: Option<PathBuf>,
    pub trackdb_sidecar: Option<bool>,
    pub filter_mode: Option<FilterMode>,
    pub filterdb_dir: Option<PathBuf>,
    pub choices_file: Option<PathBuf>,
}

/// Subcommands under `audiophile-player filters <SUBCOMMAND>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiltersCommand {
    /// Inspect suggested vs active (`mine`) filter decision for one file or album folder.
    Show {
        target: PathBuf,
        config_path: Option<PathBuf>,
        filterdb_dir: Option<PathBuf>,
        choices_file: Option<PathBuf>,
    },
    /// Show every track where user choices (`my-filters.yaml` / `AP_*`) differ from machine suggestions.
    Diff {
        config_path: Option<PathBuf>,
        filterdb_dir: Option<PathBuf>,
        choices_file: Option<PathBuf>,
    },
    /// Set a track or folder filter choice in `my-filters.yaml` (preserving comments & `.bak` backup).
    Set {
        target: PathBuf,
        filter: String,
        note: Option<String>,
        config_path: Option<PathBuf>,
        filterdb_dir: Option<PathBuf>,
        choices_file: Option<PathBuf>,
    },
    /// Parse and lint `my-filters.yaml` against `library.yaml` (unknown filters, missing paths, shadowed rules).
    Check {
        config_path: Option<PathBuf>,
        filterdb_dir: Option<PathBuf>,
        choices_file: Option<PathBuf>,
    },
    /// Preview (or `--apply`) moved-file relinks from `filterdb/relink-report.yaml` onto `my-filters.yaml`.
    Relink {
        apply: bool,
        config_path: Option<PathBuf>,
        filterdb_dir: Option<PathBuf>,
        choices_file: Option<PathBuf>,
    },
    /// Non-destructively migrate legacy Schema-1 `trackdb/` entries (and optional sidecars) to Schema 2.
    Migrate {
        config_path: Option<PathBuf>,
        trackdb_dir: Option<PathBuf>,
        filterdb_dir: Option<PathBuf>,
        choices_file: Option<PathBuf>,
        sidecar_root: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    /// Default zero-option mode: play random FLACs from `music_dir` via WDM-KS + local HTTP UI + UPnP MediaRenderer.
    PlayRandom {
        config_path: Option<PathBuf>,
        music_dir: Option<PathBuf>,
        device_filter: Option<String>,
        max_rate_hz: Option<u32>,
        optimization: OptimizationLevel,
        optimization_explicit: bool,
        http_port: Option<u16>,
        upsampler_policy: UpsamplerPolicy,
        /// Explicit CLI overrides for `[quantizer]` in `config.toml`.
        quantizer_override: QuantizerCliOverride,
        /// Explicit CLI override for `[convolution]` in `config.toml` (`None` = use config file).
        rew_override: Option<ConvolutionConfig>,
        /// Explicit CLI overrides for `[upnp]` in `config.toml`.
        upnp_override: UpnpCliOverride,
        /// Disable opt-in exclusive kernel stream capture watcher (`--no-capture`).
        capture_disabled: bool,
        /// Explicit CLI overrides for `[ultimate]` and `[trackdb]` in `config.toml`.
        ultimate_override: UltimateCliOverride,
    },
    /// Offline parallel resumable library scanner (`audiophile-player scan [FOLDER|ROOT] [OPTIONS]`).
    Scan {
        folder: Option<PathBuf>,
        report: bool,
        sidecar: bool,
        post_chain: bool,
        force: bool,
        jobs: Option<usize>,
        trackdb_dir: Option<PathBuf>,
        filterdb_dir: Option<PathBuf>,
        choices_file: Option<PathBuf>,
        config_path: Option<PathBuf>,
        rew_override: Option<ConvolutionConfig>,
        upsampler_policy: UpsamplerPolicy,
    },
    /// Inspect, validate, edit, relink, or migrate the Schema-2 Filter Database (`audiophile-player filters ...`).
    Filters(FiltersCommand),
    /// Run system/DAC/config health diagnostics (`audiophile-player doctor [--json]`).
    Doctor {
        config_path: Option<PathBuf>,
        as_json: bool,
    },
    /// Enumerate WDM-KS render devices and probe 8fS support (`audiophile-player devices [--json]`).
    Devices { as_json: bool },
    /// Inspect or safely edit `config.toml` (`audiophile-player config <show|set|unset>`).
    Config(ConfigCommand),
    /// Query the running player instance (`audiophile-player status [--json]`).
    Status { as_json: bool },
    /// Gracefully stop the running player (`audiophile-player stop [--when now|after-track] [--json]`).
    Stop { after_track: bool, as_json: bool },
    /// Gracefully restart the running player with updated options (`audiophile-player restart ...`).
    Restart {
        flags: Vec<String>,
        after_track: bool,
        as_json: bool,
    },
    /// Run thin stdio MCP supervisor server (`audiophile-player mcp`).
    Mcp,
    /// Explicit `--idle` mode: keep running without touching audio hardware (for signal tests).
    Idle,
    /// Print summary, detail, JSON spec, or Markdown CLI docs and exit `0`.
    Help(HelpMode),
    /// Print version, copyright, and GNU GPLv3+ license/no-warranty notice (or `--json`) and exit `0`.
    Version { as_json: bool },
}

fn require_utf8_arg(flag: &str, raw: OsString) -> Result<String, String> {
    raw.into_string()
        .map_err(|bad| format!("{flag} requires valid UTF-8, got '{}'", bad.to_string_lossy()))
}

pub fn parse_args<I, S>(args: I) -> Result<RunMode, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut iter = args.into_iter().map(Into::into).peekable();
    let _program = iter.next(); // skip argv[0]

    // Subcommands
    match iter.peek().and_then(|s| s.to_str()) {
        Some("scan") => {
            iter.next();
            return parse_scan_args(iter);
        }
        Some("filters") => {
            iter.next();
            return parse_filters_args(iter);
        }
        Some("help") => {
            iter.next();
            return parse_help_args(iter);
        }
        Some("doctor") => {
            iter.next();
            return parse_doctor_args(iter);
        }
        Some("devices") => {
            iter.next();
            return parse_devices_args(iter);
        }
        Some("config") => {
            iter.next();
            return parse_config_args(iter);
        }
        Some("status") => {
            iter.next();
            return parse_status_args(iter);
        }
        Some("stop") => {
            iter.next();
            return parse_stop_args(iter);
        }
        Some("restart") => {
            iter.next();
            return parse_restart_args(iter);
        }
        Some("mcp") => {
            iter.next();
            return parse_mcp_args(iter);
        }
        _ => {}
    }

    let mut config_path: Option<PathBuf> = None;
    let mut music_dir: Option<PathBuf> = None;
    let mut device_filter: Option<String> = None;
    let mut max_rate_hz: Option<u32> = None;
    let mut optimization = OptimizationLevel::High;
    let mut optimization_explicit = false;
    let mut http_port: Option<u16> = None;
    let mut upsampler_policy = UpsamplerPolicy::default();
    let mut quantizer_override = QuantizerCliOverride::default();
    let mut rew_override: Option<ConvolutionConfig> = None;
    let mut upnp_override = UpnpCliOverride::default();
    let mut capture_disabled = false;
    let mut ultimate_override = UltimateCliOverride::default();
    let mut idle = false;
    let mut show_version = false;
    let mut as_json = false;

    while let Some(arg_raw) = iter.next() {
        let arg = arg_raw.to_str().ok_or_else(|| {
            format!(
                "unknown non-UTF-8 option '{}' (run `audiophile-player help` for usage)",
                arg_raw.to_string_lossy()
            )
        })?;
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Summary)),
            "-V" | "--version" => show_version = true,
            "--json" => as_json = true,
            "--idle" => idle = true,
            "--config" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--config requires a TOML config file path".to_string())?;
                config_path = Some(PathBuf::from(val));
            }
            "--music-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--music-dir requires a folder path".to_string())?;
                music_dir = Some(PathBuf::from(val));
            }
            "--device" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--device requires a DAC name substring".to_string())?;
                device_filter = Some(require_utf8_arg("--device", val)?);
            }
            "--max-rate" => {
                let val = iter.next().ok_or_else(|| {
                    "--max-rate requires 'auto' or a standard sample rate (44100|48000|88200|96000|176400|192000|352800|384000)"
                        .to_string()
                })?;
                let val_str = require_utf8_arg("--max-rate", val)?;
                let trimmed = val_str.trim();
                if trimmed.eq_ignore_ascii_case("auto") {
                    max_rate_hz = None;
                } else {
                    let rate = trimmed.parse::<u32>().map_err(|_| {
                        format!(
                            "invalid --max-rate '{val_str}'; expected auto or one of 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000"
                        )
                    })?;
                    if !ap_core::format::is_valid_max_rate(rate) {
                        return Err(format!(
                            "unsupported --max-rate {rate}; expected auto or one of 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000"
                        ));
                    }
                    max_rate_hz = Some(rate);
                }
            }
            "--port" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--port requires a numeric TCP port".to_string())?;
                let val_str = require_utf8_arg("--port", val)?;
                let port = val_str
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port '{val_str}'; expected 1..=65535"))?;
                if port == 0 {
                    return Err("--port must be in 1..=65535".to_string());
                }
                http_port = Some(port);
            }
            "--filter" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--filter requires a filter name or 'auto'".to_string())?;
                let val_str = require_utf8_arg("--filter", val)?;
                if val_str.eq_ignore_ascii_case("auto") {
                    upsampler_policy.filter_override = None;
                } else {
                    upsampler_policy.filter_override = Some(FilterArchetype::from_str(&val_str)?);
                }
            }
            "--shaper" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--shaper requires a shaper name or 'auto'".to_string())?;
                let val_str = require_utf8_arg("--shaper", val)?;
                if val_str.eq_ignore_ascii_case("auto") {
                    upsampler_policy.shaper_override = None;
                    quantizer_override.shaper = Some(None);
                } else {
                    let parsed = NoiseShaperProfile::from_str(&val_str)?;
                    upsampler_policy.shaper_override = Some(parsed);
                    quantizer_override.shaper = Some(Some(parsed));
                }
            }
            "--dac-bits" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--dac-bits requires 16, 18, 20, or 24".to_string())?;
                let val_str = require_utf8_arg("--dac-bits", val)?;
                let bits = val_str.parse::<u8>().map_err(|_| {
                    format!("invalid --dac-bits '{val_str}'; expected 16, 18, 20, or 24")
                })?;
                if !is_valid_dac_bits(bits) {
                    return Err(format!(
                        "invalid --dac-bits {bits}; expected one of 16, 18, 20, or 24"
                    ));
                }
                upsampler_policy.dac_bits = bits;
                quantizer_override.dac_bits = Some(bits);
            }
            "--max-taps" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--max-taps requires a positive integer <= 4000000".to_string())?;
                let val_str = require_utf8_arg("--max-taps", val)?;
                let taps = val_str
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --max-taps '{val_str}'"))?;
                if taps == 0 || taps > MAX_FILTER_TAPS {
                    return Err(format!(
                        "--max-taps must be in 1..={MAX_FILTER_TAPS}, got {taps}"
                    ));
                }
                upsampler_policy.max_taps = taps;
            }
            "--conv-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--conv-dir requires a REW WAV directory path".to_string())?;
                rew_override = Some(ConvolutionConfig::auto_dir(PathBuf::from(val)));
            }
            "--conv-file" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--conv-file requires a stereo REW WAV file path".to_string())?;
                rew_override = Some(ConvolutionConfig::explicit_file(PathBuf::from(val)));
            }
            "--no-conv" => {
                rew_override = Some(ConvolutionConfig::disabled());
            }
            "--no-upnp" => {
                upnp_override.disabled = true;
            }
            "--no-capture" => {
                capture_disabled = true;
            }
            "--upnp-name" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--upnp-name requires a renderer display name".to_string())?;
                let val_str = require_utf8_arg("--upnp-name", val)?;
                let trimmed = val_str.trim();
                if trimmed.is_empty() {
                    return Err("--upnp-name cannot be empty".to_string());
                }
                upnp_override.friendly_name = Some(trimmed.to_string());
            }
            "--upnp-port" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--upnp-port requires a numeric TCP port".to_string())?;
                let val_str = require_utf8_arg("--upnp-port", val)?;
                let port = val_str
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --upnp-port '{val_str}'; expected 1..=65535"))?;
                if port == 0 {
                    return Err("--upnp-port must be in 1..=65535".to_string());
                }
                upnp_override.port = Some(port);
            }
            "--upnp-bind" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--upnp-bind requires an IPv4 address".to_string())?;
                let val_str = require_utf8_arg("--upnp-bind", val)?;
                let ip = val_str
                    .parse::<Ipv4Addr>()
                    .map_err(|_| format!("invalid --upnp-bind IPv4 address '{val_str}'"))?;
                upnp_override.bind_ip = Some(ip);
            }
            "--ultimate" => {
                ultimate_override.enabled = Some(true);
            }
            "--no-ultimate" => {
                ultimate_override.enabled = Some(false);
            }
            "--no-genre-rules" => {
                ultimate_override.genre_rules = Some(false);
            }
            "--no-learn-on-play" => {
                ultimate_override.learn_on_play = Some(false);
            }
            "--trackdb-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--trackdb-dir requires a directory path".to_string())?;
                ultimate_override.trackdb_dir = Some(PathBuf::from(val));
            }
            "--trackdb-sidecar" => {
                ultimate_override.trackdb_sidecar = Some(true);
            }
            "--filters" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--filters requires auto | suggested | mine".to_string())?;
                let val_str = require_utf8_arg("--filters", val)?;
                ultimate_override.filter_mode = Some(FilterMode::parse(&val_str)?);
            }
            "--filterdb-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--filterdb-dir requires a directory path".to_string())?;
                ultimate_override.filterdb_dir = Some(PathBuf::from(val));
            }
            "--choices-file" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--choices-file requires a YAML file path".to_string())?;
                ultimate_override.choices_file = Some(PathBuf::from(val));
            }
            "--opt" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--opt requires normal | high | extreme".to_string())?;
                let val_str = require_utf8_arg("--opt", val)?;
                optimization = match val_str.to_ascii_lowercase().as_str() {
                    "normal" => OptimizationLevel::Normal,
                    "high" => OptimizationLevel::High,
                    "extreme" => OptimizationLevel::Extreme,
                    other => {
                        return Err(format!(
                            "invalid --opt '{other}'; expected normal, high, or extreme"
                        ));
                    }
                };
                optimization_explicit = true;
            }
            unknown => {
                let did_you_mean = cli_spec::suggest_nearest_option(unknown)
                    .map(|s| format!("; did you mean '{s}'? Run `audiophile-player help {s}`"))
                    .unwrap_or_else(|| " (run `audiophile-player help` for usage)".to_string());
                return Err(format!("unknown option '{unknown}'{did_you_mean}"));
            }
        }
    }

    if show_version {
        return Ok(RunMode::Version { as_json });
    }
    if as_json {
        return Err(
            "--json is only supported on inspection/lifecycle commands (e.g. `help --json`, `doctor --json`, `--version --json`)"
                .to_string(),
        );
    }

    if idle {
        return Ok(RunMode::Idle);
    }

    upsampler_policy.validate()?;

    Ok(RunMode::PlayRandom {
        config_path,
        music_dir,
        device_filter,
        max_rate_hz,
        optimization,
        optimization_explicit,
        http_port,
        upsampler_policy,
        quantizer_override,
        rew_override,
        upnp_override,
        capture_disabled,
        ultimate_override,
    })
}

fn parse_scan_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut folder: Option<PathBuf> = None;
    let mut report = false;
    let mut sidecar = false;
    let mut post_chain = false;
    let mut force = false;
    let mut jobs: Option<usize> = None;
    let mut trackdb_dir: Option<PathBuf> = None;
    let mut filterdb_dir: Option<PathBuf> = None;
    let mut choices_file: Option<PathBuf> = None;
    let mut config_path: Option<PathBuf> = None;
    let mut rew_override: Option<ConvolutionConfig> = None;
    let mut upsampler_policy = UpsamplerPolicy::default();

    while let Some(arg_raw) = iter.next() {
        let Some(arg) = arg_raw.to_str() else {
            if folder.is_none() {
                folder = Some(PathBuf::from(arg_raw));
                continue;
            }
            return Err(format!(
                "unknown non-UTF-8 option '{}' for 'scan'",
                arg_raw.to_string_lossy()
            ));
        };
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("scan".to_string()))),
            "-V" | "--version" => return Ok(RunMode::Version { as_json: false }),
            "--report" => report = true,
            "--sidecar" => sidecar = true,
            "--post-chain" => post_chain = true,
            "--force" => force = true,
            "--jobs" | "-j" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--jobs requires a positive integer".to_string())?;
                let val_str = require_utf8_arg("--jobs", val)?;
                let n = val_str
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --jobs '{val_str}'; expected positive integer"))?;
                if n == 0 {
                    return Err("--jobs must be >= 1".to_string());
                }
                jobs = Some(n);
            }
            "--trackdb-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--trackdb-dir requires a directory path".to_string())?;
                trackdb_dir = Some(PathBuf::from(val));
            }
            "--filterdb-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--filterdb-dir requires a directory path".to_string())?;
                filterdb_dir = Some(PathBuf::from(val));
            }
            "--choices-file" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--choices-file requires a YAML file path".to_string())?;
                choices_file = Some(PathBuf::from(val));
            }
            "--config" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--config requires a TOML config file path".to_string())?;
                config_path = Some(PathBuf::from(val));
            }
            "--filter" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--filter requires a filter name or 'auto'".to_string())?;
                let val_str = require_utf8_arg("--filter", val)?;
                if val_str.eq_ignore_ascii_case("auto") {
                    upsampler_policy.filter_override = None;
                } else {
                    upsampler_policy.filter_override = Some(FilterArchetype::from_str(&val_str)?);
                }
            }
            "--conv-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--conv-dir requires a REW WAV directory path".to_string())?;
                rew_override = Some(ConvolutionConfig::auto_dir(PathBuf::from(val)));
            }
            "--conv-file" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--conv-file requires a stereo REW WAV file path".to_string())?;
                rew_override = Some(ConvolutionConfig::explicit_file(PathBuf::from(val)));
            }
            "--no-conv" => {
                rew_override = Some(ConvolutionConfig::disabled());
            }
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown scan option '{other}' (run with --help for usage)"
                ));
            }
            _ => {
                if folder.is_none() {
                    folder = Some(PathBuf::from(arg_raw));
                } else {
                    return Err(format!("unexpected extra scan argument '{arg}'"));
                }
            }
        }
    }

    upsampler_policy.validate()?;

    Ok(RunMode::Scan {
        folder,
        report,
        sidecar,
        post_chain,
        force,
        jobs,
        trackdb_dir,
        filterdb_dir,
        choices_file,
        config_path,
        rew_override,
        upsampler_policy,
    })
}

fn parse_filters_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let Some(sub_raw) = iter.next() else {
        return Err(
            "filters requires a subcommand: show | diff | set | check | relink | migrate"
                .to_string(),
        );
    };
    let sub = sub_raw.to_str().ok_or_else(|| {
        format!(
            "unknown non-UTF-8 filters subcommand '{}'",
            sub_raw.to_string_lossy()
        )
    })?;
    if matches!(sub, "-h" | "--help") {
        return Ok(RunMode::Help(HelpMode::Detail("filters".to_string())));
    }
    if matches!(sub, "-V" | "--version") {
        return Ok(RunMode::Version { as_json: false });
    }

    let mut config_path: Option<PathBuf> = None;
    let mut filterdb_dir: Option<PathBuf> = None;
    let mut choices_file: Option<PathBuf> = None;
    let mut trackdb_dir: Option<PathBuf> = None;
    let mut sidecar_root: Option<PathBuf> = None;
    let mut note: Option<String> = None;
    let mut apply = false;
    let mut positional: Vec<OsString> = Vec::new();

    while let Some(arg_raw) = iter.next() {
        let Some(arg) = arg_raw.to_str() else {
            positional.push(arg_raw);
            continue;
        };
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("filters".to_string()))),
            "-V" | "--version" => return Ok(RunMode::Version { as_json: false }),
            "--config" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--config requires a TOML config file path".to_string())?;
                config_path = Some(PathBuf::from(val));
            }
            "--filterdb-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--filterdb-dir requires a directory path".to_string())?;
                filterdb_dir = Some(PathBuf::from(val));
            }
            "--choices-file" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--choices-file requires a YAML file path".to_string())?;
                choices_file = Some(PathBuf::from(val));
            }
            "--trackdb-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--trackdb-dir requires a directory path".to_string())?;
                trackdb_dir = Some(PathBuf::from(val));
            }
            "--sidecar-root" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--sidecar-root requires a music directory path".to_string())?;
                sidecar_root = Some(PathBuf::from(val));
            }
            "--note" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--note requires a text string".to_string())?;
                note = Some(require_utf8_arg("--note", val)?);
            }
            "--apply" => apply = true,
            "--yes" => {}
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown filters {sub} option '{other}' (run with --help for usage)"
                ));
            }
            _ => positional.push(arg_raw),
        }
    }

    match sub {
        "show" => {
            if positional.len() != 1 {
                return Err(
                    "filters show requires one target file or folder (usage: audiophile-player filters show <PATH>)"
                        .to_string(),
                );
            }
            Ok(RunMode::Filters(FiltersCommand::Show {
                target: PathBuf::from(positional.remove(0)),
                config_path,
                filterdb_dir,
                choices_file,
            }))
        }
        "diff" => {
            if !positional.is_empty() {
                return Err("filters diff takes no positional arguments".to_string());
            }
            Ok(RunMode::Filters(FiltersCommand::Diff {
                config_path,
                filterdb_dir,
                choices_file,
            }))
        }
        "set" => {
            if positional.len() != 2 {
                return Err(
                    "filters set requires <PATH> <FILTER> (usage: audiophile-player filters set <PATH> <FILTER> [--note TEXT])"
                        .to_string(),
                );
            }
            let target = PathBuf::from(positional.remove(0));
            let filter = require_utf8_arg("filters set <FILTER>", positional.remove(0))?;
            Ok(RunMode::Filters(FiltersCommand::Set {
                target,
                filter,
                note,
                config_path,
                filterdb_dir,
                choices_file,
            }))
        }
        "check" => {
            if !positional.is_empty() {
                return Err("filters check takes no positional arguments".to_string());
            }
            Ok(RunMode::Filters(FiltersCommand::Check {
                config_path,
                filterdb_dir,
                choices_file,
            }))
        }
        "relink" => {
            if !positional.is_empty() {
                return Err("filters relink takes no positional arguments".to_string());
            }
            Ok(RunMode::Filters(FiltersCommand::Relink {
                apply,
                config_path,
                filterdb_dir,
                choices_file,
            }))
        }
        "migrate" => {
            if !positional.is_empty() {
                return Err("filters migrate takes no positional arguments".to_string());
            }
            Ok(RunMode::Filters(FiltersCommand::Migrate {
                config_path,
                trackdb_dir,
                filterdb_dir,
                choices_file,
                sidecar_root,
            }))
        }
        other => Err(format!(
            "unknown filters subcommand '{other}' (expected: show | diff | set | check | relink | migrate)"
        )),
    }
}

fn parse_help_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut topic: Option<String> = None;
    let mut as_json = false;
    let mut as_markdown = false;
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("help", raw)?;
        match arg.as_str() {
            "--json" => as_json = true,
            "--markdown" => as_markdown = true,
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Summary)),
            _ => {
                if topic.is_none() {
                    topic = Some(arg);
                } else {
                    return Err(format!("unexpected extra help argument '{arg}'"));
                }
            }
        }
    }
    if as_json && as_markdown {
        return Err("cannot combine --json and --markdown".to_string());
    }
    if as_json {
        return Ok(RunMode::Help(HelpMode::Json));
    }
    if as_markdown {
        return Ok(RunMode::Help(HelpMode::Markdown));
    }
    match topic {
        Some(t) => Ok(RunMode::Help(HelpMode::Detail(t))),
        None => Ok(RunMode::Help(HelpMode::Summary)),
    }
}

fn parse_doctor_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut config_path = None;
    let mut as_json = false;
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("doctor", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("doctor".to_string()))),
            "--json" => as_json = true,
            "--config" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--config requires a TOML config file path".to_string())?;
                config_path = Some(PathBuf::from(val));
            }
            other => return Err(format!("unknown doctor option '{other}'")),
        }
    }
    Ok(RunMode::Doctor {
        config_path,
        as_json,
    })
}

fn parse_devices_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut as_json = false;
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("devices", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("devices".to_string()))),
            "--json" => as_json = true,
            other => return Err(format!("unknown devices option '{other}'")),
        }
    }
    Ok(RunMode::Devices { as_json })
}

fn parse_config_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let Some(sub_raw) = iter.next() else {
        return Err("config requires a subcommand: show | set | unset".to_string());
    };
    let sub = require_utf8_arg("config", sub_raw)?;
    if matches!(sub.as_str(), "-h" | "--help") {
        return Ok(RunMode::Help(HelpMode::Detail("config".to_string())));
    }
    let mut config_path = None;
    let mut effective = false;
    let mut as_json = false;
    let mut positional: Vec<String> = Vec::new();
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("config", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("config".to_string()))),
            "--config" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--config requires a TOML config file path".to_string())?;
                config_path = Some(PathBuf::from(val));
            }
            "--effective" => effective = true,
            "--json" => as_json = true,
            "--yes" => {}
            other if other.starts_with('-') => {
                return Err(format!("unknown config {sub} option '{other}'"));
            }
            _ => positional.push(arg),
        }
    }
    match sub.as_str() {
        "show" => {
            if !positional.is_empty() {
                return Err("config show takes no positional arguments".to_string());
            }
            Ok(RunMode::Config(ConfigCommand::Show {
                config_path,
                effective,
                as_json,
            }))
        }
        "set" => {
            if positional.len() != 2 {
                return Err(
                    "config set requires <KEY> <VALUE> (usage: audiophile-player config set <key> <value>)"
                        .to_string(),
                );
            }
            let key = positional.remove(0);
            let value = positional.remove(0);
            Ok(RunMode::Config(ConfigCommand::Set {
                key,
                value,
                config_path,
                as_json,
            }))
        }
        "unset" => {
            if positional.len() != 1 {
                return Err(
                    "config unset requires <KEY> (usage: audiophile-player config unset <key>)"
                        .to_string(),
                );
            }
            let key = positional.remove(0);
            Ok(RunMode::Config(ConfigCommand::Unset {
                key,
                config_path,
                as_json,
            }))
        }
        other => Err(format!(
            "unknown config subcommand '{other}' (expected: show | set | unset)"
        )),
    }
}

fn parse_status_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut as_json = false;
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("status", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("status".to_string()))),
            "--json" => as_json = true,
            other => return Err(format!("unknown status option '{other}'")),
        }
    }
    Ok(RunMode::Status { as_json })
}

fn parse_stop_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut after_track = false;
    let mut as_json = false;
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("stop", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("stop".to_string()))),
            "--json" => as_json = true,
            "--yes" => {}
            "--when" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--when requires 'now' or 'after-track'".to_string())?;
                let v = require_utf8_arg("--when", val)?;
                match v.to_ascii_lowercase().as_str() {
                    "now" => after_track = false,
                    "after-track" | "after_track" => after_track = true,
                    other => {
                        return Err(format!(
                            "invalid --when '{other}'; expected 'now' or 'after-track'"
                        ));
                    }
                }
            }
            other => return Err(format!("unknown stop option '{other}'")),
        }
    }
    Ok(RunMode::Stop {
        after_track,
        as_json,
    })
}

fn parse_restart_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    // Revision 2 decision: `restart` defaults to `after_track`!
    let mut after_track = true;
    let mut as_json = false;
    let mut flags: Vec<String> = Vec::new();
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("restart", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("restart".to_string()))),
            "--json" => as_json = true,
            "--yes" => {}
            "--when" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--when requires 'after-track' or 'now'".to_string())?;
                let v = require_utf8_arg("--when", val)?;
                match v.to_ascii_lowercase().as_str() {
                    "now" => after_track = false,
                    "after-track" | "after_track" => after_track = true,
                    other => {
                        return Err(format!(
                            "invalid --when '{other}'; expected 'after-track' or 'now'"
                        ));
                    }
                }
            }
            other => {
                flags.push(other.to_string());
            }
        }
    }
    // Validate forwarded play flags immediately via `parse_args` so typos fail before stopping the player!
    if !flags.is_empty() {
        let mut check_argv = Vec::with_capacity(flags.len() + 1);
        check_argv.push("audiophile-player".to_string());
        check_argv.extend(flags.iter().cloned());
        parse_args(check_argv.iter().map(String::as_str))?;
    }
    Ok(RunMode::Restart {
        flags,
        after_track,
        as_json,
    })
}

fn parse_mcp_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("mcp", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("mcp".to_string()))),
            other => return Err(format!("unknown mcp option '{other}'")),
        }
    }
    Ok(RunMode::Mcp)
}

pub fn help_text() -> String {
    cli_spec::render_summary_help()
}

/// Standard FSF GNU GPLv3+ `--version` notice.
pub fn version_text() -> String {
    format!(
        "audiophile-player v{}\n\
         Copyright (C) 2026 androxylo\n\
         License GPLv3+: GNU GPL version 3 or later <https://gnu.org/licenses/gpl.html>.\n\
         This is free software: you are free to change and redistribute it.\n\
         There is NO WARRANTY, to the extent permitted by law.\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_arguments_selects_default_high_quality_random_music_mode() {
        let mode = parse_args(["audiophile-player"]).unwrap();
        assert_eq!(
            mode,
            RunMode::PlayRandom {
                config_path: None,
                music_dir: None,
                device_filter: None,
                max_rate_hz: None,
                optimization: OptimizationLevel::High,
                optimization_explicit: false,
                http_port: None,
                upsampler_policy: UpsamplerPolicy::default(),
                quantizer_override: QuantizerCliOverride::default(),
                rew_override: None,
                upnp_override: UpnpCliOverride::default(),
                capture_disabled: false,
                ultimate_override: UltimateCliOverride::default(),
            }
        );
    }

    #[test]
    fn explicit_flags_including_upsampler_overrides_parse_cleanly() {
        let mode = parse_args([
            "audiophile-player",
            "--config",
            r"C:\Cfg\player.toml",
            "--music-dir",
            r"D:\Audio",
            "--device",
            "Gungnir 2",
            "--max-rate",
            "192000",
            "--port",
            "8090",
            "--upnp-name",
            "Living Room DAC",
            "--upnp-port",
            "49170",
            "--upnp-bind",
            "192.168.0.187",
            "--filter",
            "sinc-steep-4m",
            "--dac-bits",
            "18",
            "--shaper",
            "lns15",
            "--max-taps",
            "2000000",
            "--conv-file",
            r"C:\Filters\room-384k.wav",
            "--ultimate",
            "--no-genre-rules",
            "--trackdb-dir",
            r"C:\TrackDb",
            "--trackdb-sidecar",
            "--opt",
            "extreme",
        ])
        .unwrap();
        assert_eq!(
            mode,
            RunMode::PlayRandom {
                config_path: Some(PathBuf::from(r"C:\Cfg\player.toml")),
                music_dir: Some(PathBuf::from(r"D:\Audio")),
                device_filter: Some("Gungnir 2".to_string()),
                max_rate_hz: Some(192_000),
                optimization: OptimizationLevel::Extreme,
                optimization_explicit: true,
                http_port: Some(8090),
                upsampler_policy: UpsamplerPolicy {
                    filter_override: Some(FilterArchetype::SincSteep4M),
                    shaper_override: Some(NoiseShaperProfile::Lns15Smooth),
                    max_taps: 2_000_000,
                    dac_bits: 18,
                },
                quantizer_override: QuantizerCliOverride {
                    dac_bits: Some(18),
                    shaper: Some(Some(NoiseShaperProfile::Lns15Smooth)),
                },
                rew_override: Some(ConvolutionConfig::explicit_file(
                    r"C:\Filters\room-384k.wav"
                )),
                upnp_override: UpnpCliOverride {
                    disabled: false,
                    friendly_name: Some("Living Room DAC".to_string()),
                    port: Some(49170),
                    bind_ip: Some(Ipv4Addr::new(192, 168, 0, 187)),
                },
                capture_disabled: false,
                ultimate_override: UltimateCliOverride {
                    enabled: Some(true),
                    genre_rules: Some(false),
                    learn_on_play: None,
                    trackdb_dir: Some(PathBuf::from(r"C:\TrackDb")),
                    trackdb_sidecar: Some(true),
                    filter_mode: None,
                    filterdb_dir: None,
                    choices_file: None,
                },
            }
        );

        let no_conv = parse_args(["audiophile-player", "--no-conv"]).unwrap();
        assert!(matches!(
            no_conv,
            RunMode::PlayRandom {
                rew_override: Some(ref c),
                ..
            } if !c.enabled
        ));

        let no_cap = parse_args(["audiophile-player", "--no-capture"]).unwrap();
        assert!(matches!(
            no_cap,
            RunMode::PlayRandom {
                capture_disabled: true,
                ..
            }
        ));

        assert_eq!(
            parse_args(["audiophile-player", "--idle"]).unwrap(),
            RunMode::Idle
        );
        assert_eq!(
            parse_args(["audiophile-player", "--help"]).unwrap(),
            RunMode::Help(HelpMode::Summary)
        );
        assert_eq!(
            parse_args(["audiophile-player", "help", "--filters"]).unwrap(),
            RunMode::Help(HelpMode::Detail("--filters".to_string()))
        );
        assert_eq!(
            parse_args(["audiophile-player", "--version", "--json"]).unwrap(),
            RunMode::Version { as_json: true }
        );
        assert!(
            parse_args(["audiophile-player", "--filtr"])
                .unwrap_err()
                .contains("--filter")
        );
    }

    #[test]
    fn rejects_invalid_filter_dac_bits_and_over_4m_taps() {
        assert!(parse_args(["audiophile-player", "--filter", "bogus"]).is_err());
        assert!(parse_args(["audiophile-player", "--dac-bits", "22"]).is_err());
        assert!(parse_args(["audiophile-player", "--dac-bits", "32"]).is_err());
        assert!(parse_args(["audiophile-player", "--max-taps", "4000001"]).is_err());
        assert!(parse_args(["audiophile-player", "--bad-flag"]).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn accepts_non_utf8_windows_paths_without_panicking() {
        use std::os::windows::ffi::OsStringExt;

        // Legal NTFS name containing an unpaired UTF-16 surrogate.
        let weird_path = OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xD800]);
        let args = vec![
            OsString::from("audiophile-player"),
            OsString::from("--music-dir"),
            weird_path.clone(),
            OsString::from("--config"),
            weird_path.clone(),
        ];
        let mode = parse_args(args).expect("non-UTF-8 paths should parse cleanly");
        match mode {
            RunMode::PlayRandom {
                music_dir,
                config_path,
                ..
            } => {
                assert_eq!(music_dir, Some(PathBuf::from(&weird_path)));
                assert_eq!(config_path, Some(PathBuf::from(&weird_path)));
            }
            other => panic!("unexpected mode: {other:?}"),
        }

        // Non-UTF-8 flag name returns Err instead of panicking.
        let bad_flag = parse_args(vec![OsString::from("audiophile-player"), weird_path]);
        assert!(bad_flag.is_err());
    }

    #[test]
    fn scan_subcommand_parses_all_flags_and_supports_optional_folder() {
        let mode = parse_args([
            "audiophile-player",
            "scan",
            r"D:\Music",
            "--report",
            "--sidecar",
            "--post-chain",
            "--force",
            "--jobs",
            "6",
            "--trackdb-dir",
            r"C:\DB",
            "--filterdb-dir",
            r"C:\FilterDB",
            "--no-conv",
        ])
        .unwrap();
        assert_eq!(
            mode,
            RunMode::Scan {
                folder: Some(PathBuf::from(r"D:\Music")),
                report: true,
                sidecar: true,
                post_chain: true,
                force: true,
                jobs: Some(6),
                trackdb_dir: Some(PathBuf::from(r"C:\DB")),
                filterdb_dir: Some(PathBuf::from(r"C:\FilterDB")),
                choices_file: None,
                config_path: None,
                rew_override: Some(ConvolutionConfig::disabled()),
                upsampler_policy: UpsamplerPolicy::default(),
            }
        );
        // Omitting folder scans configured roots.
        assert!(matches!(
            parse_args(["audiophile-player", "scan"]).unwrap(),
            RunMode::Scan { folder: None, .. }
        ));
        assert!(parse_args(["audiophile-player", "scan", "--jobs", "0"]).is_err());
        assert!(parse_args(["audiophile-player", "scan", r"D:\Music", "--bogus"]).is_err());
    }

    #[test]
    fn filters_subcommands_and_mode_flag_parse_cleanly() {
        let play_mode = parse_args([
            "audiophile-player",
            "--filters",
            "suggested",
            "--filterdb-dir",
            r"C:\FDB",
            "--choices-file",
            r"C:\my-filters.yaml",
        ])
        .unwrap();
        assert!(matches!(
            play_mode,
            RunMode::PlayRandom {
                ultimate_override: UltimateCliOverride {
                    filter_mode: Some(FilterMode::Suggested),
                    filterdb_dir: Some(_),
                    choices_file: Some(_),
                    ..
                },
                ..
            }
        ));

        assert!(matches!(
            parse_args(["audiophile-player", "filters", "show", r"D:\Music\A"]).unwrap(),
            RunMode::Filters(FiltersCommand::Show { .. })
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "diff"]).unwrap(),
            RunMode::Filters(FiltersCommand::Diff { .. })
        ));
        assert!(matches!(
            parse_args([
                "audiophile-player",
                "filters",
                "set",
                r"D:\Music\A",
                "gauss-long-lp",
                "--note",
                "warmer",
            ])
            .unwrap(),
            RunMode::Filters(FiltersCommand::Set {
                ref filter,
                ref note,
                ..
            }) if filter == "gauss-long-lp" && note.as_deref() == Some("warmer")
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "check"]).unwrap(),
            RunMode::Filters(FiltersCommand::Check { .. })
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "relink", "--apply"]).unwrap(),
            RunMode::Filters(FiltersCommand::Relink { apply: true, .. })
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "migrate"]).unwrap(),
            RunMode::Filters(FiltersCommand::Migrate { .. })
        ));
        assert!(parse_args(["audiophile-player", "filters"]).is_err());
        assert!(parse_args(["audiophile-player", "filters", "bogus"]).is_err());
    }
}
