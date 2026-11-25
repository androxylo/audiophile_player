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

use crate::MAX_FILTER_TAPS;
use crate::upsampler::default_gaussian_num_taps;

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
    /// (used by future smart selectors in Phase 5+; ignored by [`DefaultFilterSelector`]).
    pub apodizing_needed: bool,
    /// True for live streaming sources (Milestone 2 Tidal); false for local FLAC files.
    pub is_streaming_source: bool,
}

impl TrackSourceInfo {
    pub fn from_stream_spec(spec: StreamSpec) -> Self {
        Self {
            sample_rate_hz: spec.sample_rate_hz,
            bits_per_sample: spec.bits_per_sample,
            channels: spec.channels,
            apodizing_needed: false,
            is_streaming_source: false,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterIdentity {
    pub archetype: FilterArchetype,
    pub source_rate_hz: u32,
    pub target_rate_hz: u32,
    pub ratio: usize,
    pub num_taps: usize,
    pub taps_per_phase: usize,
    /// IEEE 754 `f64::to_bits()` of filter cutoff `fc` in Hz (`0` for non-parametric/bypass).
    pub fc_hz_bits: u64,
    /// IEEE 754 `f64::to_bits()` of Gaussian `sigma_f` in Hz (`0` for non-Gaussian).
    pub sigma_f_hz_bits: u64,
    /// Optional content-derived effective Nyquist (reserved for Phase 6 Detector H).
    pub effective_nyquist_hz: Option<u32>,
}

/// Curated audiophile PCM filter archetypes (`UPSAMPLING_RESEARCH.md` §1 & §7), plus `Null`
/// (native 8fS 1:1 `f64` pass-through) and test-only `SimpleTest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        matches!(
            self,
            Self::Null | Self::SimpleTest | Self::GaussLongLp | Self::GaussHiresLp
        )
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
            Self::GaussLongLp | Self::GaussHiresLp => {
                // Review #5 Finding #18: report the true shape-derived N rather than silently
                // clamping to `cap` so budget mismatches are explicit and honest.
                default_gaussian_num_taps(self, source_rate_hz, target_rate_hz).unwrap_or(0)
            }
            // Future Phase 2/3 researched constant-time lengths (`UPSAMPLING_RESEARCH.md` §4.4, §7).
            Self::GaussApod4M => (65_536_usize.saturating_mul(mult)).min(cap),
            Self::SincSteep4M => (131_070_usize.saturating_mul(mult)).min(cap),
            Self::ExtSharpLp => (65_536_usize.saturating_mul(mult)).min(cap),
            Self::MinphaseLong | Self::IntermediateLong => {
                let ref_arch = if source_rate_hz <= 48_000 {
                    Self::GaussLongLp
                } else {
                    Self::GaussHiresLp
                };
                default_gaussian_num_taps(ref_arch, source_rate_hz, target_rate_hz)
                    .map_or((8_192_usize.saturating_mul(mult)).min(cap), |n| n.min(cap))
            }
            Self::ShortLp | Self::ShortMp => (256_usize.saturating_mul(mult)).min(cap),
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
                ) && default_gaussian_num_taps(selected_auto, source.sample_rate_hz, t_hz)
                    .is_some_and(|n| n > self.policy.max_taps);
                if needs_fallback {
                    let req_n =
                        default_gaussian_num_taps(selected_auto, source.sample_rate_hz, t_hz)
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
