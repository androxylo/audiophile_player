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

use ap_core::config::{ConvolutionConfig, OutputTransport, is_valid_dac_bits};
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

/// Explicit CLI overrides for `[quantizer]` and `[player]` policy keys (`--dac-bits`, `--shaper`,
/// `--filter`, `--max-taps`; Review #9 E3: `Option`s distinguish explicit defaults from omitted flags).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuantizerCliOverride {
    /// `Some(bits)` when `--dac-bits` was explicitly passed on the CLI.
    pub dac_bits: Option<u8>,
    /// `Some(None)` = explicit `--shaper auto`; `Some(Some(p))` = explicit `--shaper <p>`;
    /// `None` = not passed on CLI (defer to `[quantizer] shaper` in `config.toml`).
    pub shaper: Option<Option<NoiseShaperProfile>>,
    /// `Some(None)` = explicit `--filter auto`; `Some(Some(f))` = explicit `--filter <f>`;
    /// `None` = not passed on CLI (defer to `[player] filter` in `config.toml`).
    pub filter: Option<Option<FilterArchetype>>,
    /// `Some(n)` when `--max-taps` was explicitly passed on the CLI.
    pub max_taps: Option<usize>,
}

/// Parse an `--opt` / `[player] opt` optimization level string (`normal | high | extreme`, E12).
pub fn parse_optimization_level(val: &str) -> Result<OptimizationLevel, String> {
    match val.to_ascii_lowercase().as_str() {
        "normal" => Ok(OptimizationLevel::Normal),
        "high" => Ok(OptimizationLevel::High),
        "extreme" => Ok(OptimizationLevel::Extreme),
        other => Err(format!(
            "invalid --opt '{other}'; expected normal, high, or extreme"
        )),
    }
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
    pub filter_mode: Option<FilterMode>,
    pub filterdb_dir: Option<PathBuf>,
    pub choices_file: Option<PathBuf>,
}

/// Common path override flags shared across `play`, `scan`, and `filters` (Review #9 Q3/Q12).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommonPaths {
    pub config_path: Option<PathBuf>,
    pub filterdb_dir: Option<PathBuf>,
    pub choices_file: Option<PathBuf>,
    pub trackdb_dir: Option<PathBuf>,
}

/// Subcommands under `audiophile-player filters <SUBCOMMAND>` (Review #9 Q12: shared path header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiltersCommand {
    pub paths: CommonPaths,
    pub action: FiltersAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FiltersAction {
    /// Inspect suggested vs active (`mine`) filter decision for one file or album folder.
    Show { target: PathBuf },
    /// Show every track where user choices (`my-filters.yaml` / `AP_*`) differ from machine suggestions.
    Diff,
    /// Set a track or folder filter choice in `my-filters.yaml` (preserving comments & `.bak` backup).
    Set {
        target: PathBuf,
        filter: String,
        note: Option<String>,
    },
    /// Parse and lint `my-filters.yaml` against `library.yaml` (unknown filters, missing paths, shadowed rules).
    Check,
    /// Preview (or `--apply`) moved-file relinks from `filterdb/relink-report.yaml` onto `my-filters.yaml`.
    Relink { apply: bool },
    /// Non-destructively migrate legacy Schema-1 `trackdb/` entries (and optional sidecars) to Schema 2.
    Migrate { sidecar_root: Option<PathBuf> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    /// Default zero-option mode: play random FLACs from `music_dir` via WDM-KS + local HTTP UI + UPnP MediaRenderer.
    PlayRandom {
        config_path: Option<PathBuf>,
        music_dir: Option<PathBuf>,
        device_filter: Option<String>,
        output_transport: Option<OutputTransport>,
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
        /// Emit per-wake WDM-KS WaveRT timing trace to stderr (`--trace-wavert`).
        trace_wavert: bool,
        /// Explicit CLI overrides for `[ultimate]` and `[trackdb]` in `config.toml`.
        ultimate_override: UltimateCliOverride,
    },
    /// Offline parallel resumable library scanner (`audiophile-player scan [FOLDER|ROOT] [OPTIONS]`).
    Scan {
        folder: Option<PathBuf>,
        report: bool,
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
    /// Enumerate WDM-KS or WASAPI Exclusive render devices and probe 8fS support (`audiophile-player devices [--output wdm-ks|wasapi] [--json]`).
    Devices {
        output_transport: Option<OutputTransport>,
        as_json: bool,
    },
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

/// Spec-driven raw value consumption (`cli_spec::find_option`, Review #9 Q3).
fn take_opt_raw<I: Iterator<Item = OsString>>(flag: &str, iter: &mut I) -> Result<OsString, String> {
    iter.next().ok_or_else(|| {
        let placeholder = cli_spec::find_option(flag)
            .and_then(|s| s.value.placeholder())
            .unwrap_or("VALUE");
        format!("{flag} requires {placeholder}")
    })
}

fn take_opt_path<I: Iterator<Item = OsString>>(flag: &str, iter: &mut I) -> Result<PathBuf, String> {
    take_opt_raw(flag, iter).map(PathBuf::from)
}

fn take_opt_str<I: Iterator<Item = OsString>>(flag: &str, iter: &mut I) -> Result<String, String> {
    let raw = take_opt_raw(flag, iter)?;
    require_utf8_arg(flag, raw)
}

/// Spec-driven bounded integer parser enforcing `cli_spec::ValueKind::Int { min, max }` (Q3).
fn take_opt_int<I: Iterator<Item = OsString>>(flag: &str, iter: &mut I) -> Result<i64, String> {
    let s = take_opt_str(flag, iter)?;
    let (min, max) = match cli_spec::find_option(flag).map(|o| o.value) {
        Some(cli_spec::ValueKind::Int { min, max, .. }) => (min, max),
        _ => (i64::MIN, i64::MAX),
    };
    let n = s
        .trim()
        .parse::<i64>()
        .map_err(|_| format!("invalid {flag} '{s}'; expected integer in {min}..={max}"))?;
    if n < min || n > max {
        return Err(format!("{flag} must be in {min}..={max}, got {n}"));
    }
    Ok(n)
}

/// Parse shared path flags (`--config`, `--filterdb-dir`, `--choices-file`, `--trackdb-dir`) once
/// across `play`, `scan`, and `filters` (Review #9 Q3).
fn try_parse_common_path<I: Iterator<Item = OsString>>(
    flag: &str,
    iter: &mut I,
    paths: &mut CommonPaths,
) -> Result<bool, String> {
    match flag {
        "--config" => {
            paths.config_path = Some(take_opt_path(flag, iter)?);
            Ok(true)
        }
        "--filterdb-dir" => {
            paths.filterdb_dir = Some(take_opt_path(flag, iter)?);
            Ok(true)
        }
        "--choices-file" => {
            paths.choices_file = Some(take_opt_path(flag, iter)?);
            Ok(true)
        }
        "--trackdb-dir" => {
            paths.trackdb_dir = Some(take_opt_path(flag, iter)?);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn try_parse_conv_flag<I: Iterator<Item = OsString>>(
    flag: &str,
    iter: &mut I,
    rew_override: &mut Option<ConvolutionConfig>,
) -> Result<bool, String> {
    match flag {
        "--conv-dir" => {
            *rew_override = Some(ConvolutionConfig::auto_dir(take_opt_path(flag, iter)?));
            Ok(true)
        }
        "--conv-file" => {
            *rew_override = Some(ConvolutionConfig::explicit_file(take_opt_path(flag, iter)?));
            Ok(true)
        }
        "--no-conv" => {
            *rew_override = Some(ConvolutionConfig::disabled());
            Ok(true)
        }
        _ => Ok(false),
    }
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

    let mut common = CommonPaths::default();
    let mut music_dir: Option<PathBuf> = None;
    let mut device_filter: Option<String> = None;
    let mut output_transport: Option<OutputTransport> = None;
    let mut max_rate_hz: Option<u32> = None;
    let mut optimization = OptimizationLevel::High;
    let mut optimization_explicit = false;
    let mut http_port: Option<u16> = None;
    let mut upsampler_policy = UpsamplerPolicy::default();
    let mut quantizer_override = QuantizerCliOverride::default();
    let mut rew_override: Option<ConvolutionConfig> = None;
    let mut upnp_override = UpnpCliOverride::default();
    let mut capture_disabled = false;
    let mut trace_wavert = false;
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
        if try_parse_common_path(arg, &mut iter, &mut common)?
            || try_parse_conv_flag(arg, &mut iter, &mut rew_override)?
        {
            continue;
        }
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Summary)),
            "-V" | "--version" => show_version = true,
            "--json" => as_json = true,
            "--idle" => idle = true,
            "--music-dir" => music_dir = Some(take_opt_path("--music-dir", &mut iter)?),
            "--device" => device_filter = Some(take_opt_str("--device", &mut iter)?),
            "--output" => {
                let raw = take_opt_str("--output", &mut iter)?;
                output_transport = Some(OutputTransport::parse(&raw)?);
            }
            "--kernel" => {
                output_transport = Some(OutputTransport::WdmKs);
            }
            "--trace-wavert" => trace_wavert = true,
            "--max-rate" => {
                let val_str = take_opt_str("--max-rate", &mut iter)?;
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
            "--port" => http_port = Some(take_opt_int("--port", &mut iter)? as u16),
            "--filter" => {
                let val_str = take_opt_str("--filter", &mut iter)?;
                if val_str.eq_ignore_ascii_case("auto") {
                    upsampler_policy.filter_override = None;
                    quantizer_override.filter = Some(None);
                } else {
                    let parsed = FilterArchetype::from_str(&val_str)?;
                    upsampler_policy.filter_override = Some(parsed);
                    quantizer_override.filter = Some(Some(parsed));
                }
            }
            "--shaper" => {
                let val_str = take_opt_str("--shaper", &mut iter)?;
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
                let bits = take_opt_int("--dac-bits", &mut iter)? as u8;
                if !is_valid_dac_bits(bits) {
                    return Err(format!(
                        "invalid --dac-bits {bits}; expected one of 16, 18, 20, or 24"
                    ));
                }
                upsampler_policy.dac_bits = bits;
                quantizer_override.dac_bits = Some(bits);
            }
            "--max-taps" => {
                let taps = take_opt_int("--max-taps", &mut iter)? as usize;
                if taps == 0 || taps > MAX_FILTER_TAPS {
                    return Err(format!(
                        "--max-taps must be in 1..={MAX_FILTER_TAPS}, got {taps}"
                    ));
                }
                upsampler_policy.max_taps = taps;
                quantizer_override.max_taps = Some(taps);
            }
            "--no-upnp" => upnp_override.disabled = true,
            "--no-capture" => capture_disabled = true,
            "--upnp-name" => {
                let val_str = take_opt_str("--upnp-name", &mut iter)?;
                let trimmed = val_str.trim();
                if trimmed.is_empty() {
                    return Err("--upnp-name cannot be empty".to_string());
                }
                upnp_override.friendly_name = Some(trimmed.to_string());
            }
            "--upnp-port" => upnp_override.port = Some(take_opt_int("--upnp-port", &mut iter)? as u16),
            "--upnp-bind" => {
                let val_str = take_opt_str("--upnp-bind", &mut iter)?;
                let ip = val_str
                    .parse::<Ipv4Addr>()
                    .map_err(|_| format!("invalid --upnp-bind IPv4 address '{val_str}'"))?;
                upnp_override.bind_ip = Some(ip);
            }
            "--ultimate" => ultimate_override.enabled = Some(true),
            "--no-ultimate" => ultimate_override.enabled = Some(false),
            "--no-genre-rules" => ultimate_override.genre_rules = Some(false),
            "--no-learn-on-play" => ultimate_override.learn_on_play = Some(false),
            "--filters" => {
                let val_str = take_opt_str("--filters", &mut iter)?;
                ultimate_override.filter_mode = Some(FilterMode::parse(&val_str)?);
            }
            "--opt" => {
                let val_str = take_opt_str("--opt", &mut iter)?;
                optimization = parse_optimization_level(&val_str)?;
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
    ultimate_override.trackdb_dir = common.trackdb_dir;
    ultimate_override.filterdb_dir = common.filterdb_dir;
    ultimate_override.choices_file = common.choices_file;

    Ok(RunMode::PlayRandom {
        config_path: common.config_path,
        music_dir,
        device_filter,
        output_transport,
        max_rate_hz,
        optimization,
        optimization_explicit,
        http_port,
        upsampler_policy,
        quantizer_override,
        rew_override,
        upnp_override,
        capture_disabled,
        trace_wavert,
        ultimate_override,
    })
}

fn parse_scan_args<I>(mut iter: I) -> Result<RunMode, String>
where
    I: Iterator<Item = OsString>,
{
    let mut folder: Option<PathBuf> = None;
    let mut report = false;
    let mut post_chain = false;
    let mut force = false;
    let mut jobs: Option<usize> = None;
    let mut common = CommonPaths::default();
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
        if try_parse_common_path(arg, &mut iter, &mut common)?
            || try_parse_conv_flag(arg, &mut iter, &mut rew_override)?
        {
            continue;
        }
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("scan".to_string()))),
            "-V" | "--version" => return Ok(RunMode::Version { as_json: false }),
            "--report" => report = true,
            "--post-chain" => post_chain = true,
            "--force" => force = true,
            "--jobs" | "-j" => jobs = Some(take_opt_int("--jobs", &mut iter)? as usize),
            "--filter" => {
                let val_str = take_opt_str("--filter", &mut iter)?;
                if val_str.eq_ignore_ascii_case("auto") {
                    upsampler_policy.filter_override = None;
                } else {
                    upsampler_policy.filter_override = Some(FilterArchetype::from_str(&val_str)?);
                }
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
        post_chain,
        force,
        jobs,
        trackdb_dir: common.trackdb_dir,
        filterdb_dir: common.filterdb_dir,
        choices_file: common.choices_file,
        config_path: common.config_path,
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

    let mut paths = CommonPaths::default();
    let mut sidecar_root: Option<PathBuf> = None;
    let mut note: Option<String> = None;
    let mut apply = false;
    let mut positional: Vec<OsString> = Vec::new();

    while let Some(arg_raw) = iter.next() {
        let Some(arg) = arg_raw.to_str() else {
            positional.push(arg_raw);
            continue;
        };
        if try_parse_common_path(arg, &mut iter, &mut paths)? {
            continue;
        }
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("filters".to_string()))),
            "-V" | "--version" => return Ok(RunMode::Version { as_json: false }),
            "--sidecar-root" => sidecar_root = Some(take_opt_path("--sidecar-root", &mut iter)?),
            "--note" => note = Some(take_opt_str("--note", &mut iter)?),
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

    let action = match sub {
        "show" => {
            if positional.len() != 1 {
                return Err(
                    "filters show requires one target file or folder (usage: audiophile-player filters show <PATH>)"
                        .to_string(),
                );
            }
            FiltersAction::Show {
                target: PathBuf::from(positional.remove(0)),
            }
        }
        "diff" => {
            if !positional.is_empty() {
                return Err("filters diff takes no positional arguments".to_string());
            }
            FiltersAction::Diff
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
            FiltersAction::Set {
                target,
                filter,
                note,
            }
        }
        "check" => {
            if !positional.is_empty() {
                return Err("filters check takes no positional arguments".to_string());
            }
            FiltersAction::Check
        }
        "relink" => {
            if !positional.is_empty() {
                return Err("filters relink takes no positional arguments".to_string());
            }
            FiltersAction::Relink { apply }
        }
        "migrate" => {
            if !positional.is_empty() {
                return Err("filters migrate takes no positional arguments".to_string());
            }
            FiltersAction::Migrate { sidecar_root }
        }
        other => {
            return Err(format!(
                "unknown filters subcommand '{other}' (expected: show | diff | set | check | relink | migrate)"
            ));
        }
    };
    Ok(RunMode::Filters(FiltersCommand { paths, action }))
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
    let mut output_transport: Option<OutputTransport> = None;
    let mut as_json = false;
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("devices", raw)?;
        match arg.as_str() {
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("devices".to_string()))),
            "--json" => as_json = true,
            "--output" => {
                let val = take_opt_str("--output", &mut iter)?;
                output_transport = Some(OutputTransport::parse(&val)?);
            }
            "--kernel" => {
                output_transport = Some(OutputTransport::WdmKs);
            }
            other => return Err(format!("unknown devices option '{other}'")),
        }
    }
    Ok(RunMode::Devices {
        output_transport,
        as_json,
    })
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

/// Single shared `--when` parser for CLI `stop`/`restart` and MCP `player_stop`/`player_restart`
/// (Review #9 Q6). Returns `Ok(true)` for `after-track`, `Ok(false)` for `now`.
pub fn parse_when(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "now" => Ok(false),
        "after-track" | "after_track" => Ok(true),
        other => Err(format!(
            "invalid --when '{other}'; expected 'after-track' or 'now'"
        )),
    }
}

/// Validate that `flags` form a valid **play** invocation (`RunMode::PlayRandom`) before
/// stopping a running player on `restart` or spawning from MCP (Review #9 Q1).
/// Rejects non-play modes (`--version`, `help`, `scan`, `--idle`, etc.) and invalid option values
/// such as `--dac-bits 17`.
pub fn validate_play_argv(flags: &[String]) -> Result<(), String> {
    let mut check_argv = Vec::with_capacity(flags.len() + 1);
    check_argv.push("audiophile-player".to_string());
    check_argv.extend(flags.iter().cloned());
    let mode = parse_args(check_argv.iter().map(String::as_str))?;
    match mode {
        RunMode::PlayRandom { config_path, .. } => {
            if let Some(ref cfg) = config_path {
                ap_core::config::PlayerConfig::discover_and_load(Some(cfg))
                    .map_err(|e| format!("invalid config '{}': {e}", cfg.display()))?;
            }
            Ok(())
        }
        other => Err(format!(
            "restart/start options must resolve to playback mode, not '{}'",
            match other {
                RunMode::Help(_) => "help",
                RunMode::Version { .. } => "--version",
                RunMode::Idle => "--idle",
                RunMode::Scan { .. } => "scan",
                RunMode::Filters(_) => "filters",
                RunMode::Doctor { .. } => "doctor",
                RunMode::Devices { .. } => "devices",
                RunMode::Config(_) => "config",
                RunMode::Status { .. } => "status",
                RunMode::Stop { .. } => "stop",
                RunMode::Restart { .. } => "restart",
                RunMode::Mcp => "mcp",
                RunMode::PlayRandom { .. } => unreachable!(),
            }
        )),
    }
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
                after_track = parse_when(&v)?;
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
    let mut passthrough = false;
    while let Some(raw) = iter.next() {
        let arg = require_utf8_arg("restart", raw)?;
        if passthrough {
            flags.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => passthrough = true,
            "-h" | "--help" => return Ok(RunMode::Help(HelpMode::Detail("restart".to_string()))),
            "--json" => as_json = true,
            "--yes" => {}
            "--when" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--when requires 'after-track' or 'now'".to_string())?;
                let v = require_utf8_arg("--when", val)?;
                after_track = parse_when(&v)?;
            }
            other => {
                flags.push(other.to_string());
            }
        }
    }
    // Validate forwarded play flags immediately via `validate_play_argv` so typos and non-play
    // modes (`--version`, `scan`, `--idle`, etc.) fail BEFORE stopping the running player (Q1)!
    if !flags.is_empty() {
        validate_play_argv(&flags)?;
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
                output_transport: None,
                max_rate_hz: None,
                optimization: OptimizationLevel::High,
                optimization_explicit: false,
                http_port: None,
                upsampler_policy: UpsamplerPolicy::default(),
                quantizer_override: QuantizerCliOverride::default(),
                rew_override: None,
                upnp_override: UpnpCliOverride::default(),
                capture_disabled: false,
                trace_wavert: false,
                ultimate_override: UltimateCliOverride::default(),
            }
        );
    }

    #[test]
    fn output_backend_flag_accepts_wdm_ks_and_wasapi_and_rejects_shared_mode() {
        let dev_wasapi =
            parse_args(["audiophile-player", "devices", "--output", "wasapi", "--json"]).unwrap();
        assert_eq!(
            dev_wasapi,
            RunMode::Devices {
                output_transport: Some(OutputTransport::WasapiExclusive),
                as_json: true,
            }
        );

        let dev_kernel =
            parse_args(["audiophile-player", "devices", "--kernel", "--json"]).unwrap();
        assert_eq!(
            dev_kernel,
            RunMode::Devices {
                output_transport: Some(OutputTransport::WdmKs),
                as_json: true,
            }
        );

        let play_kernel = parse_args(["audiophile-player", "--kernel"]).unwrap();
        assert!(matches!(
            play_kernel,
            RunMode::PlayRandom {
                output_transport: Some(OutputTransport::WdmKs),
                ..
            }
        ));

        let play_kernel_alias =
            parse_args(["audiophile-player", "--output", "kernel"]).unwrap();
        assert!(matches!(
            play_kernel_alias,
            RunMode::PlayRandom {
                output_transport: Some(OutputTransport::WdmKs),
                ..
            }
        ));

        let err = parse_args(["audiophile-player", "--output", "shared"])
            .expect_err("Windows Shared Mode must be rejected");
        assert!(err.contains("Shared Mode"), "unexpected error: {err}");
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
            "--output",
            "wasapi",
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
            "ns-smooth-15",
            "--max-taps",
            "2000000",
            "--conv-file",
            r"C:\Filters\room-384k.wav",
            "--ultimate",
            "--no-genre-rules",
            "--trackdb-dir",
            r"C:\TrackDb",
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
                output_transport: Some(OutputTransport::WasapiExclusive),
                max_rate_hz: Some(192_000),
                optimization: OptimizationLevel::Extreme,
                optimization_explicit: true,
                http_port: Some(8090),
                upsampler_policy: UpsamplerPolicy {
                    filter_override: Some(FilterArchetype::SincSteep4M),
                    shaper_override: Some(NoiseShaperProfile::NsSmooth15),
                    max_taps: 2_000_000,
                    dac_bits: 18,
                },
                quantizer_override: QuantizerCliOverride {
                    dac_bits: Some(18),
                    shaper: Some(Some(NoiseShaperProfile::NsSmooth15)),
                    filter: Some(Some(FilterArchetype::SincSteep4M)),
                    max_taps: Some(2_000_000),
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
                trace_wavert: false,
                ultimate_override: UltimateCliOverride {
                    enabled: Some(true),
                    genre_rules: Some(false),
                    learn_on_play: None,
                    trackdb_dir: Some(PathBuf::from(r"C:\TrackDb")),
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
            RunMode::Filters(FiltersCommand {
                action: FiltersAction::Show { .. },
                ..
            })
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "diff"]).unwrap(),
            RunMode::Filters(FiltersCommand {
                action: FiltersAction::Diff,
                ..
            })
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
            RunMode::Filters(FiltersCommand {
                action: FiltersAction::Set {
                    ref filter,
                    ref note,
                    ..
                },
                ..
            }) if filter == "gauss-long-lp" && note.as_deref() == Some("warmer")
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "check"]).unwrap(),
            RunMode::Filters(FiltersCommand {
                action: FiltersAction::Check,
                ..
            })
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "relink", "--apply"]).unwrap(),
            RunMode::Filters(FiltersCommand {
                action: FiltersAction::Relink { apply: true },
                ..
            })
        ));
        assert!(matches!(
            parse_args(["audiophile-player", "filters", "migrate"]).unwrap(),
            RunMode::Filters(FiltersCommand {
                action: FiltersAction::Migrate { .. },
                ..
            })
        ));
        assert!(parse_args(["audiophile-player", "filters"]).is_err());
        assert!(parse_args(["audiophile-player", "filters", "bogus"]).is_err());
    }
}
