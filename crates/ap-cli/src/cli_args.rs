//! Minimal zero-dependency CLI argument parser.
//!
//! Default behavior with **zero arguments**: immediately plays random FLAC files from
//! `~/Music` in highest-quality Windows Kernel Streaming (`WDM-KS` WaveRT) mode on the
//! auto-selected best DAC. Any non-default mode requires explicit command-line options.

use std::path::PathBuf;

use ap_output_win::hygiene::OptimizationLevel;

pub const MANUAL_URL: &str = "https://github.com/androxylo/audiophile_player";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
    /// Default zero-option mode: play random FLACs from `music_dir` via WDM-KS + local HTTP UI.
    PlayRandom {
        music_dir: Option<PathBuf>,
        device_filter: Option<String>,
        optimization: OptimizationLevel,
        http_port: Option<u16>,
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

    let mut music_dir: Option<PathBuf> = None;
    let mut device_filter: Option<String> = None;
    let mut optimization = OptimizationLevel::High;
    let mut http_port: Option<u16> = None;
    let mut idle = false;

    while let Some(arg_raw) = iter.next() {
        let arg = arg_raw.as_ref();
        match arg {
            "-h" | "--help" => return Ok(RunMode::Help),
            "--idle" => idle = true,
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

    Ok(RunMode::PlayRandom {
        music_dir,
        device_filter,
        optimization,
        http_port,
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
         OPTIONS:\n\
           --music-dir <PATH>   Root folder containing FLAC subfolders (default: ~/Music)\n\
           --device <NAME>      WDM-KS DAC name filter (default: auto-select best USB DAC)\n\
           --port <PORT>        Local HTTP UI port (default: auto-pick free port in 17441..17499)\n\
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
                music_dir: None,
                device_filter: None,
                optimization: OptimizationLevel::High,
                http_port: None,
            }
        );
    }

    #[test]
    fn explicit_flags_parse_cleanly() {
        let mode = parse_args([
            "audiophile-player",
            "--music-dir",
            r"D:\Audio",
            "--device",
            "Gungnir 2",
            "--port",
            "17450",
            "--opt",
            "extreme",
        ])
        .unwrap();
        assert_eq!(
            mode,
            RunMode::PlayRandom {
                music_dir: Some(PathBuf::from(r"D:\Audio")),
                device_filter: Some("Gungnir 2".to_string()),
                optimization: OptimizationLevel::Extreme,
                http_port: Some(17450),
            }
        );

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
    fn unknown_flag_returns_error() {
        assert!(parse_args(["audiophile-player", "--bad-flag"]).is_err());
    }
}
