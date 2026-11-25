//! Metadata × Analyser Fusion Engine (`Evidence<T>` & §5.3 strict per-track fusion rules).
//!
//! Strictly per-track (`OQ-40`): every decision depends solely on this track's own user overrides,
//! cached whole-track YAML entry (`WT`), header facts, 30 s window analysis (`W30`), and tags.
//!
//! Order of trust (`docs/research/07-ultimate-mode.md` §5.3):
//!   `User > Cache (WT) > Header (facts) > Window (W30) > Tag (priors) > Default`

use ap_core::headroom::{PEAK_TAG_ROUNDING_TOLERANCE, PeakKind, TrackPeakMetadata};
use ap_core::metadata::{GenreClass, PhaseTagOverride, TrackMetadata};

use crate::analysis::{AnalysisResult, DetectorVerdict};

/// Tolerance in dB for disproving a stale true-peak Vorbis tag against `W30` (`07-ultimate-mode.md` §5.3 row 3).
pub const STALE_TRUE_PEAK_TOLERANCE_DB: f64 = 0.1;

/// Authoritative provenance of a fused decision input (`07-ultimate-mode.md` §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EvidenceSource {
    User,
    Cache,
    Header,
    Window,
    Tag,
    Default,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Cache => "cache(WT)",
            Self::Header => "header",
            Self::Window => "window(W30)",
            Self::Tag => "tag",
            Self::Default => "default",
        }
    }
}

/// Typed decision input tagged with the winning evidence source and human-readable rationale.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence<T> {
    pub value: T,
    pub source: EvidenceSource,
    pub detail: String,
}

impl<T> Evidence<T> {
    pub fn new(value: T, source: EvidenceSource, detail: impl Into<String>) -> Self {
        Self {
            value,
            source,
            detail: detail.into(),
        }
    }
}

/// Optional cached whole-track (`WT`) results loaded from the track's own Schema-1 YAML entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CachedWholeTrackEvidence {
    pub user_filter: Option<String>,
    pub user_deemphasis: Option<bool>,
    pub user_apodizing: Option<bool>,
    pub effective_nyquist_hz: Option<u32>,
    pub upsampled_from_1x: DetectorVerdict,
    pub apod_events: u32,
    pub apodizing_needed: DetectorVerdict,
    pub integrated_lufs: Option<f64>,
    pub clipped_runs: u32,
    pub loud_and_clipped: DetectorVerdict,
    pub true_peak_dbtp: Option<f64>,
    pub effective_bits: Option<u16>,
    pub ultrasonic_noise_detected: DetectorVerdict,
}

/// Complete per-track fused evidence ready for `UltimateSelector` and `U13` decision reporting.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedTrackEvidence {
    /// Validated `TrackPeakMetadata` after §5.3 row 3 stale-tag disproof check.
    pub peak_meta: TrackPeakMetadata,
    pub stale_peak_tag_disproved: bool,
    pub stale_peak_tag_detail: Option<String>,

    pub apodizing: Evidence<bool>,
    pub effective_nyquist_hz: Evidence<Option<u32>>,
    pub loud_and_clipped: Evidence<bool>,
    pub hf_noise_filter: Evidence<bool>,
    pub deemphasis: Evidence<bool>,
    pub user_filter: Option<Evidence<String>>,
    pub phase_override: Option<Evidence<PhaseTagOverride>>,
    pub genre_class: Evidence<GenreClass>,

    /// Informational / diagnostic notices (Rule B real bit depth, lossy origin, MQA, OQ-41 mixed
    /// CUESHEET flags, pre-emphasis shelf suggestion).
    pub notices: Vec<String>,
}

/// Execute all strict per-track §5.3 Metadata × Analyser fusion rules.
pub fn fuse_track_evidence(
    sample_rate_hz: u32,
    container_bits: u16,
    raw_peak_meta: &TrackPeakMetadata,
    track_meta: &TrackMetadata,
    window: Option<&AnalysisResult>,
    cached_wt: Option<&CachedWholeTrackEvidence>,
) -> FusedTrackEvidence {
    let mut notices = Vec::new();

    // --- Row 3: Source peak P + Stale Peak Tag Disproof Check (safety-critical!) ---
    let mut peak_meta = *raw_peak_meta;
    let mut stale_peak_tag_disproved = false;
    let mut stale_peak_tag_detail = None;

    if let Some(wt) = cached_wt {
        if let Some(wt_tp_db) = wt.true_peak_dbtp {
            peak_meta = peak_meta.with_full_scan_true_peak_dbtp(wt_tp_db);
        }
    }

    // Disprove stale Vorbis peak tags if the 30 s window provably exceeds them.
    if peak_meta.full_scan_true_peak_linear.is_none() {
        if let Some(w) = window {
            if let Some(cp) = raw_peak_meta.classified_peak_for_bits(container_bits) {
                match cp.kind {
                    PeakKind::SamplePeak => {
                        if w.sample_peak_linear > cp.linear * (1.0 + PEAK_TAG_ROUNDING_TOLERANCE) {
                            stale_peak_tag_disproved = true;
                            let msg = format!(
                                "stale peak tag ({}={:.4} linear < W30 sample peak {:.4}); falling back to -3 dB unknown",
                                cp.source_tag, cp.linear, w.sample_peak_linear
                            );
                            notices.push(msg.clone());
                            stale_peak_tag_detail = Some(msg);
                            peak_meta = TrackPeakMetadata::default();
                        }
                    }
                    PeakKind::TruePeak => {
                        if w.true_peak_dbtp > cp.dbfs + STALE_TRUE_PEAK_TOLERANCE_DB {
                            stale_peak_tag_disproved = true;
                            let msg = format!(
                                "stale peak tag ({}={:+.2} dBTP < W30 true peak {:+.2} dBTP); falling back to -3 dB unknown",
                                cp.source_tag, cp.dbfs, w.true_peak_dbtp
                            );
                            notices.push(msg.clone());
                            stale_peak_tag_detail = Some(msg);
                            peak_meta = TrackPeakMetadata::default();
                        }
                    }
                }
            }
        }
    }

    // --- User filter & phase overrides (YAML user.filter > AP_FILTER; AP_PHASE) ---
    let user_filter = if let Some(uf) = cached_wt.and_then(|c| c.user_filter.as_ref()) {
        if let Some(ref tag_f) = track_meta.ap_filter {
            if !tag_f.eq_ignore_ascii_case(uf) {
                notices.push(format!(
                    "filter override conflict: YAML user.filter='{uf}' wins over AP_FILTER='{tag_f}'"
                ));
            }
        }
        Some(Evidence::new(
            uf.clone(),
            EvidenceSource::User,
            format!("YAML user.filter={uf}"),
        ))
    } else {
        track_meta.ap_filter.as_ref().map(|tag_f| {
            Evidence::new(
                tag_f.clone(),
                EvidenceSource::User,
                format!("AP_FILTER={tag_f}"),
            )
        })
    };

    let phase_override = track_meta.ap_phase.map(|p| {
        Evidence::new(
            p,
            EvidenceSource::User,
            format!("AP_PHASE={p:?}"),
        )
    });

    // --- Row 8: Pre-emphasis (Rule P) — facts only! Analyser NEVER enables automatically. ---
    let deemphasis = if let Some(u_de) = cached_wt.and_then(|c| c.user_deemphasis) {
        Evidence::new(u_de, EvidenceSource::User, format!("YAML user.deemphasis={u_de}"))
    } else if let Some(tag_de) = track_meta.ap_deemphasis {
        Evidence::new(tag_de, EvidenceSource::User, format!("AP_DEEMPHASIS={tag_de}"))
    } else if track_meta.cuesheet_pre_emphasis {
        Evidence::new(
            true,
            EvidenceSource::Header,
            "CUESHEET track 1 FLAGS PRE".to_string(),
        )
    } else {
        if let Some(w) = window {
            if w.pre_emphasis_shelf_suspected && track_meta.is_pre_emphasis_era_cd() {
                notices.push(
                    "bright HF shelf on 1982–1989 CD without CUESHEET flag; consider tagging AP_DEEMPHASIS=1 (not auto-applied)"
                        .to_string(),
                );
            }
        }
        Evidence::new(false, EvidenceSource::Default, "no pre-emphasis flag")
    };
    if track_meta.cuesheet_mixed_emphasis {
        notices.push(format!(
            "OQ-41 warning: album-image CUESHEET ({} tracks) has mixed pre-emphasis flags; applying track 1 flag ({})",
            track_meta.cuesheet_audio_tracks, track_meta.cuesheet_pre_emphasis
        ));
    }

    // --- Row 5: Apodizing need (Rule A) — user > WT > W30 (with Rule E era prior). Tags alone never fire A. ---
    let apodizing = if let Some(u_ap) = cached_wt.and_then(|c| c.user_apodizing) {
        Evidence::new(u_ap, EvidenceSource::User, format!("YAML user.apodizing={u_ap}"))
    } else if let Some(tag_ap) = track_meta.ap_apodizing {
        Evidence::new(tag_ap, EvidenceSource::User, format!("AP_APODIZING={tag_ap}"))
    } else if let Some(wt) = cached_wt {
        let pos = wt.apodizing_needed == DetectorVerdict::Positive;
        Evidence::new(
            pos,
            EvidenceSource::Cache,
            format!("WT apod_events={} ({})", wt.apod_events, wt.apodizing_needed.as_str()),
        )
    } else if let Some(w) = window {
        let pos = w.apodizing_needed == DetectorVerdict::Positive;
        let era_note = if track_meta.is_early_digital_era() {
            " [Rule E era prior]"
        } else {
            ""
        };
        Evidence::new(
            pos,
            EvidenceSource::Window,
            format!(
                "W30 apod_events={}/{} onsets ({}){era_note}",
                w.apod_events,
                w.onset_count,
                w.apodizing_needed.as_str()
            ),
        )
    } else {
        Evidence::new(false, EvidenceSource::Default, "no analyser data")
    };

    // --- Row 6: Effective bandwidth / Fake hi-res (Rule H) — WT > W30; ORIGINALSAMPLERATE prior only ---
    let effective_nyquist_hz = if let Some(wt) = cached_wt {
        if wt.upsampled_from_1x == DetectorVerdict::Positive {
            Evidence::new(
                wt.effective_nyquist_hz,
                EvidenceSource::Cache,
                format!("WT fake hi-res f_eff={:?} Hz", wt.effective_nyquist_hz),
            )
        } else {
            Evidence::new(None, EvidenceSource::Cache, "WT bandwidth native")
        }
    } else if let Some(w) = window {
        if w.upsampled_from_1x == DetectorVerdict::Positive {
            Evidence::new(
                w.effective_nyquist_hz,
                EvidenceSource::Window,
                format!("W30 spectral cliff f_eff={:?} Hz", w.effective_nyquist_hz),
            )
        } else {
            Evidence::new(None, EvidenceSource::Window, "W30 bandwidth native")
        }
    } else {
        Evidence::new(None, EvidenceSource::Default, "native header rate")
    };

    // --- Row 4: Loud and clipped (Rule L) — WT > RG tag + W30 clipping > W30 alone ---
    let loud_and_clipped = if let Some(wt) = cached_wt {
        let pos = wt.loud_and_clipped == DetectorVerdict::Positive;
        Evidence::new(
            pos,
            EvidenceSource::Cache,
            format!(
                "WT lufs={:?} clips={} ({})",
                wt.integrated_lufs,
                wt.clipped_runs,
                wt.loud_and_clipped.as_str()
            ),
        )
    } else if let Some(rg_gain) = raw_peak_meta.replaygain_track_gain_db {
        // ReplayGain reference is -18 LUFS: gain <= -8 dB means >= -10 LUFS loud master.
        let tag_loud = rg_gain <= -8.0;
        let has_w30_clips = window.is_some_and(|w| w.clipped_runs >= 1);
        if tag_loud && has_w30_clips {
            Evidence::new(
                true,
                EvidenceSource::Window,
                format!(
                    "RG gain {rg_gain:+.1} dB + W30 clipped_runs={}",
                    window.map_or(0, |w| w.clipped_runs)
                ),
            )
        } else if tag_loud {
            Evidence::new(
                false,
                EvidenceSource::Tag,
                format!("RG gain {rg_gain:+.1} dB (loud) but no W30 clipped runs -> inconclusive"),
            )
        } else {
            Evidence::new(
                false,
                EvidenceSource::Tag,
                format!("RG gain {rg_gain:+.1} dB (dynamic)"),
            )
        }
    } else if let Some(w) = window {
        let pos = w.loud_and_clipped == DetectorVerdict::Positive;
        Evidence::new(
            pos,
            EvidenceSource::Window,
            format!(
                "W30 lufs={:.1} clips={} ({})",
                w.integrated_lufs.unwrap_or(-144.0),
                w.clipped_runs,
                w.loud_and_clipped.as_str()
            ),
        )
    } else {
        Evidence::new(false, EvidenceSource::Default, "no loudness data")
    };

    // --- Row 7: DSD-origin / ultrasonic ADC noise above 40 kHz (`OQ-37`) ---
    let hf_noise_filter = if sample_rate_hz >= 88_200 {
        let wt_pos = cached_wt
            .is_some_and(|wt| wt.ultrasonic_noise_detected == DetectorVerdict::Positive);
        let w_pos = window
            .is_some_and(|w| w.ultrasonic_noise_detected == DetectorVerdict::Positive);
        if wt_pos {
            Evidence::new(true, EvidenceSource::Cache, "WT ultrasonic noise shelf > 40 kHz (OQ-37)")
        } else if w_pos {
            let tag_note = if track_meta.is_dsd_hint { " + DSD/SACD tag" } else { "" };
            let w = window.unwrap();
            Evidence::new(
                true,
                EvidenceSource::Window,
                format!(
                    "W30 ultrasonic noise rise {:+.1} dB (level {:.1} dBFS){tag_note} -> HF noise filter",
                    w.ultrasonic_noise_rise_db, w.ultrasonic_noise_level_dbfs
                ),
            )
        } else {
            if track_meta.is_dsd_hint {
                notices.push(
                    "DSD/SACD provenance tag present without rising ultrasonic noise shelf in W30 (tag alone: log only)"
                        .to_string(),
                );
            }
            Evidence::new(false, EvidenceSource::Window, "no ultrasonic noise shelf")
        }
    } else {
        Evidence::new(false, EvidenceSource::Header, "1x rate (not applicable)")
    };

    // --- Row 9: Real bit depth (Rule B, report only) ---
    let eff_bits = cached_wt
        .and_then(|c| c.effective_bits)
        .or_else(|| window.map(|w| w.effective_bits));
    if let Some(eb) = eff_bits {
        if eb > 0 && eb + 2 <= container_bits {
            notices.push(format!(
                "Rule B: {eb}-bit effective content in {container_bits}-bit container"
            ));
        }
    }

    // --- Row 10: Lossy origin (report only) ---
    let w_lossy = window.is_some_and(|w| w.lossy_origin_suspected);
    if w_lossy && track_meta.is_lossy_hint {
        notices.push("Lossy origin highly likely (spectral cliff 15.5–19.5 kHz + lossy ENCODER tag)".to_string());
    } else if w_lossy {
        notices.push("Lossy origin suspected (spectral cliff 15.5–19.5 kHz in W30)".to_string());
    } else if track_meta.is_lossy_hint {
        notices.push("Lossy tool mentioned in ENCODER/COMMENT tag".to_string());
    }

    // --- Row 11: MQA warning (Rule M) ---
    if let Some(ref mqa) = track_meta.mqa_encoder {
        notices.push(format!("Rule M: MQA-encoded stream detected ({mqa})"));
    }

    // --- Row 12: Genre (Rule G, lowest priority) ---
    let genre_class = if track_meta.genre_class != GenreClass::Unknown {
        Evidence::new(
            track_meta.genre_class,
            EvidenceSource::Tag,
            format!("GENRE -> {}", track_meta.genre_class),
        )
    } else {
        Evidence::new(GenreClass::Unknown, EvidenceSource::Default, "no genre tags")
    };

    FusedTrackEvidence {
        peak_meta,
        stale_peak_tag_disproved,
        stale_peak_tag_detail,
        apodizing,
        effective_nyquist_hz,
        loud_and_clipped,
        hf_noise_filter,
        deemphasis,
        user_filter,
        phase_override,
        genre_class,
        notices,
    }
}
