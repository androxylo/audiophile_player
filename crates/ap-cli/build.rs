// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Build script embedding git commit hash and UTC build date into `audiophile-player --version --json`.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=AP_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=AP_BUILD_DATE");

    // Track git HEAD and its target ref so AP_GIT_COMMIT / AP_BUILD_DATE stay fresh across commits/tags.
    let git_dir = std::path::Path::new("../../.git");
    let head_path = git_dir.join("HEAD");
    if head_path.is_file() {
        println!("cargo:rerun-if-changed={}", head_path.display());
        if let Ok(head_text) = std::fs::read_to_string(&head_path) {
            if let Some(ref_rel) = head_text.trim().strip_prefix("ref: ") {
                let ref_path = git_dir.join(ref_rel.trim());
                if ref_path.is_file() {
                    println!("cargo:rerun-if-changed={}", ref_path.display());
                }
            }
        }
        let packed_refs = git_dir.join("packed-refs");
        if packed_refs.is_file() {
            println!("cargo:rerun-if-changed={}", packed_refs.display());
        }
    }

    let commit = std::env::var("AP_GIT_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let build_date = std::env::var("AP_BUILD_DATE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["log", "-1", "--format=%cs"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=AP_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=AP_BUILD_DATE={build_date}");
}
