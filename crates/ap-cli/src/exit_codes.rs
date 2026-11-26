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

    pub fn classify_from_string(msg: String) -> Self {
        let lower = msg.to_ascii_lowercase();
        if lower.contains("config file") || lower.contains("config line") || lower.contains("invalid config") {
            Self::config(msg)
        } else if lower.contains("wdm-ks")
            || lower.contains("dac")
            || lower.contains("audio device")
            || lower.contains("no render")
            || lower.contains("352.8")
            || lower.contains("384 khz")
        {
            Self::device(msg)
        } else if lower.contains("already running") || lower.contains("not running") {
            Self {
                code: EXIT_INSTANCE_STATE,
                message: msg,
                hint: Some("run `audiophile-player status`".to_string()),
            }
        } else {
            Self::general(msg)
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
        Self::classify_from_string(s)
    }
}
