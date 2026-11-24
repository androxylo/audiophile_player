//! Resilient local FLAC library picker designed for very large libraries (20,000+ tracks).
//!
//! Instead of recursively scanning the entire library upfront, [`pick_random_flac_walk`]
//! performs a fast `O(depth)` weighted subfolder descent:
//! - Reads only the immediate entries of the current folder via [`read_dir_frame`].
//! - Weights the current folder's direct `.flac` files by `flacs.len()` and estimates immediate
//!   subdirectory weights via a cheap shallow entry-count peek (when `subdirs.len() <= 32`) so
//!   large branches are favored over tiny folders without doing a full recursive scan.
//! - Tracks visited canonical directory identities (`HashSet`) across all walkers to prevent
//!   symlink/junction loops and duplicate paths.
//! - Skips hidden/system entries (`.`-prefixed dotfiles and macOS AppleDouble `._*`,
//!   `$RECYCLE.BIN`, `System Volume Information`, and Windows `HIDDEN`/`SYSTEM` entries).
//! - Sorts directories and `.flac` files in deterministic case-insensitive natural order
//!   (numeric digit runs compared by value) shared with `ap-control`'s folder browser.

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEED_COUNTER: AtomicU64 = AtomicU64::new(1);

const MAX_WALK_DEPTH: usize = 32;
/// Maximum number of immediate subdirectories for which [`walk_pick_recursive`] performs a cheap
/// one-level shallow entry-count peek to approximate subtree weight.
const MAX_SHALLOW_PEEK_SUBDIRS: usize = 32;
/// Cap on entries counted during a shallow peek so huge flat folders stay bounded-time.
const MAX_SHALLOW_PEEK_ENTRIES: usize = 10_000;
/// Fallback per-subdirectory weight when a directory has more than [`MAX_SHALLOW_PEEK_SUBDIRS`]
/// immediate subfolders (keeps one loose `.flac` from outweighing an average album folder).
const DEFAULT_SUBDIR_WEIGHT: u64 = 10;

/// Immediate subdirectories and `.flac` files in one directory after hidden/system filtering
/// and case-insensitive natural sorting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirEntries {
    pub subdirs: Vec<PathBuf>,
    pub flacs: Vec<PathBuf>,
}

/// Resolve the user's default `~/Music` directory (`%USERPROFILE%\Music` on Windows).
pub fn default_user_music_dir() -> Option<PathBuf> {
    let home = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"))?;
    Some(PathBuf::from(home).join("Music"))
}

/// Returns `true` if `path` has a `.flac` extension (ASCII case-insensitive).
pub fn is_flac_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("flac"))
}

/// Returns `true` if a directory entry name should be excluded from library walks and browsing:
/// - dotfiles & macOS AppleDouble resource forks (`.` / `._*`)
/// - Windows system/trash roots (`$RECYCLE.BIN`, `System Volume Information`)
fn is_excluded_entry_name(name: &str) -> bool {
    name.starts_with('.')
        || name.eq_ignore_ascii_case("$RECYCLE.BIN")
        || name.eq_ignore_ascii_case("System Volume Information")
}

/// Returns `true` if a `DirEntry` should be skipped due to name or Windows `HIDDEN`/`SYSTEM`
/// file attributes on the entry itself (using `entry.metadata()` which does NOT traverse
/// junctions/volume mount points, so a normal junction pointing to a drive root like `D:\` is
/// preserved while hidden/system files and folders are excluded).
fn should_skip_dir_entry(entry: &fs::DirEntry) -> bool {
    let file_name = entry.file_name();
    let name = file_name.to_string_lossy();
    if is_excluded_entry_name(&name) {
        return true;
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        if let Ok(entry_meta) = entry.metadata() {
            if (entry_meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)) != 0
            {
                return true;
            }
        }
    }

    false
}

/// Compare two strings in case-insensitive natural order (contiguous ASCII digit runs compared
/// numerically without integer overflow), using raw lexicographical order as a deterministic
/// tiebreaker so the relation is a strict total order.
pub fn natural_str_cmp(a: &str, b: &str) -> CmpOrdering {
    let al = a.to_lowercase();
    let bl = b.to_lowercase();
    let mut ai = al.char_indices().peekable();
    let mut bi = bl.char_indices().peekable();

    while let (Some(&(a_idx, ac)), Some(&(b_idx, bc))) = (ai.peek(), bi.peek()) {
        if ac.is_ascii_digit() && bc.is_ascii_digit() {
            let a_end = consume_ascii_digits(&al, &mut ai, a_idx);
            let b_end = consume_ascii_digits(&bl, &mut bi, b_idx);
            let a_trimmed = al[a_idx..a_end].trim_start_matches('0');
            let b_trimmed = bl[b_idx..b_end].trim_start_matches('0');
            match a_trimmed
                .len()
                .cmp(&b_trimmed.len())
                .then_with(|| a_trimmed.cmp(b_trimmed))
            {
                CmpOrdering::Equal => continue,
                ord => return ord,
            }
        }
        ai.next();
        bi.next();
        match ac.cmp(&bc) {
            CmpOrdering::Equal => continue,
            ord => return ord,
        }
    }

    match (ai.peek().is_some(), bi.peek().is_some()) {
        (false, true) => CmpOrdering::Less,
        (true, false) => CmpOrdering::Greater,
        _ => a.cmp(b),
    }
}

fn consume_ascii_digits(
    s: &str,
    iter: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
    start: usize,
) -> usize {
    let mut end = start;
    while let Some(&(idx, ch)) = iter.peek() {
        if !ch.is_ascii_digit() {
            break;
        }
        end = idx + ch.len_utf8();
        iter.next();
    }
    let _ = s;
    end
}

/// Compare two paths by `file_name()` in case-insensitive natural order, falling back to full
/// OS path order as a final deterministic tiebreaker.
pub fn natural_path_cmp(a: &Path, b: &Path) -> CmpOrdering {
    let na = a
        .file_name()
        .unwrap_or_else(|| a.as_os_str())
        .to_string_lossy();
    let nb = b
        .file_name()
        .unwrap_or_else(|| b.as_os_str())
        .to_string_lossy();
    natural_str_cmp(&na, &nb).then_with(|| a.as_os_str().cmp(b.as_os_str()))
}

/// Compare two full paths component-by-component in case-insensitive natural order.
fn natural_full_path_cmp(a: &Path, b: &Path) -> CmpOrdering {
    let mut ac = a.components();
    let mut bc = b.components();
    loop {
        match (ac.next(), bc.next()) {
            (Some(ca), Some(cb)) => {
                let sa = ca.as_os_str().to_string_lossy();
                let sb = cb.as_os_str().to_string_lossy();
                match natural_str_cmp(&sa, &sb) {
                    CmpOrdering::Equal => continue,
                    ord => return ord,
                }
            }
            (None, Some(_)) => return CmpOrdering::Less,
            (Some(_), None) => return CmpOrdering::Greater,
            (None, None) => return a.as_os_str().cmp(b.as_os_str()),
        }
    }
}

/// Read ONLY the immediate subdirectories and `.flac` files of `dir`.
///
/// - Follows Windows NTFS junctions, volume mount points, and symlinks via `fs::metadata`.
/// - Skips hidden/system entries (`.`-prefixed dotfiles & macOS AppleDouble `._*`,
///   `$RECYCLE.BIN`, `System Volume Information`, and Windows `HIDDEN`/`SYSTEM` entries).
/// - Sorts both `subdirs` and `flacs` in deterministic case-insensitive natural order.
///
/// Shared by [`pick_random_flac_walk`], [`discover_flac_files`], [`SequentialFlacCursor`], and
/// `ap-control`'s HTTP folder browser so filtering and ordering never drift.
pub fn read_dir_frame(dir: &Path) -> io::Result<DirEntries> {
    let entries = fs::read_dir(dir)?;
    let mut subdirs = Vec::new();
    let mut flacs = Vec::new();

    for entry in entries.flatten() {
        if should_skip_dir_entry(&entry) {
            continue;
        }
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

    subdirs.sort_by(|a, b| natural_path_cmp(a, b));
    flacs.sort_by(|a, b| natural_path_cmp(a, b));

    Ok(DirEntries { subdirs, flacs })
}

fn canonical_dir_id(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// Cheap one-level shallow count of non-excluded immediate entries inside `dir`, capped at
/// [`MAX_SHALLOW_PEEK_ENTRIES`]. Returns `0` for empty or unreadable directories.
fn shallow_entry_weight(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| !should_skip_dir_entry(e))
        .take(MAX_SHALLOW_PEEK_ENTRIES)
        .count() as u64
}

/// Fast, scalable `O(depth)` random FLAC picker resilient to 20,000+ track libraries.
///
/// Picks a weighted random subfolder at each level without scanning the entire tree,
/// backtracking automatically past empty or non-FLAC folders and skipping symlink/junction cycles.
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
    let mut visited = HashSet::new();
    visited.insert(canonical_dir_id(root));
    walk_pick_recursive(root, &mut rng, &mut visited, 0).ok_or_else(|| {
        format!(
            "no .flac files found in '{}' or its subfolders",
            root.display()
        )
    })
}

fn walk_pick_recursive(
    dir: &Path,
    rng: &mut SplitMix64,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> Option<PathBuf> {
    if depth > MAX_WALK_DEPTH {
        return None;
    }
    let DirEntries { subdirs, mut flacs } = read_dir_frame(dir).ok()?;

    // Build weighted candidate choices:
    // - `Here` (direct `.flac` files in this folder) is weighted by `flacs.len()`.
    // - Each immediate subdirectory is weighted by a cheap 1-level shallow entry-count peek when
    //   `subdirs.len() <= MAX_SHALLOW_PEEK_SUBDIRS` (skipping completely empty folders immediately),
    //   or `DEFAULT_SUBDIR_WEIGHT` when there are many immediate subfolders.
    // This preserves O(depth) latency on 20,000+ track libraries while strongly reducing
    // over-sampling of single-track folders or loose files next to large collections.
    enum Choice {
        Here,
        Subdir(PathBuf),
    }

    let peek_subdirs = subdirs.len() <= MAX_SHALLOW_PEEK_SUBDIRS;
    let mut candidates: Vec<(Choice, u64)> =
        Vec::with_capacity(subdirs.len() + usize::from(!flacs.is_empty()));
    if !flacs.is_empty() {
        candidates.push((Choice::Here, flacs.len() as u64));
    }
    for sub in subdirs {
        let weight = if peek_subdirs {
            shallow_entry_weight(&sub)
        } else {
            DEFAULT_SUBDIR_WEIGHT
        };
        if weight > 0 {
            candidates.push((Choice::Subdir(sub), weight));
        }
    }

    // Weighted random selection without replacement so we descend into ONE weighted-random
    // branch on the happy path and backtrack cleanly only if a chosen branch has no `.flac` files.
    while !candidates.is_empty() {
        let total_weight: u64 = candidates.iter().map(|(_, w)| *w).sum();
        if total_weight == 0 {
            break;
        }
        let target = rng.next_u64() % total_weight;
        let mut acc = 0_u64;
        let mut pick_idx = candidates.len() - 1;
        for (i, (_, w)) in candidates.iter().enumerate() {
            acc += *w;
            if target < acc {
                pick_idx = i;
                break;
            }
        }

        let (choice, _) = candidates.remove(pick_idx);
        match choice {
            Choice::Here => {
                let idx = rng.next_usize(flacs.len());
                return Some(flacs.swap_remove(idx));
            }
            Choice::Subdir(sub) => {
                if !visited.insert(canonical_dir_id(&sub)) {
                    continue;
                }
                if let Some(found) = walk_pick_recursive(&sub, rng, visited, depth + 1) {
                    return Some(found);
                }
            }
        }
    }

    None
}

/// Recursively discover all `.flac` files (case-insensitive) under `root`, returned in
/// deterministic case-insensitive natural order without following symlink/junction cycles.
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
    let mut visited = HashSet::new();
    visited.insert(canonical_dir_id(root));
    collect_flacs_recursive(root, &mut out, &mut visited, 0);
    out.sort_by(|a, b| natural_full_path_cmp(a, b));
    Ok(out)
}

fn collect_flacs_recursive(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(frame) = read_dir_frame(dir) else {
        return;
    };
    out.extend(frame.flacs);
    for sub in frame.subdirs {
        if visited.insert(canonical_dir_id(&sub)) {
            collect_flacs_recursive(&sub, out, visited, depth + 1);
        }
    }
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
/// Walks directories in deterministic case-insensitive natural order keeping only the active
/// ancestor stack in memory (never scanning the entire tree upfront). Yields `.flac` files in the
/// current directory first, then descends into immediate subdirectories while skipping previously
/// visited canonical directories so symlink/junction cycles never repeat tracks.
#[derive(Debug)]
pub struct SequentialFlacCursor {
    root: PathBuf,
    stack: Vec<DirFrame>,
    visited: HashSet<PathBuf>,
    recursive: bool,
}

#[derive(Debug)]
struct DirFrame {
    flacs: Vec<PathBuf>,
    flac_idx: usize,
    subdirs: Vec<PathBuf>,
    subdir_idx: usize,
}

impl From<DirEntries> for DirFrame {
    fn from(entries: DirEntries) -> Self {
        Self {
            flacs: entries.flacs,
            flac_idx: 0,
            subdirs: entries.subdirs,
            subdir_idx: 0,
        }
    }
}

impl SequentialFlacCursor {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        if !root.exists() {
            return Err(format!("music directory does not exist: '{}'", root.display()));
        }
        if !root.is_dir() {
            return Err(format!("music path is not a directory: '{}'", root.display()));
        }
        let mut visited = HashSet::new();
        visited.insert(canonical_dir_id(&root));
        let first = DirFrame::from(read_dir_frame(&root).unwrap_or_default());
        Ok(Self {
            root,
            stack: vec![first],
            visited,
            recursive: true,
        })
    }

    /// Create a cursor iterating ONLY the `.flac` files directly inside `root` (no recursion
    /// into subdirectories), optionally starting immediately after `start_after`.
    ///
    /// If `start_after` is missing (e.g. renamed or deleted), falls back to the first file in
    /// `root` that sorts after `start_after` without walking subdirectories.
    pub fn new_folder_only(root: PathBuf, start_after: Option<&Path>) -> Result<Self, String> {
        let mut cursor = Self::new(root)?;
        cursor.recursive = false;
        if let (Some(frame), Some(after)) = (cursor.stack.first_mut(), start_after) {
            frame.flac_idx = index_after_path(&frame.flacs, after);
        }
        Ok(cursor)
    }

    /// Create a recursive sequential cursor positioned immediately after `start_after` in
    /// `O(depth)` time without scanning unrelated subtrees.
    ///
    /// If `start_after` is not inside `root` (or was deleted), positions cleanly at the next
    /// path sorting after it (or the start of `root` if unrelated) instead of draining the tree.
    pub fn new_starting_after(root: PathBuf, start_after: &Path) -> Result<Self, String> {
        if !root.exists() {
            return Err(format!("music directory does not exist: '{}'", root.display()));
        }
        if !root.is_dir() {
            return Err(format!("music path is not a directory: '{}'", root.display()));
        }

        let Ok(rel) = start_after.strip_prefix(&root) else {
            return Self::new(root);
        };
        let components: Vec<_> = rel.components().collect();
        if components.is_empty() {
            return Self::new(root);
        }

        // Components 0..n-1 are subdirectories under `root`; the last component is the file.
        let dir_components = &components[..components.len().saturating_sub(1)];
        let mut stack = Vec::with_capacity(dir_components.len() + 1);
        let mut visited = HashSet::new();
        visited.insert(canonical_dir_id(&root));
        let mut cur_dir = root.clone();

        for comp in dir_components {
            let mut frame = DirFrame::from(read_dir_frame(&cur_dir).unwrap_or_default());
            // All files directly in `cur_dir` come before descending into `next_dir`.
            frame.flac_idx = frame.flacs.len();
            let next_dir = cur_dir.join(comp.as_os_str());
            match frame.subdirs.iter().position(|d| d == &next_dir) {
                Some(idx) => {
                    frame.subdir_idx = idx + 1;
                    stack.push(frame);
                    cur_dir = next_dir;
                    visited.insert(canonical_dir_id(&cur_dir));
                }
                None => {
                    // Subdirectory was deleted/renamed: continue with the first subdir sorting after it.
                    frame.subdir_idx = index_after_path(&frame.subdirs, &next_dir);
                    stack.push(frame);
                    return Ok(Self {
                        root,
                        stack,
                        visited,
                        recursive: true,
                    });
                }
            }
        }

        let mut leaf = DirFrame::from(read_dir_frame(&cur_dir).unwrap_or_default());
        leaf.flac_idx = index_after_path(&leaf.flacs, start_after);
        stack.push(leaf);

        Ok(Self {
            root,
            stack,
            visited,
            recursive: true,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the next `.flac` file in naturally sorted sequential order, or `None` when the
    /// folder tree has been completely played through.
    pub fn next_track(&mut self) -> Option<PathBuf> {
        loop {
            let depth = self.stack.len();
            let top = self.stack.last_mut()?;
            if top.flac_idx < top.flacs.len() {
                let path = top.flacs[top.flac_idx].clone();
                top.flac_idx += 1;
                return Some(path);
            }
            if self.recursive && top.subdir_idx < top.subdirs.len() && depth <= MAX_WALK_DEPTH {
                let next_dir = top.subdirs[top.subdir_idx].clone();
                top.subdir_idx += 1;
                if !self.visited.insert(canonical_dir_id(&next_dir)) {
                    continue;
                }
                self.stack
                    .push(DirFrame::from(read_dir_frame(&next_dir).unwrap_or_default()));
                continue;
            }
            self.stack.pop();
        }
    }
}

fn index_after_path(sorted: &[PathBuf], target: &Path) -> usize {
    if let Some(pos) = sorted.iter().position(|p| p.as_path() == target) {
        return pos + 1;
    }
    sorted.partition_point(|p| natural_path_cmp(p, target) != CmpOrdering::Greater)
}
