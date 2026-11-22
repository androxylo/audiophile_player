//! Recursive local FLAC directory discovery and random track selection.
//!
//! Zero external crate dependencies: uses deterministic SplitMix64 mixing seeded from
//! system time + process ID for random playback selection, with a deterministic seeded
//! variant for repeatable TDD unit tests.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Resolve the user's default `~/Music` directory (`%USERPROFILE%\Music` on Windows).
pub fn default_user_music_dir() -> Option<PathBuf> {
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"))?;
    Some(PathBuf::from(home).join("Music"))
}

/// Recursively discover all `.flac` files (case-insensitive) under `root`, returned sorted.
pub fn discover_flac_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Err(format!("music directory does not exist: '{}'", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("music path is not a directory: '{}'", root.display()));
    }

    let mut out = Vec::new();
    collect_flacs_recursive(root, &mut out);
    out.sort();
    Ok(out)
}

fn collect_flacs_recursive(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            collect_flacs_recursive(&path, out);
        } else if ft.is_file() && is_flac_path(&path) {
            out.push(path);
        }
    }
}

fn is_flac_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("flac"))
}

/// Deterministic SplitMix64 hash for unbiased index selection without external RNG crates.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministically pick a track from `files` given an explicit `seed` (ideal for TDD).
pub fn pick_random_flac_with_seed<'a>(files: &'a [PathBuf], seed: u64) -> Option<&'a PathBuf> {
    if files.is_empty() {
        return None;
    }
    let idx = (splitmix64(seed) as usize) % files.len();
    Some(&files[idx])
}

/// Pick a random track from `files` seeded from high-resolution wall-clock time & PID.
pub fn pick_random_flac<'a>(files: &'a [PathBuf]) -> Option<&'a PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x55AA_55AA_55AA_55AA);
    let pid = u64::from(std::process::id());
    let addr = files.as_ptr() as usize as u64;
    let seed = nanos ^ pid.rotate_left(17) ^ addr.rotate_left(31);
    pick_random_flac_with_seed(files, seed)
}
