// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Reproducible YAML report writer (`target/null-test/<timestamp>.yaml`, `PIPELINE_TESTS_REVIEW.md` §2.2).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::metrics::LayerBMetrics;

/// Complete human-readable loopback null verification report.
#[derive(Debug, Clone)]
pub struct NullTestReport {
    pub timestamp_unix_ms: u128,
    pub track_path: String,
    pub seed: u64,
    pub excerpted: bool,
    pub source_frames: usize,
    pub output_frames: usize,
    pub md5: String,
    pub filter: String,
    pub f_eff_hz: u32,
    pub convolver_hash: String,
    pub source_rate_hz: u32,
    pub internal_rate_hz: u32,
    pub output_rate_hz: u32,
    pub deemphasis: bool,
    pub shaper: String,
    pub dac_bits: u8,
    pub wire_format: String,
    pub gain_db: f64,
    pub latency_samples: usize,
    pub layer_a_peak_residual_db: f64,
    pub layer_a_rms_residual_db: f64,
    pub layer_a_passed: bool,
    pub layer_b: LayerBMetrics,
    pub layer_c_mismatches: usize,
    pub layer_c_passed: bool,
    pub layer_d_applicable: bool,
    pub layer_d_mismatches: usize,
    pub layer_d_passed: bool,
    pub passed: bool,
}

fn yaml_escape(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

impl NullTestReport {
    /// Render this report as clean, human-readable YAML suitable for attaching to a bug report.
    pub fn to_yaml(&self) -> String {
        let mut s = String::with_capacity(2048);
        s.push_str("schema: ap.null_test/1\n");
        s.push_str(&format!("timestamp_unix_ms: {}\n", self.timestamp_unix_ms));
        s.push_str(&format!("passed: {}\n", self.passed));
        s.push_str("track:\n");
        s.push_str(&format!("  path: {}\n", yaml_escape(&self.track_path)));
        s.push_str(&format!("  seed: {}\n", self.seed));
        s.push_str(&format!("  md5: {}\n", yaml_escape(&self.md5)));
        s.push_str(&format!("  excerpted: {}\n", self.excerpted));
        s.push_str(&format!("  source_frames: {}\n", self.source_frames));
        s.push_str(&format!("  output_frames: {}\n", self.output_frames));
        s.push_str("plan:\n");
        s.push_str(&format!("  filter: {}\n", yaml_escape(&self.filter)));
        s.push_str(&format!("  f_eff_hz: {}\n", self.f_eff_hz));
        s.push_str(&format!(
            "  convolver_hash: {}\n",
            yaml_escape(&self.convolver_hash)
        ));
        s.push_str(&format!("  source_rate_hz: {}\n", self.source_rate_hz));
        s.push_str(&format!("  internal_rate_hz: {}\n", self.internal_rate_hz));
        s.push_str(&format!("  output_rate_hz: {}\n", self.output_rate_hz));
        s.push_str(&format!("  deemphasis: {}\n", self.deemphasis));
        s.push_str(&format!("  shaper: {}\n", yaml_escape(&self.shaper)));
        s.push_str(&format!("  dac_bits: {}\n", self.dac_bits));
        s.push_str(&format!("  wire_format: {}\n", yaml_escape(&self.wire_format)));
        s.push_str(&format!("  gain_db: {:.6}\n", self.gain_db));
        s.push_str(&format!("  latency_samples: {}\n", self.latency_samples));
        s.push_str("layer_a_linear:\n");
        s.push_str(&format!(
            "  peak_residual_db: {:.3}\n",
            self.layer_a_peak_residual_db
        ));
        s.push_str(&format!(
            "  rms_residual_db: {:.3}\n",
            self.layer_a_rms_residual_db
        ));
        s.push_str(&format!("  passed: {}\n", self.layer_a_passed));
        s.push_str("layer_b_quantizer:\n");
        s.push_str(&format!("  overs: {}\n", self.layer_b.overs_count));
        s.push_str(&format!(
            "  max_abs_err_lsb: {:.6}\n",
            self.layer_b.max_abs_err_lsb
        ));
        s.push_str(&format!(
            "  err_bound_lsb: {:.6}\n",
            self.layer_b.err_bound_lsb
        ));
        s.push_str(&format!(
            "  abs_mean_err_lsb: {:.6}\n",
            self.layer_b.abs_mean_err_lsb
        ));
        s.push_str(&format!(
            "  max_abs_xcorr: {:.6e}\n",
            self.layer_b.max_abs_xcorr
        ));
        s.push_str(&format!(
            "  max_coherence_excess_db: {:.3}\n",
            self.layer_b.max_coherence_excess_db
        ));
        s.push_str(&format!(
            "  ntf_max_dev_db: {:.3}\n",
            self.layer_b.ntf_max_dev_db
        ));
        s.push_str(&format!("  passed: {}\n", self.layer_b.passed));
        if !self.layer_b.failure_reasons.is_empty() {
            s.push_str("  failures:\n");
            for r in &self.layer_b.failure_reasons {
                s.push_str(&format!("    - {}\n", yaml_escape(r)));
            }
        }
        s.push_str("layer_c_wire:\n");
        s.push_str(&format!("  mismatches: {}\n", self.layer_c_mismatches));
        s.push_str(&format!("  passed: {}\n", self.layer_c_passed));
        s.push_str("layer_d_identity:\n");
        s.push_str(&format!("  applicable: {}\n", self.layer_d_applicable));
        s.push_str(&format!("  mismatches: {}\n", self.layer_d_mismatches));
        s.push_str(&format!("  passed: {}\n", self.layer_d_passed));
        s
    }
}

/// Default output directory for loopback null YAML reports (`<workspace>/target/null-test`).
pub fn default_report_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("null-test")
}

/// Write `report` as `<dir>/<timestamp>-<seed>.yaml`, creating parent directories automatically.
pub fn write_yaml(dir: &Path, report: &NullTestReport) -> Result<PathBuf, String> {
    fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create report dir '{}': {e}", dir.display()))?;
    let ts = if report.timestamp_unix_ms > 0 {
        report.timestamp_unix_ms
    } else {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    };
    let path = dir.join(format!("{ts}-{:016x}.yaml", report.seed));
    fs::write(&path, report.to_yaml())
        .map_err(|e| format!("failed to write report '{}': {e}", path.display()))?;
    Ok(path)
}
