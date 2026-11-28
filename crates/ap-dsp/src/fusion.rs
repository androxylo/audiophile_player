// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Metadata × Analyser Fusion Engine (`Evidence<T>` & §5.3 strict per-track fusion rules).
//!
//! Strictly per-track (`OQ-40`): every decision depends solely on this track's own user overrides,
//! cached whole-track YAML entry (`WT`), header facts, 30 s window analysis (`W30`), and tags.
//!
//! Order of trust (`docs/research/07-ultimate-mode.md` §5.3):
//!   `User > Cache (WT) > Header (facts) > Window (W30) > Tag (priors) > Default`

use ap_core::filter_db::{AlbumTrackAnalysis, TRUE_PEAK_METER_4X_12};
use ap_core::headroom::{PEAK_TAG_ROUNDING_TOLERANCE, PeakKind, TrackPeakMetadata};
use ap_core::metadata::{GenreClass, PhaseTagOverride, TrackMetadata};
use ap_core::track_db::{
    TRACK_DB_SCHEMA_VERSION, TrackDbDecision, TrackDbEntry, TrackDbFileInfo, TrackDbResults,
    TrackDbScanInfo, TrackDbUserOverrides, now_unix_secs,
};
use ap_core::user_filters::{ResolvedUserOverrides, UserFilterProvenance};

use crate::analysis::{AnalysisResult, DETECTOR_VERSION, DetectorVerdict};

/// Tolerance in dB for disproving a stale true-peak Vorbis tag against `W30` (`07-ultimate-mode.md` §5.3 row 3).
pub const STALE_TRUE_PEAK_TOLERANCE_DB: f64 = 0.1;

/// Threshold in dB above which a Vorbis ReplayGain / true-peak tag disagreeing with a measured
/// whole-file peak (E3 or E4) triggers an informational `"stale ReplayGain tag"` notice (`Issue #23 §3.3`).
pub const TAG_DISAGREEMENT_NOTICE_DB: f64 = 0.5;

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

/// Optional cached whole-track (`WT`) results loaded from Schema-2 album analysis or legacy Schema-1 YAML.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CachedWholeTrackEvidence {
    /// True only when the cached detector results match the current `DETECTOR_VERSION` and file
    /// identity (Review #7 F1). When `false`, detector rules (`A`, `H`, `L`, `HF`, `B`) are
    /// recomputed from fresh `W30`/whole-track analysis while `user_*` overrides still apply.
    pub results_current: bool,
    /// True when the cached entry's audio identity (MD5 / size+mtime) matches the current file,
    /// independent of `DETECTOR_VERSION` (Issue #23 P1 / K5). Source true peak, sample peak, and
    /// exact post-chain `M` depend only on the audio and DSP chain — never on detector versions.
    pub peak_current: bool,
    pub user_filter: Option<String>,
    /// Provenance of `user_filter` when resolved from Schema 2 `my-filters.yaml` (`Track`/`Folder`/`Tag`/`Default`).
    pub user_filter_provenance: Option<UserFilterProvenance>,
    /// Human-readable source detail for `user_filter` (e.g. `"my-filters.yaml folders:main:Jazz"`).
    pub user_filter_detail: Option<String>,
    /// Optional phase override from `my-filters.yaml` (`min` / `ip` / `lin`), overriding `AP_PHASE`.
    pub user_phase: Option<PhaseTagOverride>,
    /// Provenance of `user_phase` (`Track`/`Folder`/`Tag`/`Default`) for HP-09 specificity ordering.
    pub user_phase_provenance: Option<UserFilterProvenance>,
    pub user_phase_detail: Option<String>,
    pub user_deemphasis: Option<bool>,
    pub user_apodizing: Option<bool>,
    /// Non-fatal warnings from user override resolution (e.g. `my-filters.yaml` vs `AP_FILTER` conflicts).
    pub user_warnings: Vec<String>,
    /// When `true` (`FilterMode::Suggested`), ignore ALL user overrides (`my-filters.yaml`,
    /// Schema-1 `user:`, and `AP_*` Vorbis tags) so pure machine suggestions are evaluated.
    pub ignore_user_overrides: bool,
    pub effective_nyquist_hz: Option<u32>,
    pub upsampled_from_1x: DetectorVerdict,
    pub apod_events: u32,
    pub apodizing_needed: DetectorVerdict,
    pub integrated_lufs: Option<f64>,
    pub clipped_runs: u32,
    pub loud_and_clipped: DetectorVerdict,
    pub sample_peak: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
    /// True-peak meter identifier (`"tp4x-12"` / `"tp8x-32"`), populated when `peak_current` is true.
    pub true_peak_meter: Option<String>,
    pub effective_bits: Option<u16>,
    pub ultrasonic_noise_detected: DetectorVerdict,
}

impl CachedWholeTrackEvidence {
    /// Convert a validated Schema-1 [`TrackDbEntry`] into [`CachedWholeTrackEvidence`].
    /// When `results_current == false`, only `user:` overrides are preserved while stale
    /// detector results are left `Inconclusive` / `None`.
    pub fn from_track_db_entry(entry: &TrackDbEntry, results_current: bool) -> Self {
        Self::from_track_db_entry_with_peak(entry, results_current, results_current)
    }

    /// Convert a legacy Schema-1 [`TrackDbEntry`] into [`CachedWholeTrackEvidence`].
    ///
    /// Review #10 §1.3 (T-C4): Schema-1 `trackdb` peaks came from the 4x meter and MUST NEVER
    /// feed headroom (`peak_current = false`, `true_peak_dbtp = None`). Only non-peak detector
    /// results (when `results_current`) and `user:` overrides are preserved.
    pub fn from_track_db_entry_with_peak(
        entry: &TrackDbEntry,
        results_current: bool,
        _peak_current: bool,
    ) -> Self {
        let mut out = Self {
            results_current,
            peak_current: false,
            user_filter: entry.user.filter.clone(),
            user_filter_provenance: entry.user.filter.as_ref().map(|_| UserFilterProvenance::Track),
            user_deemphasis: entry.user.deemphasis,
            user_apodizing: entry.user.apodizing,
            ..Self::default()
        };
        if results_current {
            out.effective_nyquist_hz = entry.results.effective_nyquist_hz;
            out.upsampled_from_1x = DetectorVerdict::from_str_lossy(&entry.results.upsampled_from_1x);
            out.apod_events = entry.results.apod_events;
            out.apodizing_needed = DetectorVerdict::from_str_lossy(&entry.results.apodizing_needed);
            out.integrated_lufs = entry.results.integrated_lufs;
            out.clipped_runs = entry.results.clipped_runs;
            out.loud_and_clipped = DetectorVerdict::from_str_lossy(&entry.results.loud_and_clipped);
            out.effective_bits = (entry.results.effective_bits > 0).then_some(entry.results.effective_bits);
            out.ultrasonic_noise_detected =
                DetectorVerdict::from_str_lossy(&entry.results.ultrasonic_noise_detected);
        }
        out
    }

    /// Convert a validated Schema-2 [`AlbumTrackAnalysis`] into [`CachedWholeTrackEvidence`].
    ///
    /// Note: Schema 2 separates machine-owned analysis from user choices (`my-filters.yaml`), so
    /// chain `.with_user_overrides(...)` (in `Mine` mode) or `.suggested_only()` (in `Suggested` mode).
    pub fn from_album_track(track: &AlbumTrackAnalysis, results_current: bool) -> Self {
        Self::from_album_track_with_peak(track, results_current, results_current)
    }

    /// Convert a Schema-2 [`AlbumTrackAnalysis`] with independent `results_current` (identity +
    /// `DETECTOR_VERSION`) and `peak_current` (audio identity only, Issue #23 P1).
    pub fn from_album_track_with_peak(
        track: &AlbumTrackAnalysis,
        results_current: bool,
        peak_current: bool,
    ) -> Self {
        let mut out = Self {
            results_current,
            peak_current: peak_current || results_current,
            ..Self::default()
        };
        if out.peak_current && track.has_accurate_source_peak() {
            out.sample_peak =
                (track.sample_peak.is_finite() && track.sample_peak > 0.0).then_some(track.sample_peak);
            out.true_peak_dbtp = track.true_peak_dbtp.filter(|v| v.is_finite());
            out.true_peak_meter = track.effective_true_peak_meter().map(str::to_string);
        }
        if results_current {
            out.effective_nyquist_hz = track.effective_nyquist_hz;
            out.upsampled_from_1x = DetectorVerdict::from_str_lossy(&track.fake_hires);
            out.apod_events = track.apod_events;
            out.apodizing_needed = DetectorVerdict::from_str_lossy(&track.apodizing_needed);
            out.integrated_lufs = track.lufs;
            out.clipped_runs = track.clipped_runs;
            out.loud_and_clipped = DetectorVerdict::from_str_lossy(&track.loud_and_clipped);
            out.effective_bits = (track.effective_bits > 0).then_some(track.effective_bits);
            out.ultrasonic_noise_detected =
                DetectorVerdict::from_str_lossy(&track.hf_noise);
        }
        out
    }

    /// Apply resolved Schema-2 [`ResolvedUserOverrides`] (from `my-filters.yaml` + `AP_*` tags).
    pub fn with_user_overrides(mut self, overrides: &ResolvedUserOverrides) -> Self {
        self.ignore_user_overrides = false;
        if let Some((ref f, prov, ref detail)) = overrides.filter {
            self.user_filter = Some(f.clone());
            self.user_filter_provenance = Some(prov);
            self.user_filter_detail = Some(detail.clone());
        }
        if let Some((p, prov, ref detail)) = overrides.phase {
            self.user_phase = Some(p);
            self.user_phase_provenance = Some(prov);
            self.user_phase_detail = Some(detail.clone());
        }
        if let Some((ap, _)) = overrides.apodizing {
            self.user_apodizing = Some(ap);
        }
        if let Some((de, _)) = overrides.deemphasis {
            self.user_deemphasis = Some(de);
        }
        self.user_warnings.extend(overrides.warnings.iter().cloned());
        self
    }

    /// Mark this evidence as `FilterMode::Suggested` (ignore all user overrides and `AP_*` tags).
    pub fn suggested_only(mut self) -> Self {
        self.ignore_user_overrides = true;
        self.user_filter = None;
        self.user_filter_provenance = None;
        self.user_filter_detail = None;
        self.user_phase = None;
        self.user_phase_provenance = None;
        self.user_phase_detail = None;
        self.user_apodizing = None;
        self.user_deemphasis = None;
        self.user_warnings.clear();
        self
    }

    /// Construct current whole-track (`WT`) evidence directly from a fresh full-track
    /// [`AnalysisResult`] plus optional persisted `user:` overrides (Review #7 re-review R4).
    ///
    /// Used by `audiophile-player scan` so scan-time fusion takes the exact same `WT` branches in
    /// [`fuse_track_evidence`] (Rules A, H, L, HF-noise, effective bits, true peak) as cached
    /// playback does, guaranteeing identical filter decisions and `--post-chain` keys.
    pub fn from_whole_track_analysis(
        analysis: &AnalysisResult,
        user: Option<&ap_core::track_db::TrackDbUserOverrides>,
    ) -> Self {
        let tp = analysis
            .true_peak_dbtp
            .is_finite()
            .then_some(analysis.true_peak_dbtp);
        Self {
            results_current: true,
            peak_current: true,
            user_filter: user.and_then(|u| u.filter.clone()),
            user_filter_provenance: user
                .and_then(|u| u.filter.as_ref())
                .map(|_| UserFilterProvenance::Track),
            user_deemphasis: user.and_then(|u| u.deemphasis),
            user_apodizing: user.and_then(|u| u.apodizing),
            effective_nyquist_hz: analysis.effective_nyquist_hz,
            upsampled_from_1x: analysis.upsampled_from_1x,
            apod_events: analysis.apod_events,
            apodizing_needed: analysis.apodizing_needed,
            integrated_lufs: analysis.integrated_lufs,
            clipped_runs: analysis.clipped_runs,
            loud_and_clipped: analysis.loud_and_clipped,
            sample_peak: (analysis.sample_peak_linear.is_finite()
                && analysis.sample_peak_linear > 0.0)
                .then_some(analysis.sample_peak_linear),
            true_peak_dbtp: tp,
            true_peak_meter: tp.map(|_| TRUE_PEAK_METER_4X_12.to_string()),
            effective_bits: (analysis.effective_bits > 0).then_some(analysis.effective_bits),
            ultrasonic_noise_detected: analysis.ultrasonic_noise_detected,
            ..Self::default()
        }
    }
}

/// Build a canonical Schema-1 [`TrackDbEntry`] from whole-track [`AnalysisResult`] + identity.
#[allow(clippy::too_many_arguments)]
pub fn build_track_db_entry(
    file_path: String,
    file_size: u64,
    mtime_unix_s: u64,
    streaminfo_md5_hex: Option<String>,
    analysis: &AnalysisResult,
    deemphasis_flag: bool,
    decision_filter: String,
    decision_rule: String,
) -> TrackDbEntry {
    TrackDbEntry {
        schema: TRACK_DB_SCHEMA_VERSION,
        file: TrackDbFileInfo {
            path: file_path,
            size: file_size,
            mtime_unix_s,
            streaminfo_md5: streaminfo_md5_hex,
            sample_rate: analysis.sample_rate_hz,
            bits: analysis.container_bits,
        },
        scan: TrackDbScanInfo {
            detector_version: DETECTOR_VERSION,
            scanned_at_unix_s: now_unix_secs(),
            duration_s: analysis.duration_s,
        },
        results: TrackDbResults {
            effective_bandwidth_hz: analysis.effective_bandwidth_hz,
            upsampled_from_1x: analysis.upsampled_from_1x.as_str().to_string(),
            effective_nyquist_hz: analysis.effective_nyquist_hz,
            ultrasonic_noise_detected: analysis.ultrasonic_noise_detected.as_str().to_string(),
            lossy_origin_suspected: analysis.lossy_origin_suspected,
            apod_events: analysis.apod_events,
            apodizing_needed: analysis.apodizing_needed.as_str().to_string(),
            integrated_lufs: analysis.integrated_lufs,
            sample_peak: analysis.sample_peak_linear,
            true_peak_dbtp: analysis.true_peak_dbtp.is_finite().then_some(analysis.true_peak_dbtp),
            clipped_runs: analysis.clipped_runs,
            loud_and_clipped: analysis.loud_and_clipped.as_str().to_string(),
            effective_bits: analysis.effective_bits,
            deemphasis_flag,
            post_chain_peaks: Vec::new(),
        },
        decision: TrackDbDecision {
            filter: decision_filter,
            rule: decision_rule,
        },
        user: TrackDbUserOverrides::default(),
    }
}

/// Complete per-track fused evidence ready for `UltimateSelector` and `U13` decision reporting.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedTrackEvidence {
    /// Validated `TrackPeakMetadata` after §5.3 row 3 stale-tag disproof check.
    pub peak_meta: TrackPeakMetadata,
    pub stale_peak_tag_disproved: bool,
    pub stale_peak_tag_detail: Option<String>,
    /// Lower-bound true peak (`max(tag, W30 true peak)`) preserved for reporting when a stale tag
    /// is disproved by W30 without E3/E4 whole-track evidence (`Issue #23 §3.3`).
    pub w30_peak_lower_bound_dbtp: Option<f64>,

    pub apodizing: Evidence<bool>,
    pub effective_nyquist_hz: Evidence<Option<u32>>,
    pub loud_and_clipped: Evidence<bool>,
    pub hf_noise_filter: Evidence<bool>,
    pub deemphasis: Evidence<bool>,
    pub user_filter: Option<Evidence<String>>,
    /// Provenance of `user_filter` (`Track` / `Folder` / `Tag` / `Default`) for rule naming & dual display.
    pub user_filter_provenance: Option<UserFilterProvenance>,
    pub phase_override: Option<Evidence<PhaseTagOverride>>,
    /// Provenance of `phase_override` (`Track` / `Folder` / `Tag` / `Default`) for HP-09 specificity.
    pub phase_override_provenance: Option<UserFilterProvenance>,
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
    let ignore_user = cached_wt.is_some_and(|c| c.ignore_user_overrides);
    if !ignore_user {
        if let Some(c) = cached_wt {
            notices.extend(c.user_warnings.iter().cloned());
        }
    }
    // Gate detector-result lookups on `results_current` so stale entries never mask fresh
    // W30 / whole-track analysis, while `peak_current` independently gates audio-identity-only
    // peak measurements (Issue #23 P1 / K5) and `cached_wt` still provides `user:` overrides.
    let wt_results = cached_wt.filter(|c| c.results_current);
    let wt_peak = cached_wt.filter(|c| c.peak_current || c.results_current);

    // --- Row 3: Source peak P + Stale Peak Tag Disproof Check (safety-critical!) ---
    let mut peak_meta = *raw_peak_meta;
    let mut stale_peak_tag_disproved = false;
    let mut stale_peak_tag_detail = None;
    let mut w30_peak_lower_bound_dbtp = None;

    if let Some(wt) = wt_peak {
        if let Some(wt_tp_db) = wt.true_peak_dbtp {
            peak_meta = peak_meta.with_full_scan_true_peak_dbtp(wt_tp_db);
            // Issue #23 §3.3 + Review #10 HP-12: E4 cached whole-track peak unconditionally wins;
            // compare TruePeak tags against cached true peak, SamplePeak tags against cached
            // sample peak, and skip album-peak-only tags.
            if let Some(cp) = raw_peak_meta.classified_peak_for_bits(container_bits) {
                if cp.source_tag != "REPLAYGAIN_ALBUM_PEAK" {
                    let measured_opt = match cp.kind {
                        PeakKind::TruePeak => Some((wt_tp_db, "cached true peak", "dBTP")),
                        PeakKind::SamplePeak => wt
                            .sample_peak
                            .filter(|&s| s.is_finite() && s > 0.0)
                            .map(|s| (20.0 * s.log10(), "cached sample peak", "dBFS")),
                    };
                    if let Some((measured_db, label, unit)) = measured_opt {
                        let diff = (measured_db - cp.dbfs).abs();
                        if diff > TAG_DISAGREEMENT_NOTICE_DB {
                            let msg = format!(
                                "stale ReplayGain tag ({}={:+.2} dBFS differs from {label} {:+.2} {unit} by {:.2} dB)",
                                cp.source_tag, cp.dbfs, measured_db, diff
                            );
                            notices.push(msg);
                        }
                    }
                }
            }
        }
    }

    // Disprove stale Vorbis peak tags if the 30 s window provably exceeds them.
    // Review #10 HP-20: condition ONLY on `full_scan_true_peak_linear.is_none()` so a cached
    // entry without an accurate true peak never skips W30 stale-tag validation.
    if peak_meta.full_scan_true_peak_linear.is_none() {
        if let Some(w) = window {
            if let Some(cp) = raw_peak_meta.classified_peak_for_bits(container_bits) {
                let lb = if w.true_peak_dbtp.is_finite() {
                    w.true_peak_dbtp.max(cp.dbfs)
                } else {
                    cp.dbfs
                };
                match cp.kind {
                    PeakKind::SamplePeak => {
                        if w.sample_peak_linear > cp.linear * (1.0 + PEAK_TAG_ROUNDING_TOLERANCE) {
                            stale_peak_tag_disproved = true;
                            w30_peak_lower_bound_dbtp = Some(lb);
                            let msg = format!(
                                "stale peak tag ({}={:.4} linear < W30 sample peak {:.4}; W30 lower bound {:+.2} dBTP, falling back to -3 dB unknown)",
                                cp.source_tag, cp.linear, w.sample_peak_linear, lb
                            );
                            notices.push(msg.clone());
                            stale_peak_tag_detail = Some(msg);
                            peak_meta = TrackPeakMetadata::default();
                        }
                    }
                    PeakKind::TruePeak => {
                        if w.true_peak_dbtp > cp.dbfs + STALE_TRUE_PEAK_TOLERANCE_DB {
                            stale_peak_tag_disproved = true;
                            w30_peak_lower_bound_dbtp = Some(lb);
                            let msg = format!(
                                "stale peak tag ({}={:+.2} dBTP < W30 true peak {:+.2} dBTP; W30 lower bound {:+.2} dBTP, falling back to -3 dB unknown)",
                                cp.source_tag, cp.dbfs, w.true_peak_dbtp, lb
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

    // --- User filter & phase overrides (`my-filters.yaml` > `AP_FILTER` / `AP_PHASE`, skipped in `suggested` mode) ---
    let (user_filter, user_filter_provenance) = if ignore_user {
        (None, None)
    } else if let Some(uf) = cached_wt.and_then(|c| c.user_filter.as_ref()) {
        let prov = cached_wt
            .and_then(|c| c.user_filter_provenance)
            .unwrap_or(UserFilterProvenance::Track);
        let detail = cached_wt
            .and_then(|c| c.user_filter_detail.clone())
            .unwrap_or_else(|| format!("YAML user.filter={uf}"));
        if prov != UserFilterProvenance::Tag {
            if let Some(ref tag_f) = track_meta.ap_filter {
                if !tag_f.eq_ignore_ascii_case(uf)
                    && !notices.iter().any(|n| n.contains("wins over AP_FILTER"))
                {
                    notices.push(format!(
                        "filter override conflict: {detail} wins over AP_FILTER='{tag_f}'"
                    ));
                }
            }
        }
        (
            Some(Evidence::new(uf.clone(), EvidenceSource::User, detail)),
            Some(prov),
        )
    } else if let Some(tag_f) = track_meta.ap_filter.as_ref() {
        (
            Some(Evidence::new(
                tag_f.clone(),
                EvidenceSource::User,
                format!("AP_FILTER={tag_f}"),
            )),
            Some(UserFilterProvenance::Tag),
        )
    } else {
        (None, None)
    };

    let (phase_override, phase_override_provenance) = if ignore_user {
        (None, None)
    } else if let Some(up) = cached_wt.and_then(|c| c.user_phase) {
        let prov = cached_wt
            .and_then(|c| c.user_phase_provenance)
            .unwrap_or(UserFilterProvenance::Track);
        let detail = cached_wt
            .and_then(|c| c.user_phase_detail.clone())
            .unwrap_or_else(|| format!("my-filters.yaml phase={up:?}"));
        (Some(Evidence::new(up, EvidenceSource::User, detail)), Some(prov))
    } else if let Some(p) = track_meta.ap_phase {
        (
            Some(Evidence::new(p, EvidenceSource::User, format!("AP_PHASE={p:?}"))),
            Some(UserFilterProvenance::Tag),
        )
    } else {
        (None, None)
    };

    // --- Row 8: Pre-emphasis (Rule P) — facts only! Analyser NEVER enables automatically. ---
    let deemphasis = if !ignore_user && cached_wt.and_then(|c| c.user_deemphasis).is_some() {
        let u_de = cached_wt.and_then(|c| c.user_deemphasis).unwrap();
        Evidence::new(u_de, EvidenceSource::User, format!("YAML user.deemphasis={u_de}"))
    } else if !ignore_user && track_meta.ap_deemphasis.is_some() {
        let tag_de = track_meta.ap_deemphasis.unwrap();
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
    let apodizing = if !ignore_user && cached_wt.and_then(|c| c.user_apodizing).is_some() {
        let u_ap = cached_wt.and_then(|c| c.user_apodizing).unwrap();
        Evidence::new(u_ap, EvidenceSource::User, format!("YAML user.apodizing={u_ap}"))
    } else if !ignore_user && track_meta.ap_apodizing.is_some() {
        let tag_ap = track_meta.ap_apodizing.unwrap();
        Evidence::new(tag_ap, EvidenceSource::User, format!("AP_APODIZING={tag_ap}"))
    } else if let Some(wt) = wt_results {
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
    // Review #7 F11: Reject out-of-range `f_eff` values (<= 19 kHz DR-1 floor + 2% margin, or > fs/2)
    // gracefully with a notice instead of letting them fail filter synthesis downstream.
    let validate_feff = |feff_opt: Option<u32>, notices: &mut Vec<String>| -> Option<u32> {
        let feff = feff_opt?;
        let min_valid = 19_380_u32; // DR1_MIN_PASSBAND_HZ (19_000) * 1.02
        let max_valid = sample_rate_hz / 2;
        if (sample_rate_hz >= 44_100 && feff <= min_valid) || feff > max_valid {
            notices.push(format!(
                "f_eff={feff} Hz outside valid DR-1/Nyquist bounds ({min_valid}..={max_valid} Hz); ignored"
            ));
            None
        } else {
            Some(feff)
        }
    };
    let effective_nyquist_hz = if let Some(wt) = wt_results {
        if wt.upsampled_from_1x == DetectorVerdict::Positive {
            let valid_feff = validate_feff(wt.effective_nyquist_hz, &mut notices);
            if let Some(f) = valid_feff {
                Evidence::new(
                    Some(f),
                    EvidenceSource::Cache,
                    format!("WT fake hi-res f_eff={f} Hz"),
                )
            } else {
                Evidence::new(None, EvidenceSource::Cache, "WT f_eff out of DR-1 range; ignored")
            }
        } else {
            Evidence::new(None, EvidenceSource::Cache, "WT bandwidth native")
        }
    } else if let Some(w) = window {
        if w.upsampled_from_1x == DetectorVerdict::Positive {
            let valid_feff = validate_feff(w.effective_nyquist_hz, &mut notices);
            if let Some(f) = valid_feff {
                Evidence::new(
                    Some(f),
                    EvidenceSource::Window,
                    format!("W30 spectral cliff f_eff={f} Hz"),
                )
            } else {
                Evidence::new(None, EvidenceSource::Window, "W30 f_eff out of DR-1 range; ignored")
            }
        } else {
            Evidence::new(None, EvidenceSource::Window, "W30 bandwidth native")
        }
    } else {
        Evidence::new(None, EvidenceSource::Default, "native header rate")
    };

    // --- Row 4: Loud and clipped (Rule L) — Review #10 HP-11: WT > W30 > RG tag tie-breaker ---
    let loud_and_clipped = if let Some(wt) = wt_results {
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
    } else if let Some(w) = window.filter(|w| w.loud_and_clipped != DetectorVerdict::Inconclusive) {
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
    } else if let Some(rg_gain) = raw_peak_meta.replaygain_track_gain_db {
        // ReplayGain reference is -18 LUFS: gain <= -8 dB means >= -10 LUFS loud master.
        // Used only as a tie-breaker when W30 is Inconclusive or absent (preserves Window > Tag).
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
    // Review #8 C1: When current whole-track (`wt_results`) is available, treat WT as
    // authoritative (matching trust order Cache (WT) > Window) so scan and cached playback
    // always produce identical `PostChainKey.hf_noise_filter` values.
    let hf_noise_filter = if sample_rate_hz >= 88_200 {
        if let Some(wt) = wt_results {
            let pos = wt.ultrasonic_noise_detected == DetectorVerdict::Positive;
            if !pos && track_meta.is_dsd_hint {
                notices.push(
                    "DSD/SACD provenance tag present without rising ultrasonic noise shelf in WT (tag alone: log only)"
                        .to_string(),
                );
            }
            Evidence::new(
                pos,
                EvidenceSource::Cache,
                format!(
                    "WT ultrasonic noise shelf > 40 kHz ({})",
                    wt.ultrasonic_noise_detected.as_str()
                ),
            )
        } else if window.is_some_and(|w| w.ultrasonic_noise_detected == DetectorVerdict::Positive) {
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
    let eff_bits = wt_results
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
        w30_peak_lower_bound_dbtp,
        apodizing,
        effective_nyquist_hz,
        loud_and_clipped,
        hf_noise_filter,
        deemphasis,
        user_filter,
        user_filter_provenance,
        phase_override,
        phase_override_provenance,
        genre_class,
        notices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wt_negative_overrides_w30_positive_hf_noise_c1() {
        let mut w30 = crate::analysis::TrackAnalyzer::new(96_000, 24)
            .snapshot(false, false, false, false);
        w30.ultrasonic_noise_detected = DetectorVerdict::Positive;
        w30.ultrasonic_noise_rise_db = 8.5;
        w30.ultrasonic_noise_level_dbfs = -65.0;

        let wt = CachedWholeTrackEvidence {
            results_current: true,
            true_peak_dbtp: Some(-1.0),
            integrated_lufs: Some(-16.0),
            clipped_runs: 0,
            loud_and_clipped: DetectorVerdict::Negative,
            apodizing_needed: DetectorVerdict::Negative,
            apod_events: 0,
            upsampled_from_1x: DetectorVerdict::Negative,
            effective_nyquist_hz: None,
            ultrasonic_noise_detected: DetectorVerdict::Negative,
            effective_bits: Some(24),
            ..CachedWholeTrackEvidence::default()
        };

        let fused = fuse_track_evidence(
            96_000,
            24,
            &TrackPeakMetadata::default(),
            &TrackMetadata::default(),
            Some(&w30),
            Some(&wt),
        );
        assert!(
            !fused.hf_noise_filter.value,
            "C1: current WT Negative must override W30 Positive for hf_noise_filter"
        );
        assert_eq!(fused.hf_noise_filter.source, EvidenceSource::Cache);
    }

    #[test]
    fn suggested_only_ignores_user_overrides_and_ap_tags_but_keeps_cuesheet_preemphasis() {
        let mut meta = TrackMetadata::default();
        meta.ap_filter = Some("short-mp".to_string());
        meta.ap_phase = Some(PhaseTagOverride::Minimum);
        meta.ap_apodizing = Some(true);
        meta.cuesheet_pre_emphasis = true;

        let mut ov = ResolvedUserOverrides::default();
        ov.filter = Some((
            "gauss-long-lp".to_string(),
            UserFilterProvenance::Folder,
            "my-filters.yaml folders:main:Jazz".to_string(),
        ));
        ov.phase = Some((
            PhaseTagOverride::Intermediate,
            UserFilterProvenance::Track,
            "my-filters.yaml phase=ip".to_string(),
        ));

        // Mine mode applies folder override and user phase
        let wt_mine = CachedWholeTrackEvidence::default().with_user_overrides(&ov);
        let fused_mine = fuse_track_evidence(
            44_100,
            16,
            &TrackPeakMetadata::default(),
            &meta,
            None,
            Some(&wt_mine),
        );
        assert_eq!(
            fused_mine.user_filter.as_ref().map(|e| e.value.as_str()),
            Some("gauss-long-lp")
        );
        assert_eq!(fused_mine.user_filter_provenance, Some(UserFilterProvenance::Folder));
        assert_eq!(
            fused_mine.phase_override.as_ref().map(|e| e.value),
            Some(PhaseTagOverride::Intermediate)
        );
        assert!(fused_mine.apodizing.value);
        assert!(fused_mine.deemphasis.value);

        // Suggested mode ignores all user overrides and AP_* tags, keeping CUESHEET facts
        let wt_sug = wt_mine.suggested_only();
        let fused_sug = fuse_track_evidence(
            44_100,
            16,
            &TrackPeakMetadata::default(),
            &meta,
            None,
            Some(&wt_sug),
        );
        assert!(fused_sug.user_filter.is_none());
        assert!(fused_sug.user_filter_provenance.is_none());
        assert!(fused_sug.phase_override.is_none());
        assert!(!fused_sug.apodizing.value);
        assert!(fused_sug.deemphasis.value, "CUESHEET pre-emphasis is a disc header fact");
    }
}
