// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Track selection, seeded excerpting, and seeded random chunk sizing for loopback null tests
//! (`PIPELINE_TESTS_REVIEW.md` §2.2).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ap_core::config::PlayerConfig;
use ap_source::library::{default_user_music_dir, pick_random_flac_walk_with_seed};

/// Default excerpt prefix duration in seconds (`15 s`).
pub const DEFAULT_EXCERPT_HEAD_SECONDS: usize = 15;

/// Default excerpt suffix duration in seconds (`5 s`, covering EOF flush and tail).
pub const DEFAULT_EXCERPT_TAIL_SECONDS: usize = 5;

/// Origin of a track selected for loopback null verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackOrigin {
    EnvFile,
    ExplicitFile,
    LibraryRandom { root: PathBuf, seed: u64 },
    FixtureFallback,
}

/// Selected FLAC track and its deterministic provenance.
#[derive(Debug, Clone)]
pub struct PickedTrack {
    pub path: PathBuf,
    pub seed: u64,
    pub origin: TrackOrigin,
}

/// Path to the bundled GPL-compatible 24/96 Nocturne FLAC fixture.
pub fn bundled_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join("original_nocturne_24_96.flac")
}

/// Generate a fresh non-zero seed from wall-clock nanoseconds (always printed to stdout/YAML).
pub fn fresh_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x2026_1126_9E37_79B9);
    let mixed = nanos ^ (u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    if mixed == 0 { 0xA5A5_5A5A_1234_5678 } else { mixed }
}

/// Resolve the seed from `seed_override`, else `AP_NULL_SEED`, else a fresh timestamp seed.
pub fn resolve_seed(seed_override: Option<u64>) -> u64 {
    if let Some(s) = seed_override {
        return s;
    }
    if let Ok(raw) = std::env::var("AP_NULL_SEED") {
        let trimmed = raw.trim();
        if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
            if let Ok(v) = u64::from_str_radix(hex, 16) {
                return v;
            }
        }
        if let Ok(v) = trimmed.parse::<u64>() {
            return v;
        }
    }
    fresh_seed()
}

/// Priority track picker (`PIPELINE_TESTS_REVIEW.md` §2.2):
/// 1. `AP_NULL_FILE=<path>`
/// 2. Random FLAC from `AP_NULL_LIBRARY`, else `[player] music_dir`, else `~/Music`
/// 3. Bundled fixture `tests/fixtures/audio/original_nocturne_24_96.flac`
pub fn track_from_env_or_library_or_fixture(
    seed_override: Option<u64>,
) -> Result<PickedTrack, String> {
    let seed = resolve_seed(seed_override);

    if let Ok(file) = std::env::var("AP_NULL_FILE") {
        let p = PathBuf::from(file.trim());
        if !p.as_os_str().is_empty() {
            if !p.is_file() {
                return Err(format!("AP_NULL_FILE does not exist: '{}'", p.display()));
            }
            return Ok(PickedTrack {
                path: p,
                seed,
                origin: TrackOrigin::EnvFile,
            });
        }
    }

    let library_root = std::env::var("AP_NULL_LIBRARY")
        .ok()
        .map(|s| PathBuf::from(s.trim().to_string()))
        .filter(|p| p.is_dir())
        .or_else(|| {
            PlayerConfig::discover_and_load(None)
                .ok()
                .and_then(|loaded| loaded.config.player.music_dir)
                .filter(|p| p.is_dir())
        })
        .or_else(|| default_user_music_dir().filter(|p| p.is_dir()));

    if let Some(root) = library_root {
        if let Ok(picked) = pick_random_flac_walk_with_seed(&root, seed) {
            return Ok(PickedTrack {
                path: picked,
                seed,
                origin: TrackOrigin::LibraryRandom { root, seed },
            });
        }
    }

    let fixture = bundled_fixture_path();
    if !fixture.is_file() {
        return Err(format!(
            "bundled fixture missing at '{}'",
            fixture.display()
        ));
    }
    Ok(PickedTrack {
        path: fixture,
        seed,
        origin: TrackOrigin::FixtureFallback,
    })
}

/// Extract default 20 s excerpt (first 15 s + last 5 s so flush & tail are always covered),
/// or return the full track when `full == true` or `left.len() <= 20 s`.
pub fn extract_excerpt(
    left: &[f64],
    right: &[f64],
    sample_rate_hz: u32,
    full: bool,
) -> (Vec<f64>, Vec<f64>, bool) {
    let n = left.len().min(right.len());
    let head = (sample_rate_hz as usize) * DEFAULT_EXCERPT_HEAD_SECONDS;
    let tail = (sample_rate_hz as usize) * DEFAULT_EXCERPT_TAIL_SECONDS;
    if full || n <= head + tail {
        return (left[..n].to_vec(), right[..n].to_vec(), false);
    }
    let mut out_l = Vec::with_capacity(head + tail);
    let mut out_r = Vec::with_capacity(head + tail);
    out_l.extend_from_slice(&left[..head]);
    out_l.extend_from_slice(&left[n - tail..n]);
    out_r.extend_from_slice(&right[..head]);
    out_r.extend_from_slice(&right[n - tail..n]);
    (out_l, out_r, true)
}

/// Deterministic SplitMix64 generator producing varied streaming chunk sizes covering small,
/// prime, odd, and large blocks summing exactly to `total_frames`.
pub fn random_chunk_sizes(total_frames: usize, seed: u64) -> Vec<usize> {
    let mut st = if seed == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        seed
    };
    let mut next_u64 = || -> u64 {
        st = st.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = st;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };

    let mut sizes = Vec::new();
    let mut rem = total_frames;
    while rem > 0 {
        // Mix tiny (1..=37), medium (128..=4096), and large (4097..=24576) chunks so every
        // ring-buffer wrap and UPOLS partition boundary gets exercised.
        let bucket = next_u64() % 10;
        let ask = match bucket {
            0 => 1 + (next_u64() % 37) as usize,
            1..=4 => 128 + (next_u64() % 3_969) as usize,
            _ => 4_097 + (next_u64() % 20_480) as usize,
        };
        let take = ask.min(rem);
        sizes.push(take);
        rem -= take;
    }
    sizes
}

/// Discover a random track from an explicit library root using `seed`.
pub fn pick_from_library(root: &Path, seed: u64) -> Result<PickedTrack, String> {
    let path = pick_random_flac_walk_with_seed(root, seed)?;
    Ok(PickedTrack {
        path,
        seed,
        origin: TrackOrigin::LibraryRandom {
            root: root.to_path_buf(),
            seed,
        },
    })
}
