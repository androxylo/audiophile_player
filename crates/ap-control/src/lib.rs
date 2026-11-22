//! Extremely rudimentary local HTTP control page for out-of-the-box zero-CLI testing.
//!
//! Scope strictly limited to:
//! - Pick a local FLAC file or music folder.
//! - Play / Stop commands sent over a decoupled channel to the headless engine.
//! - Direct link to the user manual at `https://github.com/androxylo/audiophile_player`.
//!
//! Runs strictly on a low-priority control thread, completely isolated from audio output.

pub mod html;
pub mod server;

/// Manual URL linked directly from the rudimentary local control page.
pub const GITHUB_MANUAL_URL: &str = "https://github.com/androxylo/audiophile_player";
