//! Single-folder directory listing (never recursive) and URL/HTML/JSON encoding helpers.

use std::fs;
use std::path::{Path, PathBuf};

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
    let parent_dir = dir.parent().map(Path::to_path_buf);
    let mut subdirs = Vec::new();
    let mut flac_files = Vec::new();

    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                let p = entry.path();
                if ft.is_dir() {
                    subdirs.push(p);
                } else if ft.is_file()
                    && p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("flac"))
                {
                    flac_files.push(p);
                }
            }
            subdirs.sort();
            flac_files.sort();
            FolderListing {
                current_dir: dir.to_path_buf(),
                parent_dir,
                subdirs,
                flac_files,
                error: None,
            }
        }
        Err(err) => FolderListing {
            current_dir: dir.to_path_buf(),
            parent_dir,
            subdirs,
            flac_files,
            error: Some(format!("Cannot read folder '{}': {err}", dir.display())),
        },
    }
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
