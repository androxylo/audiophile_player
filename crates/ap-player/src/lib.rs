// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Audiophile Player application library (`PIPELINE_TESTS_REVIEW.md` §4.1).
//!
//! Exposes the real engine, Ultimate Mode `W0..W4`, offline library scanner, prefetch buffer,
//! filter database commands, diagnostics, and CLI contract so integration tests and verification
//! harnesses can drive the exact production code without hardware.

pub mod cli_args;
pub mod cli_spec;
pub mod diag;
pub mod engine;
pub mod exit_codes;
#[cfg(any(test, feature = "verify"))]
pub mod file_wire_sink;
pub mod filters_cmd;
pub mod lifecycle;
pub mod mcp;
pub mod prefetch;
pub mod scanner;
pub mod signals;
pub mod ultimate;

use std::process::ExitCode;

use cli_args::{HelpMode, RunMode, help_text, parse_args, version_text};
use exit_codes::CliError;

/// Main CLI entrypoint invoked by the thin `audiophile-player` binary (`crates/ap-cli`).
pub fn run_cli_main() -> ExitCode {
    // Review #9 Q13: use `args_os()` so non-Unicode OS arguments never panic before `parse_args`.
    let raw_argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
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
            ExitCode::from(exit_codes::EXIT_OK)
        }
        RunMode::Help(HelpMode::Detail(topic)) => match cli_spec::render_detail_help(&topic) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                let cli_err = CliError::usage(err, "run `audiophile-player help`");
                eprint!("{}", cli_err.format_stderr());
                cli_err.exit_code()
            }
        },
        RunMode::Help(HelpMode::Json) => {
            println!("{}", cli_spec::render_help_json().to_pretty_string());
            ExitCode::SUCCESS
        }
        RunMode::Help(HelpMode::Markdown) => {
            print!("{}", cli_spec::render_help_markdown());
            ExitCode::SUCCESS
        }
        RunMode::Version { as_json } => {
            if as_json {
                println!("{}", diag::version_json().to_pretty_string());
            } else {
                print!("{}", version_text());
            }
            ExitCode::SUCCESS
        }
        RunMode::Doctor {
            config_path,
            as_json,
        } => finish_cli(diag::run_doctor_cli(config_path.as_deref(), as_json)),
        RunMode::Devices {
            output_transport,
            as_json,
        } => finish_cli(diag::run_devices_cli(output_transport, as_json)),
        RunMode::Config(cmd) => finish_cli(diag::run_config_cli(cmd)),
        RunMode::Status { as_json } => finish_cli(lifecycle::run_status_cli(as_json)),
        RunMode::Stop {
            after_track,
            as_json,
        } => finish_cli(lifecycle::run_stop_cli(after_track, as_json)),
        RunMode::Restart {
            flags,
            after_track,
            as_json,
        } => finish_cli(lifecycle::run_restart_cli(flags, after_track, as_json)),
        RunMode::Filters(cmd) => finish_cli(filters_cmd::run_filters_cli(cmd)),
        RunMode::Mcp => finish_cli(mcp::run_mcp_stdio()),
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
                } => engine::run_random_music_player(
                    config_path,
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
                    raw_argv,
                    controller,
                ),
                RunMode::Scan {
                    folder,
                    report,
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
                ),
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
