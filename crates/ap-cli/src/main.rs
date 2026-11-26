// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Headless `audiophile-player` binary entrypoint.
//!
//! Default mode (no command-line options): immediately starts playing random FLAC files
//! from `~/Music` in highest-quality Windows Kernel Streaming (`WDM-KS` WaveRT) mode on
//! the auto-selected best DAC. Any other mode requires explicit CLI options (`help`).

mod cli_args;
mod cli_spec;
mod diag;
mod engine;
mod exit_codes;
mod filters_cmd;
mod lifecycle;
mod mcp;
mod prefetch;
mod scanner;
mod signals;
mod ultimate;

use std::process::ExitCode;

use cli_args::{HelpMode, RunMode, help_text, parse_args, version_text};
use exit_codes::CliError;

fn main() -> ExitCode {
    let raw_argv: Vec<String> = std::env::args().skip(1).collect();
    let mode = match parse_args(std::env::args_os()) {
        Ok(m) => m,
        Err(err) => {
            let cli_err = CliError::usage(err, "run `audiophile-player help` for usage");
            eprint!("{}", cli_err.format_stderr());
            return cli_err.exit_code();
        }
    };

    match mode {
        RunMode::Help(HelpMode::Summary) => {
            print!("{}", help_text());
            return ExitCode::from(exit_codes::EXIT_OK);
        }
        RunMode::Help(HelpMode::Detail(topic)) => match cli_spec::render_detail_help(&topic) {
            Ok(text) => {
                print!("{text}");
                return ExitCode::SUCCESS;
            }
            Err(err) => {
                let cli_err = CliError::usage(err, "run `audiophile-player help`");
                eprint!("{}", cli_err.format_stderr());
                return cli_err.exit_code();
            }
        },
        RunMode::Help(HelpMode::Json) => {
            println!("{}", cli_spec::render_help_json().to_pretty_string());
            return ExitCode::SUCCESS;
        }
        RunMode::Help(HelpMode::Markdown) => {
            print!("{}", cli_spec::render_help_markdown());
            return ExitCode::SUCCESS;
        }
        RunMode::Version { as_json } => {
            if as_json {
                println!("{}", diag::version_json().to_pretty_string());
            } else {
                print!("{}", version_text());
            }
            return ExitCode::SUCCESS;
        }
        RunMode::Doctor {
            config_path,
            as_json,
        } => {
            return finish_cli(diag::run_doctor_cli(config_path.as_deref(), as_json));
        }
        RunMode::Devices { as_json } => {
            return finish_cli(diag::run_devices_cli(as_json));
        }
        RunMode::Config(cmd) => {
            return finish_cli(diag::run_config_cli(cmd));
        }
        RunMode::Status { as_json } => {
            return finish_cli(lifecycle::run_status_cli(as_json));
        }
        RunMode::Stop {
            after_track,
            as_json,
        } => {
            return finish_cli(lifecycle::run_stop_cli(after_track, as_json));
        }
        RunMode::Restart {
            flags,
            after_track,
            as_json,
        } => {
            return finish_cli(lifecycle::run_restart_cli(flags, after_track, as_json));
        }
        RunMode::Filters(cmd) => {
            return finish_cli(filters_cmd::run_filters_cli(cmd).map_err(CliError::from));
        }
        RunMode::Mcp => {
            return finish_cli(mcp::run_mcp_stdio());
        }
        other => {
            let controller = match signals::install_signal_handlers() {
                Ok(ctrl) => ctrl,
                Err(err) => {
                    let cli_err = CliError::general(format!("startup error: {err}"));
                    eprint!("{}", cli_err.format_stderr());
                    return cli_err.exit_code();
                }
            };

            let result: Result<(), CliError> = match other {
                RunMode::Idle => {
                    engine::run_idle(controller);
                    Ok(())
                }
                RunMode::PlayRandom {
                    config_path,
                    music_dir,
                    device_filter,
                    optimization,
                    optimization_explicit,
                    http_port,
                    upsampler_policy,
                    quantizer_override,
                    rew_override,
                    upnp_override,
                    capture_disabled,
                    ultimate_override,
                } => engine::run_random_music_player(
                    config_path,
                    music_dir,
                    device_filter,
                    optimization,
                    optimization_explicit,
                    http_port,
                    upsampler_policy,
                    quantizer_override,
                    rew_override,
                    upnp_override,
                    capture_disabled,
                    ultimate_override,
                    raw_argv,
                    controller,
                ),
                RunMode::Scan {
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
                } => scanner::run_scan_cli(
                    scanner::ScanOptions {
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
                    },
                    controller,
                )
                .map_err(CliError::from),
                _ => unreachable!(),
            };

            controller.mark_cleanup_complete();
            finish_cli(result)
        }
    }
}

fn finish_cli(res: Result<(), CliError>) -> ExitCode {
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprint!("{}", err.format_stderr());
            err.exit_code()
        }
    }
}
