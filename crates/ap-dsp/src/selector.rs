//! Dynamic per-track upsampler & 8fS noise-shaper picker with CLI override support.
//!
//! Architectural design:
//! 1. **Per-track dynamic switching:** Every track is evaluated fresh by [`UpsamplerPicker`]
//!    so the engine always expects a potentially different upsampler/ratio per track.
//! 2. **Initial simple test upsampler (`SimpleTest`):** Before deep `INV-03` algorithm
//!    research lands, `SimpleTest` is the sole implemented upsampling filter (and therefore
//!    the active temporary default for non-8fS tracks). Once the curated audiophile filter
//!    families (`GaussApod4M`, `SincSteep4M`, `GaussLongLp`, `GaussHiresIp`, etc.) are
//!    implemented, `SimpleTest` will be retired as default and retained strictly for testing.
//! 3. **Always floating point:** Every upsampler operates strictly on `StereoF64Buffer`
//!    (`f64` -> `f64`) targeting `352.8 kHz` / `384 kHz`.
//! 4. **CLI enforcement:** `--filter`, `--shaper`, and `--max-taps` can override selection.

use std::fmt;
use std::str::FromStr;

use ap_core::format::{StreamSpec, TargetDspRate};

use crate::MAX_FILTER_TAPS;

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
    /// True when pre-analysis detects mastering/ADC pre-ringing errors needing an apodizing filter.
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

/// Curated audiophile PCM filter archetypes (plus `Null` 1:1 `f64` pass-through and initial `SimpleTest`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterArchetype {
    /// Null 1:1 floating-point upsampler (`StereoF64Buffer -> StereoF64Buffer`, zero copy).
    /// Used for native `352.8 / 384 kHz` sources so they still enter the `f64` DSP domain
    /// (REW convolution, headroom, channel delay) without resampling.
    Null,
    /// Initial simple windowed-sinc `f64` polyphase upsampler (temporary sole implemented
    /// upsampler until deep `INV-03` algorithms land; will NOT remain default once curated
    /// filters are implemented).
    SimpleTest,
    /// Future (`INV-03`): ultra-long Gaussian-windowed apodizing sinc (`sinc-MGa` / PGGB class).
    GaussApod4M,
    /// Future (`INV-03`): ultra-steep linear-phase windowed sinc (`sinc-M`/`sinc-L`/WTA class).
    SincSteep4M,
    /// Future (`INV-03`): long linear-phase Gaussian polyphase (`poly-sinc-gauss-long` class).
    GaussLongLp,
    /// Future (`INV-03`): intermediate-phase HiRes Gaussian (`poly-sinc-gauss-hires-ip` class).
    GaussHiresIp,
    /// Future (`INV-03`): minimum-phase HiRes Gaussian (`poly-sinc-gauss-hires-mp` class).
    GaussHiresMp,
    /// Future (`INV-03`): sharp extended-passband linear-phase (`poly-sinc-ext3` class).
    ExtSharpLp,
}

impl FilterArchetype {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::SimpleTest => "simple-test",
            Self::GaussApod4M => "gauss-apod-4m",
            Self::SincSteep4M => "sinc-steep-4m",
            Self::GaussLongLp => "gauss-long-lp",
            Self::GaussHiresIp => "gauss-hires-ip",
            Self::GaussHiresMp => "gauss-hires-mp",
            Self::ExtSharpLp => "ext-sharp-lp",
        }
    }

    /// True if this filter kernel is currently implemented and runnable in `ap-dsp`.
    pub fn is_implemented(self) -> bool {
        matches!(self, Self::Null | Self::SimpleTest)
    }

    /// Recommended tap budget per archetype (strictly bounded by `max_taps <= 4,000,000`).
    pub fn default_taps(self, ratio: UpsampleRatio, max_taps: usize) -> usize {
        let cap = max_taps.min(MAX_FILTER_TAPS);
        let mult = match ratio {
            UpsampleRatio::Bypass1x => return 0,
            UpsampleRatio::Integer(n) => n as usize,
            UpsampleRatio::Arbitrary => 8,
        };
        if self == Self::SimpleTest {
            return simple_test_taps_per_phase(mult, cap)
                .map_or(0, |m| m.saturating_mul(mult));
        }
        let desired = match self {
            Self::Null => 0,
            Self::SimpleTest => unreachable!(),
            Self::GaussApod4M => 262_144 * mult, // 2.1M @ 8x, 1.05M @ 4x, 524k @ 2x
            Self::SincSteep4M => 500_000 * mult, // 4.0M @ 8x, 2.0M @ 4x, 1.0M @ 2x
            Self::GaussLongLp => 32_768 * mult,
            Self::GaussHiresIp | Self::GaussHiresMp => 8_192 * mult,
            Self::ExtSharpLp => 65_536 * mult,
        };
        desired.min(cap)
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
            "gauss-apod-4m" => Ok(Self::GaussApod4M),
            "sinc-steep-4m" => Ok(Self::SincSteep4M),
            "gauss-long-lp" => Ok(Self::GaussLongLp),
            "gauss-hires-ip" => Ok(Self::GaussHiresIp),
            "gauss-hires-mp" => Ok(Self::GaussHiresMp),
            "ext-sharp-lp" => Ok(Self::ExtSharpLp),
            other => Err(format!(
                "unknown filter '{other}'; expected auto, null, simple-test, gauss-apod-4m, \
                 sinc-steep-4m, gauss-long-lp, gauss-hires-ip, gauss-hires-mp, or ext-sharp-lp"
            )),
        }
    }
}

/// Curated `8fS` (`352.8 / 384 kHz`) noise shapers & dithers (from `INV-02` / `INV-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseShaperProfile {
    /// Pass-through (no re-quantization when source is already untouched 32-bit at 8fS).
    Bypass,
    /// Flat triangular PDF dither (initial implemented quantizer; zero ultrasonic noise shaping).
    TpdfDither,
    /// Future (`INV-04`): 5th-order multibit-optimized (`NS5` class).
    Ns5Multibit,
    /// Future (`INV-04`): 15th-order smooth linear (`LNS15` class).
    Lns15Smooth,
    /// Future (`INV-04`): PGGB-class 8fS noise shaper (low noise floor >= 40 kHz).
    PggbOptimal,
}

impl NoiseShaperProfile {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Bypass => "bypass",
            Self::TpdfDither => "tpdf",
            Self::Ns5Multibit => "ns5",
            Self::Lns15Smooth => "lns15",
            Self::PggbOptimal => "pggb-optimal",
        }
    }

    /// True if this quantizer / noise-shaper profile is currently implemented in `ap-dsp`.
    pub fn is_implemented(self) -> bool {
        matches!(self, Self::Bypass | Self::TpdfDither)
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
            "ns5" => Ok(Self::Ns5Multibit),
            "lns15" => Ok(Self::Lns15Smooth),
            "pggb-optimal" => Ok(Self::PggbOptimal),
            other => Err(format!(
                "unknown shaper '{other}'; expected auto, bypass, tpdf, ns5, lns15, or pggb-optimal"
            )),
        }
    }
}

/// User/CLI policy configuring [`UpsamplerPicker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsamplerPolicy {
    pub filter_override: Option<FilterArchetype>,
    pub shaper_override: Option<NoiseShaperProfile>,
    pub max_taps: usize,
}

impl Default for UpsamplerPolicy {
    fn default() -> Self {
        Self {
            filter_override: None,
            shaper_override: None,
            max_taps: MAX_FILTER_TAPS,
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
    /// Intended long-term curated filter archetype for this source class once `INV-03` lands.
    pub target_audiophile_filter: FilterArchetype,
    pub shaper: NoiseShaperProfile,
    pub tap_budget: usize,
    pub filter_overridden: bool,
    pub shaper_overridden: bool,
    pub reason: String,
}

impl SelectedDspPlan {
    /// Concise one-line summary suitable for stdout and the HTTP bottom action log.
    pub fn summary(&self) -> String {
        format!(
            "DSP plan: filter={} ({} -> {} Hz f64, {} taps) + shaper={} [{}]",
            self.filter,
            self.ratio,
            self.target_rate_hz,
            self.tap_budget,
            self.shaper,
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

/// Per-track upsampler & noise-shaper picker.
///
/// Always called fresh on every track so the engine naturally expects the upsampler,
/// ratio, and target rate to vary from track to track.
#[derive(Debug, Clone)]
pub struct UpsamplerPicker {
    policy: UpsamplerPolicy,
}

impl UpsamplerPicker {
    pub fn new(policy: UpsamplerPolicy) -> Result<Self, String> {
        policy.validate()?;
        Ok(Self { policy })
    }

    pub fn policy(&self) -> UpsamplerPolicy {
        self.policy
    }

    /// Select the per-track `f64` upsampling plan, honoring CLI overrides.
    pub fn select(&self, source: &TrackSourceInfo) -> SelectedDspPlan {
        let (target_rate, raw_ratio) = select_target_rate(source.sample_rate_hz);
        let t_hz = target_rate_hz(target_rate);
        let target_audiophile = planned_audiophile_filter(source, raw_ratio);

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
            None => auto_select_active_filter(source, raw_ratio, target_audiophile),
        };

        let (shaper, shaper_overridden) = match self.policy.shaper_override {
            Some(forced) => (forced, true),
            None => (auto_select_shaper(source, filter, ratio), false),
        };

        let tap_budget = filter.default_taps(ratio, self.policy.max_taps);
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

        let resolved_target_hz = if filter == FilterArchetype::Null {
            source.sample_rate_hz
        } else {
            t_hz
        };

        SelectedDspPlan {
            source_rate_hz: source.sample_rate_hz,
            target_rate: target_rate_from_hz(resolved_target_hz),
            target_rate_hz: resolved_target_hz,
            ratio,
            filter,
            target_audiophile_filter: target_audiophile,
            shaper,
            tap_budget,
            filter_overridden,
            shaper_overridden,
            reason,
        }
    }
}

/// Curated long-term audiophile filter mapping (active once `INV-03` filters are implemented).
fn planned_audiophile_filter(source: &TrackSourceInfo, ratio: UpsampleRatio) -> FilterArchetype {
    if ratio == UpsampleRatio::Bypass1x {
        return FilterArchetype::Null;
    }
    if source.apodizing_needed {
        return FilterArchetype::GaussApod4M;
    }
    match ratio {
        UpsampleRatio::Integer(r) if r >= 8 => FilterArchetype::GaussApod4M,
        UpsampleRatio::Integer(4) => FilterArchetype::GaussLongLp,
        UpsampleRatio::Integer(2) => FilterArchetype::GaussHiresIp,
        _ => FilterArchetype::GaussApod4M,
    }
}

/// Select the currently implemented active filter: uses `target_audiophile` once implemented,
/// otherwise cleanly selects our initial bootstrap `SimpleTest` kernel for non-8fS tracks.
fn auto_select_active_filter(
    source: &TrackSourceInfo,
    ratio: UpsampleRatio,
    target_audiophile: FilterArchetype,
) -> (FilterArchetype, UpsampleRatio, bool, String) {
    if ratio == UpsampleRatio::Bypass1x {
        return (
            FilterArchetype::Null,
            UpsampleRatio::Bypass1x,
            false,
            format!(
                "auto: native 8fS ({} Hz) -> null upsampler (f64 domain)",
                source.sample_rate_hz
            ),
        );
    }

    if target_audiophile.is_implemented() {
        return (
            target_audiophile,
            ratio,
            false,
            format!("auto: {} Hz -> {target_audiophile}", source.sample_rate_hz),
        );
    }

    // Only initial `SimpleTest` upsampler is implemented prior to deep INV-03 filter research.
    (
        FilterArchetype::SimpleTest,
        ratio,
        false,
        format!(
            "auto: initial simple-test ({} Hz -> {} f64; future default: {target_audiophile})",
            source.sample_rate_hz, ratio
        ),
    )
}

fn auto_select_shaper(
    source: &TrackSourceInfo,
    filter: FilterArchetype,
    ratio: UpsampleRatio,
) -> NoiseShaperProfile {
    if filter == FilterArchetype::Null
        && ratio == UpsampleRatio::Bypass1x
        && source.bits_per_sample >= 32
    {
        return NoiseShaperProfile::Bypass;
    }
    // Initial implemented quantizer (TPDF dither); upgraded to NS5/LNS15/PGGB after INV-04.
    NoiseShaperProfile::TpdfDither
}
