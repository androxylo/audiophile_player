//! Path and HTTP request security helpers for the localhost control server and playback engine.
//!
//! Enforces:
//! - Strict rejection of UNC, verbatim, device-namespace, and Windows reserved DOS device paths
//!   (`\\server\share`, `\\?\...`, `\\.\...`, `CONIN$`, `NUL`, `COM1`, etc.) **before** any
//!   filesystem call (`fs::metadata`, `fs::read_dir`, `File::open`) can initiate SMB/NTLM traffic
//!   or block on a console/pipe handle.
//! - Lexical root confinement under the configured music root while permitting local NTFS
//!   junctions and volume mount points mounted inside that root (rejecting any reparse target
//!   that points to a UNC/network share or device namespace).
//! - Regular-file + `.flac` validation for `/action/play-file` and engine open paths.
//! - Strict `Host`, `Origin`, and `Sec-Fetch-Site` checks against DNS rebinding and cross-site
//!   requests.

use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf, Prefix};

pub use ap_source::library::{check_reparse_target_is_allowed, extract_unc_share};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

#[cfg(windows)]
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
#[cfg(windows)]
const FILE_ATTRIBUTE_DEVICE: u32 = 0x0000_0040;

/// Returns `true` only when `meta` represents an actual symlink or Windows NTFS mount
/// point/junction (`meta.file_type().is_symlink()`, which checks `IsReparseTagNameSurrogate` on
/// Windows — true for `IO_REPARSE_TAG_SYMLINK` and `IO_REPARSE_TAG_MOUNT_POINT`, false for
/// non-link reparse points such as OneDrive cloud placeholders, WCI, and Data Deduplication).
pub fn is_symlink_or_mount_point(_path: &Path, meta: &fs::Metadata) -> bool {
    ap_source::library::is_symlink_or_mount_point(meta)
}

/// Windows reserved DOS device base names (case-insensitive, matched before the first `.` after
/// trimming trailing spaces/dots).
const RESERVED_DOS_DEVICES: &[&str] = &[
    "CON", "CONIN$", "CONOUT$", "PRN", "AUX", "NUL", "CLOCK$", "COM0", "COM1", "COM2", "COM3",
    "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4",
    "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Normalize a trusted configured music root into an absolute, lexically clean path.
///
/// Supports both local disk roots (e.g. `C:\Users\name\Music`) and trusted UNC NAS roots
/// explicitly configured at startup (e.g. `\\NAS\Music`). Unwraps Windows verbatim prefixes
/// (`\\?\C:\...` -> `C:\...`, `\\?\UNC\server\share\...` -> `\\server\share\...`) so lexical
/// prefix matching against normal paths is consistent.
pub fn normalize_allowed_root(root: &Path) -> PathBuf {
    let candidate = unwrap_verbatim_prefix(root);
    let unc_share = extract_unc_share(&candidate);
    let abs = if unc_share.is_some() || candidate.is_absolute() {
        candidate
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(candidate)
    } else {
        candidate
    };
    let share_ref = unc_share
        .as_ref()
        .map(|(s, sh)| (s.as_str(), sh.as_str()));
    validate_path_syntax_for_root(&abs, share_ref).unwrap_or(abs)
}

fn unwrap_verbatim_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let mut comps = path.components();
        if let Some(Component::Prefix(p)) = comps.next() {
            match p.kind() {
                Prefix::VerbatimDisk(drive) => {
                    let mut out =
                        PathBuf::from(format!("{}:\\", (drive as char).to_ascii_uppercase()));
                    for c in comps {
                        match c {
                            Component::RootDir => {}
                            other => out.push(other.as_os_str()),
                        }
                    }
                    return out;
                }
                Prefix::VerbatimUNC(server, share) => {
                    let mut out = PathBuf::from(format!(
                        "\\\\{}\\{}\\",
                        server.to_string_lossy(),
                        share.to_string_lossy()
                    ));
                    for c in comps {
                        match c {
                            Component::RootDir => {}
                            other => out.push(other.as_os_str()),
                        }
                    }
                    return out;
                }
                _ => {}
            }
        }
    }
    path.to_path_buf()
}

/// Validate raw syntax of an untrusted local path and return its lexically normalized local form.
///
/// Performs **zero** filesystem I/O. Rejects:
/// - Empty paths or embedded NUL bytes.
/// - Leading `\\`, `//`, `/\`, `\/` (UNC, verbatim `\\?\`, device namespace `\\.\`).
/// - Non-disk prefixes (`UNC`, `Verbatim`, `VerbatimUNC`, `VerbatimDisk`, `DeviceNS`).
/// - Relative paths (must be an absolute local disk path on Windows, or `/` rooted on Unix).
/// - `..` components that escape above root.
/// - Any segment containing `:` (blocks NTFS Alternate Data Streams / misplaced drive specs).
/// - Any segment matching a Windows reserved DOS device name (`CON`, `CONIN$`, `NUL`, `COM1`, ...).
pub fn validate_local_path_syntax(path: &Path) -> Result<PathBuf, String> {
    validate_path_syntax_for_root(path, None)
}

/// Validate raw syntax of an untrusted path against an optional trusted UNC `(server, share)`
/// configured as `--music-dir`.
///
/// When `allowed_unc_share` is `None` (normal local disk root), ALL UNC paths are rejected before
/// any filesystem syscall. When `allowed_unc_share` is `Some((srv, shr))`, ONLY paths whose
/// `\\server\share` prefix matches that exact trusted share are accepted; all other UNC shares,
/// Win32 device paths (`\\.\`, `\\?\`), `..` escapes, `:`, and reserved DOS names remain blocked.
pub fn validate_path_syntax_for_root(
    path: &Path,
    allowed_unc_share: Option<(&str, &str)>,
) -> Result<PathBuf, String> {
    let raw = path.as_os_str().as_encoded_bytes();
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }
    if raw.contains(&0) {
        return Err("path must not contain NUL bytes".to_string());
    }

    let leading_double_sep = raw.len() >= 2
        && matches!(raw[0], b'\\' | b'/')
        && matches!(raw[1], b'\\' | b'/');
    if leading_double_sep {
        // Always reject device-namespace / verbatim prefixes (`\\.\`, `\\?\`, `//./`, `//?/`).
        if raw.len() >= 4
            && matches!(raw[2], b'.' | b'?')
            && matches!(raw[3], b'\\' | b'/')
        {
            return Err("UNC and Windows device paths are forbidden".to_string());
        }
        let matches_trusted_unc = allowed_unc_share.is_some_and(|(srv, shr)| {
            extract_unc_share(path)
                .is_some_and(|(ps, psh)| ps == srv && psh == shr)
        });
        if !matches_trusted_unc {
            return Err("UNC and Windows device paths are forbidden".to_string());
        }
    }

    let mut out = PathBuf::new();
    let mut saw_root = false;
    let mut depth: usize = 0;

    #[cfg(windows)]
    let mut saw_allowed_prefix = false;

    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(drive) => {
                    #[cfg(windows)]
                    {
                        saw_allowed_prefix = true;
                    }
                    let d = (drive as char).to_ascii_uppercase();
                    out.push(format!("{d}:"));
                }
                Prefix::UNC(server, share) => {
                    let s = server.to_string_lossy().to_ascii_lowercase();
                    let sh = share.to_string_lossy().to_ascii_lowercase();
                    if allowed_unc_share == Some((s.as_str(), sh.as_str())) {
                        #[cfg(windows)]
                        {
                            saw_allowed_prefix = true;
                        }
                        out.push(format!("\\\\{}\\{}", server.to_string_lossy(), share.to_string_lossy()));
                    } else {
                        return Err("UNC network paths are forbidden".to_string());
                    }
                }
                Prefix::VerbatimUNC(_, _) => {
                    return Err("UNC network paths are forbidden".to_string());
                }
                Prefix::DeviceNS(_) | Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) => {
                    return Err("Windows device and verbatim paths are forbidden".to_string());
                }
            },
            Component::RootDir => {
                saw_root = true;
                out.push(comp.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err("path escapes filesystem root".to_string());
                }
                out.pop();
                depth -= 1;
            }
            Component::Normal(seg) => {
                if seg.as_encoded_bytes().contains(&b':') {
                    return Err("path component must not contain ':'".to_string());
                }
                if is_reserved_dos_device_name(seg) {
                    return Err(format!(
                        "reserved Windows device name '{}' is forbidden",
                        seg.to_string_lossy()
                    ));
                }
                out.push(seg);
                depth += 1;
            }
        }
    }

    #[cfg(windows)]
    if !saw_allowed_prefix || !saw_root {
        return Err("path must be an absolute local drive path (e.g. C:\\...)".to_string());
    }

    #[cfg(not(windows))]
    if !saw_root {
        return Err("path must be absolute".to_string());
    }

    Ok(out)
}

fn is_reserved_dos_device_name(seg: &OsStr) -> bool {
    let bytes = seg.as_encoded_bytes();
    // Trim trailing ASCII spaces and dots (Windows strips these when matching DOS device names).
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b' ' | b'.') {
        end -= 1;
    }
    let trimmed = &bytes[..end];
    // Base name is everything before the first '.'.
    let stem = match trimmed.iter().position(|&b| b == b'.') {
        Some(idx) => &trimmed[..idx],
        None => trimmed,
    };
    let Ok(stem_str) = std::str::from_utf8(stem) else {
        return false;
    };
    RESERVED_DOS_DEVICES
        .iter()
        .any(|&dev| stem_str.eq_ignore_ascii_case(dev))
}

/// Compare two path components for equality (case-insensitive on Windows, exact on Unix),
/// preserving lossless comparison for non-UTF-8 OS strings.
pub(crate) fn path_component_eq(a: &OsStr, b: &OsStr) -> bool {
    #[cfg(windows)]
    {
        match (a.to_str(), b.to_str()) {
            (Some(sa), Some(sb)) => {
                if sa.is_ascii() && sb.is_ascii() {
                    sa.eq_ignore_ascii_case(sb)
                } else {
                    sa.to_lowercase() == sb.to_lowercase()
                }
            }
            _ => a.as_encoded_bytes().eq_ignore_ascii_case(b.as_encoded_bytes()),
        }
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

/// Return `true` if lexically normalized `path` is equal to or inside lexically normalized `root`.
pub fn lexically_within_root(norm_path: &Path, norm_root: &Path) -> bool {
    let mut p_comps = norm_path.components();
    for r_comp in norm_root.components() {
        let Some(p_comp) = p_comps.next() else {
            return false;
        };
        if !path_component_eq(p_comp.as_os_str(), r_comp.as_os_str()) {
            return false;
        }
    }
    true
}

/// Return `true` if `a` and `b` represent the same lexically normalized path.
pub fn normalized_paths_equal(a: &Path, b: &Path) -> bool {
    let a_comps: Vec<_> = a.components().collect();
    let b_comps: Vec<_> = b.components().collect();
    if a_comps.len() != b_comps.len() {
        return false;
    }
    a_comps
        .iter()
        .zip(b_comps.iter())
        .all(|(ca, cb)| path_component_eq(ca.as_os_str(), cb.as_os_str()))
}

/// Validate that `path` is a safe path lexically confined under `allowed_root`, and verify
/// that no symlink or Windows mount point along the path (or chained link target) redirects to
/// an untrusted UNC share or Win32 device namespace.
///
/// - Local NTFS directory junctions and volume mount points inside `allowed_root` (e.g.
///   `C:\Users\name\Music\Favorite -> D:\Music`) are explicitly supported.
/// - Non-link Windows reparse points (such as OneDrive Files On-Demand placeholders, WCI, and
///   Data Deduplication) are treated as ordinary files/directories rather than rejected.
/// - When `allowed_root` itself is a trusted UNC NAS path (`\\NAS\Music`), paths confined under
///   that exact `\\server\share` root are allowed while all other UNC shares remain blocked.
pub fn validate_path_within_root(path: &Path, allowed_root: &Path) -> Result<PathBuf, String> {
    let norm_root = normalize_allowed_root(allowed_root);
    let allowed_unc = extract_unc_share(&norm_root);
    let share_ref = allowed_unc
        .as_ref()
        .map(|(s, sh)| (s.as_str(), sh.as_str()));
    let norm_path = validate_path_syntax_for_root(path, share_ref)?;

    if !lexically_within_root(&norm_path, &norm_root) {
        return Err(format!(
            "path '{}' is outside allowed music root '{}'",
            norm_path.display(),
            norm_root.display()
        ));
    }

    verify_no_unc_reparse_points(&norm_root, &norm_path, share_ref)?;
    Ok(norm_path)
}

/// Inspect each existing component from `norm_root` down to `norm_path` using `symlink_metadata`
/// (which never follows links or initiates SMB). Only actual symlinks and NTFS mount points
/// (`meta.file_type().is_symlink()`, which checks `IO_REPARSE_TAG_SYMLINK` and
/// `IO_REPARSE_TAG_MOUNT_POINT` on Windows) are inspected via `read_link`, resolving chained
/// links up to `MAX_SYMLINK_HOPS` hops and rejecting any untrusted UNC or device target.
fn verify_no_unc_reparse_points(
    norm_root: &Path,
    norm_path: &Path,
    allowed_unc_share: Option<(&str, &str)>,
) -> Result<(), String> {
    let root_count = norm_root.components().count();
    let mut cur = PathBuf::new();

    for (idx, comp) in norm_path.components().enumerate() {
        cur.push(comp.as_os_str());
        if idx + 1 < root_count {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&cur) else {
            // Non-existent suffix is safe here; downstream read_dir/metadata will report NotFound.
            break;
        };
        if is_symlink_or_mount_point(&cur, &meta) {
            ap_source::library::verify_symlink_chain_is_allowed(&cur, allowed_unc_share)?;
        }
    }
    Ok(())
}

/// Check case-insensitively whether `path` has a `.flac` extension.
pub fn is_flac_extension(path: &Path) -> bool {
    ap_source::library::is_flac_path(path)
}

/// Validate a file path before submitting `PlayFile` or calling `FlacBlockReader::open`.
///
/// Rejects UNC/device/reserved names, paths outside `allowed_root` (when supplied), non-`.flac`
/// extensions, and non-regular files (directories, named pipes, FIFOs, devices).
pub fn validate_playable_flac_path(
    path: &Path,
    allowed_root: Option<&Path>,
) -> Result<PathBuf, String> {
    let norm = match allowed_root {
        Some(root) => validate_path_within_root(path, root)?,
        None => {
            let n = validate_local_path_syntax(path)?;
            verify_no_unc_reparse_points(&n, &n, None)?;
            n
        }
    };

    if !is_flac_extension(&norm) {
        return Err(format!(
            "file '{}' does not have a .flac extension",
            norm.display()
        ));
    }

    let meta = fs::metadata(&norm)
        .map_err(|e| format!("cannot stat '{}': {e}", norm.display()))?;

    if !meta.is_file() {
        return Err(format!("'{}' is not a regular file", norm.display()));
    }

    #[cfg(windows)]
    {
        let attrs = meta.file_attributes();
        if (attrs & FILE_ATTRIBUTE_DEVICE) != 0 || (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0 {
            return Err(format!("'{}' is a device or directory, not a regular file", norm.display()));
        }
    }

    Ok(norm)
}

/// Validate the HTTP `Host` header against the server's bound localhost port.
///
/// Strictly accepts `127.0.0.1:<port>` or `localhost:<port>` (case-insensitive hostname, exact
/// bound port) to prevent DNS rebinding and cross-site GET abuse.
pub fn is_valid_localhost_host(host: &str, bound_port: u16) -> bool {
    let trimmed = host.trim();
    let expected_ip = format!("127.0.0.1:{bound_port}");
    let expected_name = format!("localhost:{bound_port}");
    trimmed.eq_ignore_ascii_case(&expected_ip) || trimmed.eq_ignore_ascii_case(&expected_name)
}

/// Validate the HTTP `Origin` header (when present) for state-changing requests.
///
/// Strictly accepts `http://127.0.0.1:<port>` or `http://localhost:<port>` (case-insensitive).
/// Rejects `null`, foreign domains, wrong ports, and HTTPS origins.
pub fn is_valid_localhost_origin(origin: &str, bound_port: u16) -> bool {
    let trimmed = origin.trim();
    let expected_ip = format!("http://127.0.0.1:{bound_port}");
    let expected_name = format!("http://localhost:{bound_port}");
    trimmed.eq_ignore_ascii_case(&expected_ip) || trimmed.eq_ignore_ascii_case(&expected_name)
}

/// Validate `Sec-Fetch-Site` (when present): only `same-origin` or `none` are permitted for
/// sensitive endpoints (`POST /action/*`, `GET /api/status`, and `GET /?dir=...`).
pub fn is_allowed_fetch_site(sec_fetch_site: &str) -> bool {
    let trimmed = sec_fetch_site.trim();
    trimmed.eq_ignore_ascii_case("same-origin") || trimmed.eq_ignore_ascii_case("none")
}

/// Validate `Sec-Fetch-Dest` (when present): reject embedded framing contexts (`iframe`, `frame`,
/// `object`, `embed`, `fencedframe`) to block clickjacking even in browsers that omit Origin.
pub fn is_allowed_fetch_dest(sec_fetch_dest: &str) -> bool {
    let trimmed = sec_fetch_dest.trim();
    !trimmed.eq_ignore_ascii_case("iframe")
        && !trimmed.eq_ignore_ascii_case("frame")
        && !trimmed.eq_ignore_ascii_case("object")
        && !trimmed.eq_ignore_ascii_case("embed")
        && !trimmed.eq_ignore_ascii_case("fencedframe")
}
