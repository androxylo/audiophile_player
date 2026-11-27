// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! `ap-null` — standalone Layers A–D loopback null verifier CLI (`PIPELINE_TESTS_REVIEW.md` §4.3).
//!
//! Usage:
//!   ap-null [--file F | --library DIR --seed S --count N] [--full] [--conv-file IR]
//!           [--max-rate R] [--filter F] [--shaper S] [--dac-bits B] [--wire-format W]
//!           [--out-dir DIR]

use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use ap_core::config::ConvolutionConfig;
use ap_core::format::DacWireFormat;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile};
use ap_source::library::discover_flac_files;
use ap_verify::analog::{analyze_analog_loopback, read_wav_f64};
use ap_verify::pick::{
    pick_from_library, resolve_seed, track_from_env_or_library_or_fixture,
};
use ap_verify::report::{default_report_dir, write_yaml};
use ap_verify::runner::{NullRunConfig, run_loopback_null_file};

fn print_usage() {
    println!(
        "ap-null — Audiophile Player independent f64 loopback null verifier\n\
         \n\
         Usage:\n\
           ap-null [--file FILE | --library DIR [--seed SEED] [--count N|all]]\n\
                   [--full] [--conv-file WAV] [--max-rate HZ] [--filter NAME]\n\
                   [--shaper NAME] [--dac-bits 16..24] [--wire-format int32|int24in32|int24|int16]\n\
                   [--deemphasis] [--analog CAPTURE.WAV] [--out-dir DIR]\n"
    );
}

fn parse_wire_format(s: &str) -> Result<DacWireFormat, String> {
    match s.to_ascii_lowercase().as_str() {
        "int32" | "32" => Ok(DacWireFormat::Int32),
        "int24in32" | "24in32" => Ok(DacWireFormat::Int24In32),
        "int24" | "int24packed" | "24" => Ok(DacWireFormat::Int24Packed),
        "int16" | "16" => Ok(DacWireFormat::Int16),
        other => Err(format!("unknown --wire-format '{other}'")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("ap-null error: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut file: Option<PathBuf> = None;
    let mut library: Option<PathBuf> = None;
    let mut seed_arg: Option<u64> = None;
    let mut count_all = false;
    let mut count: usize = std::env::var("AP_NULL_COUNT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    let mut full = std::env::var("AP_NULL_FULL")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let mut out_dir = default_report_dir();
    let mut analog_capture: Option<PathBuf> = None;
    let mut base_cfg = NullRunConfig::default();

    let mut i = 0_usize;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                return Ok(true);
            }
            "--file" => {
                i += 1;
                file = Some(PathBuf::from(
                    args.get(i).ok_or("--file requires a path")?,
                ));
            }
            "--library" => {
                i += 1;
                library = Some(PathBuf::from(
                    args.get(i).ok_or("--library requires a directory")?,
                ));
            }
            "--seed" => {
                i += 1;
                let raw = args.get(i).ok_or("--seed requires a u64")?;
                seed_arg = Some(
                    raw.parse::<u64>()
                        .map_err(|e| format!("invalid --seed '{raw}': {e}"))?,
                );
            }
            "--count" => {
                i += 1;
                let raw = args.get(i).ok_or("--count requires a number or 'all'")?;
                if raw.eq_ignore_ascii_case("all") {
                    count_all = true;
                } else {
                    count = raw
                        .parse::<usize>()
                        .map_err(|e| format!("invalid --count '{raw}': {e}"))?
                        .max(1);
                }
            }
            "--full" => full = true,
            "--deemphasis" => base_cfg.deemphasis = true,
            "--conv-file" => {
                i += 1;
                let p = PathBuf::from(args.get(i).ok_or("--conv-file requires a WAV path")?);
                base_cfg.conv_config = ConvolutionConfig {
                    enabled: true,
                    file: Some(p),
                    ..ConvolutionConfig::default()
                };
            }
            "--max-rate" => {
                i += 1;
                let raw = args.get(i).ok_or("--max-rate requires Hz")?;
                base_cfg.max_rate_hz = Some(
                    raw.parse::<u32>()
                        .map_err(|e| format!("invalid --max-rate '{raw}': {e}"))?,
                );
            }
            "--filter" => {
                i += 1;
                let raw = args.get(i).ok_or("--filter requires a filter name")?;
                base_cfg.filter_override = Some(FilterArchetype::from_str(raw)?);
            }
            "--shaper" => {
                i += 1;
                let raw = args.get(i).ok_or("--shaper requires a shaper name")?;
                base_cfg.shaper_override = Some(NoiseShaperProfile::from_str(raw)?);
            }
            "--dac-bits" => {
                i += 1;
                let raw = args.get(i).ok_or("--dac-bits requires 16..=24")?;
                base_cfg.dac_bits = Some(
                    raw.parse::<u8>()
                        .map_err(|e| format!("invalid --dac-bits '{raw}': {e}"))?,
                );
            }
            "--wire-format" => {
                i += 1;
                let raw = args.get(i).ok_or("--wire-format requires a format")?;
                base_cfg.wire_format = parse_wire_format(raw)?;
            }
            "--analog" => {
                i += 1;
                analog_capture = Some(PathBuf::from(
                    args.get(i).ok_or("--analog requires a capture WAV path")?,
                ));
            }
            "--out-dir" => {
                i += 1;
                out_dir = PathBuf::from(args.get(i).ok_or("--out-dir requires a directory")?);
            }
            other => {
                return Err(format!("unknown argument '{other}' (use --help)"));
            }
        }
        i += 1;
    }

    base_cfg.full_track = full;
    let base_seed = resolve_seed(seed_arg);

    // Optional L12 manual analog loopback report mode (`--analog <capture.wav>`):
    // compares the ADC capture against the reference FLAC (or `--file`) without pass/fail.
    if let Some(cap_path) = analog_capture {
        let cap = read_wav_f64(&cap_path)?;
        let ref_path = match file {
            Some(f) => f,
            None => track_from_env_or_library_or_fixture(Some(base_seed))?.path,
        };
        let ref_decoded = ap_source::flac::decode_flac_full_memory(&ref_path)?;
        let rep = analyze_analog_loopback(&ref_decoded.audio.left, &cap.left, cap.sample_rate_hz);
        print!("{}", rep.to_yaml());
        return Ok(true);
    }

    let targets: Vec<(PathBuf, u64)> = if let Some(f) = file {
        vec![(f, base_seed)]
    } else if let Some(ref lib) = library {
        if count_all {
            let files = discover_flac_files(lib)?;
            files
                .into_iter()
                .enumerate()
                .map(|(idx, p)| (p, base_seed.wrapping_add(idx as u64)))
                .collect()
        } else {
            let mut list = Vec::with_capacity(count);
            for idx in 0..count {
                let s = base_seed.wrapping_add(idx as u64);
                let picked = pick_from_library(lib, s)?;
                list.push((picked.path, picked.seed));
            }
            list
        }
    } else {
        let mut list = Vec::with_capacity(count);
        for idx in 0..count {
            let s = base_seed.wrapping_add(idx as u64);
            let picked = track_from_env_or_library_or_fixture(Some(s))?;
            list.push((picked.path, picked.seed));
        }
        list
    };

    if targets.is_empty() {
        return Err("no FLAC files found to verify".to_string());
    }

    let mut all_passed = true;
    for (idx, (path, seed)) in targets.iter().enumerate() {
        let mut cfg = base_cfg.clone();
        cfg.seed = *seed;
        println!(
            "[{}/{}] Verifying '{}' (seed={})...",
            idx + 1,
            targets.len(),
            path.display(),
            seed
        );
        let report = run_loopback_null_file(path, &cfg)?;
        let yaml_path = write_yaml(&out_dir, &report)?;
        println!(
            "  result={} | Layer A: peak={:+.2} dB (rms={:+.2} dB) | Layer B: overs={} max|e|={:.3}/{:.3} LSB |rho|={:.2e} coh_ex={:+.2} dB ntf_dev={:.3} dB | Layer C: {} mm | report={}",
            if report.passed { "PASS" } else { "FAIL" },
            report.layer_a_peak_residual_db,
            report.layer_a_rms_residual_db,
            report.layer_b.overs_count,
            report.layer_b.max_abs_err_lsb,
            report.layer_b.err_bound_lsb,
            report.layer_b.max_abs_xcorr,
            report.layer_b.max_coherence_excess_db,
            report.layer_b.ntf_max_dev_db,
            report.layer_c_mismatches,
            yaml_path.display()
        );
        if !report.passed {
            for r in &report.layer_b.failure_reasons {
                eprintln!("    Layer B failure: {r}");
            }
            all_passed = false;
        }
    }

    Ok(all_passed)
}
