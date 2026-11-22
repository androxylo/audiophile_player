//! Headless `audiophile-player` binary entrypoint.
//!
//! Minimal runnable shell:
//! - Prints startup banner.
//! - Installs Windows console interrupt/termination handlers (`Ctrl+C`, `Ctrl+Break`,
//!   console close, logoff, shutdown).
//! - Parks cleanly at 0% CPU until signaled, then prints graceful stop output and exits `0`.

mod engine;
mod signals;

use std::process::ExitCode;

use engine::PlaybackEngine;

fn main() -> ExitCode {
    let controller = match signals::install_signal_handlers() {
        Ok(ctrl) => ctrl,
        Err(err) => {
            eprintln!("audiophile-player startup error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut player = PlaybackEngine::new_default();
    player.start();

    let signal = controller.wait_for_shutdown();
    player.stop(signal);
    controller.mark_cleanup_complete();

    ExitCode::SUCCESS
}
