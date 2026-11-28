// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

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

/// Maximum realistic True Peak linear amplitude (`2.0`, i.e., `+6.0206 dBTP`).
/// Peaks above this are treated as unit errors / unknown (`HEADROOM_DESIGN.md` §2).
pub const MAX_REALISTIC_TRUE_PEAK_LINEAR: f64 = 2.0;

/// Maximum realistic True Peak in dBTP (`20 * log10(2.0) ~= +6.0206 dBTP`).
pub const MAX_REALISTIC_TRUE_PEAK_DB: f64 = 6.020_599_913_279_624;

/// Relative rounding tolerance (0.5%) applied to cross-tag consistency checks
/// (`track_peak <= album_peak` and `true_peak >= sample_peak`).
pub const PEAK_TAG_ROUNDING_TOLERANCE: f64 = 0.005;

/// Relative tag-rounding slack (`1e-5`, ~0.00009 dB) applied above `2^(b-1) / (2^(b-1) - 1)`
/// so 6–7 decimal ReplayGain sample-peak tags (e.g. `1.000031` / `1.0000306` at 16-bit) stay
/// classified as `SamplePeak` (preserving `A_isp = 3 dB`, Review #6 Finding #14).
pub const SAMPLE_PEAK_UPPER_ROUNDING_TOLERANCE: f64 = 1e-5;

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

/// Optional loudness & peak annotations parsed from FLAC Vorbis comments (plus optional
/// Priority #1 full-scan YAML `true_peak_dbtp` override when track identity matches).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TrackPeakMetadata {
    pub replaygain_track_gain_db: Option<f64>,
    pub replaygain_track_peak: Option<f64>,
    pub replaygain_album_gain_db: Option<f64>,
    pub replaygain_album_peak: Option<f64>,
    /// Measured True Peak amplitude (linear relative to full scale `1.0`), parsed from
    /// `TRUE_PEAK`, `REPLAYGAIN_TRACK_TRUE_PEAK`, or `R128_TRACK_TRUE_PEAK`.
    pub true_peak_linear: Option<f64>,
    /// Set by the Vorbis tag parser when a true-peak tag was present with an implausible value
    /// (`> 2.0` linear / `> +6.02 dBTP`), preventing unsafe fallback to a quieter sample-peak tag.
    pub true_peak_invalid: bool,
    /// Priority #1 full-scan YAML `results.true_peak_dbtp` (converted to linear `10^(db/20)`),
    /// populated ONLY when YAML identity (`streaminfo_md5`, sample rate, bit depth) matches.
    pub full_scan_true_peak_linear: Option<f64>,
}

/// Valid sample-peak upper limit for `bits_per_sample` (`2^(b-1) / (2^(b-1) - 1)`), accounting
/// for ReplayGain scanners that normalize by `2^(b-1) - 1` (Review #5 Finding #16).
pub fn max_sample_peak_linear_for_bits(bits_per_sample: u16) -> f64 {
    let b = bits_per_sample.clamp(8, 32);
    let half = (1_u64 << (b - 1)) as f64;
    half / (half - 1.0)
}

/// 1-LSB realism floor in dBFS for `bits_per_sample` (clamped at `-120 dBFS`, Review #5 #16).
pub fn min_realistic_peak_dbfs_for_bits(bits_per_sample: u16) -> f64 {
    let b = bits_per_sample.clamp(8, 32);
    let lsb_db = -20.0 * ((1_u64 << (b - 1)) as f64).log10();
    lsb_db.max(MIN_REALISTIC_PEAK_DB)
}

fn realistic_dbfs_for_bits(linear: f64, bits_per_sample: u16) -> Option<f64> {
    if !linear.is_finite() || linear <= 0.0 {
        return None;
    }
    let dbfs = 20.0 * linear.log10();
    let min_db = min_realistic_peak_dbfs_for_bits(bits_per_sample);
    if dbfs >= min_db - 1e-9 && dbfs <= MAX_REALISTIC_TRUE_PEAK_DB + 1e-9 {
        Some(dbfs)
    } else {
        None
    }
}

impl TrackPeakMetadata {
    /// Return a copy with Priority #1 full-scan YAML `results.true_peak_dbtp` applied.
    pub fn with_full_scan_true_peak_dbtp(mut self, true_peak_dbtp: f64) -> Self {
        if true_peak_dbtp.is_finite() {
            self.full_scan_true_peak_linear = Some(10.0_f64.powf(true_peak_dbtp / 20.0));
        }
        self
    }

    /// Classify the best realistic peak amplitude using default 24-bit bounds.
    pub fn classified_peak(&self) -> Option<ClassifiedPeak> {
        self.classified_peak_for_bits(24)
    }

    /// Classify the best realistic peak amplitude per `HEADROOM_DESIGN.md` §2 for `bits_per_sample`:
    /// 1. Full-scan YAML `results.true_peak_dbtp` (Priority #1; overrides every tag when present).
    /// 2. Track/album true-peak Vorbis comment tags.
    /// 3. `REPLAYGAIN_TRACK_PEAK` (sample peak, or `TruePeak` if `> 2^(b-1)/(2^(b-1)-1)`).
    /// 4. `REPLAYGAIN_ALBUM_PEAK`.
    /// 5. Unknown (`None` -> `P = 0 dBFS`, `A_isp = 3 dB`, `G_up = -3 dB`).
    ///
    /// Realism checks enforced here:
    /// - True peak must lie in `[1 LSB, 2.0]` linear (`<= +6.0206 dBTP`); values `> 2.0` count as
    ///   unknown immediately instead of falling back to a quieter sample-peak tag.
    /// - When both `true_peak_linear` and `replaygain_track_peak` exist, `true_peak < sample_peak`
    ///   (beyond 0.5% rounding) discards the true-peak tag and falls back to the sample peak.
    /// - When both `replaygain_track_peak` and `replaygain_album_peak` exist, `track_peak > album_peak`
    ///   (beyond 0.5% rounding) rejects contradictory sample-peak tags as unknown (`None`).
    pub fn classified_peak_for_bits(&self, bits_per_sample: u16) -> Option<ClassifiedPeak> {
        let min_db = min_realistic_peak_dbfs_for_bits(bits_per_sample);
        let sp_upper = max_sample_peak_linear_for_bits(bits_per_sample);

        // Priority 1: Full-scan YAML `results.true_peak_dbtp` (overrides all Vorbis tags).
        if let Some(p) = self.full_scan_true_peak_linear {
            if !p.is_finite() || p <= 0.0 || p > MAX_REALISTIC_TRUE_PEAK_LINEAR + 1e-9 {
                return None;
            }
            let dbfs = 20.0 * p.log10();
            if dbfs >= min_db - 1e-9 {
                return Some(ClassifiedPeak {
                    linear: p,
                    dbfs,
                    kind: PeakKind::TruePeak,
                    source_tag: "FULL_SCAN_TRUE_PEAK",
                });
            }
            return None;
        }

        // Priority 2: Vorbis true-peak tag. An explicitly implausible true-peak tag (> 2.0 linear)
        // must immediately resolve to unknown rather than falling back to a quieter sample peak.
        if self.true_peak_invalid {
            return None;
        }
        if let Some(p) = self.true_peak_linear {
            if p.is_finite() && p > 0.0 {
                if p > MAX_REALISTIC_TRUE_PEAK_LINEAR + 1e-9 {
                    return None;
                }
                let below_sample_peak = self
                    .replaygain_track_peak
                    .is_some_and(|sp| sp.is_finite() && sp > 0.0 && p < sp * (1.0 - PEAK_TAG_ROUNDING_TOLERANCE));
                if !below_sample_peak {
                    let dbfs = 20.0 * p.log10();
                    if dbfs >= min_db - 1e-9 {
                        return Some(ClassifiedPeak {
                            linear: p,
                            dbfs,
                            kind: PeakKind::TruePeak,
                            source_tag: "TRUE_PEAK",
                        });
                    }
                }
            }
        }

        // §2 tag sanity: track sample peak must be <= album sample peak (within 0.5% rounding)
        // whenever both positive finite tags exist.
        if let (Some(tp), Some(ap)) = (self.replaygain_track_peak, self.replaygain_album_peak) {
            if tp.is_finite()
                && ap.is_finite()
                && tp > 0.0
                && ap > 0.0
                && tp > ap * (1.0 + PEAK_TAG_ROUNDING_TOLERANCE)
            {
                return None;
            }
        }

        // Priority 3: REPLAYGAIN_TRACK_PEAK.
        if let Some(p) = self.replaygain_track_peak {
            if p.is_finite() && p > 0.0 {
                if p > MAX_REALISTIC_TRUE_PEAK_LINEAR + 1e-9 {
                    return None;
                }
                if let Some(dbfs) = realistic_dbfs_for_bits(p, bits_per_sample) {
                    let kind = if p > sp_upper * (1.0 + SAMPLE_PEAK_UPPER_ROUNDING_TOLERANCE) {
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

        // Priority 4: REPLAYGAIN_ALBUM_PEAK.
        if let Some(p) = self.replaygain_album_peak {
            if p.is_finite() && p > 0.0 {
                if p > MAX_REALISTIC_TRUE_PEAK_LINEAR + 1e-9 {
                    return None;
                }
                if let Some(dbfs) = realistic_dbfs_for_bits(p, bits_per_sample) {
                    let kind = if p > sp_upper * (1.0 + SAMPLE_PEAK_UPPER_ROUNDING_TOLERANCE) {
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

    /// Combined non-boosting linear-stage gain in dB (`G_up + S`, strictly `<= 0.0 dB`).
    pub fn total_gain_db(&self) -> f64 {
        let s = if self.convolution_scale_db.is_finite() && self.convolution_scale_db < 0.0 {
            self.convolution_scale_db
        } else {
            0.0
        };
        self.mode.gain_db() + s
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

/// Choose strictly `-3 dB` or `0 dB` upsampler gain `G_up` and excess-only scaling `S`
/// using default 24-bit sample-peak bounds.
pub fn select_headroom(
    meta: &TrackPeakMetadata,
    is_streaming_source: bool,
    active_conv_max_gain_db: Option<f64>,
    full_scan_post_chain_peak_db: Option<f64>,
) -> HeadroomDecision {
    select_headroom_for_bits(
        meta,
        24,
        is_streaming_source,
        active_conv_max_gain_db,
        full_scan_post_chain_peak_db,
    )
}

/// Choose strictly `-3 dB` or `0 dB` upsampler gain `G_up` and excess-only scaling `S`
/// per `HEADROOM_DESIGN.md` §3–§4, accounting for source `bits_per_sample`.
///
/// Even when convolution is bypassed (`active_conv_max_gain_db` is `None` or `Some(0.0)`),
/// `G_c = 0.0` so realistic true peaks above `+3 dBTP` (e.g. `+4 dBTP`) still receive
/// `S = -1 dB` (Review #5 Finding #14).
pub fn select_headroom_for_bits(
    meta: &TrackPeakMetadata,
    bits_per_sample: u16,
    is_streaming_source: bool,
    active_conv_max_gain_db: Option<f64>,
    full_scan_post_chain_peak_db: Option<f64>,
) -> HeadroomDecision {
    let classified = if is_streaming_source {
        None
    } else {
        meta.classified_peak_for_bits(bits_per_sample)
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

    // 2. Excess-only scaling S over 0 dBFS (`G_c = 0.0` when NullConvolver, Review #5 #14).
    let g_c_db = active_conv_max_gain_db.unwrap_or(0.0);
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
    let convolution_scale_db = if predicted_db > 1e-9 {
        reason.push_str(&format!(
            "; conv excess +{predicted_db:.2} dB -> conv scale -{predicted_db:.2} dB"
        ));
        -predicted_db
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

/// Compute the quantizer margin `m_q` in dB (`docs/research/07-ultimate-mode.md` §4.2):
/// worst-case shaped-dither excursion `(1.5 * sum(|c_k|) + 1.5)` LSB below digital full scale.
///
/// For `ns-gentle-2` (`sum(|c_k|) = 2.0`), this is `4.5 LSB`.
pub fn quantizer_margin_db(dac_bits: u8, shaper_l1_sum: f64) -> f64 {
    let b = dac_bits.clamp(16, 24);
    let l1 = if shaper_l1_sum.is_finite() && shaper_l1_sum >= 0.0 {
        shaper_l1_sum
    } else {
        0.0
    };
    let margin_lsb = 1.5 * l1 + 1.5;
    let half_scale = (1_u64 << (b - 1)) as f64;
    let max_linear = (1.0 - margin_lsb / half_scale).clamp(0.5, 1.0);
    -20.0 * max_linear.log10()
}

/// DAC safety margin (`1.0 dB`, matching ITU-R BS.1770 / EBU R128 `-1.0 dBTP` ceiling) reserved
/// below digital full scale in Ultimate Mode so peak-aware gain never drives multibit or
/// delta-sigma DACs into near-0 dBFS overload.
pub const ULTIMATE_DAC_MARGIN_DB: f64 = 1.0;

/// True-peak meter error `A_tp` for the 8x / 32-tap-per-phase Kaiser meter (`tp8x-32`, Issue #23 §2).
pub const TP_METER_ERR_8X_32_DB: f64 = 0.1;

/// True-peak meter error `A_tp` for the 4x / 12-tap-per-phase meter (`tp4x-12`) or Vorbis true-peak
/// tag (Issue #23 §2).
pub const TP_METER_ERR_4X_12_DB: f64 = 0.7;

/// Inter-sample peak allowance `A_tp` when only a sample-peak tag is available (`3.0 dB`, Issue #23 §2).
pub const TP_METER_ERR_SAMPLE_PEAK_DB: f64 = SAMPLE_PEAK_ISP_ALLOWANCE_DB;

/// Typed peak evidence consumed by [`select_ultimate_headroom_v2`] (Issue #23 §§2, 3.4).
#[derive(Debug, Clone, PartialEq)]
pub enum PeakEvidence {
    /// E1 / E2: Cached exact unity-gain post-chain peak `M` for this audio identity and
    /// `PostChainKey`. `source_tp_db` (when present) governs the binary `-3 / 0 dB` `G_up` step
    /// (Owner Decision #1); legacy entries lacking a source true peak fall back to `m_db`.
    ExactPostChain {
        m_db: f64,
        source_tp_db: Option<f64>,
    },
    /// E3 / E4 / E5 (true-peak tag): Known source true peak `tp_db` (dBTP) with meter error
    /// `meter_err_db` (`0.1` for `tp8x-32`, `0.7` for `tp4x-12` or TP tag) and provenance label
    /// (`"scan-tp"`, `"cached-tp"`, `"tag-tp"`).
    SourceTruePeak {
        tp_db: f64,
        meter_err_db: f64,
        source: &'static str,
    },
    /// E5 (sample-peak tag only): Known source sample peak `sp_db` (dBFS); adds `A_isp = 3.0 dB`.
    SourceSamplePeak {
        sp_db: f64,
        source: &'static str,
    },
    /// No trustworthy source peak (fail-safe: `G_up = -3 dB`, `P = 0 dBFS`, `A_isp = 3.0 dB`).
    Unknown,
}

impl PeakEvidence {
    /// Short provenance tag for decision reports, `status --json`, and UI (`exact-M`, `scan-tp`,
    /// `cached-tp`, `tag-tp`, `tag-sp`, `unknown`).
    pub fn source_label(&self) -> &'static str {
        match self {
            Self::ExactPostChain { .. } => "exact-M",
            Self::SourceTruePeak { source, .. } => source,
            Self::SourceSamplePeak { source, .. } => source,
            Self::Unknown => "unknown",
        }
    }

    /// True-peak meter error / ISP allowance `A_tp` in dB associated with this evidence tier.
    pub fn a_tp_db(&self) -> f64 {
        match self {
            Self::ExactPostChain { .. } => 0.0,
            Self::SourceTruePeak { meter_err_db, .. } => {
                if meter_err_db.is_finite() && *meter_err_db >= 0.0 {
                    *meter_err_db
                } else {
                    TP_METER_ERR_4X_12_DB
                }
            }
            Self::SourceSamplePeak { .. } | Self::Unknown => TP_METER_ERR_SAMPLE_PEAK_DB,
        }
    }
}

/// Choose a **mandatory fixed whole-track gain** for Ultimate Mode using typed [`PeakEvidence`]
/// (Issue #23 §§2, 3.4, Owner Decisions #1–#2):
///
/// - **Gain step `G_up` is strictly binary (`0 dB` or `-3 dB`):** `0 dB` only when the known source
///   peak is `<= -3.00 dBFS`; otherwise `-3 dB` — including the cached-`M` branch (`ExactPostChain`),
///   which decides `G_up` on `source_tp_db` (falling back to `m_db` if absent).
/// - **Continuous excess term `S <= 0`:**
///   - `ExactPostChain`: `M_eff = max(M, M30)`, `S = min(0, -(M_eff + G_up + m_q + DAC_MARGIN))`.
///   - `SourceTruePeak` / `SourceSamplePeak`: `M_hat = max(M30, P + A_tp + A_up + max(G_c, 0))`,
///     `S = min(0, -(M_hat + G_up + m_q + DAC_MARGIN))`.
///   - `Unknown` / streaming (`is_streaming_source == true`): fail-safe `G_up = -3 dB`,
///     `G = min(-3 - max(G_c, 0), -(M30 + m_q + DAC_MARGIN))` (`OQ-33`).
/// - Normal mode (`select_headroom_for_bits`) is untouched.
#[allow(clippy::too_many_arguments)]
pub fn select_ultimate_headroom_v2(
    evidence: &PeakEvidence,
    is_streaming_source: bool,
    active_conv_max_gain_db: Option<f64>,
    window_m30_db: Option<f64>,
    a_up_db: f64,
    margin_m_q_db: f64,
) -> HeadroomDecision {
    let m_q = if margin_m_q_db.is_finite() && margin_m_q_db >= 0.0 {
        margin_m_q_db
    } else {
        0.0
    };
    let a_up = if a_up_db.is_finite() && a_up_db >= 0.0 {
        a_up_db
    } else {
        0.0
    };
    let g_c_pos = active_conv_max_gain_db
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(0.0);
    let m30_opt = window_m30_db.filter(|v| v.is_finite());
    let total_margin = m_q + ULTIMATE_DAC_MARGIN_DB;

    let fail_safe = |label: &str| -> HeadroomDecision {
        let g_up = UNKNOWN_PEAK_HEADROOM_DB; // -3.0 dB
        let mut s_db = -g_c_pos;
        let mut reason = if g_c_pos > 1e-9 {
            format!("{label} -> -3 dB; conv excess +{g_c_pos:.2} dB -> conv scale -{g_c_pos:.2} dB")
        } else {
            format!("{label} -> -3 dB")
        };
        if let Some(m30) = m30_opt {
            let s_m30 = (-(m30 + g_up + total_margin)).min(0.0);
            if s_m30 < s_db - 1e-9 {
                s_db = s_m30;
                let total = g_up + s_db;
                reason.push_str(&format!(
                    "; W30 exact peak M30={m30:+.2} dBFS (dac_margin={ULTIMATE_DAC_MARGIN_DB:.2} dB) -> fixed gain {total:+.2} dB"
                ));
            }
        }
        HeadroomDecision {
            mode: HeadroomMode::Minus3Db,
            peak_dbfs: None,
            peak_kind: None,
            convolution_scale_db: s_db,
            reason,
        }
    };

    // Streaming: OQ-33 mandates ALWAYS -3 dB safety (ignores evidence completely).
    if is_streaming_source {
        return fail_safe("streaming source (unknown max peak, OQ-33 fail-safe)");
    }

    match evidence {
        PeakEvidence::ExactPostChain { m_db, source_tp_db } if m_db.is_finite() => {
            let step_peak = source_tp_db.filter(|v| v.is_finite()).unwrap_or(*m_db);
            let mode = if step_peak <= UNKNOWN_PEAK_HEADROOM_DB + 1e-9 {
                HeadroomMode::ZeroDb
            } else {
                HeadroomMode::Minus3Db
            };
            let g_up = mode.gain_db();
            let m_eff = m30_opt.map_or(*m_db, |m30| m_db.max(m30));
            let s_db = (-(m_eff + g_up + total_margin)).min(0.0);
            let total = g_up + s_db;
            let step_src = if source_tp_db.filter(|v| v.is_finite()).is_some() {
                "source_tp"
            } else {
                "M_fallback"
            };
            let reason = format!(
                "ultimate cached exact M={m_db:+.2} dBFS (eff={m_eff:+.2} dBFS, {step_src}={step_peak:+.2} dBFS -> G_up={g_up:+.0} dB, S={s_db:+.2} dB, m_q={m_q:.5} dB, dac_margin={ULTIMATE_DAC_MARGIN_DB:.2} dB) -> fixed gain {total:+.2} dB"
            );
            HeadroomDecision {
                mode,
                peak_dbfs: Some(step_peak),
                peak_kind: Some(PeakKind::TruePeak),
                convolution_scale_db: s_db,
                reason,
            }
        }
        PeakEvidence::SourceTruePeak {
            tp_db,
            meter_err_db,
            source,
        } if tp_db.is_finite() => {
            let a_tp = if meter_err_db.is_finite() && *meter_err_db >= 0.0 {
                *meter_err_db
            } else {
                TP_METER_ERR_4X_12_DB
            };
            let mode = if *tp_db <= UNKNOWN_PEAK_HEADROOM_DB + 1e-9 {
                HeadroomMode::ZeroDb
            } else {
                HeadroomMode::Minus3Db
            };
            let g_up = mode.gain_db();
            let m_pred = *tp_db + a_tp + a_up + g_c_pos;
            let m_hat = m30_opt.map_or(m_pred, |m30| m_pred.max(m30));
            let s_db = (-(m_hat + g_up + total_margin)).min(0.0);
            let total = g_up + s_db;
            let reason = format!(
                "ultimate {source}={tp_db:+.2} dBTP -> G_up={g_up:+.0} dB (M_hat={m_hat:+.2} dBFS, A_tp={a_tp:.2}, A_up={a_up:.2}, G_c=+{g_c_pos:.2}, m_q={m_q:.5}, dac_margin={ULTIMATE_DAC_MARGIN_DB:.2}, S={s_db:+.2}) -> fixed gain {total:+.2} dB"
            );
            HeadroomDecision {
                mode,
                peak_dbfs: Some(*tp_db),
                peak_kind: Some(PeakKind::TruePeak),
                convolution_scale_db: s_db,
                reason,
            }
        }
        PeakEvidence::SourceSamplePeak { sp_db, source } if sp_db.is_finite() => {
            let a_tp = TP_METER_ERR_SAMPLE_PEAK_DB;
            let mode = if *sp_db <= UNKNOWN_PEAK_HEADROOM_DB + 1e-9 {
                HeadroomMode::ZeroDb
            } else {
                HeadroomMode::Minus3Db
            };
            let g_up = mode.gain_db();
            let m_pred = *sp_db + a_tp + a_up + g_c_pos;
            let m_hat = m30_opt.map_or(m_pred, |m30| m_pred.max(m30));
            let s_db = (-(m_hat + g_up + total_margin)).min(0.0);
            let total = g_up + s_db;
            let reason = format!(
                "ultimate {source}={sp_db:+.2} dBFS -> G_up={g_up:+.0} dB (M_hat={m_hat:+.2} dBFS, A_isp={a_tp:.2}, A_up={a_up:.2}, G_c=+{g_c_pos:.2}, m_q={m_q:.5}, dac_margin={ULTIMATE_DAC_MARGIN_DB:.2}, S={s_db:+.2}) -> fixed gain {total:+.2} dB"
            );
            HeadroomDecision {
                mode,
                peak_dbfs: Some(*sp_db),
                peak_kind: Some(PeakKind::SamplePeak),
                convolution_scale_db: s_db,
                reason,
            }
        }
        _ => fail_safe("unknown local max peak (fail-safe)"),
    }
}

/// Choose a **mandatory fixed whole-track gain** for Ultimate Mode (`OQ-33`, `07-ultimate-mode.md` §4.2):
/// - **Streaming sources (`is_streaming_source == true`):** track peak is never known, so ALWAYS
///   enforce `-3 dB` upsampling safety (+ REW excess `S`), tightened further only if `M30` requires.
/// - **Local file with cached whole-track exact `M`:**
///   `G = min(0, -(max(M, M30) + m_q + ULTIMATE_DAC_MARGIN_DB))` (zero overs, DAC margin preserved,
///   tracks peaking `<= -1 dBFS` untouched at `0 dB`).
/// - **Local file without cached `M` (first play):**
///   `G = min(G_HR, -(M30 + m_q + ULTIMATE_DAC_MARGIN_DB))` (`M30` can only add attenuation and
///   never relaxes normal HR safety).
#[allow(clippy::too_many_arguments)]
pub fn select_ultimate_headroom(
    meta: &TrackPeakMetadata,
    bits_per_sample: u16,
    is_streaming_source: bool,
    active_conv_max_gain_db: Option<f64>,
    cached_exact_m_db: Option<f64>,
    window_m30_db: Option<f64>,
    margin_m_q_db: f64,
) -> HeadroomDecision {
    let m_q = if margin_m_q_db.is_finite() && margin_m_q_db >= 0.0 {
        margin_m_q_db
    } else {
        0.0
    };
    let total_margin = m_q + ULTIMATE_DAC_MARGIN_DB;

    // Pack any constant total gain `g_db <= 0.0` into `(HeadroomMode, convolution_scale_db)` so
    // `HeadroomDecision::total_gain_db()` equals `g_db` exactly.
    let pack_gain = |g_db: f64,
                     peak_dbfs: Option<f64>,
                     peak_kind: Option<PeakKind>,
                     reason: String|
     -> HeadroomDecision {
        let g = g_db.min(0.0);
        if g <= UNKNOWN_PEAK_HEADROOM_DB + 1e-9 {
            HeadroomDecision {
                mode: HeadroomMode::Minus3Db,
                peak_dbfs,
                peak_kind,
                convolution_scale_db: (g - UNKNOWN_PEAK_HEADROOM_DB).min(0.0),
                reason,
            }
        } else {
            HeadroomDecision {
                mode: HeadroomMode::ZeroDb,
                peak_dbfs,
                peak_kind,
                convolution_scale_db: g,
                reason,
            }
        }
    };

    // Streaming: OQ-33 mandates ALWAYS -3 dB safety (never relaxed by cached M or tags).
    if is_streaming_source {
        let mut hr = select_headroom_for_bits(
            meta,
            bits_per_sample,
            true,
            active_conv_max_gain_db,
            None,
        );
        if let Some(m30) = window_m30_db.filter(|v| v.is_finite()) {
            let g_m30 = (-(m30 + total_margin)).min(0.0);
            if g_m30 < hr.total_gain_db() - 1e-9 {
                let reason = format!(
                    "{}; W30 exact peak M30={m30:+.2} dBFS (dac_margin={ULTIMATE_DAC_MARGIN_DB:.2} dB) -> tightened to {g_m30:.2} dB",
                    hr.reason
                );
                hr = pack_gain(g_m30, hr.peak_dbfs, hr.peak_kind, reason);
            }
        }
        return hr;
    }

    // Local file with cached whole-track exact post-chain peak M:
    // G = min(0, -(max(M, M30) + m_q + ULTIMATE_DAC_MARGIN_DB)).
    if let Some(m_db) = cached_exact_m_db.filter(|v| v.is_finite()) {
        let m_eff = window_m30_db
            .filter(|v| v.is_finite())
            .map_or(m_db, |m30| m_db.max(m30));
        let g_exact = (-(m_eff + total_margin)).min(0.0);
        let reason = format!(
            "ultimate cached exact M={m_db:+.2} dBFS (eff={m_eff:+.2} dBFS, m_q={m_q:.5} dB, dac_margin={ULTIMATE_DAC_MARGIN_DB:.2} dB) -> fixed gain {g_exact:+.2} dB"
        );
        return pack_gain(g_exact, Some(m_eff), Some(PeakKind::TruePeak), reason);
    }

    // Local file without cached M: G = min(G_HR, -(M30 + m_q + ULTIMATE_DAC_MARGIN_DB))
    // (M30 only adds safety, never relaxes).
    let hr = select_headroom_for_bits(
        meta,
        bits_per_sample,
        false,
        active_conv_max_gain_db,
        None,
    );
    let g_hr = hr.total_gain_db();
    if let Some(m30) = window_m30_db.filter(|v| v.is_finite()) {
        let g_m30 = (-(m30 + total_margin)).min(0.0);
        if g_m30 < g_hr - 1e-9 {
            let reason = format!(
                "{}; W30 exact peak M30={m30:+.2} dBFS (dac_margin={ULTIMATE_DAC_MARGIN_DB:.2} dB) -> fixed gain {g_m30:+.2} dB",
                hr.reason
            );
            return pack_gain(g_m30, hr.peak_dbfs, hr.peak_kind, reason);
        }
    }
    hr
}

/// Apply `HEADROOM_DESIGN.md` §4 gapless tail-overlap scaling (`S_B`) for Track B without ever
/// altering Stage-1 `G_up`.
///
/// NOTE (Review #6 Finding #1): `up_hist_peak` is the continuing LTI upsampler signal from Track A,
/// NOT an additive signal on top of Track B — summing it into `T` drove `T >= 1.0` and cut all of
/// Track B by `-60 dB` on every normal-level gapless REW transition. It is therefore excluded from
/// `total_tail`, and [`ap_dsp::AudioPipeline`] no longer applies whole-track `S_B` attenuation on
/// gapless REW transitions (keeping Track B's gain equal to Track A on continuous material and
/// relying on the pre-dither quantizer overs backstop).
#[allow(clippy::too_many_arguments)]
pub fn apply_gapless_tail_headroom(
    base: &mut HeadroomDecision,
    meta: &TrackPeakMetadata,
    bits_per_sample: u16,
    is_streaming_source: bool,
    conv_max_boost_db: f64,
    full_scan_post_chain_peak_db: Option<f64>,
    conv_tail_peak: f64,
    _up_hist_peak: f64,
) {
    let g_c_pos = if conv_max_boost_db.is_finite() && conv_max_boost_db > 0.0 {
        conv_max_boost_db
    } else {
        0.0
    };
    let total_tail = conv_tail_peak.max(0.0);
    if !total_tail.is_finite() || total_tail <= 1e-6 {
        return;
    }

    let b_out_db = match full_scan_post_chain_peak_db {
        Some(m_db) if m_db.is_finite() => m_db + base.mode.gain_db(),
        _ => {
            let (p_db, a_isp_db) = if is_streaming_source {
                (0.0, SAMPLE_PEAK_ISP_ALLOWANCE_DB)
            } else if let Some(cp) = meta.classified_peak_for_bits(bits_per_sample) {
                (cp.dbfs, cp.isp_allowance_db())
            } else {
                (0.0, SAMPLE_PEAK_ISP_ALLOWANCE_DB)
            };
            p_db + a_isp_db + base.mode.gain_db() + g_c_pos
        }
    };
    let room_db = 20.0 * (1.0 - total_tail).max(1e-3).log10();
    let s_b_db = 0.0_f64.min(-b_out_db).min(room_db - b_out_db);

    if s_b_db < base.convolution_scale_db - 1e-9 {
        base.convolution_scale_db = s_b_db;
        base.reason.push_str(&format!(
            "; incoming gapless tail T={total_tail:.3} (B_out={b_out_db:+.2} dB) -> S_B={s_b_db:.2} dB"
        ));
    }
}
