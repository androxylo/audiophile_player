//! Dynamic per-track upsampler & 8fS noise-shaper picker with pluggable [`FilterSelector`].
//!
//! Architectural design:
//! 1. **Per-track dynamic switching:** Every track is evaluated fresh by [`UpsamplerPicker`]
//!    using its pluggable [`FilterSelector`] so the engine always expects a potentially
//!    different upsampler, ratio, or filter identity per track.
//! 2. **Phase 1 dummy selector ([`DefaultFilterSelector`]):** Always selects the researched
//!    default per-rate filter (`gauss-long-lp` for 1x `<= 48 kHz`, `gauss-hires-lp` for hi-res
//!    `88.2–192 kHz`, `null` for native `8fS` `352.8 / 384 kHz`), while honoring `--filter` CLI
//!    overrides. A future Phase 5 `SmartFilterSelector` plugs into the same [`FilterSelector`]
//!    trait without changing pipeline callers.
//! 3. **Always floating point:** Every upsampler operates strictly on `StereoF64Buffer`
//!    (`f64` -> `f64`) targeting `352.8 kHz` / `384 kHz`.
//! 4. **Legacy `SimpleTest`:** Retired as default and retained strictly for testing
//!    (`--filter simple-test`).
//! 5. **CLI enforcement:** `--filter`, `--shaper`, and `--max-taps` can override selection.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use ap_core::config::{DEFAULT_DAC_BITS, is_valid_dac_bits};
use ap_core::format::{StreamSpec, TargetDspRate};
use ap_core::metadata::{GenreClass, PhaseTagOverride};
use ap_core::track_db::PostChainKey;

use crate::MAX_FILTER_TAPS;
use crate::fusion::{EvidenceSource, FusedTrackEvidence};
use crate::upsampler::planned_taps_for_archetype;

/// Hard look-ahead ceiling in seconds for Ultimate Mode (Mandates M1–M4; non-configurable).
pub const ULTIMATE_LOOKAHEAD_SECONDS: u32 = 30;

/// Minimum sane source sample rate accepted by the DSP pipeline (protects against corrupt
/// ultra-low-rate headers causing extreme upsampling ratios and OOM allocations).
pub const MIN_SUPPORTED_SOURCE_RATE_HZ: u32 = 8_000;

/// Maximum supported source sample rate (player never downsamples; DAC ceiling is `384 kHz`).
pub const MAX_SUPPORTED_SOURCE_RATE_HZ: u32 = 384_000;

/// Hard ceiling on integer upsampling ratios (`8 kHz -> 384 kHz` is `48x <= 64`).
pub const MAX_UPSAMPLE_RATIO: u32 = 64;

/// Default taps per polyphase branch for `SimpleTest` (Kaiser-windowed sinc, flat through 20 kHz
/// at `44.1 -> 352.8 kHz` with `> 110 dB` stopband attenuation).
pub const SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE: usize = 256;

/// Minimum allowed even taps per polyphase branch for `SimpleTest`.
pub const SIMPLE_TEST_MIN_TAPS_PER_PHASE: usize = 8;

/// Recommended quality floor (taps per polyphase branch) for `SimpleTest` below which
/// stopband attenuation and transition sharpness visibly degrade.
pub const SIMPLE_TEST_QUALITY_FLOOR_TAPS_PER_PHASE: usize = 64;

/// Error prefix used when a track's sample rate cannot be processed by the active DSP pipeline
/// (allows the playback engine to skip unsupported tracks cleanly in Random/Sequential modes).
pub const UNSUPPORTED_SAMPLE_RATE_PREFIX: &str = "unsupported source sample rate:";

/// Derive the exact even taps-per-phase count used by [`FilterArchetype::SimpleTest`] for a given
/// integer upsampling `ratio` and `max_taps` ceiling so `SelectedDspPlan::tap_budget` and the
/// constructed polyphase kernel always agree and never silently exceed `max_taps`.
///
/// Returns `None` when `max_taps < SIMPLE_TEST_MIN_TAPS_PER_PHASE * ratio`.
pub fn simple_test_taps_per_phase(ratio: usize, max_taps: usize) -> Option<usize> {
    let cap = max_taps.min(MAX_FILTER_TAPS);
    let max_for_ratio = cap / ratio.max(1);
    let even = max_for_ratio & !1;
    if even < SIMPLE_TEST_MIN_TAPS_PER_PHASE {
        None
    } else {
        Some(SIMPLE_TEST_DEFAULT_TAPS_PER_PHASE.min(even))
    }
}

/// Per-track source attributes inspected by [`UpsamplerPicker`] before each track plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSourceInfo {
    pub sample_rate_hz: u32,
    pub bits_per_sample: u16,
    pub channels: u16,
    /// True when pre-analysis detects mastering/ADC pre-ringing errors needing an apodizing filter
    /// (used by smart/ultimate selectors; ignored by [`DefaultFilterSelector`]).
    pub apodizing_needed: bool,
    /// True for live streaming sources (UPnP / HTTP / Tidal); false for local FLAC files.
    pub is_streaming_source: bool,
    /// Optional content-derived effective Nyquist in Hz (Detector H fake hi-res, e.g. `22_050` Hz).
    pub effective_nyquist_hz: Option<u32>,
    /// True when Detector H detects 4fS ultrasonic ADC/DSD noise above 40 kHz (`OQ-37`).
    pub hf_noise_filter: bool,
    /// True when CD-DA 50/15 µs de-emphasis is active from CUESHEET or `AP_DEEMPHASIS` (Rule P).
    pub deemphasis: bool,
}

impl TrackSourceInfo {
    pub fn from_stream_spec(spec: StreamSpec) -> Self {
        Self {
            sample_rate_hz: spec.sample_rate_hz,
            bits_per_sample: spec.bits_per_sample,
            channels: spec.channels,
            apodizing_needed: false,
            is_streaming_source: false,
            effective_nyquist_hz: None,
            hf_noise_filter: false,
            deemphasis: false,
        }
    }
}

/// Exact integer upsampling ratio to reach `352.8 kHz` or `384 kHz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsampleRatio {
    /// Input is already at `352,800 Hz` or `384,000 Hz` (e.g. pre-upsampled PGGB FLAC).
    Bypass1x,
    /// Exact integer upsampling multiplier (e.g. `2`, `4`, `8`, `12`, `16`).
    Integer(u32),
    /// Non-integer rate fallback (rare non-standard source rates).
    Arbitrary,
}

impl fmt::Display for UpsampleRatio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bypass1x => write!(f, "1x (bypass)"),
            Self::Integer(n) => write!(f, "{n}x"),
            Self::Arbitrary => write!(f, "arbitrary"),
        }
    }
}

/// Phase response class of an upsampling filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPhase {
    Linear,
    Minimum,
    Intermediate,
}

impl fmt::Display for FilterPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Linear => f.write_str("linear"),
            Self::Minimum => f.write_str("minimum"),
            Self::Intermediate => f.write_str("intermediate"),
        }
    }
}

/// Design and diagnostic metadata exposed by an active upsampling filter for logging, UI, and
/// DR-1 compliance verification (`UPSAMPLING_RESEARCH.md` §4.5, §6, §7, §9.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterDesignMetrics {
    pub phase: FilterPhase,
    /// Exact prototype filter length `N` (odd for linear-phase integer group delay).
    pub num_taps: usize,
    /// Taps per polyphase branch.
    pub taps_per_phase: usize,
    /// Total prototype duration `(N - 1) / fs_out` in seconds.
    pub length_seconds: f64,
    /// Group delay at DC in seconds (`length_seconds / 2.0` for linear phase).
    pub group_delay_seconds: f64,
    /// Passband `-0.01 dB` edge in Hz (`fc - 3.05 * sigma_f` for Gaussian).
    pub passband_edge_001db_hz: f64,
    /// Exact response magnitude at `20 kHz` in dB (`None` when `source_rate_hz <= 40_000`).
    pub level_at_20khz_db: Option<f64>,
    /// Stopband edge frequency in Hz (where response reaches `stopband_attenuation_db`).
    pub stopband_edge_hz: f64,
    /// Target stopband attenuation in dB (negative, e.g. `-300.0`).
    pub stopband_attenuation_db: f64,
    /// Worst-case per-phase L1 norm `max_p sum_k |bank[p][k]|` (diagnostic only per HEADROOM_DESIGN.md).
    pub l1_norm: f64,
    /// Worst-case per-phase L1 norm in dB (`20 * log10(l1_norm)`).
    pub l1_norm_db: f64,
}

impl FilterDesignMetrics {
    /// Concise diagnostic string suitable for stdout and the HTTP bottom action log.
    pub fn summary(&self) -> String {
        let lvl_20k = match self.level_at_20khz_db {
            Some(db) => format!("{db:+.4}dB@20k"),
            None => "n/a@20k".to_string(),
        };
        format!(
            "phase={} N={} ({}t/ph, {:.2}ms, gd={:.2}ms) -0.01dB={:.0}Hz {} stop={:.0}Hz({:.0}dB) L1={:.3}({:+.2}dB)",
            self.phase,
            self.num_taps,
            self.taps_per_phase,
            self.length_seconds * 1_000.0,
            self.group_delay_seconds * 1_000.0,
            self.passband_edge_001db_hz,
            lvl_20k,
            self.stopband_edge_hz,
            self.stopband_attenuation_db,
            self.l1_norm,
            self.l1_norm_db,
        )
    }
}

/// Unique mathematical identity of an active upsampler kernel (`FILTER_SELECTION_RESEARCH.md` §4).
///
/// Checked inside `Upsampler::set_incoming_carryover` so two different filters (or the same
/// archetype with different parameters/effective Nyquist) NEVER splice their FIR histories even
/// when their tap lengths happen to match — forcing a clean `ConvolverOnlyHandoff` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FilterIdentity {
    pub archetype: FilterArchetype,
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    pub num_taps: usize,
    pub taps_per_phase: usize,
    /// IEEE 754 `f64::to_bits()` of filter cutoff `fc` in Hz (`0` for non-parametric/bypass).
    pub fc_hz_bits: u64,
    /// IEEE 754 `f64::to_bits()` of Gaussian `sigma_f` in Hz (`0` for non-Gaussian) or Kaiser beta.
    pub sigma_f_hz_bits: u64,
    /// Optional content-derived effective Nyquist (Detector H / `OQ-37` HF noise filter stop edge).
    pub effective_nyquist_hz: Option<u32>,
}

/// Curated audiophile PCM filter archetypes (`UPSAMPLING_RESEARCH.md` §1 & §7), plus `Null`
/// (native 8fS 1:1 `f64` pass-through) and test-only `SimpleTest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterArchetype {
    /// Null 1:1 floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`, zero copy).
    /// Used for native `352.8 / 384 kHz` sources so they enter the `f64` DSP domain without resampling.
    Null,
    /// Legacy Kaiser-windowed sinc `f64` polyphase upsampler retained strictly for testing
    /// (`--filter simple-test`).
    SimpleTest,
    /// #1 (DEFAULT 1x): Long linear-phase Gaussian windowed-sinc (`poly-sinc-gauss-long` class).
    /// Flat `-0.01 dB` to `20 kHz` (scaled with `fs_in`), `-300 dB` at `fs_in / 2`.
    GaussLongLp,
    /// #2 (DEFAULT hi-res): Linear-phase hi-res Gaussian (`poly-sinc-gauss-hires-lp` class).
    /// Flat `-0.01 dB` to `0.42 * fs_in`, `-300 dB` at `fs_in / 2`.
    GaussHiresLp,
    /// #3 (Phase 2): Extra-long strong-apodizing Gaussian (`poly-sinc-gauss-xla` / `sinc-MGa` class).
    GaussApod4M,
    /// #4 (Phase 2): Ultra-steep constant-time Kaiser sinc (`sinc-L` class, `131,070 * ratio`).
    SincSteep4M,
    /// #5 (Phase 2): Sharp extended-passband linear-phase (`ext3` / `sinc-Mx` class, `65,536 * ratio`).
    ExtSharpLp,
    /// #6 (Phase 3): Minimum-phase Gaussian (`minphase-long`; CLI alias `gauss-hires-mp`).
    MinphaseLong,
    /// #7 (Phase 3): Intermediate-phase Gaussian (`intermediate-long`; CLI alias `gauss-hires-ip`).
    IntermediateLong,
    /// #8a (Phase 2): Short linear-phase Gaussian (`short-lp`, flat to 19 kHz DR-1 floor).
    ShortLp,
    /// #8b (Phase 3): Short minimum-phase Gaussian (`short-mp`, flat to 19 kHz DR-1 floor).
    ShortMp,
}

impl FilterArchetype {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::SimpleTest => "simple-test",
            Self::GaussLongLp => "gauss-long-lp",
            Self::GaussHiresLp => "gauss-hires-lp",
            Self::GaussApod4M => "gauss-apod-4m",
            Self::SincSteep4M => "sinc-steep-4m",
            Self::ExtSharpLp => "ext-sharp-lp",
            Self::MinphaseLong => "minphase-long",
            Self::IntermediateLong => "intermediate-long",
            Self::ShortLp => "short-lp",
            Self::ShortMp => "short-mp",
        }
    }

    /// True if this filter kernel is currently implemented and runnable in `ap-dsp`.
    pub fn is_implemented(self) -> bool {
        true
    }

    /// Shape-derived (for Gaussian archetypes) or researched constant-time tap count, bounded by
    /// `max_taps <= MAX_FILTER_TAPS`.
    pub fn planned_taps(
        self,
        source_rate_hz: u32,
        target_rate_hz: u32,
        ratio: UpsampleRatio,
        max_taps: usize,
    ) -> usize {
        let cap = max_taps.min(MAX_FILTER_TAPS);
        let mult = match ratio {
            UpsampleRatio::Bypass1x => return 0,
            UpsampleRatio::Integer(n) => n as usize,
            UpsampleRatio::Arbitrary => 8,
        };
        match self {
            Self::Null => 0,
            Self::SimpleTest => simple_test_taps_per_phase(mult, cap)
                .map_or(0, |m| m.saturating_mul(mult)),
            _ => planned_taps_for_archetype(self, source_rate_hz, target_rate_hz, cap).unwrap_or(0),
        }
    }

    /// Backwards-compatible ratio-only tap budget helper (uses canonical 44.1/96/192k reference rates).
    pub fn default_taps(self, ratio: UpsampleRatio, max_taps: usize) -> usize {
        let (src, dst) = match ratio {
            UpsampleRatio::Bypass1x => (384_000, 384_000),
            UpsampleRatio::Integer(2) => (192_000, 384_000),
            UpsampleRatio::Integer(4) => (96_000, 384_000),
            UpsampleRatio::Integer(r) => (352_800 / r.max(1), 352_800),
            UpsampleRatio::Arbitrary => (44_100, 352_800),
        };
        self.planned_taps(src, dst, ratio, max_taps)
    }
}

impl fmt::Display for FilterArchetype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.cli_name())
    }
}

impl FromStr for FilterArchetype {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "null" | "bypass" => Ok(Self::Null),
            "simple-test" => Ok(Self::SimpleTest),
            "gauss-long-lp" => Ok(Self::GaussLongLp),
            "gauss-hires-lp" => Ok(Self::GaussHiresLp),
            "gauss-apod-4m" => Ok(Self::GaussApod4M),
            "sinc-steep-4m" => Ok(Self::SincSteep4M),
            "ext-sharp-lp" => Ok(Self::ExtSharpLp),
            "minphase-long" | "gauss-hires-mp" => Ok(Self::MinphaseLong),
            "intermediate-long" | "gauss-hires-ip" => Ok(Self::IntermediateLong),
            "short-lp" => Ok(Self::ShortLp),
            "short-mp" => Ok(Self::ShortMp),
            other => Err(format!(
                "unknown filter '{other}'; expected auto, null, simple-test, gauss-long-lp, \
                 gauss-hires-lp, gauss-apod-4m, sinc-steep-4m, ext-sharp-lp, minphase-long \
                 (alias gauss-hires-mp), intermediate-long (alias gauss-hires-ip), short-lp, \
                 or short-mp"
            )),
        }
    }
}

/// Pluggable per-track filter selector (`FILTER_SELECTION_RESEARCH.md`).
///
/// Phase 1 ships [`DefaultFilterSelector`], which always picks the researched default per-rate
/// Gaussian filter (`gauss-long-lp` for 1x, `gauss-hires-lp` for hi-res, `null` for native 8fS).
/// Phase 5 plugs `SmartFilterSelector` into this same trait without changing pipeline callers.
pub trait FilterSelector: Send + Sync {
    /// Short identifier for logs and diagnostics.
    fn name(&self) -> &'static str;

    /// Select the per-track [`FilterArchetype`] and an explainable reason string.
    fn select_filter(
        &self,
        source: &TrackSourceInfo,
        ratio: UpsampleRatio,
    ) -> (FilterArchetype, String);
}

/// Dummy initial filter selector (Phase 1): always picks the researched default per-rate filter:
/// - Native `8fS` (`352.8 / 384 kHz`, `Bypass1x`) -> [`FilterArchetype::Null`]
/// - 1x rates (`<= 48 kHz`, e.g. `44.1 / 48 kHz` and sub-44.1k integer divisors) -> [`FilterArchetype::GaussLongLp`]
/// - Hi-res rates (`88.2 / 96 / 176.4 / 192 kHz`) -> [`FilterArchetype::GaussHiresLp`]
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultFilterSelector;

impl FilterSelector for DefaultFilterSelector {
    fn name(&self) -> &'static str {
        "default"
    }

    fn select_filter(
        &self,
        source: &TrackSourceInfo,
        ratio: UpsampleRatio,
    ) -> (FilterArchetype, String) {
        if ratio == UpsampleRatio::Bypass1x {
            return (
                FilterArchetype::Null,
                format!(
                    "auto (default selector): native 8fS ({} Hz) -> null (1:1 f64 pass-through)",
                    source.sample_rate_hz
                ),
            );
        }
        if source.sample_rate_hz <= 48_000 {
            (
                FilterArchetype::GaussLongLp,
                format!(
                    "auto (default selector): {} Hz (1x) -> gauss-long-lp",
                    source.sample_rate_hz
                ),
            )
        } else {
            (
                FilterArchetype::GaussHiresLp,
                format!(
                    "auto (default selector): {} Hz (hi-res) -> gauss-hires-lp",
                    source.sample_rate_hz
                ),
            )
        }
    }
}

/// Curated `8fS` (`352.8 / 384 kHz`) noise shapers & dithers (`docs/research/04-noise-shapers.md`).
///
/// Note on naming (§3): `Ns5Multibit` (`ns5` / `ns5-class`), `Lns15Smooth` (`lns15` / `lns15-class`),
/// and `PggbOptimal` (`pggb-optimal` / `wide40-9`) are independent clean-room Levinson–Durbin
/// minimum-phase designs matching those order/passband/envelope classes — never reverse-engineered
/// or claimed bit-identical to proprietary shapers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseShaperProfile {
    /// Pass-through (no re-quantization when source is already untouched 32-bit at 8fS).
    Bypass,
    /// Flat triangular PDF dither (zero ultrasonic noise shaping; default at 24-bit).
    TpdfDither,
    /// Clean-room 2nd-order gentle shaper (`ns-gentle-2`, 20 kHz, `+6 dB` ceiling; auto default at `<= 20` bits).
    NsGentle2,
    /// Clean-room 5th-order multibit-class shaper (`ns5` / `ns5-class`, 20 kHz, `+20 dB` ceiling).
    Ns5Multibit,
    /// Clean-room 15th-order tapered-ultrasonic shaper (`lns15` / `lns15-class`, 20->60 kHz, `+15 dB` ceiling).
    Lns15Smooth,
    /// Clean-room 9th-order 40 kHz-wideband shaper (`pggb-optimal` / `wide40-9`, 40 kHz, `+18 dB` ceiling).
    PggbOptimal,
}

impl NoiseShaperProfile {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Bypass => "bypass",
            Self::TpdfDither => "tpdf",
            Self::NsGentle2 => "ns-gentle-2",
            Self::Ns5Multibit => "ns5",
            Self::Lns15Smooth => "lns15",
            Self::PggbOptimal => "pggb-optimal",
        }
    }

    /// True if this quantizer / noise-shaper profile is currently implemented in `ap-dsp`.
    pub fn is_implemented(self) -> bool {
        match self {
            Self::Bypass
            | Self::TpdfDither
            | Self::NsGentle2
            | Self::Ns5Multibit
            | Self::Lns15Smooth
            | Self::PggbOptimal => true,
        }
    }
}

impl fmt::Display for NoiseShaperProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.cli_name())
    }
}

impl FromStr for NoiseShaperProfile {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bypass" => Ok(Self::Bypass),
            "tpdf" => Ok(Self::TpdfDither),
            "ns-gentle-2" => Ok(Self::NsGentle2),
            "ns5" | "ns5-class" => Ok(Self::Ns5Multibit),
            "lns15" | "lns15-class" => Ok(Self::Lns15Smooth),
            "pggb-optimal" | "wide40-9" => Ok(Self::PggbOptimal),
            other => Err(format!(
                "unknown shaper '{other}'; expected auto, bypass, tpdf, ns-gentle-2, ns5 \
                 (alias ns5-class), lns15 (alias lns15-class), or pggb-optimal (alias wide40-9)"
            )),
        }
    }
}

/// User/CLI/config policy configuring [`UpsamplerPicker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsamplerPolicy {
    pub filter_override: Option<FilterArchetype>,
    pub shaper_override: Option<NoiseShaperProfile>,
    pub max_taps: usize,
    /// Target DAC quantization bit depth (`16`, `18`, `20`, or `24`; default `24`).
    pub dac_bits: u8,
}

impl Default for UpsamplerPolicy {
    fn default() -> Self {
        Self {
            filter_override: None,
            shaper_override: None,
            max_taps: MAX_FILTER_TAPS,
            dac_bits: DEFAULT_DAC_BITS,
        }
    }
}

impl UpsamplerPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_taps > MAX_FILTER_TAPS {
            return Err(format!(
                "max_taps ({}) exceeds hard 4,000,000-tap cap ({MAX_FILTER_TAPS})",
                self.max_taps
            ));
        }
        if !is_valid_dac_bits(self.dac_bits) {
            return Err(format!(
                "dac_bits ({}) is invalid; expected one of 16, 18, 20, or 24",
                self.dac_bits
            ));
        }
        Ok(())
    }
}

/// Complete per-track floating-point DSP plan produced by [`UpsamplerPicker::select`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedDspPlan {
    pub source_rate_hz: u32,
    /// `Some(TargetDspRate)` when `target_rate_hz` is an `8fS` rate (`352,800` or `384,000 Hz`),
    /// or `None` when `--filter null` passes through a non-`8fS` native source rate.
    pub target_rate: Option<TargetDspRate>,
    pub target_rate_hz: u32,
    pub ratio: UpsampleRatio,
    /// Active filter selected to execute right now.
    pub filter: FilterArchetype,
    /// Filter archetype chosen by the active [`FilterSelector`] before any fallback/override.
    pub target_audiophile_filter: FilterArchetype,
    pub shaper: NoiseShaperProfile,
    /// Target DAC quantization bit depth (`16`, `18`, `20`, or `24`).
    pub dac_bits: u8,
    /// Planned tap count (`<= max_taps <= 4,000,000`).
    pub tap_budget: usize,
    /// User/policy `--max-taps` ceiling (preserved so `build_upsampler` enforces the hard cap).
    pub max_taps: usize,
    /// Optional content-derived effective Nyquist in Hz (Detector H fake hi-res).
    pub effective_nyquist_hz: Option<u32>,
    /// True when 4fS ultrasonic HF noise filtering above 40 kHz is requested (`OQ-37`).
    pub hf_noise_filter: bool,
    /// True when 50/15 µs de-emphasis runs post-upsampler / pre-REW (`U6`, Rule P).
    pub deemphasis: bool,
    pub filter_overridden: bool,
    pub shaper_overridden: bool,
    pub reason: String,
}

impl SelectedDspPlan {
    /// Concise one-line summary suitable for stdout and the HTTP bottom action log.
    pub fn summary(&self) -> String {
        format!(
            "DSP plan: filter={} ({} -> {} Hz f64, {} taps) + shaper={} ({}-bit) [{}]",
            self.filter,
            self.ratio,
            self.target_rate_hz,
            self.tap_budget,
            self.shaper,
            self.dac_bits,
            self.reason
        )
    }
}

/// Map a sane input sample rate (`8 kHz..=384 kHz`) to its exact integer-family `8fS` target
/// (`352.8` or `384 kHz`), enforcing `ratio <= MAX_UPSAMPLE_RATIO`.
///
/// Out-of-range rates (`< 8 kHz` or `> 384 kHz`) and non-integer divisors map to
/// [`UpsampleRatio::Arbitrary`] so crafted low-rate headers cannot trigger extreme OOM ratios.
pub fn select_target_rate(sample_rate_hz: u32) -> (TargetDspRate, UpsampleRatio) {
    match sample_rate_hz {
        352_800 => (TargetDspRate::Hz352800, UpsampleRatio::Bypass1x),
        384_000 => (TargetDspRate::Hz384000, UpsampleRatio::Bypass1x),
        r if (MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ).contains(&r)
            && 352_800 % r == 0
            && (352_800 / r) <= MAX_UPSAMPLE_RATIO =>
        {
            (TargetDspRate::Hz352800, UpsampleRatio::Integer(352_800 / r))
        }
        r if (MIN_SUPPORTED_SOURCE_RATE_HZ..=MAX_SUPPORTED_SOURCE_RATE_HZ).contains(&r)
            && 384_000 % r == 0
            && (384_000 / r) <= MAX_UPSAMPLE_RATIO =>
        {
            (TargetDspRate::Hz384000, UpsampleRatio::Integer(384_000 / r))
        }
        r if r > 0 && r % 44_100 == 0 => (TargetDspRate::Hz352800, UpsampleRatio::Arbitrary),
        _ => (TargetDspRate::Hz384000, UpsampleRatio::Arbitrary),
    }
}

fn target_rate_hz(target: TargetDspRate) -> u32 {
    match target {
        TargetDspRate::Hz352800 => 352_800,
        TargetDspRate::Hz384000 => 384_000,
    }
}

fn target_rate_from_hz(hz: u32) -> Option<TargetDspRate> {
    match hz {
        352_800 => Some(TargetDspRate::Hz352800),
        384_000 => Some(TargetDspRate::Hz384000),
        _ => None,
    }
}

/// Per-track upsampler & noise-shaper picker backed by a pluggable [`FilterSelector`].
///
/// Always called fresh on every track so the engine naturally expects the upsampler,
/// ratio, and target rate to vary from track to track.
#[derive(Clone)]
pub struct UpsamplerPicker {
    policy: UpsamplerPolicy,
    selector: Arc<dyn FilterSelector>,
}

impl fmt::Debug for UpsamplerPicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpsamplerPicker")
            .field("policy", &self.policy)
            .field("selector", &self.selector.name())
            .finish()
    }
}

impl UpsamplerPicker {
    /// Construct a picker backed by [`DefaultFilterSelector`] (Phase 1 dummy selector).
    pub fn new(policy: UpsamplerPolicy) -> Result<Self, String> {
        Self::with_selector(policy, Arc::new(DefaultFilterSelector))
    }

    /// Construct a picker backed by a custom [`FilterSelector`] (e.g. Phase 5 `SmartFilterSelector`).
    pub fn with_selector(
        policy: UpsamplerPolicy,
        selector: Arc<dyn FilterSelector>,
    ) -> Result<Self, String> {
        policy.validate()?;
        Ok(Self { policy, selector })
    }

    pub fn policy(&self) -> UpsamplerPolicy {
        self.policy
    }

    pub fn selector_name(&self) -> &'static str {
        self.selector.name()
    }

    /// Select the per-track `f64` upsampling plan, honoring CLI overrides.
    pub fn select(&self, source: &TrackSourceInfo) -> SelectedDspPlan {
        let (target_rate, raw_ratio) = select_target_rate(source.sample_rate_hz);
        let t_hz = target_rate_hz(target_rate);
        let (selected_auto, selected_reason) = self.selector.select_filter(source, raw_ratio);

        let (filter, ratio, filter_overridden, filter_reason) = match self.policy.filter_override {
            Some(FilterArchetype::Null) => (
                FilterArchetype::Null,
                UpsampleRatio::Bypass1x,
                true,
                "cli --filter null (1:1 f64 pass-through)".to_string(),
            ),
            Some(forced) => {
                if raw_ratio == UpsampleRatio::Bypass1x {
                    (
                        FilterArchetype::Null,
                        UpsampleRatio::Bypass1x,
                        true,
                        format!(
                            "already at {} Hz (null 1:1 f64 upsampler; cli --filter {} not needed)",
                            source.sample_rate_hz, forced
                        ),
                    )
                } else {
                    (forced, raw_ratio, true, format!("cli --filter {forced}"))
                }
            }
            None => {
                // Review #5 Finding #18: when auto-mode selects a Gaussian filter whose shape-
                // derived tap count exceeds `--max-taps`, degrade cleanly to `SimpleTest` sized
                // to the user's cap instead of failing every Redbook track.
                let needs_fallback = matches!(
                    selected_auto,
                    FilterArchetype::GaussLongLp | FilterArchetype::GaussHiresLp
                ) && planned_taps_for_archetype(
                    selected_auto,
                    source.sample_rate_hz,
                    t_hz,
                    MAX_FILTER_TAPS,
                )
                .is_some_and(|n| n > self.policy.max_taps);
                if needs_fallback {
                    let req_n = planned_taps_for_archetype(
                        selected_auto,
                        source.sample_rate_hz,
                        t_hz,
                        MAX_FILTER_TAPS,
                    )
                    .unwrap_or(0);
                    (
                        FilterArchetype::SimpleTest,
                        raw_ratio,
                        false,
                        format!(
                            "auto fallback to simple-test ({selected_auto} requires {req_n} taps > --max-taps {})",
                            self.policy.max_taps
                        ),
                    )
                } else {
                    (selected_auto, raw_ratio, false, selected_reason)
                }
            }
        };

        let resolved_target_hz = if filter == FilterArchetype::Null {
            source.sample_rate_hz
        } else {
            t_hz
        };

        let (shaper, shaper_overridden) = match self.policy.shaper_override {
            Some(forced) => (forced, true),
            None => (
                auto_select_shaper(source, filter, ratio, resolved_target_hz, self.policy.dac_bits),
                false,
            ),
        };

        let tap_budget = filter.planned_taps(
            source.sample_rate_hz,
            resolved_target_hz,
            ratio,
            self.policy.max_taps,
        );

        let mut reason = if shaper_overridden {
            format!("{filter_reason}; cli --shaper {shaper}")
        } else {
            filter_reason
        };

        if filter == FilterArchetype::SimpleTest {
            if let UpsampleRatio::Integer(r) = ratio {
                let mult = r as usize;
                match simple_test_taps_per_phase(mult, self.policy.max_taps) {
                    None => {
                        reason.push_str(&format!(
                            " [error: --max-taps {} below {}x minimum {} taps]",
                            self.policy.max_taps,
                            mult,
                            mult * SIMPLE_TEST_MIN_TAPS_PER_PHASE
                        ));
                    }
                    Some(tpp) if tpp < SIMPLE_TEST_QUALITY_FLOOR_TAPS_PER_PHASE => {
                        reason.push_str(&format!(
                            " [warn: {tpp} taps/phase below {SIMPLE_TEST_QUALITY_FLOOR_TAPS_PER_PHASE}-tap quality floor]"
                        ));
                    }
                    Some(_) => {}
                }
            }
        }

        SelectedDspPlan {
            source_rate_hz: source.sample_rate_hz,
            target_rate: target_rate_from_hz(resolved_target_hz),
            target_rate_hz: resolved_target_hz,
            ratio,
            filter,
            target_audiophile_filter: selected_auto,
            shaper,
            dac_bits: self.policy.dac_bits,
            tap_budget,
            max_taps: self.policy.max_taps,
            effective_nyquist_hz: source.effective_nyquist_hz,
            hf_noise_filter: source.hf_noise_filter,
            deemphasis: source.deemphasis,
            filter_overridden,
            shaper_overridden,
            reason,
        }
    }
}

/// Automatic `8fS` shaper selection (`docs/research/04-noise-shapers.md`):
/// - Untouched native `>= 32-bit` `8fS` at `dac_bits >= 24` -> `Bypass` (bit-perfect pass-through).
/// - All `8fS` (`352.8 / 384 kHz`) DSP output -> `NsGentle2` (best all-around shaper across all
///   genres and bit depths: `-14.6 dB` in-band @ 384k, `-6.5 dB` 20–40k, `+6 dB` ultrasonic ceiling,
///   minimal `1.21 LSB` R-2R code activity, safe for NOS multibit DACs and wideband amps).
/// - Non-`8fS` rates (e.g. `--filter null` at 44.1–192 kHz) -> `TpdfDither`.
/// Higher-order shapers (`ns5`, `lns15`, `pggb-optimal`) and flat `tpdf` remain explicit overrides.
fn auto_select_shaper(
    source: &TrackSourceInfo,
    filter: FilterArchetype,
    ratio: UpsampleRatio,
    resolved_target_hz: u32,
    dac_bits: u8,
) -> NoiseShaperProfile {
    if filter == FilterArchetype::Null
        && ratio == UpsampleRatio::Bypass1x
        && source.bits_per_sample >= 32
        && dac_bits >= 24
    {
        return NoiseShaperProfile::Bypass;
    }
    if matches!(resolved_target_hz, 352_800 | 384_000) {
        NoiseShaperProfile::NsGentle2
    } else {
        NoiseShaperProfile::TpdfDither
    }
}

impl FilterArchetype {
    /// Returns `true` when this filter archetype is permitted on live/HTTP/Tidal streaming sources.
    ///
    /// Mandate M4: mega-tap `#3 gauss-apod-4m` (and 4M-class long-FFT archetypes) are **local files
    /// only** because streaming sources cannot buffer beyond 30 s and must start with low latency.
    pub const fn is_streaming_safe(self) -> bool {
        !matches!(
            self,
            Self::GaussApod4M | Self::SincSteep4M | Self::ExtSharpLp
        )
    }
}

/// Structured audit trail of how [`select_ultimate_plan`] resolved a single track (`U13`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UltimateSelection {
    pub plan: SelectedDspPlan,
    /// Display mode label: `"ultimate: cached"`, `"ultimate: live"`, or `"normal (<30 s)"`.
    pub mode_label: String,
    /// Winning rule identifier (e.g. `"rule_A_apodizing"`, `"rule_G_classical"`, `"short_track_bypass"`).
    pub winning_rule: String,
    /// Ordered list of all rules/signals that fired for this track.
    pub fired_rules: Vec<String>,
    /// True when M3 bypassed ultimate analysis because the track is `< 30 s`.
    pub short_track_bypass: bool,
    /// Optional non-fatal warnings (e.g. streaming guard overriding `gauss-apod-4m`, MQA, stale tags).
    pub warnings: Vec<String>,
}

/// Pluggable [`FilterSelector`] implementing the Ultimate Mode §3 decision table
/// (`docs/research/07-ultimate-mode.md` §3 + resolved `OQ-34`, `OQ-35`, `OQ-37`, `OQ-40`).
///
/// Strictly per-track: depends solely on this track's own [`TrackSourceInfo`] and fused evidence.
#[derive(Debug, Clone)]
pub struct UltimateSelector {
    pub fused: FusedTrackEvidence,
    pub genre_rules: bool,
}

impl FilterSelector for UltimateSelector {
    fn name(&self) -> &'static str {
        "ultimate-selector"
    }

    fn select_filter(
        &self,
        source: &TrackSourceInfo,
        ratio: UpsampleRatio,
    ) -> (FilterArchetype, String) {
        let (filter, _rule, detail, _warnings) = resolve_ultimate_filter_choice(
            source.sample_rate_hz,
            source.is_streaming_source,
            ratio,
            &self.fused,
            self.genre_rules,
        );
        (filter, detail)
    }
}

fn streaming_safe_fallback(is_effective_1x: bool) -> FilterArchetype {
    if is_effective_1x {
        FilterArchetype::GaussLongLp
    } else {
        FilterArchetype::GaussHiresLp
    }
}

fn resolve_ultimate_filter_choice(
    sample_rate_hz: u32,
    is_streaming: bool,
    ratio: UpsampleRatio,
    fused: &FusedTrackEvidence,
    genre_rules: bool,
) -> (FilterArchetype, String, String, Vec<String>) {
    let mut warnings = Vec::new();

    if ratio == UpsampleRatio::Bypass1x || sample_rate_hz >= 352_800 {
        return (
            FilterArchetype::Null,
            "native_8fs".to_string(),
            format!("ultimate: native 8fS ({sample_rate_hz} Hz) -> null"),
            warnings,
        );
    }

    let is_fake_hires = fused.effective_nyquist_hz.value.is_some();
    let is_effective_1x = sample_rate_hz <= 48_000 || is_fake_hires;

    // Priority 1: Explicit user override (YAML `user.filter` > Vorbis `AP_FILTER`).
    if let Some(ref uf) = fused.user_filter {
        match FilterArchetype::from_str(&uf.value) {
            Ok(arch) => {
                if is_streaming && !arch.is_streaming_safe() {
                    let fb = streaming_safe_fallback(is_effective_1x);
                    warnings.push(format!(
                        "M4 streaming guard: user override '{}' is local-only; using '{}'",
                        arch, fb
                    ));
                    return (
                        fb,
                        "streaming_guard_user".to_string(),
                        format!(
                            "ultimate: streaming guard substituted {fb} for local-only {arch} ({})",
                            uf.source.as_str()
                        ),
                        warnings,
                    );
                }
                return (
                    arch,
                    format!("user_filter({})", uf.source.as_str()),
                    format!("ultimate: user override {} ({})", arch, uf.detail),
                    warnings,
                );
            }
            Err(err) => {
                warnings.push(format!(
                    "ignoring unrecognized user filter override '{}': {err}",
                    uf.value
                ));
            }
        }
    }

    // Priority 2: Explicit user `AP_PHASE` Vorbis tag (`min` -> #6, `ip` -> #7).
    if let Some(ref phase_ev) = fused.phase_override {
        match phase_ev.value {
            PhaseTagOverride::Minimum => {
                return (
                    FilterArchetype::MinphaseLong,
                    "user_phase_min".to_string(),
                    format!("ultimate: {} -> minphase-long", phase_ev.detail),
                    warnings,
                );
            }
            PhaseTagOverride::Intermediate => {
                return (
                    FilterArchetype::IntermediateLong,
                    "user_phase_ip".to_string(),
                    format!("ultimate: {} -> intermediate-long", phase_ev.detail),
                    warnings,
                );
            }
            PhaseTagOverride::Linear => {}
        }
    }

    // Priority 3: Rule A — Apodizing needed (highest automatic priority: A > L > G > R).
    if fused.apodizing.value {
        if is_streaming {
            let fb = streaming_safe_fallback(is_effective_1x);
            return (
                fb,
                "rule_A_streaming".to_string(),
                format!(
                    "ultimate: Rule A ({}) on stream (M4 local-only gauss-apod-4m) -> {fb}",
                    fused.apodizing.detail
                ),
                warnings,
            );
        }
        return (
            FilterArchetype::GaussApod4M,
            "rule_A_apodizing".to_string(),
            format!(
                "ultimate: Rule A ({}, {}) -> gauss-apod-4m",
                fused.apodizing.source.as_str(),
                fused.apodizing.detail
            ),
            warnings,
        );
    }

    // Priority 4: Rule L — Loud & clipped (`OQ-34`: local -> #3 gauss-apod-4m, stream -> #1/#2).
    if fused.loud_and_clipped.value {
        if is_streaming {
            let fb = streaming_safe_fallback(is_effective_1x);
            return (
                fb,
                "rule_L_streaming".to_string(),
                format!(
                    "ultimate: Rule L ({}) on stream -> {fb}",
                    fused.loud_and_clipped.detail
                ),
                warnings,
            );
        }
        return (
            FilterArchetype::GaussApod4M,
            "rule_L_loud_clipped".to_string(),
            format!(
                "ultimate: Rule L ({}, {}) -> gauss-apod-4m",
                fused.loud_and_clipped.source.as_str(),
                fused.loud_and_clipped.detail
            ),
            warnings,
        );
    }

    // Priority 5: Rule G — Genre-aware defaults (`OQ-35`, enabled by default, lowest priority A > L > G > R).
    if genre_rules && is_effective_1x {
        match fused.genre_class.value {
            GenreClass::Classical => {
                let arch = if is_streaming {
                    FilterArchetype::GaussLongLp
                } else {
                    FilterArchetype::SincSteep4M
                };
                return (
                    arch,
                    "rule_G_classical".to_string(),
                    format!("ultimate: Rule G (Classical) -> {arch}"),
                    warnings,
                );
            }
            GenreClass::JazzAcoustic => {
                return (
                    FilterArchetype::GaussLongLp,
                    "rule_G_jazz_acoustic".to_string(),
                    "ultimate: Rule G (Jazz/Acoustic) -> gauss-long-lp".to_string(),
                    warnings,
                );
            }
            GenreClass::RockPopElectronic => {
                let arch = if is_streaming {
                    FilterArchetype::GaussLongLp
                } else {
                    FilterArchetype::ExtSharpLp
                };
                return (
                    arch,
                    "rule_G_rock_pop_electronic".to_string(),
                    format!("ultimate: Rule G (Rock/Pop/Electronic) -> {arch}"),
                    warnings,
                );
            }
            GenreClass::AmbientSoundtrack => {
                return (
                    FilterArchetype::GaussLongLp,
                    "rule_G_ambient_soundtrack".to_string(),
                    "ultimate: Rule G (Ambient/Soundtrack) -> gauss-long-lp".to_string(),
                    warnings,
                );
            }
            GenreClass::Ambiguous | GenreClass::Unknown => {}
        }
    }

    // Priority 6: Rule R — Rate defaults (with Rule H fake hi-res treated as 1x, OQ-37 HF noise).
    if is_effective_1x {
        let arch = if is_streaming {
            FilterArchetype::GaussLongLp
        } else {
            FilterArchetype::SincSteep4M
        };
        let rule = if is_fake_hires {
            "rule_H_fake_hires"
        } else if is_streaming {
            "rule_R_1x_stream"
        } else {
            "rule_R_1x_clean"
        };
        let detail = if let Some(feff) = fused.effective_nyquist_hz.value {
            format!("ultimate: Rule H (fake hi-res f_eff={feff} Hz) -> {arch}")
        } else if is_streaming {
            format!("ultimate: 1x stream ({sample_rate_hz} Hz) -> {arch}")
        } else {
            format!("ultimate: clean 1x ({sample_rate_hz} Hz) -> {arch}")
        };
        return (arch, rule.to_string(), detail, warnings);
    }

    // Clean hi-res (`88.2..=192 kHz`): `#2 gauss-hires-lp` (+ OQ-37 HF noise filter when flagged).
    let (rule, detail) = if fused.hf_noise_filter.value {
        (
            "rule_R_hires_hf_noise".to_string(),
            format!(
                "ultimate: hi-res ({sample_rate_hz} Hz) + OQ-37 HF ultrasonic noise filter -> gauss-hires-lp"
            ),
        )
    } else {
        (
            "rule_R_hires_clean".to_string(),
            format!("ultimate: clean hi-res ({sample_rate_hz} Hz) -> gauss-hires-lp"),
        )
    };
    (FilterArchetype::GaussHiresLp, rule, detail, warnings)
}

/// Select the complete Ultimate Mode [`SelectedDspPlan`] and structured [`UltimateSelection`]
/// audit trail for one track.
///
/// Honors:
/// - **M3 (`< 30 s` bypass):** short tracks return normal-mode defaults with `"normal (<30 s)"`.
/// - **Explicit CLI overrides:** `--filter` / `--shaper` / `--dac-bits` always win (with M4
///   streaming guard protecting streams from `gauss-apod-4m`).
/// - **Strict per-track independence (`OQ-40`):** zero cross-track state.
#[allow(clippy::too_many_arguments)]
pub fn select_ultimate_plan(
    spec: StreamSpec,
    is_streaming_source: bool,
    duration_s_hint: Option<f64>,
    fused: &FusedTrackEvidence,
    policy: &UpsamplerPolicy,
    genre_rules: bool,
    used_cached_wt: bool,
) -> Result<UltimateSelection, String> {
    let mut warnings = fused.notices.clone();

    // Mandate M3: tracks shorter than 30 s take the normal-mode path cleanly.
    if duration_s_hint.is_some_and(|d| d.is_finite() && d < f64::from(ULTIMATE_LOOKAHEAD_SECONDS)) {
        let mut src = TrackSourceInfo::from_stream_spec(spec);
        src.is_streaming_source = is_streaming_source;
        src.deemphasis = fused.deemphasis.value;
        let picker = UpsamplerPicker::new(*policy)?;
        let plan = picker.select(&src);
        return Ok(UltimateSelection {
            plan,
            mode_label: "normal (<30 s)".to_string(),
            winning_rule: "short_track_bypass".to_string(),
            fired_rules: vec!["M3(<30 s)".to_string()],
            short_track_bypass: true,
            warnings,
        });
    }

    let mut fired_rules = Vec::new();
    if fused.deemphasis.value {
        fired_rules.push(format!("P(deemphasis:{})", fused.deemphasis.source.as_str()));
    }
    if fused.apodizing.value {
        fired_rules.push(format!("A(apod:{})", fused.apodizing.source.as_str()));
    }
    if let Some(feff) = fused.effective_nyquist_hz.value {
        fired_rules.push(format!(
            "H(fake_hires:f_eff={}Hz:{})",
            feff,
            fused.effective_nyquist_hz.source.as_str()
        ));
    }
    if fused.hf_noise_filter.value {
        fired_rules.push(format!(
            "H(hf_noise>40kHz:{})",
            fused.hf_noise_filter.source.as_str()
        ));
    }
    if fused.loud_and_clipped.value {
        fired_rules.push(format!(
            "L(loud_clipped:{})",
            fused.loud_and_clipped.source.as_str()
        ));
    }
    if genre_rules
        && !matches!(
            fused.genre_class.value,
            GenreClass::Unknown | GenreClass::Ambiguous
        )
    {
        fired_rules.push(format!("G({:?})", fused.genre_class.value));
    }

    let (_, raw_ratio) = select_target_rate(spec.sample_rate_hz);
    let (_, mut winning_rule, _, mut choice_warnings) = resolve_ultimate_filter_choice(
        spec.sample_rate_hz,
        is_streaming_source,
        raw_ratio,
        fused,
        genre_rules,
    );
    warnings.append(&mut choice_warnings);

    // Protect streaming sources even if CLI explicitly passed `--filter gauss-apod-4m` (Mandate M4).
    let mut effective_policy = *policy;
    if let Some(cli_f) = policy.filter_override {
        if is_streaming_source && !cli_f.is_streaming_safe() {
            let is_1x = spec.sample_rate_hz <= 48_000 || fused.effective_nyquist_hz.value.is_some();
            let fb = streaming_safe_fallback(is_1x);
            warnings.push(format!(
                "M4 streaming guard: CLI --filter {cli_f} is local-only; substituting {fb} on stream"
            ));
            effective_policy.filter_override = Some(fb);
            winning_rule = "cli_streaming_guard".to_string();
        } else {
            winning_rule = "cli_override".to_string();
        }
    }

    let mut src = TrackSourceInfo::from_stream_spec(spec);
    src.is_streaming_source = is_streaming_source;
    src.apodizing_needed = fused.apodizing.value;
    src.effective_nyquist_hz = fused.effective_nyquist_hz.value;
    src.hf_noise_filter = fused.hf_noise_filter.value && fused.effective_nyquist_hz.value.is_none();
    src.deemphasis = fused.deemphasis.value;

    let selector = Arc::new(UltimateSelector {
        fused: fused.clone(),
        genre_rules,
    });
    let picker = UpsamplerPicker::with_selector(effective_policy, selector)?;
    let plan = picker.select(&src);

    let mode_label = if used_cached_wt
        || fused.apodizing.source == EvidenceSource::Cache
        || fused.effective_nyquist_hz.source == EvidenceSource::Cache
    {
        "ultimate: cached".to_string()
    } else {
        "ultimate: live".to_string()
    };

    Ok(UltimateSelection {
        plan,
        mode_label,
        winning_rule,
        fired_rules,
        short_track_bypass: false,
        warnings,
    })
}

/// Build the canonical [`PostChainKey`] identifying an exact linear DSP chain configuration.
///
/// Shared by both `audiophile-player scan --post-chain` and Ultimate Mode playback (`Review #7 F6`)
/// so scanned whole-track `M` values always match playback lookups identically.
pub fn build_post_chain_key(
    plan: &SelectedDspPlan,
    source_rate_hz: u32,
    output_rate_hz: u32,
    rew_filter_hash: Option<&str>,
) -> PostChainKey {
    PostChainKey {
        upsampler: plan.filter.to_string(),
        f_eff_hz: plan.effective_nyquist_hz.unwrap_or(source_rate_hz / 2),
        output_rate_hz,
        deemphasis: plan.deemphasis,
        hf_noise_filter: plan.hf_noise_filter,
        rew_filter_hash: rew_filter_hash.unwrap_or("none").to_string(),
    }
}

/// Outcome of the Ultimate Mode §4.3 throughput self-check.
#[derive(Debug, Clone, PartialEq)]
pub struct ThroughputCheckResult {
    /// Measured processing speed in multiples of real time (`rendered_audio_s / wall_s`).
    pub measured_x_rt: f64,
    /// Minimum required speed (`>= 1.05x RT` safety floor, or `T / (T + 30)` with headroom).
    pub required_x_rt: f64,
    /// True when the active lossless configuration comfortably outruns real-time playback.
    pub passed: bool,
    /// Loud diagnostic message explaining pass margin or exact lossless fallback when refused.
    pub detail: String,
    /// Recommended lossless fallback filter when `passed == false` (`#1 gauss-long-lp` or `#2 gauss-hires-lp`).
    pub fallback_filter: Option<FilterArchetype>,
}

/// Evaluate measured first-window processing throughput against Ultimate Mode §4.3's real-time
/// requirement. Never silently truncates tails or drops to `f32`: if too slow, returns
/// `passed = false` with a loud diagnostic and clean lossless fallback (`#1` / `#2`).
pub fn check_ultimate_throughput(
    active_filter: FilterArchetype,
    source_rate_hz: u32,
    rendered_audio_s: f64,
    wall_seconds: f64,
    track_duration_s: Option<f64>,
) -> ThroughputCheckResult {
    check_ultimate_throughput_with_buffer(
        active_filter,
        source_rate_hz,
        rendered_audio_s,
        wall_seconds,
        track_duration_s,
        rendered_audio_s,
        None,
        0.0,
    )
}

/// Extended throughput check that also enforces the W2 **startup burst** budget against the
/// currently buffered playback duration (`buffered_s`) ahead of the DAC (Review #7 F9).
///
/// When `buffered_s` is `Some(b)` with `b > 0.0` (gapless transition while Track A is playing),
/// the projected wall-clock time to finish the `w0_total_audio_s` W2 render plus `decode_wall_s`
/// MUST fit strictly inside `(b - safety_s)` so Track A's queue cannot underrun.
#[allow(clippy::too_many_arguments)]
pub fn check_ultimate_throughput_with_buffer(
    active_filter: FilterArchetype,
    source_rate_hz: u32,
    rendered_audio_s: f64,
    wall_seconds: f64,
    track_duration_s: Option<f64>,
    w0_total_audio_s: f64,
    buffered_s: Option<f64>,
    decode_wall_s: f64,
) -> ThroughputCheckResult {
    let wall = wall_seconds.max(1e-9);
    let measured_x_rt = if rendered_audio_s.is_finite() && rendered_audio_s > 0.0 {
        rendered_audio_s / wall
    } else {
        0.0
    };

    // Sustained requirement: >= 1.15x RT on unknown/long streams, scaling gently on finite tracks
    // while never dropping below 1.0x RT.
    let base_req = match track_duration_s {
        Some(t) if t.is_finite() && t > 0.0 => t / (t + f64::from(ULTIMATE_LOOKAHEAD_SECONDS)),
        _ => 1.0,
    };
    let sustained_req = (base_req * 1.15).max(1.0);

    // Startup burst constraint (Review #7 F9): when Track A still has `avail_s` buffered ahead of
    // the DAC, finishing Track B's W2 render (`w0_total_audio_s / x_rt + decode_wall_s`) must
    // complete with >= 2 s safety cushion (or 25% of `avail_s` for shorter cushions).
    let burst_req = match buffered_s {
        Some(avail_s) if avail_s.is_finite() && avail_s > 0.0 && w0_total_audio_s > 0.0 => {
            let safety_s = 2.0_f64.min(avail_s * 0.25);
            let budget_s = (avail_s - decode_wall_s.max(0.0) - safety_s).max(0.5);
            w0_total_audio_s / budget_s
        }
        _ => 0.0,
    };
    let required_x_rt = sustained_req.max(burst_req);
    let passed = measured_x_rt >= required_x_rt;

    if passed {
        ThroughputCheckResult {
            measured_x_rt,
            required_x_rt,
            passed: true,
            detail: format!(
                "{active_filter} throughput {measured_x_rt:.2}x RT >= {required_x_rt:.2}x RT required (OK)"
            ),
            fallback_filter: None,
        }
    } else {
        let fallback = if source_rate_hz <= 48_000 {
            FilterArchetype::GaussLongLp
        } else {
            FilterArchetype::GaussHiresLp
        };
        let reason = if burst_req > sustained_req {
            format!(
                "W2 burst exceeds {:.1}s buffered cushion (need >= {required_x_rt:.2}x RT)",
                buffered_s.unwrap_or(0.0)
            )
        } else {
            format!("sustained speed < {required_x_rt:.2}x RT required")
        };
        ThroughputCheckResult {
            measured_x_rt,
            required_x_rt,
            passed: false,
            detail: format!(
                "REFUSED {active_filter}: measured {measured_x_rt:.2}x RT ({reason}); falling back to lossless {fallback}"
            ),
            fallback_filter: Some(fallback),
        }
    }
}

#[cfg(test)]
mod throughput_tests {
    use super::*;

    #[test]
    fn throughput_check_fails_when_w2_burst_exceeds_buffered_duration() {
        // Buffered 10 s, rendered 30 s in 20 s wall (1.5x RT) -> burst takes 20 s > 8 s budget!
        let res = check_ultimate_throughput_with_buffer(
            FilterArchetype::GaussApod4M,
            44_100,
            30.0,
            20.0,
            Some(240.0),
            30.0,
            Some(10.0),
            0.0,
        );
        assert!(!res.passed, "1.5x RT must fail W2 burst against 10 s buffer");
        assert!(res.required_x_rt > 3.5);
        assert_eq!(res.fallback_filter, Some(FilterArchetype::GaussLongLp));
    }

    #[test]
    fn throughput_check_passes_with_ample_buffer() {
        // Buffered 28 s, 30 s rendered in 1.5 s wall (20x RT) -> comfortably passes!
        let res = check_ultimate_throughput_with_buffer(
            FilterArchetype::GaussApod4M,
            44_100,
            30.0,
            1.5,
            Some(240.0),
            30.0,
            Some(28.0),
            0.2,
        );
        assert!(res.passed);
        assert!(res.fallback_filter.is_none());
    }
}
