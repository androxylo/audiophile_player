//! Dynamic per-track upsampler & 8fS noise-shaper picker with CLI override support.
//!
//! Designed around two core architectural rules:
//! 1. **Per-track dynamic switching:** Every track may require a different upsampling
//!    ratio (`8x / 4x / 2x / 1x bypass`), target rate (`352.8 kHz` vs `384 kHz`), and
//!    audiophile filter family depending on its sample rate, bit depth, and mastering flags.
//! 2. **CLI enforcement:** Command-line options (`--filter`, `--shaper`) can enforce a
//!    specific filter or noise shaper across tracks while keeping the strict `4,000,000`-tap
//!    cap invariant intact.

use std::fmt;
use std::str::FromStr;

use ap_core::format::{StreamSpec, TargetDspRate};

use crate::MAX_FILTER_TAPS;

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

/// Curated audiophile PCM filter archetypes (shortlisted in `INV-02`, capped `<= 4M` taps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterArchetype {
    /// No upsampling filter applied (1:1 pass-through).
    Bypass,
    /// Ultra-long Gaussian-windowed apodizing sinc (`sinc-MGa` / PGGB class, <= 4M taps).
    GaussApod4M,
    /// Ultra-steep linear-phase windowed sinc (`sinc-M` / `sinc-L` / Chord WTA class, <= 4M taps).
    SincSteep4M,
    /// Long linear-phase Gaussian polyphase (`poly-sinc-gauss-long` / `xla` class).
    GaussLongLp,
    /// Intermediate-phase HiRes Gaussian (`poly-sinc-gauss-hires-ip` class, minimal pre-ringing).
    GaussHiresIp,
    /// Minimum-phase HiRes Gaussian (`poly-sinc-gauss-hires-mp` class, zero pre-ringing).
    GaussHiresMp,
    /// Sharp extended-passband linear-phase (`poly-sinc-ext3` class).
    ExtSharpLp,
}

impl FilterArchetype {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Bypass => "bypass",
            Self::GaussApod4M => "gauss-apod-4m",
            Self::SincSteep4M => "sinc-steep-4m",
            Self::GaussLongLp => "gauss-long-lp",
            Self::GaussHiresIp => "gauss-hires-ip",
            Self::GaussHiresMp => "gauss-hires-mp",
            Self::ExtSharpLp => "ext-sharp-lp",
        }
    }

    /// Recommended tap budget per archetype (strictly bounded by `max_taps <= 4,000,000`).
    pub fn default_taps(self, ratio: UpsampleRatio, max_taps: usize) -> usize {
        let cap = max_taps.min(MAX_FILTER_TAPS);
        let mult = match ratio {
            UpsampleRatio::Bypass1x => return 0,
            UpsampleRatio::Integer(n) => n as usize,
            UpsampleRatio::Arbitrary => 8,
        };
        let desired = match self {
            Self::Bypass => 0,
            Self::GaussApod4M => 262_144 * mult, // 2.1M @ 8x, 1.05M @ 4x, 524k @ 2x (all <= 4M)
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
            "bypass" => Ok(Self::Bypass),
            "gauss-apod-4m" => Ok(Self::GaussApod4M),
            "sinc-steep-4m" => Ok(Self::SincSteep4M),
            "gauss-long-lp" => Ok(Self::GaussLongLp),
            "gauss-hires-ip" => Ok(Self::GaussHiresIp),
            "gauss-hires-mp" => Ok(Self::GaussHiresMp),
            "ext-sharp-lp" => Ok(Self::ExtSharpLp),
            other => Err(format!(
                "unknown filter '{other}'; expected auto, bypass, gauss-apod-4m, sinc-steep-4m, \
                 gauss-long-lp, gauss-hires-ip, gauss-hires-mp, or ext-sharp-lp"
            )),
        }
    }
}

/// Curated `8fS` (`352.8 / 384 kHz`) noise shapers & dithers (from `INV-02` / `INV-04`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseShaperProfile {
    /// Pass-through (no re-quantization when source is already untouched 32-bit at 8fS).
    Bypass,
    /// 5th-order multibit-optimized (`NS5` class, Signalyst recommendation for 8x multibit DACs).
    Ns5Multibit,
    /// 15th-order smooth linear (`LNS15` class).
    Lns15Smooth,
    /// PGGB-class 8fS noise shaper (low noise floor >= 40 kHz).
    PggbOptimal,
    /// Flat triangular PDF dither (zero ultrasonic noise shaping).
    TpdfDither,
}

impl NoiseShaperProfile {
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::Bypass => "bypass",
            Self::Ns5Multibit => "ns5",
            Self::Lns15Smooth => "lns15",
            Self::PggbOptimal => "pggb-optimal",
            Self::TpdfDither => "tpdf",
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
            "ns5" => Ok(Self::Ns5Multibit),
            "lns15" => Ok(Self::Lns15Smooth),
            "pggb-optimal" => Ok(Self::PggbOptimal),
            "tpdf" => Ok(Self::TpdfDither),
            other => Err(format!(
                "unknown shaper '{other}'; expected auto, bypass, ns5, lns15, pggb-optimal, or tpdf"
            )),
        }
    }
}

/// User/CLI policy configuring [`UpsamplerPicker`].
///
/// Default policy (`filter_override: None`, `shaper_override: None`, `max_taps: 4_000_000`)
/// selects the best filter and shaper dynamically for every track.
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

/// Complete per-track DSP plan produced by [`UpsamplerPicker::select`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedDspPlan {
    pub source_rate_hz: u32,
    pub target_rate: TargetDspRate,
    pub target_rate_hz: u32,
    pub ratio: UpsampleRatio,
    pub filter: FilterArchetype,
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
            "DSP plan: filter={} ({} -> {} Hz, {} taps) + shaper={} [{}]",
            self.filter,
            self.ratio,
            self.target_rate_hz,
            self.tap_budget,
            self.shaper,
            self.reason
        )
    }
}

/// Map any input sample rate to its exact integer-family `8fS` target (`352.8` or `384 kHz`).
pub fn select_target_rate(sample_rate_hz: u32) -> (TargetDspRate, UpsampleRatio) {
    match sample_rate_hz {
        352_800 => (TargetDspRate::Hz352800, UpsampleRatio::Bypass1x),
        384_000 => (TargetDspRate::Hz384000, UpsampleRatio::Bypass1x),
        r if r > 0 && 352_800 % r == 0 => (TargetDspRate::Hz352800, UpsampleRatio::Integer(352_800 / r)),
        r if r > 0 && 384_000 % r == 0 => (TargetDspRate::Hz384000, UpsampleRatio::Integer(384_000 / r)),
        r if r % 44_100 == 0 => (TargetDspRate::Hz352800, UpsampleRatio::Arbitrary),
        _ => (TargetDspRate::Hz384000, UpsampleRatio::Arbitrary),
    }
}

fn target_rate_hz(target: TargetDspRate) -> u32 {
    match target {
        TargetDspRate::Hz352800 => 352_800,
        TargetDspRate::Hz384000 => 384_000,
    }
}

/// Stateful-free per-track upsampler & noise-shaper picker.
///
/// The playback engine invokes [`UpsamplerPicker::select`] fresh on **every single track**
/// so algorithm changes happen seamlessly on the fly between tracks.
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

    /// Select the optimal upsampling filter, tap budget, target 8fS rate, and noise shaper
    /// for `source`, honoring any CLI override enforced in [`UpsamplerPolicy`].
    pub fn select(&self, source: &TrackSourceInfo) -> SelectedDspPlan {
        let (target_rate, raw_ratio) = select_target_rate(source.sample_rate_hz);
        let t_hz = target_rate_hz(target_rate);

        // 1. Choose filter (respecting CLI override and 1x already-8fS bypass invariant).
        let (filter, ratio, filter_overridden, filter_reason) = match self.policy.filter_override {
            Some(FilterArchetype::Bypass) => (
                FilterArchetype::Bypass,
                UpsampleRatio::Bypass1x,
                true,
                "cli --filter bypass".to_string(),
            ),
            Some(forced) => {
                if raw_ratio == UpsampleRatio::Bypass1x {
                    (
                        FilterArchetype::Bypass,
                        UpsampleRatio::Bypass1x,
                        true,
                        format!(
                            "already at {} Hz (1x bypass; cli --filter {} not needed)",
                            source.sample_rate_hz, forced
                        ),
                    )
                } else {
                    (
                        forced,
                        raw_ratio,
                        true,
                        format!("cli --filter {forced}"),
                    )
                }
            }
            None => auto_select_filter(source, raw_ratio),
        };

        // 2. Choose 8fS noise shaper (respecting CLI override).
        let (shaper, shaper_overridden) = match self.policy.shaper_override {
            Some(forced) => (forced, true),
            None => (auto_select_shaper(source, filter, ratio), false),
        };

        let tap_budget = filter.default_taps(ratio, self.policy.max_taps);
        let reason = if shaper_overridden {
            format!("{filter_reason}; cli --shaper {shaper}")
        } else {
            filter_reason
        };

        SelectedDspPlan {
            source_rate_hz: source.sample_rate_hz,
            target_rate,
            target_rate_hz: if filter == FilterArchetype::Bypass {
                source.sample_rate_hz
            } else {
                t_hz
            },
            ratio,
            filter,
            shaper,
            tap_budget,
            filter_overridden,
            shaper_overridden,
            reason,
        }
    }
}

fn auto_select_filter(
    source: &TrackSourceInfo,
    ratio: UpsampleRatio,
) -> (FilterArchetype, UpsampleRatio, bool, String) {
    if ratio == UpsampleRatio::Bypass1x {
        return (
            FilterArchetype::Bypass,
            UpsampleRatio::Bypass1x,
            false,
            format!("auto: native 8fS ({} Hz) pass-through", source.sample_rate_hz),
        );
    }

    if source.apodizing_needed {
        return (
            FilterArchetype::GaussApod4M,
            ratio,
            false,
            "auto: apodizing mastering error detected".to_string(),
        );
    }

    match ratio {
        UpsampleRatio::Integer(r) if r >= 8 => (
            FilterArchetype::GaussApod4M,
            ratio,
            false,
            format!("auto: 1x base rate ({} Hz) -> apodizing 4M-class", source.sample_rate_hz),
        ),
        UpsampleRatio::Integer(4) => (
            FilterArchetype::GaussLongLp,
            ratio,
            false,
            format!("auto: 2x HiRes ({} Hz) -> linear-phase Gaussian", source.sample_rate_hz),
        ),
        UpsampleRatio::Integer(2) => (
            FilterArchetype::GaussHiresIp,
            ratio,
            false,
            format!("auto: 4x HiRes ({} Hz) -> intermediate-phase HiRes Gaussian", source.sample_rate_hz),
        ),
        _ => (
            FilterArchetype::GaussApod4M,
            ratio,
            false,
            format!("auto: non-standard rate ({} Hz) -> apodizing Gaussian", source.sample_rate_hz),
        ),
    }
}

fn auto_select_shaper(
    source: &TrackSourceInfo,
    filter: FilterArchetype,
    ratio: UpsampleRatio,
) -> NoiseShaperProfile {
    if filter == FilterArchetype::Bypass
        && ratio == UpsampleRatio::Bypass1x
        && source.bits_per_sample >= 32
    {
        return NoiseShaperProfile::Bypass;
    }
    // Signalyst's primary recommendation for 8fS (352.8/384 kHz) multibit DACs (Gungnir 2).
    NoiseShaperProfile::Ns5Multibit
}
