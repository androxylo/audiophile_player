//! Extremely rudimentary local HTTP control interface for out-of-the-box zero-CLI testing.
//!
//! Strict scope:
//! - Select a folder (scrollable for large folders) or `.flac` file.
//! - **Play Random**, **Play Sequential**, and unified **Pause** (releases the DAC driver
//!   immediately so Audirvana or another player can use the DAC while paused).
//! - Bounded bottom action log (discards history beyond latest 50 entries).
//! - Direct link to the GitHub manual (`https://github.com/androxylo/audiophile_player`).
//! - Safe conventional localhost port picker (`17441..=17499` — never 80 or 8080).
//! - **Strictly NO volume control.**

pub mod browser;
pub mod html;
pub mod log;
pub mod server;
pub mod state;

/// Manual URL linked directly from the rudimentary local control page.
pub const GITHUB_MANUAL_URL: &str = "https://github.com/androxylo/audiophile_player";
