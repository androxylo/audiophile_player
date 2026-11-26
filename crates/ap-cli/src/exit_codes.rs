// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Stable, documented CLI exit codes & structured error diagnostics (`QUICK_START_AI_DESIGN.md` §4.3).
//!
//! Exit codes allow both shell scripts and AI agents to branch deterministically without parsing text:
//! - `0`: Success
//! - `1`: General / runtime error
//! - `2`: CLI argument / usage error
//! - `3`: Audio device / hardware error (missing DAC, unsupported 8fS, DAC busy)
//! - `4`: Configuration file error (`config.toml` syntax or validation)
//! - `5`: Process lifecycle conflict (already running on start, or not running on stop/status)

use std::process::ExitCode;

pub const EXIT_OK: u8 = 0;
pub const EXIT_GENERAL_ERROR: u8 = 1;
pub const EXIT_USAGE_ERROR: u8 = 2;
pub const EXIT_DEVICE_ERROR: u8 = 3;
pub const EXIT_CONFIG_ERROR: u8 = 4;
pub const EXIT_INSTANCE_STATE: u8 = 5;

/// Single source of truth for documented CLI exit codes (Review #9 Q15).
#[derive(Debug, Clone, Copy)]
pub struct ExitCodeSpec {
    pub code: u8,
    pub code_str: &'static str,
    pub json_slug: &'static str,
    pub summary_tag: &'static str,
    pub markdown_meaning: &'static str,
}

pub const EXIT_CODE_SPECS: &[ExitCodeSpec] = &[
    ExitCodeSpec {
        code: EXIT_OK,
        code_str: "0",
        json_slug: "success",
        summary_tag: "ok",
        markdown_meaning: "Success",
    },
    ExitCodeSpec {
        code: EXIT_GENERAL_ERROR,
        code_str: "1",
        json_slug: "general_error",
        summary_tag: "error",
        markdown_meaning: "General runtime error",
    },
    ExitCodeSpec {
        code: EXIT_USAGE_ERROR,
        code_str: "2",
        json_slug: "usage_error",
        summary_tag: "usage",
        markdown_meaning: "CLI argument / usage error",
    },
    ExitCodeSpec {
        code: EXIT_DEVICE_ERROR,
        code_str: "3",
        json_slug: "device_or_hardware_error",
        summary_tag: "device",
        markdown_meaning: "Audio device / hardware error (missing DAC, unsupported 8fS, DAC busy)",
    },
    ExitCodeSpec {
        code: EXIT_CONFIG_ERROR,
        code_str: "4",
        json_slug: "config_error",
        summary_tag: "config",
        markdown_meaning: "Configuration file error (`config.toml` syntax or validation)",
    },
    ExitCodeSpec {
        code: EXIT_INSTANCE_STATE,
        code_str: "5",
        json_slug: "instance_already_or_not_running",
        summary_tag: "instance-state",
        markdown_meaning: "Process lifecycle state conflict (already running on start, or not running on stop/status)",
    },
];

/// Structured CLI error with stable exit code and optional actionable `hint:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub code: u8,
    pub message: String,
    pub hint: Option<String>,
}

impl CliError {
    pub fn general(msg: impl Into<String>) -> Self {
        Self {
            code: EXIT_GENERAL_ERROR,
            message: msg.into(),
            hint: None,
        }
    }

    pub fn usage(msg: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE_ERROR,
            message: msg.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn device(msg: impl Into<String>) -> Self {
        Self {
            code: EXIT_DEVICE_ERROR,
            message: msg.into(),
            hint: Some("run `audiophile-player devices` or `audiophile-player doctor`".to_string()),
        }
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self {
            code: EXIT_CONFIG_ERROR,
            message: msg.into(),
            hint: Some("run `audiophile-player config show` or `audiophile-player doctor`".to_string()),
        }
    }

    pub fn instance(msg: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            code: EXIT_INSTANCE_STATE,
            message: msg.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn exit_code(&self) -> ExitCode {
        ExitCode::from(self.code)
    }

    pub fn format_stderr(&self) -> String {
        match &self.hint {
            Some(h) => format!("error: {}\nhint:  {h}\n", self.message),
            None => format!("error: {}\n", self.message),
        }
    }
}

impl From<String> for CliError {
    fn from(s: String) -> Self {
        // Review #9 Q8: never guess exit codes by substring matching error text!
        // Callers with device/config/usage/instance errors must construct typed `CliError`s.
        Self::general(s)
    }
}
