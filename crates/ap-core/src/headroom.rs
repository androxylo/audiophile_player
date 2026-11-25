//! Decided audiophile safety headroom & convolution excess-scaling policy (`HEADROOM_DESIGN.md`).
//!
//! Strict per-track rules:
//! - **Upsampler gain `G_up` is strictly `-3 dB` or `0 dB`:**
//!   - `0 dB` ([`HeadroomMode::ZeroDb`]) when the realistic metadata (or full-scan) peak is
//!     `<= -3.0 dBFS` (and not a live streaming source).
//!   - `-3 dB` ([`HeadroomMode::Minus3Db`]) otherwise. An unknown peak counts as `0.0 dBFS`.
//! - **Convolution is scaled ONLY by the excess** over `0.0 dBFS` of:
//!   - `P + A_isp + G_up + max(G_c, 0)` (metadata-driven), or
//!   - `M + G_up` (when a current full-scan post-chain peak `M` exists),
//!   where `A_isp = 3.0 dB` for a sample-peak or unknown peak and `0.0 dB` for a true peak,
//!   and `G_c` is the active REW filter's `max |H(f)|` in dB (uncapped; REW filters are never
//!   normalized at load so their boosts fill hardware dips on purpose).
//! - **Peak realism rules:**
//!   - Sample-peak tags above full scale (`> 0.0 dBFS`, i.e., linear `> 1.0`) count as true peaks.
//!   - True peaks (or above-FS sample peaks) above `+6.0 dBTP` count as unknown.
//!   - Non-finite, non-positive, or absurdly tiny (`< -120.0 dBFS`) values count as unknown.
//! - **L1 norm is diagnostic only** (never used as a gain criterion).
//! - **Never boost (`<= 0 dB`):** Neither upsampler gain nor convolution scale can ever exceed `0 dB`.

use std::fmt;

/// Default fail-safe attenuation applied whenever maximum track peak is unknown or `> -3 dBFS`.
pub const UNKNOWN_PEAK_HEADROOM_DB: f64 = -3.0;

/// Maximum permitted digital gain (boosting above `0 dB` is strictly forbidden).
pub const MAX_ALLOWED_GAIN_DB: f64 = 0.0;

/// Linear amplitude corresponding to `-3.0 dBFS` (`10^(-3/20) ~= 0.7079457843841379`).
pub const MINUS_3_DB_LINEAR_THRESHOLD: f64 = 0.707_945_784_384_137_9;

/// Inter-sample peak allowance (`A_isp`) added for discrete sample-peak or unknown tracks.
pub const SAMPLE_PEAK_ISP_ALLOWANCE_DB: f64 = 3.0;

/// Maximum realistic True Peak (`+6.0 dBTP`); peaks above this are treated as unknown.
pub const MAX_REALISTIC_TRUE_PEAK_DB: f64 = 6.0;

/// Minimum realistic peak (`-120.0 dBFS`); values below this are treated as corrupted/unknown.
pub const MIN_REALISTIC_PEAK_DB: f64 = -120.0;

/// Strictly two hardcoded upsampler headroom modes (`0 dB` or `-3 dB` only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadroomMode {
    /// `0.0 dB` (unity gain = `1.0`) — skipped attenuation for quiet tracks (`peak <= -3 dBFS`).
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

/// Classification of a realistic track peak for ISP allowance (`A_isp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeakKind {
    /// True peak tag or sample-peak tag above full scale (`> 0 dBFS`, `<= +6 dBTP`): `A_isp = 0 dB`.
    TruePeak,
    /// Discrete sample peak (`<= 0 dBFS`): `A_isp = 3 dB`.
    SamplePeak,
}

/// Validated, realistic track peak with its ISP allowance classification.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClassifiedPeak {
    pub linear: f64,
    pub dbfs: f64,
    pub kind: PeakKind,
    pub source_tag: &'static str,
}

impl ClassifiedPeak {
    /// Inter-sample peak allowance `A_isp` in dB (`0.0` for TruePeak, `3.0` for SamplePeak).
    pub fn isp_allowance_db(self) -> f64 {
        match self.kind {
            PeakKind::TruePeak => 0.0,
            PeakKind::SamplePeak => SAMPLE_PEAK_ISP_ALLOWANCE_DB,
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
    /// Measured True Peak amplitude (linear relative to full scale `1.0`), parsed from
    /// `TRUE_PEAK`, `REPLAYGAIN_TRACK_TRUE_PEAK`, or `R128_TRACK_TRUE_PEAK`.
    pub true_peak_linear: Option<f64>,
}

fn realistic_dbfs(linear: f64) -> Option<f64> {
    if !linear.is_finite() || linear <= 0.0 {
        return None;
    }
    let dbfs = 20.0 * linear.log10();
    if dbfs >= MIN_REALISTIC_PEAK_DB - 1e-9 && dbfs <= MAX_REALISTIC_TRUE_PEAK_DB + 1e-9 {
        Some(dbfs)
    } else {
        None
    }
}

impl TrackPeakMetadata {
    /// Classify the best realistic peak amplitude per the decided headroom rules:
    /// - True peak tags take priority over ReplayGain track peak, which takes priority over album peak.
    /// - Sample-peak tags `> 1.0` (`> 0 dBFS`) count as [`PeakKind::TruePeak`].
    /// - Any peak `> +6.0 dBTP` (or `< -120.0 dBFS`, `<= 0`, non-finite) counts as unknown (`None`).
    /// - If a `true_peak_linear` tag is present and exceeds `+6.0 dBTP`, returns `None` immediately
    ///   (unknown) instead of falling back to a potentially quieter sample-peak tag.
    pub fn classified_peak(&self) -> Option<ClassifiedPeak> {
        if let Some(p) = self.true_peak_linear {
            if p.is_finite() && p > 0.0 {
                let dbfs = 20.0 * p.log10();
                if dbfs > MAX_REALISTIC_TRUE_PEAK_DB + 1e-9 {
                    return None;
                }
                if dbfs >= MIN_REALISTIC_PEAK_DB - 1e-9 {
                    return Some(ClassifiedPeak {
                        linear: p,
                        dbfs,
                        kind: PeakKind::TruePeak,
                        source_tag: "TRUE_PEAK",
                    });
                }
            }
        }
        if let Some(p) = self.replaygain_track_peak {
            if p.is_finite() && p > 0.0 {
                let dbfs = 20.0 * p.log10();
                if dbfs > MAX_REALISTIC_TRUE_PEAK_DB + 1e-9 {
                    return None;
                }
                if let Some(dbfs) = realistic_dbfs(p) {
                    let kind = if p > 1.0 {
                        PeakKind::TruePeak
                    } else {
                        PeakKind::SamplePeak
                    };
                    return Some(ClassifiedPeak {
                        linear: p,
                        dbfs,
                        kind,
                        source_tag: "REPLAYGAIN_TRACK_PEAK",
                    });
                }
            }
        }
        if let Some(p) = self.replaygain_album_peak {
            if p.is_finite() && p > 0.0 {
                let dbfs = 20.0 * p.log10();
                if dbfs > MAX_REALISTIC_TRUE_PEAK_DB + 1e-9 {
                    return None;
                }
                if let Some(dbfs) = realistic_dbfs(p) {
                    let kind = if p > 1.0 {
                        PeakKind::TruePeak
                    } else {
                        PeakKind::SamplePeak
                    };
                    return Some(ClassifiedPeak {
                        linear: p,
                        dbfs,
                        kind,
                        source_tag: "REPLAYGAIN_ALBUM_PEAK",
                    });
                }
            }
        }
        None
    }

    /// True when a realistic True Peak (or above-FS sample peak `<= +6 dBTP`) is present.
    pub fn has_verified_true_peak(&self) -> bool {
        self.classified_peak()
            .is_some_and(|cp| cp.kind == PeakKind::TruePeak)
    }

    /// Return the best realistic peak amplitude `(linear, tag_name)` or `None` if unknown.
    pub fn effective_peak_linear(&self) -> Option<(f64, &'static str)> {
        self.classified_peak()
            .map(|cp| (cp.linear, cp.source_tag))
    }
}

/// Result of per-track headroom and convolution excess-scaling selection.
#[derive(Debug, Clone, PartialEq)]
pub struct HeadroomDecision {
    /// Strictly `-3 dB` or `0 dB` upsampler headroom gain `G_up`.
    pub mode: HeadroomMode,
    /// Realistic track peak in dBFS (or `None` when unknown).
    pub peak_dbfs: Option<f64>,
    /// Classification of the peak (`TruePeak` vs `SamplePeak`), or `None` when unknown.
    pub peak_kind: Option<PeakKind>,
    /// Attenuation (strictly `<= 0.0 dB`) applied ONLY to the active convolution stage by the
    /// exact excess over `0 dBFS` of `P + A_isp + G_up + max(G_c, 0)` (or `M + G_up` when a
    /// full-scan post-chain peak `M` exists). Always `0.0` when convolution is inactive.
    pub convolution_scale_db: f64,
    pub reason: String,
}

impl HeadroomDecision {
    /// Linear convolution scaling factor (`<= 1.0`, strictly `1.0` when no excess cut is needed).
    pub fn convolution_scale_linear(&self) -> f64 {
        if self.convolution_scale_db.is_finite() && self.convolution_scale_db < 0.0 {
            10.0_f64.powf(self.convolution_scale_db / 20.0)
        } else {
            1.0
        }
    }

    pub fn summary(&self) -> String {
        if self.convolution_scale_db < -1e-6 {
            format!(
                "Headroom: {} + ConvScale: {:.2} dB [{}]",
                self.mode, self.convolution_scale_db, self.reason
            )
        } else {
            format!("Headroom: {} [{}]", self.mode, self.reason)
        }
    }
}

/// Choose strictly `-3 dB` or `0 dB` upsampler gain `G_up` and excess-only convolution scaling.
///
/// - `is_streaming_source`: live streams have unknown peak (`P = 0 dBFS`, `A_isp = 3 dB`, `G_up = -3 dB`).
/// - `active_conv_max_gain_db`: `Some(G_c)` (`max |H(f)|` in dB, uncapped) when REW convolution is
///   active; `None` when convolution is bypassed (`NullConvolver`).
/// - `full_scan_post_chain_peak_db`: optional current full-scan post-chain peak `M` (dBFS); when
///   present and finite, convolution excess is computed from `M + G_up` over `0 dBFS`.
pub fn select_headroom(
    meta: &TrackPeakMetadata,
    is_streaming_source: bool,
    active_conv_max_gain_db: Option<f64>,
    full_scan_post_chain_peak_db: Option<f64>,
) -> HeadroomDecision {
    let classified = if is_streaming_source {
        None
    } else {
        meta.classified_peak()
    };

    // 1. Upsampler gain G_up is strictly 0 dB when realistic peak <= -3.0 dBFS, else -3 dB.
    let (mode, p_db, a_isp_db, peak_dbfs, peak_kind, mut reason) = match classified {
        Some(cp) => {
            if cp.dbfs <= UNKNOWN_PEAK_HEADROOM_DB + 1e-9 {
                (
                    HeadroomMode::ZeroDb,
                    cp.dbfs,
                    cp.isp_allowance_db(),
                    Some(cp.dbfs),
                    Some(cp.kind),
                    format!("{} = {:.2} dBFS <= -3.00 dBFS -> 0 dB", cp.source_tag, cp.dbfs),
                )
            } else {
                (
                    HeadroomMode::Minus3Db,
                    cp.dbfs,
                    cp.isp_allowance_db(),
                    Some(cp.dbfs),
                    Some(cp.kind),
                    format!("{} = {:.2} dBFS > -3.00 dBFS -> -3 dB", cp.source_tag, cp.dbfs),
                )
            }
        }
        None => {
            let r = if is_streaming_source {
                "streaming source (unknown max peak -> -3 dB fail-safe)".to_string()
            } else {
                "no realistic FLAC peak annotation (unknown max peak -> -3 dB fail-safe)".to_string()
            };
            (
                HeadroomMode::Minus3Db,
                0.0,
                SAMPLE_PEAK_ISP_ALLOWANCE_DB,
                None,
                None,
                r,
            )
        }
    };

    // 2. Active convolution is scaled ONLY by the excess over 0 dBFS.
    let convolution_scale_db = if let Some(g_c_db) = active_conv_max_gain_db {
        let g_up_db = mode.gain_db();
        let predicted_db = match full_scan_post_chain_peak_db {
            Some(m_db) if m_db.is_finite() => m_db + g_up_db,
            _ => {
                let g_c_pos = if g_c_db.is_finite() && g_c_db > 0.0 {
                    g_c_db
                } else {
                    0.0
                };
                p_db + a_isp_db + g_up_db + g_c_pos
            }
        };
        if predicted_db > 1e-9 {
            reason.push_str(&format!(
                "; conv excess +{predicted_db:.2} dB -> conv scale -{predicted_db:.2} dB"
            ));
            -predicted_db
        } else {
            0.0
        }
    } else {
        0.0
    };

    HeadroomDecision {
        mode,
        peak_dbfs,
        peak_kind,
        convolution_scale_db,
        reason,
    }
}
