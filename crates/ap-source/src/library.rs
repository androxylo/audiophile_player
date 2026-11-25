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
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SEED_COUNTER: AtomicU64 = AtomicU64::new(1);

const MAX_WALK_DEPTH: usize = 32;
/// Maximum number of immediate subdirectories for which [`walk_pick_recursive`] performs a
/// one-time cached shallow FLAC/subdir weight peek on a cold directory.
const MAX_SHALLOW_PEEK_SUBDIRS: usize = 32;
/// Fallback per-subdirectory weight for unvisited subdirectories when a directory has more than
/// [`MAX_SHALLOW_PEEK_SUBDIRS`] immediate subfolders (updated lazily once visited).
const DEFAULT_SUBDIR_WEIGHT: u64 = 10;
/// Maximum cached directory weight entries before evicting the cache to keep RAM bounded.
const MAX_WEIGHT_CACHE_ENTRIES: usize = 8_192;
/// Maximum symlink/junction chain hops resolved when checking for unsafe UNC/device targets.
const MAX_SYMLINK_HOPS: usize = 8;

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
/// - macOS AppleDouble resource forks (`._*`)
/// - known OS/indexer metadata files & folders (`.DS_Store`, `.AppleDouble`, `.Trash`,
///   `.Trashes`, `.Spotlight-V100`, `.fseventsd`, `.TemporaryItems`, `.git`, `.svn`, `.hg`,
///   `$RECYCLE.BIN`, `System Volume Information`, `Thumbs.db`, `desktop.ini`)
///
/// Intentionally does **not** reject arbitrary leading-`.` names so real albums and tracks such
/// as `...And Justice for All`, `...Like Clockwork`, and `...Baby One More Time.flac` remain
/// visible; real hidden entries on Windows are filtered via `FILE_ATTRIBUTE_HIDDEN`/`SYSTEM`.
fn is_excluded_entry_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("._")
        || lower.starts_with(".trash")
        || lower.starts_with(".@__")
    {
        return true;
    }
    const EXCLUDED: &[&str] = &[
        ".ds_store",
        ".appledouble",
        ".spotlight-v100",
        ".fseventsd",
        ".temporaryitems",
        ".git",
        ".svn",
        ".hg",
        ".recycle",
        ".recycle.bin",
        "#recycle",
        "$recycle.bin",
        ".stversions",
        ".stfolder",
        ".snapshot",
        ".snapshots",
        ".zfs",
        "@eadir",
        "system volume information",
        "thumbs.db",
        "desktop.ini",
    ];
    EXCLUDED.iter().any(|&ex| lower == ex)
}

/// Returns `true` if a `DirEntry` should be skipped due to name or Windows `HIDDEN`/`SYSTEM`
/// file attributes on the entry itself (using `entry.metadata()` which does NOT traverse
/// junctions/volume mount points, so a normal junction pointing to a drive root like `D:\` is
/// preserved).
///
/// Review #3 #8 + Review #4 #5:
/// - Always excludes known metadata, recycle-bin, snapshot, and sync-version folders (`.recycle`,
///   `.stversions`, `.stfolder`, `.Trash-*`, `.Recycle.Bin`, `.snapshot`, `.zfs`, `.@__thumb`,
///   `@eaDir`, `#recycle`, `$RECYCLE.BIN`, `.git`, `._*`, etc.) regardless of attributes.
/// - Always excludes `FILE_ATTRIBUTE_SYSTEM` entries and non-dot `FILE_ATTRIBUTE_HIDDEN` entries.
/// - For dot-prefixed `FILE_ATTRIBUTE_HIDDEN` entries, exempts ONLY ellipsis names (`..*`, e.g.
///   `...And Justice for All`) or entries on a remote UNC/SMB share (`is_unc_dir`, where Samba
///   `hide dot files = yes` marks legitimate dot-prefixed albums hidden). Local hidden single-dot
///   folders remain skipped.
fn should_skip_dir_entry(entry: &fs::DirEntry, is_unc_dir: bool) -> bool {
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
            let attrs = entry_meta.file_attributes();
            if (attrs & FILE_ATTRIBUTE_SYSTEM) != 0 {
                return true;
            }
            if (attrs & FILE_ATTRIBUTE_HIDDEN) != 0 {
                let is_exempt_dot = name.starts_with("..") || (is_unc_dir && name.starts_with('.'));
                if !is_exempt_dot {
                    return true;
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = is_unc_dir;

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

/// Extract `(server, share)` (lowercased) if `path` is a standard or verbatim UNC path
/// (`\\server\share...` or `\\?\UNC\server\share...`, case-insensitive on `UNC`), or `None`
/// for local paths.
pub fn extract_unc_share(path: &Path) -> Option<(String, String)> {
    let raw = path.to_string_lossy();
    let rest = if raw.len() >= 8
        && (raw.starts_with(r"\\?\") || raw.starts_with("//?/"))
        && raw[4..8].eq_ignore_ascii_case(r"unc\")
        || (raw.len() >= 8
            && (raw.starts_with(r"\\?\") || raw.starts_with("//?/"))
            && raw[4..8].eq_ignore_ascii_case("unc/"))
    {
        &raw[8..]
    } else if (raw.starts_with(r"\\") || raw.starts_with("//"))
        && !(raw.len() >= 4 && matches!(raw.as_bytes()[2], b'.' | b'?') && matches!(raw.as_bytes()[3], b'\\' | b'/'))
    {
        &raw[2..]
    } else {
        return None;
    };
    let mut parts = rest.split(['\\', '/']).filter(|s| !s.is_empty());
    let server = parts.next()?;
    let share = parts.next()?;
    if server.contains(':') || share.contains(':') {
        return None;
    }
    Some((server.to_ascii_lowercase(), share.to_ascii_lowercase()))
}

/// Strict allow-list check for a symlink or Windows NTFS junction target (shared by `ap-source`
/// and `ap-control`).
///
/// Allows ONLY:
/// - Relative targets (no Windows prefix, no leading `\\` / `//` / `\??\`),
/// - Local disk paths (`C:\...`, `\\?\C:\...`),
/// - Local NTFS volume mount points (`\\?\Volume{...}\`),
/// - UNC targets matching `allowed_unc_share` (when a trusted UNC `--music-dir` is configured).
///
/// Rejects ALL untrusted UNC shares (including mixed-case `\\?\Unc\...` and
/// `\\?\Global\UNC\...`), NT object manager paths (`\??\...`), and Win32 device namespaces
/// (`\\.\...`, `\\?\Pipe\...`, `\\?\GLOBALROOT\...`) BEFORE any `fs::metadata` call.
pub fn check_reparse_target_is_allowed(
    target: &Path,
    allowed_unc_share: Option<(&str, &str)>,
) -> Result<(), String> {
    let raw_bytes = target.as_os_str().as_encoded_bytes();
    if raw_bytes.contains(&0) {
        return Err("reparse target contains NUL byte".to_string());
    }

    let raw = target.to_string_lossy();
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with(r"\??\") || lower.starts_with("/??/") {
        return Err("symlink/junction targeting NT object namespace is forbidden".into());
    }
    if lower.starts_with(r"\\.\") || lower.starts_with("//./") {
        return Err("symlink/junction targeting a device namespace is forbidden".into());
    }

    // Strict allow-list for Windows verbatim `\\?\...` targets (blocks `\\?\Global\UNC\...`,
    // `\\?\GLOBALROOT\...`, `\\?\Pipe\...`, and untrusted `\\?\Unc\...` case-insensitively).
    if lower.starts_with(r"\\?\") || lower.starts_with("//?/") {
        let after = &raw[4..];
        let after_lower = &lower[4..];
        if after_lower.starts_with(r"unc\") || after_lower.starts_with("unc/") {
            let Some((ts, tsh)) = extract_unc_share(target) else {
                return Err("symlink/junction targeting an invalid UNC path is forbidden".into());
            };
            if allowed_unc_share != Some((ts.as_str(), tsh.as_str())) {
                return Err("symlink/junction targeting a UNC network share is forbidden".into());
            }
            return Ok(());
        }
        let is_verbatim_disk = after.len() >= 2
            && after.as_bytes()[0].is_ascii_alphabetic()
            && after.as_bytes()[1] == b':'
            && (after.len() == 2 || matches!(after.as_bytes()[2], b'\\' | b'/'));
        let is_volume_guid = after_lower.starts_with("volume{");
        if !is_verbatim_disk && !is_volume_guid {
            return Err(
                "symlink/junction targeting a non-local verbatim path is forbidden".into(),
            );
        }
        return Ok(());
    }

    // Standard UNC (`\\server\share` or `//server/share`): allow ONLY the trusted root share.
    if raw.starts_with(r"\\") || raw.starts_with("//") {
        let Some((ts, tsh)) = extract_unc_share(target) else {
            return Err("symlink/junction targeting a UNC network share is forbidden".into());
        };
        if allowed_unc_share != Some((ts.as_str(), tsh.as_str())) {
            return Err("symlink/junction targeting a UNC network share is forbidden".into());
        }
        return Ok(());
    }

    // Reject prefix-less rooted paths (e.g. `\Device\Mup\evil\share`, `\GLOBAL??\...`,
    // `\RPC Control\...`, `\BaseNamedObjects\...`) which point into the Windows NT object
    // namespace when followed by the I/O manager (Review #4 Finding #21).
    if matches!(target.components().next(), Some(Component::RootDir))
        || raw.starts_with('\\')
        || raw.starts_with('/')
    {
        return Err(
            "symlink/junction targeting a rooted NT object or prefix-less path is forbidden".into(),
        );
    }

    // Defense-in-depth component Prefix allow-list.
    if let Some(Component::Prefix(p)) = target.components().next() {
        match p.kind() {
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {}
            Prefix::Verbatim(v) => {
                let vb = v.as_encoded_bytes();
                if !(vb.len() >= 7 && vb[..7].eq_ignore_ascii_case(b"Volume{")) {
                    return Err(
                        "symlink/junction targeting a non-local verbatim path is forbidden".into(),
                    );
                }
            }
            Prefix::UNC(s, sh) | Prefix::VerbatimUNC(s, sh) => {
                let ts = s.to_string_lossy().to_ascii_lowercase();
                let tsh = sh.to_string_lossy().to_ascii_lowercase();
                if allowed_unc_share != Some((ts.as_str(), tsh.as_str())) {
                    return Err(
                        "symlink/junction targeting a UNC network share is forbidden".into(),
                    );
                }
            }
            Prefix::DeviceNS(_) => {
                return Err("symlink/junction targeting a device namespace is forbidden".into());
            }
        }
    }

    Ok(())
}

/// Returns `true` when `meta` represents a symlink or Windows NTFS junction/mount point.
///
/// On Windows, Rust's `FileType::is_symlink()` checks `IsReparseTagNameSurrogate`, which is
/// already `true` for both `IO_REPARSE_TAG_SYMLINK` and `IO_REPARSE_TAG_MOUNT_POINT` (NTFS
/// junctions) and `false` for non-link reparse points such as OneDrive cloud placeholders.
pub fn is_symlink_or_mount_point(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// Purely lexical `.` / `..` resolution without touching the filesystem.
fn lexical_clean_path(path: &Path) -> PathBuf {
    let mut prefix_or_root = PathBuf::new();
    let mut has_root = false;
    let mut parts: Vec<std::ffi::OsString> = Vec::new();

    for comp in path.components() {
        match comp {
            Component::Prefix(p) => {
                prefix_or_root.push(p.as_os_str());
            }
            Component::RootDir => {
                prefix_or_root.push(comp.as_os_str());
                has_root = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                } else if !has_root {
                    parts.push(comp.as_os_str().to_os_string());
                }
            }
            Component::Normal(seg) => {
                parts.push(seg.to_os_string());
            }
        }
    }

    for part in parts {
        prefix_or_root.push(part);
    }
    prefix_or_root
}

/// Walk `resolved_path` component by component from its safe root prefix using `symlink_metadata`,
/// recursively inspecting and resolving EVERY intermediate AND final symlink/junction hop BEFORE
/// any metadata call can cross an unchecked intermediate reparse point (Review #4 Finding #4).
fn verify_resolved_path_components(
    resolved_path: &Path,
    allowed_unc_share: Option<(&str, &str)>,
    remaining_hops: usize,
) -> Result<(), String> {
    let clean = lexical_clean_path(resolved_path);
    let mut cur = PathBuf::new();
    let mut comps = clean.components().peekable();

    // Assemble safe prefix/root first without statting `C:\` or `\\NAS\Share\` as a link.
    while let Some(c) = comps.peek() {
        match c {
            Component::Prefix(_) | Component::RootDir => {
                cur.push(c.as_os_str());
                comps.next();
            }
            _ => break,
        }
    }

    while let Some(comp) = comps.next() {
        cur.push(comp.as_os_str());
        let Ok(meta) = fs::symlink_metadata(&cur) else {
            // Non-existent suffix is syntactically safe; downstream open will fail cleanly.
            return Ok(());
        };
        if is_symlink_or_mount_point(&meta) {
            if remaining_hops == 0 {
                return Err(format!(
                    "symlink/junction chain exceeded {MAX_SYMLINK_HOPS} hops at '{}'",
                    cur.display()
                ));
            }
            let target = fs::read_link(&cur).map_err(|e| {
                format!(
                    "cannot inspect symlink/junction target for '{}': {e}",
                    cur.display()
                )
            })?;
            check_reparse_target_is_allowed(&target, allowed_unc_share)?;

            let mut next = if target.is_absolute() {
                target
            } else {
                cur.parent()
                    .map(|p| p.join(&target))
                    .unwrap_or(target)
            };
            // Append any remaining suffix components after this intermediate reparse point and
            // continue walking component-by-component from the resolved target.
            for rest in comps {
                next.push(rest.as_os_str());
            }
            return verify_resolved_path_components(&next, allowed_unc_share, remaining_hops - 1);
        }
    }

    Ok(())
}

/// Walk a symlink/junction chain starting at `start_link` up to [`MAX_SYMLINK_HOPS`] hops
/// WITHOUT calling `fs::metadata` on unsafe hops or crossing unchecked intermediate links.
/// Returns `Ok(())` only if every hop target (including any intermediate reparse points along
/// resolved relative or local targets) satisfies [`check_reparse_target_is_allowed`] and the
/// chain terminates within the hop cap.
pub fn verify_symlink_chain_is_allowed(
    start_link: &Path,
    allowed_unc_share: Option<(&str, &str)>,
) -> Result<(), String> {
    let target = fs::read_link(start_link).map_err(|e| {
        format!(
            "cannot inspect symlink/junction target for '{}': {e}",
            start_link.display()
        )
    })?;
    check_reparse_target_is_allowed(&target, allowed_unc_share)?;
    let next = if target.is_absolute() {
        target
    } else {
        start_link
            .parent()
            .map(|p| p.join(&target))
            .unwrap_or(target)
    };
    verify_resolved_path_components(&next, allowed_unc_share, MAX_SYMLINK_HOPS.saturating_sub(1))
}

/// Read ONLY the immediate subdirectories and `.flac` files of `dir`.
///
/// - Uses `entry.file_type()` directly from directory enumeration (avoiding per-entry `CreateFile`
///   syscalls on Windows/SMB) and only resolves `fs::metadata` on actual symlinks / NTFS junctions
///   AFTER verifying the entire link chain does not target untrusted UNC or Win32 device paths.
/// - Skips excluded metadata entries (`._*`, `.DS_Store`, `.Trash`, `$RECYCLE.BIN`,
///   `System Volume Information`, Windows `SYSTEM` and non-dot `HIDDEN`) while preserving real
///   dot-prefixed albums and tracks (e.g. `...And Justice for All`), even on Samba NAS shares.
/// - Sorts both `subdirs` and `flacs` in deterministic case-insensitive natural order.
///
/// Shared by [`pick_random_flac_walk`], [`discover_flac_files`], [`SequentialFlacCursor`], and
/// `ap-control`'s HTTP folder browser so filtering and ordering never drift.
pub fn read_dir_frame(dir: &Path) -> io::Result<DirEntries> {
    let entries = fs::read_dir(dir)?;
    let allowed_unc = extract_unc_share(dir);
    let is_unc_dir = allowed_unc.is_some();
    let share_ref = allowed_unc
        .as_ref()
        .map(|(s, sh)| (s.as_str(), sh.as_str()));
    let mut subdirs = Vec::new();
    let mut flacs = Vec::new();

    for entry in entries.flatten() {
        if should_skip_dir_entry(&entry, is_unc_dir) {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_symlink() {
            // Inspect link chain BEFORE calling fs::metadata so a malicious symlink/junction to
            // `\\attacker\share` (direct or chained) never triggers outbound SMB auth.
            if verify_symlink_chain_is_allowed(&path, share_ref).is_err() {
                continue;
            }
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                subdirs.push(path);
            } else if meta.is_file() && is_flac_path(&path) {
                flacs.push(path);
            }
        } else if ft.is_dir() {
            subdirs.push(path);
        } else if ft.is_file() && is_flac_path(&path) {
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

/// Wall-clock TTL for cached directory weights (60 minutes — spans many 3–6 min random-play
/// track intervals so auto-advance stays hot and learned aggregates in the `> 32`-subdir branch
/// persist throughout a listening session, while never caching `0`).
const WEIGHT_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy)]
struct CachedDirWeight {
    stored_at: Instant,
    mtime: Option<std::time::SystemTime>,
    weight: u64,
}

fn dir_weight_cache() -> &'static Mutex<HashMap<PathBuf, CachedDirWeight>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedDirWeight>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Pure in-memory TTL lookup (zero `fs::metadata` syscalls) for the `> 32`-subdir branch.
fn lookup_cached_weight(dir: &Path) -> Option<u64> {
    let cache = dir_weight_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.get(dir).and_then(|entry| {
        if entry.weight > 0 && entry.stored_at.elapsed() < WEIGHT_CACHE_TTL {
            Some(entry.weight)
        } else {
            None
        }
    })
}

/// Mtime-validated TTL lookup for the `<= 32`-subdir shallow-peek branch (catches newly added
/// direct files immediately while avoiding `read_dir` on unchanged folders across 3–6 min tracks).
fn lookup_cached_weight_with_mtime(dir: &Path, current_mtime: Option<std::time::SystemTime>) -> Option<u64> {
    let cache = dir_weight_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.get(dir).and_then(|entry| {
        if entry.weight > 0
            && entry.stored_at.elapsed() < WEIGHT_CACHE_TTL
            && (current_mtime.is_none() || entry.mtime == current_mtime)
        {
            Some(entry.weight)
        } else {
            None
        }
    })
}

fn store_cached_weight(dir: PathBuf, mtime: Option<std::time::SystemTime>, weight: u64) {
    let mut cache = dir_weight_cache().lock().unwrap_or_else(|e| e.into_inner());
    // Review #3 Finding #5: NEVER cache a 0 aggregate weight (e.g. a newly created empty rip
    // directory or transient NAS error); removing the entry ensures future picks revisit it once
    // tracks arrive instead of making the subtree permanently unreachable.
    if weight == 0 {
        cache.remove(&dir);
        return;
    }
    if cache.len() >= MAX_WEIGHT_CACHE_ENTRIES {
        cache.clear();
    }
    cache.insert(
        dir,
        CachedDirWeight {
            stored_at: Instant::now(),
            mtime,
            weight,
        },
    );
}

/// Compute or fetch the cached FLAC-aware weight for `dir` (counting only `.flac` files and
/// non-excluded subdirectories, never `cover.jpg`/`.cue`/`.log`), avoiding repeated directory
/// scans across consecutive random picks on NAS/SMB libraries.
fn cached_shallow_flac_weight(dir: &Path) -> u64 {
    let current_mtime = fs::metadata(dir).ok().and_then(|m| m.modified().ok());
    if let Some(w) = lookup_cached_weight_with_mtime(dir, current_mtime) {
        return w;
    }
    let Ok(frame) = read_dir_frame(dir) else {
        // Transient read failure: skip on this pick without caching 0.
        return 0;
    };
    let flac_w = frame.flacs.len() as u64;
    let sub_w = (frame.subdirs.len() as u64).saturating_mul(DEFAULT_SUBDIR_WEIGHT);
    let weight = flac_w.saturating_add(sub_w);
    store_cached_weight(dir.to_path_buf(), current_mtime, weight);
    weight
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
    //   `subdirs.len() <= MAX_SHALLOW_PEEK_SUBDIRS` (skipping completely empty folders on this pick
    //   without caching 0), or an in-memory TTL-cached weight / `DEFAULT_SUBDIR_WEIGHT` with ZERO
    //   per-subdir metadata syscalls when there are >32 immediate subfolders (Review #3 #22).
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
    let mut learned_total = flacs.len() as u64;
    for sub in subdirs {
        let weight = if peek_subdirs {
            cached_shallow_flac_weight(&sub)
        } else {
            // Review #3 Finding #22: purely in-memory TTL lookup (zero fs::metadata/CreateFile
            // calls per subdir), preserving O(depth) latency on 2,000+ folder NAS roots.
            lookup_cached_weight(&sub).unwrap_or(DEFAULT_SUBDIR_WEIGHT)
        };
        learned_total = learned_total.saturating_add(weight);
        if weight > 0 {
            candidates.push((Choice::Subdir(sub), weight));
        }
    }
    // Refine this directory's cached weight from its actual FLACs + child weights (only when > 0,
    // never caching 0 so newly populated grandchildren remain immediately reachable).
    let dir_mtime = fs::metadata(dir).ok().and_then(|m| m.modified().ok());
    store_cached_weight(dir.to_path_buf(), dir_mtime, learned_total);

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
