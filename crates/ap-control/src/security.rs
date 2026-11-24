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

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

#[cfg(windows)]
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
#[cfg(windows)]
const FILE_ATTRIBUTE_DEVICE: u32 = 0x0000_0040;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Windows reserved DOS device base names (case-insensitive, matched before the first `.` after
/// trimming trailing spaces/dots).
const RESERVED_DOS_DEVICES: &[&str] = &[
    "CON", "CONIN$", "CONOUT$", "PRN", "AUX", "NUL", "CLOCK$", "COM0", "COM1", "COM2", "COM3",
    "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4",
    "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Normalize a trusted configured music root into an absolute, lexically clean local path.
///
/// If `root` is relative, it is resolved against `std::env::current_dir()`. On Windows, a
/// `\\?\C:\...` verbatim disk prefix (e.g. from `fs::canonicalize`) is unwrapped to `C:\...`
/// so lexical prefix matching against normal user paths is consistent.
pub fn normalize_allowed_root(root: &Path) -> PathBuf {
    let candidate = unwrap_verbatim_disk(root);
    let abs = if candidate.is_absolute() {
        candidate
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(candidate)
    } else {
        candidate
    };
    validate_local_path_syntax(&abs).unwrap_or(abs)
}

fn unwrap_verbatim_disk(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let mut comps = path.components();
        if let Some(Component::Prefix(p)) = comps.next() {
            if let Prefix::VerbatimDisk(drive) = p.kind() {
                let mut out = PathBuf::from(format!("{}:\\", (drive as char).to_ascii_uppercase()));
                for c in comps {
                    match c {
                        Component::RootDir => {}
                        other => out.push(other.as_os_str()),
                    }
                }
                return out;
            }
        }
    }
    path.to_path_buf()
}

/// Validate raw syntax of an untrusted path and return its lexically normalized local form.
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
    let raw = path.as_os_str().as_encoded_bytes();
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }
    if raw.contains(&0) {
        return Err("path must not contain NUL bytes".to_string());
    }
    if raw.len() >= 2
        && matches!(raw[0], b'\\' | b'/')
        && matches!(raw[1], b'\\' | b'/')
    {
        return Err("UNC and Windows device paths are forbidden".to_string());
    }

    let mut out = PathBuf::new();
    let mut saw_root = false;
    let mut depth: usize = 0;

    #[cfg(windows)]
    let mut saw_disk_prefix = false;

    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(drive) => {
                    #[cfg(windows)]
                    {
                        saw_disk_prefix = true;
                    }
                    let d = (drive as char).to_ascii_uppercase();
                    out.push(format!("{d}:"));
                }
                Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) => {
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
    if !saw_disk_prefix || !saw_root {
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

/// Validate that `path` is a safe local path lexically confined under `allowed_root`, and verify
/// that no symlink or Windows reparse point along the path redirects to a UNC share or device.
///
/// Local NTFS directory junctions and volume mount points inside `allowed_root` (e.g.
/// `C:\Users\name\Music\Favorite -> D:\Music`) are explicitly supported.
pub fn validate_path_within_root(path: &Path, allowed_root: &Path) -> Result<PathBuf, String> {
    let norm_root = normalize_allowed_root(allowed_root);
    let norm_path = validate_local_path_syntax(path)?;

    if !lexically_within_root(&norm_path, &norm_root) {
        return Err(format!(
            "path '{}' is outside allowed music root '{}'",
            norm_path.display(),
            norm_root.display()
        ));
    }

    verify_no_unc_reparse_points(&norm_root, &norm_path)?;
    Ok(norm_path)
}

/// Inspect each existing component from `norm_root` down to `norm_path` using `symlink_metadata`
/// (which never follows links or initiates SMB) and reject any reparse point whose target is a
/// UNC/network share or device namespace.
fn verify_no_unc_reparse_points(norm_root: &Path, norm_path: &Path) -> Result<(), String> {
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
        let is_link = is_symlink_or_reparse(&meta);
        if is_link {
            let target = fs::read_link(&cur).map_err(|e| {
                format!(
                    "cannot inspect symlink/junction target for '{}': {e}",
                    cur.display()
                )
            })?;
            check_reparse_target_is_local(&target)?;
        }
    }
    Ok(())
}

fn is_symlink_or_reparse(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn check_reparse_target_is_local(target: &Path) -> Result<(), String> {
    let raw = target.as_os_str().as_encoded_bytes();
    if raw.contains(&0) {
        return Err("reparse target contains NUL byte".to_string());
    }
    if let Some(Component::Prefix(p)) = target.components().next() {
        match p.kind() {
            Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) => {
                return Err("symlink/junction targeting a UNC network share is forbidden".into());
            }
            Prefix::DeviceNS(_) => {
                return Err("symlink/junction targeting a device namespace is forbidden".into());
            }
            Prefix::Verbatim(v) => {
                // Allow NTFS volume mount points (`\\?\Volume{...}\`), reject anything else.
                let v_bytes = v.as_encoded_bytes();
                let is_volume = v_bytes.len() >= 7
                    && v_bytes[..7].eq_ignore_ascii_case(b"Volume{");
                if !is_volume {
                    return Err("symlink/junction targeting a non-local verbatim path is forbidden".into());
                }
            }
            Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {}
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
            verify_no_unc_reparse_points(&n, &n)?;
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
