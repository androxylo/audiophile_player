//! Safety headroom policy (`-3 dB` to `0 dB`, strictly never boosting `> 0 dB`).
//!
//! Uses FLAC ReplayGain & True Peak annotations when available so quiet, well-mastered
//! tracks avoid unnecessary `-3 dB` attenuation; always defaults to `-3 dB` whenever
//! track peak level is unknown (untagged FLAC or live Tidal stream).

/// Default fail-safe attenuation applied whenever maximum track peak is unknown.
pub const UNKNOWN_PEAK_HEADROOM_DB: f64 = -3.0;

/// Maximum permitted digital gain (boosting above `0 dB` is strictly forbidden).
pub const MAX_ALLOWED_GAIN_DB: f64 = 0.0;

/// Optional loudness & peak annotations parsed from FLAC Vorbis comments.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackPeakMetadata {
    pub replaygain_track_gain_db: Option<f64>,
    pub replaygain_track_peak: Option<f64>,
    pub replaygain_album_gain_db: Option<f64>,
    pub replaygain_album_peak: Option<f64>,
}

/// Computed linear gain factor guaranteed to lie in `(0.0, 1.0]` (`<= 0 dB`).
#[derive(Debug, Clone, Copy)]
pub struct SafeGain {
    pub gain_db: f64,
    pub linear: f64,
}
