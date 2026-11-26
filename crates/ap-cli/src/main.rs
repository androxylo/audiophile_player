// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Headless `audiophile-player` binary entrypoint.
//!
//! Default mode (no command-line options): immediately starts playing random FLAC files
//! from `~/Music` in highest-quality Windows Kernel Streaming (`WDM-KS` WaveRT) mode on
//! the auto-selected best DAC. Any other mode requires explicit CLI options (`--help`).

mod cli_args;
mod engine;
mod filters_cmd;
mod prefetch;
mod scanner;
mod signals;
mod ultimate;

use std::process::ExitCode;

use cli_args::{RunMode, help_text, parse_args, version_text};

fn main() -> ExitCode {
    let mode = match parse_args(std::env::args_os()) {
        Ok(m) => m,
        Err(err) => {
            eprintln!("audiophile-player argument error: {err}");
            return ExitCode::FAILURE;
        }
    };

    if mode == RunMode::Help {
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }
    if mode == RunMode::Version {
        print!("{}", version_text());
        return ExitCode::SUCCESS;
    }

    let controller = match signals::install_signal_handlers() {
        Ok(ctrl) => ctrl,
        Err(err) => {
            eprintln!("audiophile-player startup error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let result = match mode {
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
        ),
        RunMode::Filters(cmd) => filters_cmd::run_filters_cli(cmd),
        RunMode::Help | RunMode::Version => unreachable!(),
    };

    controller.mark_cleanup_complete();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("audiophile-player error: {err}");
            ExitCode::FAILURE
        }
    }
}
