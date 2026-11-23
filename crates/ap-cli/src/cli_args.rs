//! Minimal zero-dependency CLI argument parser.
//!
//! Default behavior with **zero arguments**: immediately plays random FLAC files from
//! `~/Music` in highest-quality Windows Kernel Streaming (`WDM-KS` WaveRT) mode on the
//! auto-selected best DAC, with automatic per-track upsampler selection (`UpsamplerPicker`)
//! and the local HTTP UI starting at port `8085`. Any non-default mode requires explicit flags.

use std::path::PathBuf;
use std::str::FromStr;

use ap_core::config::ConvolutionConfig;
use ap_dsp::MAX_FILTER_TAPS;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};
use ap_output_win::hygiene::OptimizationLevel;

pub const MANUAL_URL: &str = "https://github.com/androxylo/audiophile_player";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    /// Default zero-option mode: play random FLACs from `music_dir` via WDM-KS + local HTTP UI.
    PlayRandom {
        config_path: Option<PathBuf>,
        music_dir: Option<PathBuf>,
        device_filter: Option<String>,
        optimization: OptimizationLevel,
        http_port: Option<u16>,
        upsampler_policy: UpsamplerPolicy,
        /// Explicit CLI override for `[convolution]` in `config.toml` (`None` = use config file).
        rew_override: Option<ConvolutionConfig>,
    },
    /// Explicit `--idle` mode: keep running without touching audio hardware (for signal tests).
    Idle,
    /// Print concise help text and exit `0`.
    Help,
}

pub fn parse_args<I, S>(args: I) -> Result<RunMode, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut iter = args.into_iter();
    let _program = iter.next(); // skip argv[0]

    let mut config_path: Option<PathBuf> = None;
    let mut music_dir: Option<PathBuf> = None;
    let mut device_filter: Option<String> = None;
    let mut optimization = OptimizationLevel::High;
    let mut http_port: Option<u16> = None;
    let mut upsampler_policy = UpsamplerPolicy::default();
    let mut rew_override: Option<ConvolutionConfig> = None;
    let mut idle = false;

    while let Some(arg_raw) = iter.next() {
        let arg = arg_raw.as_ref();
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help),
            "--idle" => idle = true,
            "--config" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--config requires a TOML config file path".to_string())?;
                config_path = Some(PathBuf::from(val.as_ref()));
            }
            "--music-dir" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--music-dir requires a folder path".to_string())?;
                music_dir = Some(PathBuf::from(val.as_ref()));
            }
            "--device" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--device requires a DAC name substring".to_string())?;
                device_filter = Some(val.as_ref().to_string());
            }
            "--port" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--port requires a numeric TCP port".to_string())?;
                let port = val
                    .as_ref()
                    .parse::<u16>()
                    .map_err(|_| format!("invalid --port '{}'; expected 1..=65535", val.as_ref()))?;
                if port == 0 {
                    return Err("--port must be in 1..=65535".to_string());
                }
                http_port = Some(port);
            }
            "--filter" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--filter requires a filter name or 'auto'".to_string())?;
                if val.as_ref().eq_ignore_ascii_case("auto") {
                    upsampler_policy.filter_override = None;
                } else {
                    upsampler_policy.filter_override = Some(FilterArchetype::from_str(val.as_ref())?);
                }
            }
            "--shaper" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--shaper requires a shaper name or 'auto'".to_string())?;
                if val.as_ref().eq_ignore_ascii_case("auto") {
                    upsampler_policy.shaper_override = None;
                } else {
                    upsampler_policy.shaper_override =
                        Some(NoiseShaperProfile::from_str(val.as_ref())?);
                }
            }
            "--max-taps" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--max-taps requires a positive integer <= 4000000".to_string())?;
                let taps = val
                    .as_ref()
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --max-taps '{}'", val.as_ref()))?;
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
                rew_override = Some(ConvolutionConfig::auto_dir(PathBuf::from(val.as_ref())));
            }
            "--conv-file" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--conv-file requires a stereo REW WAV file path".to_string())?;
                rew_override = Some(ConvolutionConfig::explicit_file(PathBuf::from(val.as_ref())));
            }
            "--no-conv" => {
                rew_override = Some(ConvolutionConfig::disabled());
            }
            "--opt" => {
                let val = iter
                    .next()
                    .ok_or_else(|| "--opt requires normal | high | extreme".to_string())?;
                optimization = match val.as_ref().to_ascii_lowercase().as_str() {
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
        rew_override,
    })
}

pub fn help_text() -> String {
    format!(
        "audiophile-player v{} — Headless Windows Kernel Streaming (WDM-KS) Player\n\
         Manual: {MANUAL_URL}\n\n\
         USAGE:\n\
           audiophile-player                  Start immediately in highest-quality WDM-KS mode,\n\
                                              playing random FLACs from ~/Music + local HTTP UI.\n\
           audiophile-player [OPTIONS]\n\n\
         CONFIG FILE:\n\
           Auto-loads .\\audiophile-player.toml or %APPDATA%\\audiophile-player\\config.toml\n\
           (or pass --config <PATH>). Currently configures [convolution].\n\n\
         OPTIONS:\n\
           --config <PATH>      Path to TOML config file (default: auto-discover)\n\
           --music-dir <PATH>   Root folder containing FLAC subfolders (default: ~/Music)\n\
           --device <NAME>      WDM-KS DAC name filter (default: auto-select best USB DAC)\n\
           --port <PORT>        Local HTTP UI port (default: auto-pick free port in 8085..8199)\n\
           --filter <NAME>      Enforce upsampling filter: auto | simple-test | null |\n\
                                gauss-apod-4m | sinc-steep-4m | gauss-long-lp |\n\
                                gauss-hires-ip | gauss-hires-mp | ext-sharp-lp\n\
                                (default: auto per track; simple-test/null until INV-03 lands)\n\
           --shaper <NAME>      Enforce 8fS shaper: auto | bypass | ns5 | lns15 |\n\
                                pggb-optimal | tpdf (default: auto per track)\n\
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
                rew_override: None,
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
            "--filter",
            "sinc-steep-4m",
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
                },
                rew_override: Some(ConvolutionConfig::explicit_file(
                    r"C:\Filters\room-384k.wav"
                )),
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
    fn rejects_invalid_filter_and_over_4m_taps() {
        assert!(parse_args(["audiophile-player", "--filter", "bogus"]).is_err());
        assert!(parse_args(["audiophile-player", "--max-taps", "4000001"]).is_err());
        assert!(parse_args(["audiophile-player", "--bad-flag"]).is_err());
    }
}
