//! Recursive local FLAC directory discovery and random shuffle queue.

use std::path::{Path, PathBuf};

/// Recursively discover `.flac` files under `root` (e.g. `~/Music`).
pub fn discover_flac_files(_root: &Path) -> Result<Vec<PathBuf>, String> {
    todo!("Implement recursive FLAC folder discovery & shuffle queue")
}
