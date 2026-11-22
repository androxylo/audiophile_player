//! Ultra-minimal static HTML template (file/folder picker, Play/Stop, GitHub manual link).

use crate::GITHUB_MANUAL_URL;

/// Render the bare-minimum HTML control page (no JS frameworks, no volume slider, no DSP UI).
pub fn render_control_page() -> String {
    format!(
        "<!doctype html><html><body><h1>Audiophile Player</h1>\
         <p><a href=\"{GITHUB_MANUAL_URL}\">Manual &amp; Documentation (GitHub)</a></p>\
         </body></html>"
    )
}
