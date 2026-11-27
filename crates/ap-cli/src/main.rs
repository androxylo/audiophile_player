// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Thin `audiophile-player` binary entrypoint delegating to [`ap_player::run_cli_main`].

use std::process::ExitCode;

fn main() -> ExitCode {
    ap_player::run_cli_main()
}
