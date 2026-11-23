//! Hardcoded `-3 dB` / `0 dB` safety headroom selector driven by FLAC ReplayGain & True Peak tags.
//!
//! Strict audiophile rules (from `REQUIREMENTS.md` §5.2):
//! - **Only two hardcoded modes exist:** `-3 dB` ([`HeadroomMode::Minus3Db`]) or `0 dB`
//!   ([`HeadroomMode::ZeroDb`]). Intermediate attenuation or `> 0 dB` boosting is impossible.
//! - **Quiet tracks with verified native headroom** (`peak <= -3.0 dBFS`, adjusted for any REW
//!   convolution boost) run at **`0 dB`**, giving dynamic/low-volume recordings a natural
//!   leveling advantage without ever boosting a single sample.
//! - **Unknown max peak fail-safe:** Untagged FLACs, malformed peak tags, or live streaming
//!   sources (Tidal) **always** receive **`-3 dB`**.

use std::fmt;

/// Default fail-safe attenuation applied whenever maximum track peak is unknown.
pub const UNKNOWN_PEAK_HEADROOM_DB: f64 = -3.0;

/// Maximum permitted digital gain (boosting above `0 dB` is strictly forbidden).
pub const MAX_ALLOWED_GAIN_DB: f64 = 0.0;

/// Linear amplitude corresponding to `-3.0 dBFS` (`10^(-3/20) ~= 0.7079457843841379`).
pub const MINUS_3_DB_LINEAR_THRESHOLD: f64 = 0.707_945_784_384_137_9;

/// Strictly two hardcoded audiophile safety headroom modes (`0 dB` or `-3 dB` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadroomMode {
    /// `0.0 dB` (unity gain = `1.0`) — skipped attenuation for quiet tracks with verified headroom.
    ZeroDb,
    /// `-3.0 dB` (`10^(-3/20)`) — ISP protection for hot tracks or tracks with unknown max peak.
    Minus3Db,
}

impl HeadroomMode {
    pub fn gain_db(self) -> f64 {
        match self {
            Self::ZeroDb => 0.0,
            Self::Minus3Db => UNKNOWN_PEAK_HEADROOM_DB,
        }
    }

    pub fn linear(self) -> f64 {
        match self {
            Self::ZeroDb => 1.0,
            Self::Minus3Db => MINUS_3_DB_LINEAR_THRESHOLD,
        }
    }
}

impl fmt::Display for HeadroomMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDb => write!(f, "0 dB"),
            Self::Minus3Db => write!(f, "-3 dB"),
        }
    }
}

/// Optional loudness & peak annotations parsed from FLAC Vorbis comments.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackPeakMetadata {
    pub replaygain_track_gain_db: Option<f64>,
    pub replaygain_track_peak: Option<f64>,
    pub replaygain_album_gain_db: Option<f64>,
    pub replaygain_album_peak: Option<f64>,
}

impl TrackPeakMetadata {
    /// Return the best verified peak amplitude (linear relative to full scale `1.0`),
    /// prioritizing track peak over album peak and rejecting invalid/non-positive values.
    pub fn effective_peak_linear(&self) -> Option<(f64, &'static str)> {
        if let Some(p) = self.replaygain_track_peak {
            if p.is_finite() && p > 0.0 {
                return Some((p, "REPLAYGAIN_TRACK_PEAK"));
            }
        }
        if let Some(p) = self.replaygain_album_peak {
            if p.is_finite() && p > 0.0 {
                return Some((p, "REPLAYGAIN_ALBUM_PEAK"));
            }
        }
        None
    }
}

/// Result of per-track headroom selection with human-readable explanation for logging.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadroomDecision {
    pub mode: HeadroomMode,
    pub peak_dbfs: Option<f64>,
    pub reason: String,
}

impl HeadroomDecision {
    pub fn summary(&self) -> String {
        format!("Headroom: {} [{}]", self.mode, self.reason)
    }
}

/// Choose strictly between hardcoded **`0 dB`** and **`-3 dB`** safety headroom for a track.
///
/// - `is_streaming_source`: when `true` (e.g. live Tidal stream), peak is unknown -> always `-3 dB`.
/// - `rew_max_boost_db`: positive peak boost (`>= 0.0`) introduced by any active REW convolution
///   filter (`0.0` when no REW boost is present).
pub fn select_headroom(
    meta: &TrackPeakMetadata,
    is_streaming_source: bool,
    rew_max_boost_db: f64,
) -> HeadroomDecision {
    if is_streaming_source {
        return HeadroomDecision {
            mode: HeadroomMode::Minus3Db,
            peak_dbfs: None,
            reason: "streaming source (unknown max peak -> -3 dB fail-safe)".to_string(),
        };
    }

    let Some((peak_lin, tag_name)) = meta.effective_peak_linear() else {
        return HeadroomDecision {
            mode: HeadroomMode::Minus3Db,
            peak_dbfs: None,
            reason: "no valid FLAC peak annotation (unknown max peak -> -3 dB fail-safe)".to_string(),
        };
    };

    let peak_dbfs = 20.0 * peak_lin.log10();
    let extra_rew_db = if rew_max_boost_db.is_finite() && rew_max_boost_db > 0.0 {
        rew_max_boost_db
    } else {
        0.0
    };
    let required_ceiling_dbfs = UNKNOWN_PEAK_HEADROOM_DB - extra_rew_db;

    // Small epsilon (1e-9 dB) ensures exact -3.0 dBFS boundary cleanly qualifies for 0 dB.
    if peak_dbfs <= required_ceiling_dbfs + 1e-9 {
        HeadroomDecision {
            mode: HeadroomMode::ZeroDb,
            peak_dbfs: Some(peak_dbfs),
            reason: format!(
                "{tag_name} = {peak_dbfs:.2} dBFS <= {required_ceiling_dbfs:.2} dBFS -> 0 dB"
            ),
        }
    } else {
        HeadroomDecision {
            mode: HeadroomMode::Minus3Db,
            peak_dbfs: Some(peak_dbfs),
            reason: format!(
                "{tag_name} = {peak_dbfs:.2} dBFS > {required_ceiling_dbfs:.2} dBFS -> -3 dB"
            ),
        }
    }
}
