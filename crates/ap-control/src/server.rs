//! Localhost-only HTTP control server strictly isolated from the audio output thread.

use std::path::PathBuf;

/// Minimal control commands accepted from the rudimentary local HTTP page.
#[derive(Debug, Clone)]
pub enum ControlCommand {
    PlayPath(PathBuf),
    Stop,
}
