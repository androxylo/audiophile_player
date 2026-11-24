//! Single-folder directory listing (never recursive), lossless OS path tokens, and URL/HTML/JSON
//! encoding helpers.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ap_source::library::read_dir_frame;

use crate::security::{normalize_allowed_root, normalized_paths_equal};

/// Prefix for lossless UTF-16 code-unit hex encoding of Windows paths containing unpaired
/// surrogates (or paths colliding with this prefix).
pub const RAW_OS_PATH_PREFIX_UTF16: &str = "ap-os-u16:";
/// Prefix for lossless raw-byte hex encoding of non-UTF-8 Unix paths.
pub const RAW_OS_PATH_PREFIX_BYTES: &str = "ap-os-hex:";

/// Immediate contents of one browsed folder (sorted folders first, then `.flac` files).
#[derive(Debug, Clone)]
pub struct FolderListing {
    pub current_dir: PathBuf,
    pub parent_dir: Option<PathBuf>,
    pub subdirs: Vec<PathBuf>,
    pub flac_files: Vec<PathBuf>,
    pub error: Option<String>,
}

/// Read ONLY the immediate children of `dir` so browsing stays instant even in huge libraries.
pub fn list_folder_entries(dir: &Path) -> FolderListing {
    list_folder_entries_inner(dir, None)
}

/// Read ONLY the immediate children of `dir`, omitting `parent_dir` when `dir` is at `allowed_root`
/// so the browser UI stays confined to the configured music root.
pub fn list_folder_entries_in_root(dir: &Path, allowed_root: &Path) -> FolderListing {
    list_folder_entries_inner(dir, Some(allowed_root))
}

/// Maximum folder entries rendered in one HTML browser page so huge flat directories cannot
/// create multi-MB responses that stall HTTP workers.
pub const MAX_BROWSER_ENTRIES: usize = 5_000;

fn list_folder_entries_inner(dir: &Path, allowed_root: Option<&Path>) -> FolderListing {
    let at_root = allowed_root.is_some_and(|root| {
        let norm_root = normalize_allowed_root(root);
        let norm_dir = normalize_allowed_root(dir);
        normalized_paths_equal(&norm_dir, &norm_root)
    });
    let parent_dir = if at_root {
        None
    } else {
        dir.parent().map(Path::to_path_buf)
    };
    match read_dir_frame(dir) {
        Ok(mut frame) => {
            let total = frame.subdirs.len() + frame.flacs.len();
            let mut note = None;
            if total > MAX_BROWSER_ENTRIES {
                let keep_dirs = frame.subdirs.len().min(MAX_BROWSER_ENTRIES);
                frame.subdirs.truncate(keep_dirs);
                let keep_flacs = MAX_BROWSER_ENTRIES.saturating_sub(keep_dirs);
                frame.flacs.truncate(keep_flacs);
                note = Some(format!(
                    "Showing first {MAX_BROWSER_ENTRIES} of {total} entries in '{}'",
                    dir.display()
                ));
            }
            FolderListing {
                current_dir: dir.to_path_buf(),
                parent_dir,
                subdirs: frame.subdirs,
                flac_files: frame.flacs,
                error: note,
            }
        }
        Err(err) => FolderListing {
            current_dir: dir.to_path_buf(),
            parent_dir,
            subdirs: Vec::new(),
            flac_files: Vec::new(),
            error: Some(format!("Cannot read folder '{}': {err}", dir.display())),
        },
    }
}

/// Encode a `Path` for lossless round-tripping through HTML hidden inputs, links, and redirects.
///
/// Valid UTF-8 paths (not starting with an internal escape prefix) are returned as-is for
/// readability. Paths containing invalid UTF-8 (Unix) or unpaired UTF-16 surrogates (Windows NTFS)
/// are encoded with an unambiguous hex prefix so no OS path characters are ever lost to U+FFFD.
pub fn encode_path_token(path: &Path) -> String {
    if let Some(s) = path.to_str() {
        if !s.starts_with(RAW_OS_PATH_PREFIX_UTF16) && !s.starts_with(RAW_OS_PATH_PREFIX_BYTES) {
            return s.to_owned();
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let units: Vec<u16> = path.as_os_str().encode_wide().collect();
        let mut out = String::with_capacity(RAW_OS_PATH_PREFIX_UTF16.len() + units.len() * 4);
        out.push_str(RAW_OS_PATH_PREFIX_UTF16);
        for u in units {
            out.push_str(&format!("{u:04x}"));
        }
        out
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        let mut out = String::with_capacity(RAW_OS_PATH_PREFIX_BYTES.len() + bytes.len() * 2);
        out.push_str(RAW_OS_PATH_PREFIX_BYTES);
        for &b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }
}

/// Decode a path token produced by [`encode_path_token`] (or plain UTF-8 text typed by the user).
pub fn decode_path_token(raw: &str) -> Result<PathBuf, String> {
    const MAX_HEX_LEN: usize = 65_536;

    if let Some(hex) = raw.strip_prefix(RAW_OS_PATH_PREFIX_UTF16) {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            if hex.is_empty() || hex.len() > MAX_HEX_LEN || hex.len() % 4 != 0 {
                return Err("malformed UTF-16 path token".to_string());
            }
            let bytes = hex.as_bytes();
            let mut units = Vec::with_capacity(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                let d0 = hex_val(chunk[0]).ok_or_else(|| "invalid hex in path token".to_string())?;
                let d1 = hex_val(chunk[1]).ok_or_else(|| "invalid hex in path token".to_string())?;
                let d2 = hex_val(chunk[2]).ok_or_else(|| "invalid hex in path token".to_string())?;
                let d3 = hex_val(chunk[3]).ok_or_else(|| "invalid hex in path token".to_string())?;
                units.push(((d0 as u16) << 12) | ((d1 as u16) << 8) | ((d2 as u16) << 4) | (d3 as u16));
            }
            return Ok(PathBuf::from(OsString::from_wide(&units)));
        }
        #[cfg(not(windows))]
        {
            let _ = (hex, MAX_HEX_LEN);
            return Err("UTF-16 path tokens are only supported on Windows".to_string());
        }
    }

    if let Some(hex) = raw.strip_prefix(RAW_OS_PATH_PREFIX_BYTES) {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            if hex.is_empty() || hex.len() > MAX_HEX_LEN || hex.len() % 2 != 0 {
                return Err("malformed raw-byte path token".to_string());
            }
            let bytes = hex.as_bytes();
            let mut out = Vec::with_capacity(bytes.len() / 2);
            for chunk in bytes.chunks_exact(2) {
                let hi = hex_val(chunk[0]).ok_or_else(|| "invalid hex in path token".to_string())?;
                let lo = hex_val(chunk[1]).ok_or_else(|| "invalid hex in path token".to_string())?;
                out.push((hi << 4) | lo);
            }
            return Ok(PathBuf::from(OsString::from_vec(out)));
        }
        #[cfg(not(unix))]
        {
            let _ = (hex, MAX_HEX_LEN);
            return Err("raw-byte path tokens are only supported on Unix".to_string());
        }
    }

    Ok(PathBuf::from(raw))
}

pub fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Extract a single parameter from a query string or `application/x-www-form-urlencoded` body.
pub fn parse_form_param(encoded: &str, key: &str) -> Option<String> {
    for pair in encoded.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if url_decode(k) == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}
