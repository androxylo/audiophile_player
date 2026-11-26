// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! PCM sample rates, locked `352.8 / 384 kHz` DSP families, last-stage DAC output capabilities,
//! and integer DAC wire formats.

/// Locked `8fS` floating-point DSP target rates (integer multiples of 44.1 / 48 kHz families).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDspRate {
    /// `352,800 Hz` — exact 8x / 4x / 2x integer target for 44.1 / 88.2 / 176.4 kHz sources.
    Hz352800,
    /// `384,000 Hz` — exact 8x / 4x / 2x integer target for 48 / 96 / 192 kHz sources.
    Hz384000,
}

/// Strictly integer PCM wire formats accepted by `usbaudio2.sys` / WDM-KS WaveRT DACs.
/// Floating-point wire output is intentionally unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DacWireFormat {
    Int32,
    Int24In32,
    Int24Packed,
    Int16,
}

/// Descending wire-format preference order when probing a WDM-KS DAC.
pub const WIRE_FORMAT_PREFERENCE: [DacWireFormat; 4] = [
    DacWireFormat::Int32,
    DacWireFormat::Int24In32,
    DacWireFormat::Int24Packed,
    DacWireFormat::Int16,
];

/// Standard descending output rates for the 44.1 kHz clock family (`8fS -> 4fS -> 2fS -> 1fS`).
pub const STANDARD_DAC_RATES_44K: [u32; 4] = [352_800, 176_400, 88_200, 44_100];

/// Standard descending output rates for the 48 kHz clock family (`8fS -> 4fS -> 2fS -> 1fS`).
pub const STANDARD_DAC_RATES_48K: [u32; 4] = [384_000, 192_000, 96_000, 48_000];

/// All valid explicit `--max-rate` / `[player] max_rate` ceiling values in Hz.
pub const ALL_SUPPORTED_MAX_RATES: [u32; 8] = [
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];

/// Return `true` iff `rate_hz` is a supported DAC output ceiling (`44.1..=384 kHz` standard rates).
pub const fn is_valid_max_rate(rate_hz: u32) -> bool {
    matches!(
        rate_hz,
        44_100 | 48_000 | 88_200 | 96_000 | 176_400 | 192_000 | 352_800 | 384_000
    )
}

impl DacWireFormat {
    /// Bytes per individual channel sample on the wire.
    #[inline]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Int32 | Self::Int24In32 => 4,
            Self::Int24Packed => 3,
            Self::Int16 => 2,
        }
    }

    /// Bytes per stereo (2-channel) PCM frame on the wire.
    #[inline]
    pub const fn bytes_per_frame(self) -> usize {
        self.bytes_per_sample() * 2
    }

    /// Maximum significant PCM bit depth representable cleanly on the wire.
    #[inline]
    pub const fn bit_depth(self) -> u8 {
        match self {
            Self::Int32 => 32,
            Self::Int24In32 | Self::Int24Packed => 24,
            Self::Int16 => 16,
        }
    }

    /// Human-readable wire format label for logs, web UI, and JSON diagnostics.
    #[inline]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Int32 => "Int32",
            Self::Int24In32 => "Int24In32",
            Self::Int24Packed => "Int24Packed",
            Self::Int16 => "Int16",
        }
    }
}

/// Resolved wire sample rate and integer PCM container for one DSP clock family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DacRateTarget {
    pub sample_rate_hz: u32,
    pub wire_format: DacWireFormat,
}

/// Probed (or configured) DAC output rate & wire-format ceiling for both clock families.
///
/// Hard architectural invariant: the main DSP pipeline ALWAYS operates at `8fS` (`352,800 Hz` or
/// `384,000 Hz`). When a DAC supports `8fS` (default `native_8fs()`), zero downsampling occurs.
/// When a DAC only accepts lower rates (e.g. `192/96/48 kHz`), a pure-`f64` anti-aliasing
/// downsampler runs strictly at the very last stage right before the single quantizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DacOutputCapabilities {
    /// Best supported `(rate, wire_format)` for the `352,800 Hz` (44.1 kHz) internal DSP family.
    pub target_for_352800: Option<DacRateTarget>,
    /// Best supported `(rate, wire_format)` for the `384,000 Hz` (48 kHz) internal DSP family.
    pub target_for_384000: Option<DacRateTarget>,
    /// Optional explicit user `--max-rate` ceiling in Hz (`None` = auto).
    pub max_rate_hz: Option<u32>,
}

impl Default for DacOutputCapabilities {
    fn default() -> Self {
        Self::native_8fs()
    }
}

impl DacOutputCapabilities {
    /// Native `8fS` (`352.8 / 384 kHz`, `Int32`) capabilities — zero downsampling.
    pub const fn native_8fs() -> Self {
        Self {
            target_for_352800: Some(DacRateTarget {
                sample_rate_hz: 352_800,
                wire_format: DacWireFormat::Int32,
            }),
            target_for_384000: Some(DacRateTarget {
                sample_rate_hz: 384_000,
                wire_format: DacWireFormat::Int32,
            }),
            max_rate_hz: None,
        }
    }

    /// Fallback capabilities when the DAC is temporarily busy at startup: assumes standard `Int32`
    /// rates up to `max_rate_hz` (preserving `--max-rate` / `[player] max_rate` until the live
    /// hardware probe succeeds).
    pub fn native_8fs_capped(max_rate_hz: Option<u32>) -> Self {
        match max_rate_hz {
            None => Self::native_8fs(),
            Some(cap) => {
                let mut all = Vec::with_capacity(
                    STANDARD_DAC_RATES_44K.len() + STANDARD_DAC_RATES_48K.len(),
                );
                for &r in STANDARD_DAC_RATES_44K.iter().chain(STANDARD_DAC_RATES_48K.iter()) {
                    all.push((r, DacWireFormat::Int32));
                }
                Self::from_supported_rates(&all, Some(cap))
            }
        }
    }

    /// Resolve the best per-family targets from a list of supported `(sample_rate_hz, wire_format)`
    /// pairs and an optional `max_rate_hz` ceiling.
    ///
    /// Policy:
    /// 1. Always prefer the highest supported rate within the **same clock family** (`÷1`, `÷2`,
    ///    `÷4`, `÷8`), never crossing clock families when any same-family rate works.
    /// 2. ONLY if a DAC rejects ALL rates in one family (e.g. 48/96/192-only HDMI/SPDIF), fall
    ///    back to the highest supported rate in the other family (exact `f64` rational polyphase
    ///    decimation).
    pub fn from_supported_rates(
        supported: &[(u32, DacWireFormat)],
        max_rate_hz: Option<u32>,
    ) -> Self {
        let ceiling = max_rate_hz.unwrap_or(u32::MAX);
        let pick_best = |candidates: &[u32]| -> Option<DacRateTarget> {
            for &rate in candidates {
                if rate > ceiling {
                    continue;
                }
                for &fmt in &WIRE_FORMAT_PREFERENCE {
                    if supported.iter().any(|&(r, f)| r == rate && f == fmt) {
                        return Some(DacRateTarget {
                            sample_rate_hz: rate,
                            wire_format: fmt,
                        });
                    }
                }
            }
            None
        };

        let same_44 = pick_best(&STANDARD_DAC_RATES_44K);
        let same_48 = pick_best(&STANDARD_DAC_RATES_48K);

        let target_for_352800 = same_44.or(same_48);
        let target_for_384000 = same_48.or(same_44);

        Self {
            target_for_352800,
            target_for_384000,
            max_rate_hz,
        }
    }

    /// True iff both `352,800 Hz` and `384,000 Hz` are supported directly without downsampling.
    pub const fn is_native_8fs(&self) -> bool {
        matches!(
            (self.target_for_352800, self.target_for_384000),
            (
                Some(DacRateTarget {
                    sample_rate_hz: 352_800,
                    ..
                }),
                Some(DacRateTarget {
                    sample_rate_hz: 384_000,
                    ..
                })
            )
        )
    }

    /// True iff at least one output rate is supported.
    pub const fn has_any_supported_rate(&self) -> bool {
        self.target_for_352800.is_some() || self.target_for_384000.is_some()
    }

    /// Resolve the actual DAC wire `(sample_rate_hz, wire_format)` for an internal pipeline rate.
    pub fn resolve_for_internal_rate(&self, internal_rate_hz: u32) -> Result<DacRateTarget, String> {
        match internal_rate_hz {
            352_800 => self.target_for_352800.ok_or_else(|| {
                "DAC does not support any usable output rate for the 352.8 kHz DSP family"
                    .to_string()
            }),
            384_000 => self.target_for_384000.ok_or_else(|| {
                "DAC does not support any usable output rate for the 384 kHz DSP family".to_string()
            }),
            // Pass-through (`--filter null`) at an arbitrary non-8fS source rate:
            other => {
                let family_target = if other % 44_100 == 0 {
                    self.target_for_352800
                } else {
                    self.target_for_384000
                };
                match family_target {
                    Some(t) if other <= t.sample_rate_hz => Ok(DacRateTarget {
                        sample_rate_hz: other,
                        wire_format: t.wire_format,
                    }),
                    Some(t) => Ok(t),
                    None => Ok(DacRateTarget {
                        sample_rate_hz: other,
                        wire_format: DacWireFormat::Int32,
                    }),
                }
            }
        }
    }
}

/// Metadata describing an audio stream's sample rate and bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamSpec {
    pub sample_rate_hz: u32,
    pub bits_per_sample: u16,
    pub channels: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_prefer_8fs_and_same_family_and_fallback_cross_family_only_when_needed() {
        // 1. 384 kHz DAC -> native 8fS Int32, zero downsampling.
        let full_8fs = DacOutputCapabilities::from_supported_rates(
            &[
                (352_800, DacWireFormat::Int32),
                (384_000, DacWireFormat::Int32),
                (192_000, DacWireFormat::Int32),
            ],
            None,
        );
        assert!(full_8fs.is_native_8fs());
        assert_eq!(
            full_8fs.resolve_for_internal_rate(352_800).unwrap(),
            DacRateTarget {
                sample_rate_hz: 352_800,
                wire_format: DacWireFormat::Int32
            }
        );

        // 2. 192 kHz DAC supporting both 176.4 and 192 -> exact ÷2 same-family!
        let dac_192 = DacOutputCapabilities::from_supported_rates(
            &[
                (176_400, DacWireFormat::Int24Packed),
                (192_000, DacWireFormat::Int24Packed),
                (96_000, DacWireFormat::Int32),
            ],
            None,
        );
        assert!(!dac_192.is_native_8fs());
        assert_eq!(
            dac_192.resolve_for_internal_rate(352_800).unwrap(),
            DacRateTarget {
                sample_rate_hz: 176_400,
                wire_format: DacWireFormat::Int24Packed
            }
        );
        assert_eq!(
            dac_192.resolve_for_internal_rate(384_000).unwrap(),
            DacRateTarget {
                sample_rate_hz: 192_000,
                wire_format: DacWireFormat::Int24Packed
            }
        );

        // 3. 48/96/192-only DAC (no 44.1 family at all) -> falls back to 192 kHz for 352.8 kHz!
        let dac_48_only = DacOutputCapabilities::from_supported_rates(
            &[
                (192_000, DacWireFormat::Int32),
                (96_000, DacWireFormat::Int32),
                (48_000, DacWireFormat::Int32),
            ],
            None,
        );
        assert_eq!(
            dac_48_only.resolve_for_internal_rate(352_800).unwrap().sample_rate_hz,
            192_000
        );
        assert_eq!(
            dac_48_only.resolve_for_internal_rate(384_000).unwrap().sample_rate_hz,
            192_000
        );

        // 4. Explicit --max-rate 96000 cap on a 384 kHz DAC -> caps cleanly to 88.2 / 96 kHz!
        let capped = DacOutputCapabilities::from_supported_rates(
            &[
                (384_000, DacWireFormat::Int32),
                (352_800, DacWireFormat::Int32),
                (192_000, DacWireFormat::Int32),
                (176_400, DacWireFormat::Int32),
                (96_000, DacWireFormat::Int32),
                (88_200, DacWireFormat::Int32),
            ],
            Some(96_000),
        );
        assert_eq!(
            capped.resolve_for_internal_rate(352_800).unwrap().sample_rate_hz,
            88_200
        );
        assert_eq!(
            capped.resolve_for_internal_rate(384_000).unwrap().sample_rate_hz,
            96_000
        );

        // 5. E2: Busy-DAC startup fallback preserves --max-rate cap.
        let busy_capped = DacOutputCapabilities::native_8fs_capped(Some(192_000));
        assert_eq!(
            busy_capped.resolve_for_internal_rate(352_800).unwrap().sample_rate_hz,
            176_400
        );
        assert_eq!(
            busy_capped.resolve_for_internal_rate(384_000).unwrap().sample_rate_hz,
            192_000
        );
        assert_eq!(busy_capped.max_rate_hz, Some(192_000));
    }
}
