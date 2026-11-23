//! Headless `audiophile-player` binary entrypoint.
//!
//! Default mode (no command-line options): immediately starts playing random FLAC files
//! from `~/Music` in highest-quality Windows Kernel Streaming (`WDM-KS` WaveRT) mode on
//! the auto-selected best DAC. Any other mode requires explicit CLI options (`--help`).

mod cli_args;
mod engine;
mod prefetch;
mod signals;

use std::process::ExitCode;

use cli_args::{RunMode, help_text, parse_args};

fn main() -> ExitCode {
    let mode = match parse_args(std::env::args()) {
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
            music_dir,
            device_filter,
            optimization,
            http_port,
            upsampler_policy,
            rew_convolution,
        } => engine::run_random_music_player(
            music_dir,
            device_filter,
            optimization,
            http_port,
            upsampler_policy,
            rew_convolution,
            controller,
        ),
        RunMode::Help => unreachable!(),
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
