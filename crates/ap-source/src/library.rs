//! Resilient local FLAC library picker designed for very large libraries (20,000+ tracks).
//!
//! Instead of recursively scanning the entire library upfront, [`pick_random_flac_walk`]
//! performs a fast `O(depth)` randomized subfolder descent:
//! - Reads only the immediate entries of the current folder.
//! - Picks a random subfolder to descend into (or stays if the folder contains `.flac` files).
//! - Backtracks cleanly if a chosen subfolder contains no `.flac` files (e.g. empty or
//!   artwork-only folders), never scanning unrelated branches of the directory tree.
//! - Once a folder with `.flac` files is selected, picks a random `.flac` from that folder.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEED_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Resolve the user's default `~/Music` directory (`%USERPROFILE%\Music` on Windows).
pub fn default_user_music_dir() -> Option<PathBuf> {
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"))?;
    Some(PathBuf::from(home).join("Music"))
}

/// Fast, scalable `O(depth)` random FLAC picker resilient to 20,000+ track libraries.
///
/// Picks a random subfolder at each level without scanning the entire tree, backtracking
/// automatically past empty or non-FLAC folders.
pub fn pick_random_flac_walk(root: &Path) -> Result<PathBuf, String> {
    let seed = next_entropy_seed(root as *const Path as *const () as usize as u64);
    pick_random_flac_walk_with_seed(root, seed)
}

/// Deterministic seeded version of [`pick_random_flac_walk`] for repeatable TDD tests.
pub fn pick_random_flac_walk_with_seed(root: &Path, seed: u64) -> Result<PathBuf, String> {
    if !root.exists() {
        return Err(format!("music directory does not exist: '{}'", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("music path is not a directory: '{}'", root.display()));
    }

    let mut rng = SplitMix64::new(seed);
    walk_pick_recursive(root, &mut rng, 0).ok_or_else(|| {
        format!(
            "no .flac files found in '{}' or its subfolders",
            root.display()
        )
    })
}

const MAX_WALK_DEPTH: usize = 32;

fn walk_pick_recursive(dir: &Path, rng: &mut SplitMix64, depth: usize) -> Option<PathBuf> {
    if depth > MAX_WALK_DEPTH {
        return None;
    }
    let entries = fs::read_dir(dir).ok()?;

    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut flacs: Vec<PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        // Follow Windows NTFS junctions / volume mount points / symlinks via fs::metadata.
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            subdirs.push(path);
        } else if meta.is_file() && is_flac_path(&path) {
            flacs.push(path);
        }
    }

    // Sort first so deterministic seeds behave identically regardless of filesystem order.
    subdirs.sort();
    flacs.sort();

    // Build randomized candidate order: each immediate subfolder + one "this directory's flacs" slot
    // (if this directory directly contains any .flac files).
    enum Choice {
        Here,
        Subdir(PathBuf),
    }

    let mut choices: Vec<Choice> = Vec::with_capacity(subdirs.len() + usize::from(!flacs.is_empty()));
    if !flacs.is_empty() {
        choices.push(Choice::Here);
    }
    for s in subdirs {
        choices.push(Choice::Subdir(s));
    }

    if choices.is_empty() {
        return None;
    }

    // Fisher-Yates shuffle of only this single folder's immediate choices; try in random order
    // so we descend into just ONE random subfolder on the happy path, and backtrack cleanly
    // only if that branch had no .flac files.
    rng.shuffle(&mut choices);

    for choice in choices {
        match choice {
            Choice::Here => {
                let idx = rng.next_usize(flacs.len());
                return Some(flacs.swap_remove(idx));
            }
            Choice::Subdir(sub) => {
                if let Some(found) = walk_pick_recursive(&sub, rng, depth + 1) {
                    return Some(found);
                }
            }
        }
    }

    None
}

/// Recursively discover all `.flac` files (case-insensitive) under `root`, returned sorted.
///
/// Intended for small directories/tests; for large user libraries (20,000+ tracks) prefer
/// [`pick_random_flac_walk`].
pub fn discover_flac_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Err(format!("music directory does not exist: '{}'", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("music path is not a directory: '{}'", root.display()));
    }

    let mut out = Vec::new();
    collect_flacs_recursive(root, &mut out, 0);
    out.sort();
    Ok(out)
}

fn collect_flacs_recursive(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            collect_flacs_recursive(&path, out, depth + 1);
        } else if meta.is_file() && is_flac_path(&path) {
            out.push(path);
        }
    }
}

fn is_flac_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("flac"))
}

/// Compact SplitMix64 PRNG (zero external crate dependencies).
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound
    }

    fn shuffle<T>(&mut self, slice: &mut [T]) {
        for i in (1..slice.len()).rev() {
            let j = self.next_usize(i + 1);
            slice.swap(i, j);
        }
    }
}

fn next_entropy_seed(salt: u64) -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x55AA_55AA_55AA_55AA);
    let seq = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    nanos ^ pid.rotate_left(17) ^ seq.rotate_left(29) ^ salt.rotate_left(41)
}

/// Deterministically pick a track from `files` given an explicit `seed` (ideal for TDD).
pub fn pick_random_flac_with_seed<'a>(files: &'a [PathBuf], seed: u64) -> Option<&'a PathBuf> {
    if files.is_empty() {
        return None;
    }
    let mut rng = SplitMix64::new(seed);
    Some(&files[rng.next_usize(files.len())])
}

/// Pick a random track from `files` seeded from high-resolution wall-clock time & PID.
pub fn pick_random_flac<'a>(files: &'a [PathBuf]) -> Option<&'a PathBuf> {
    if files.is_empty() {
        return None;
    }
    let seed = next_entropy_seed(files.as_ptr() as usize as u64);
    pick_random_flac_with_seed(files, seed)
}

/// Lazy, bounded-memory sequential `.flac` cursor resilient to 20,000+ track libraries.
///
/// Walks directories in deterministic sorted order keeping only the active ancestor stack
/// in memory (never scanning the entire tree upfront). Yields `.flac` files in the current
/// directory first, then descends into immediate subdirectories in alphabetical order.
#[derive(Debug)]
pub struct SequentialFlacCursor {
    root: PathBuf,
    stack: Vec<DirFrame>,
}

#[derive(Debug)]
struct DirFrame {
    flacs: Vec<PathBuf>,
    flac_idx: usize,
    subdirs: Vec<PathBuf>,
    subdir_idx: usize,
}

impl SequentialFlacCursor {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        if !root.exists() {
            return Err(format!("music directory does not exist: '{}'", root.display()));
        }
        if !root.is_dir() {
            return Err(format!("music path is not a directory: '{}'", root.display()));
        }
        let first = read_dir_frame(&root);
        Ok(Self {
            root,
            stack: vec![first],
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the next `.flac` file in sorted sequential order, or `None` when the folder tree
    /// has been completely played through.
    pub fn next_track(&mut self) -> Option<PathBuf> {
        loop {
            let depth = self.stack.len();
            let top = self.stack.last_mut()?;
            if top.flac_idx < top.flacs.len() {
                let path = top.flacs[top.flac_idx].clone();
                top.flac_idx += 1;
                return Some(path);
            }
            if top.subdir_idx < top.subdirs.len() && depth <= MAX_WALK_DEPTH {
                let next_dir = top.subdirs[top.subdir_idx].clone();
                top.subdir_idx += 1;
                self.stack.push(read_dir_frame(&next_dir));
                continue;
            }
            self.stack.pop();
        }
    }
}

fn read_dir_frame(dir: &Path) -> DirFrame {
    let mut flacs = Vec::new();
    let mut subdirs = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                subdirs.push(path);
            } else if meta.is_file() && is_flac_path(&path) {
                flacs.push(path);
            }
        }
    }
    flacs.sort();
    subdirs.sort();
    DirFrame {
        flacs,
        flac_idx: 0,
        subdirs,
        subdir_idx: 0,
    }
}
