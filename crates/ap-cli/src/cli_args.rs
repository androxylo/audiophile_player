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
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};
use ap_output_win::hygiene::OptimizationLevel;

pub const MANUAL_URL: &str = "https://github.com/androxylo/audiophile_player";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    /// Default zero-option mode: play random FLACs from `music_dir` via WDM-KS + local HTTP UI + UPnP MediaRenderer.
    PlayRandom {
        config_path: Option<PathBuf>,
        music_dir: Option<PathBuf>,
        device_filter: Option<String>,
        optimization: OptimizationLevel,
        http_port: Option<u16>,
        upsampler_policy: UpsamplerPolicy,
        /// Explicit CLI overrides for `[quantizer]` in `config.toml`.
        quantizer_override: QuantizerCliOverride,
        /// Explicit CLI override for `[convolution]` in `config.toml` (`None` = use config file).
        rew_override: Option<ConvolutionConfig>,
        /// Explicit CLI overrides for `[upnp]` in `config.toml`.
        upnp_override: UpnpCliOverride,
    },
    /// Offline parallel resumable library scanner (`audiophile-player scan <folder> [OPTIONS]`).
    Scan {
        folder: PathBuf,
        report: bool,
        sidecar: bool,
        post_chain: bool,
        force: bool,
        trackdb_dir: Option<PathBuf>,
        config_path: Option<PathBuf>,
        rew_override: Option<ConvolutionConfig>,
        upsampler_policy: UpsamplerPolicy,
    },
    /// Explicit `--idle` mode: keep running without touching audio hardware (for signal tests).
    Idle,
    /// Print concise help text and exit `0`.
    Help,
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

    // Subcommand: `audiophile-player scan <folder> [--report] [--sidecar] [--post-chain] [--force] ...`
    if iter.peek().and_then(|s| s.to_str()) == Some("scan") {
        iter.next();
        return parse_scan_args(iter);
    }

    let mut config_path: Option<PathBuf> = None;
    let mut music_dir: Option<PathBuf> = None;
    let mut device_filter: Option<String> = None;
    let mut optimization = OptimizationLevel::High;
    let mut http_port: Option<u16> = None;
    let mut upsampler_policy = UpsamplerPolicy::default();
    let mut quantizer_override = QuantizerCliOverride::default();
    let mut rew_override: Option<ConvolutionConfig> = None;
    let mut upnp_override = UpnpCliOverride::default();
    let mut idle = false;

    while let Some(arg_raw) = iter.next() {
        let arg = arg_raw.to_str().ok_or_else(|| {
            format!(
                "unknown non-UTF-8 option '{}' (run with --help for usage)",
                arg_raw.to_string_lossy()
            )
        })?;
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help),
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
            }
            unknown => {
                return Err(format!(
                    "unknown option '{unknown}' (run with --help for usage)"
                ));
            }
        }
    }

    if idle {
        return Ok(RunMode::Idle);
    }

    upsampler_policy.validate()?;

    Ok(RunMode::PlayRandom {
        config_path,
        music_dir,
        device_filter,
        optimization,
        http_port,
        upsampler_policy,
        quantizer_override,
        rew_override,
        upnp_override,
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
    let mut trackdb_dir: Option<PathBuf> = None;
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
            "-h" | "--help" => return Ok(RunMode::Help),
            "--report" => report = true,
            "--sidecar" => sidecar = true,
            "--post-chain" => post_chain = true,
            "--force" => force = true,
            "--trackdb-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--trackdb-dir requires a directory path".to_string())?;
                trackdb_dir = Some(PathBuf::from(val));
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

    let folder = folder.ok_or_else(|| {
        "scan requires a folder path (usage: audiophile-player scan <folder> [--report] [--sidecar] [--post-chain])"
            .to_string()
    })?;
    upsampler_policy.validate()?;

    Ok(RunMode::Scan {
        folder,
        report,
        sidecar,
        post_chain,
        force,
        trackdb_dir,
        config_path,
        rew_override,
        upsampler_policy,
    })
}

pub fn help_text() -> String {
    format!(
        "audiophile-player v{} — Headless Windows Kernel Streaming (WDM-KS) Player\n\
         Manual: {MANUAL_URL}\n\n\
         USAGE:\n\
           audiophile-player                  Start immediately in highest-quality WDM-KS mode,\n\
                                              playing random FLACs from ~/Music + local HTTP UI\n\
                                              + UPnP AV MediaRenderer (for foobar2000 / BubbleUPnP).\n\
           audiophile-player [OPTIONS]\n\
           audiophile-player scan <FOLDER> [--report] [--sidecar] [--post-chain] [--force]\n\
                                              Offline whole-track library scanner (writes Schema-1\n\
                                              YAML trackdb entries; never modifies FLAC files).\n\n\
         CONFIG FILE:\n\
           Auto-loads .\\audiophile-player.toml or %APPDATA%\\audiophile-player\\config.toml\n\
           (or pass --config <PATH>). Configures [convolution], [quantizer], and [upnp].\n\n\
         OPTIONS:\n\
           --config <PATH>      Path to TOML config file (default: auto-discover)\n\
           --music-dir <PATH>   Root folder containing FLAC subfolders (default: ~/Music)\n\
           --device <NAME>      WDM-KS DAC name filter (default: auto-select best USB DAC)\n\
           --port <PORT>        Local HTTP UI port (default: auto-pick free port in 8085..8199)\n\
           --upnp-name <NAME>   UPnP MediaRenderer display name (default: Audiophile Player)\n\
           --upnp-port <PORT>   UPnP HTTP/SOAP port (default: auto-pick free port in 49152..49250)\n\
           --upnp-bind <IPV4>   Bind/advertise UPnP renderer on a specific local IPv4 address\n\
           --no-upnp            Disable the built-in UPnP AV MediaRenderer\n\
           --filter <NAME>      Enforce upsampling filter: auto | gauss-long-lp | gauss-hires-lp |\n\
                                null | simple-test | gauss-apod-4m | sinc-steep-4m | ext-sharp-lp |\n\
                                minphase-long | intermediate-long | short-lp | short-mp\n\
                                (default: auto -> gauss-long-lp 1x, gauss-hires-lp hi-res, null 8fS)\n\
           --dac-bits <N>       Quantizer DAC bit depth: 16 | 18 | 20 | 24 (default: 24)\n\
           --shaper <NAME>      Enforce 8fS shaper: auto | bypass | tpdf | ns-gentle-2 |\n\
                                ns5 (clean-room ns5-class) | lns15 (clean-room lns15-class) |\n\
                                pggb-optimal (clean-room wide40-9)\n\
                                (default: auto -> ns-gentle-2 at 8fS, bypass native 32-bit, tpdf non-8fS)\n\
           --max-taps <N>       Tap ceiling <= 4000000 (default: 4000000)\n\
           --conv-dir <PATH>    Override config: REW stereo WAV folder (auto-picks newest\n\
                                2-ch WAV matching 352.8 / 384 kHz)\n\
           --conv-file <PATH>   Override config: explicit 2-ch REW WAV file\n\
           --no-conv            Override config: disable REW stereo convolution\n\
           --opt <LEVEL>        KS priority tier: normal | high | extreme (default: high)\n\
           --idle               Start idle without opening audio hardware (signal test mode)\n\
           -h, --help           Show this help message\n",
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
                optimization: OptimizationLevel::High,
                http_port: None,
                upsampler_policy: UpsamplerPolicy::default(),
                quantizer_override: QuantizerCliOverride::default(),
                rew_override: None,
                upnp_override: UpnpCliOverride::default(),
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
                optimization: OptimizationLevel::Extreme,
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

        assert_eq!(
            parse_args(["audiophile-player", "--idle"]).unwrap(),
            RunMode::Idle
        );
        assert_eq!(
            parse_args(["audiophile-player", "--help"]).unwrap(),
            RunMode::Help
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
    fn scan_subcommand_parses_all_flags_and_rejects_missing_folder() {
        let mode = parse_args([
            "audiophile-player",
            "scan",
            r"D:\Music",
            "--report",
            "--sidecar",
            "--post-chain",
            "--force",
            "--trackdb-dir",
            r"C:\DB",
            "--no-conv",
        ])
        .unwrap();
        assert_eq!(
            mode,
            RunMode::Scan {
                folder: PathBuf::from(r"D:\Music"),
                report: true,
                sidecar: true,
                post_chain: true,
                force: true,
                trackdb_dir: Some(PathBuf::from(r"C:\DB")),
                config_path: None,
                rew_override: Some(ConvolutionConfig::disabled()),
                upsampler_policy: UpsamplerPolicy::default(),
            }
        );
        assert!(parse_args(["audiophile-player", "scan"]).is_err());
        assert!(parse_args(["audiophile-player", "scan", r"D:\Music", "--bogus"]).is_err());
    }
}
