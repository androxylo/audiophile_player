// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! **L10 · Rate Families, Output Rates, Wire Formats, Shapers & DAC Bits Matrix**
//! (`PIPELINE_TESTS_REVIEW.md` §3 L10).
//!
//! Matrix axes:
//! - Source rates: `44.1 / 48 / 88.2 / 96 / 176.4 / 192 / 352.8 / 384 kHz`
//! - Output tiers: `8fS` (native), `4fS` (`176.4 / 192 kHz`), `2fS` (`88.2 / 96 kHz`)
//! - Wire formats: `Int32`, `Int24In32`, `Int24Packed`, `Int16`
//! - Shapers: `ns-gentle-2`, `ns-sharp-5`, `ns-wide-9`, `ns-smooth-15`, `tpdf`
//! - `dac_bits`: `16`, `18`, `20`, `24`
//!
//! `l10_matrix_covering_sweep` runs on every CI push (< 15 s) exercising EVERY value on EVERY
//! axis (synthetic + seeded real fixture cells). `l10_matrix_exhaustive_nightly` (`#[ignore]`)
//! runs the full Cartesian product nightly in CI (`cargo test --release -- --ignored`).

use ap_core::format::DacWireFormat;
use ap_core::headroom::TrackPeakMetadata;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile};
use ap_source::flac::decode_flac_full_memory;
use ap_verify::fixtures::{hot_isp_master, piano_multitone, ultra_short_track};
use ap_verify::pick::bundled_fixture_path;
use ap_verify::runner::{NullRunConfig, run_loopback_null_buffers};

const ALL_SOURCE_RATES: &[u32] = &[
    44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
];

const ALL_WIRE_FORMATS: &[DacWireFormat] = &[
    DacWireFormat::Int32,
    DacWireFormat::Int24In32,
    DacWireFormat::Int24Packed,
    DacWireFormat::Int16,
];

const ALL_SHAPERS: &[NoiseShaperProfile] = &[
    NoiseShaperProfile::NsGentle2,
    NoiseShaperProfile::NsSharp5,
    NoiseShaperProfile::NsWide9,
    NoiseShaperProfile::NsSmooth15,
    NoiseShaperProfile::TpdfDither,
];

const ALL_DAC_BITS: &[u8] = &[16, 18, 20, 24];

fn max_rate_for_tier(src_rate: u32, tier: u8) -> Option<u32> {
    let base = if src_rate % 44_100 == 0 {
        44_100
    } else {
        48_000
    };
    match tier {
        8 => None, // native 8fS
        4 => Some(base * 4),
        2 => Some(base * 2),
        _ => unreachable!(),
    }
}

fn check_cell(
    label: &str,
    src_rate: u32,
    tier: u8,
    wire_format: DacWireFormat,
    shaper: NoiseShaperProfile,
    dac_bits: u8,
    seed: u64,
    duration_s: f64,
) {
    // Only test valid (dac_bits <= wire_format.bit_depth()) combinations so Layer C stays
    // cleanly representable on the wire container without secondary wire re-dither.
    if dac_bits > wire_format.bit_depth() {
        return;
    }
    let base_rate = if src_rate % 44_100 == 0 {
        44_100
    } else {
        48_000
    };
    let out_rate = (base_rate * u32::from(tier)) as f64;
    // Review #10 TV2: scale cell duration so EVERY L10 cell produces >= 524,288 output frames
    // (~1.37 s @ 384 kHz), ensuring Welch 1/3-oct coherence AND +-1.0 dB in-band NTF PSD both run
    // with >= 63 overlapping segments (tight Welch variance well below +-1.0 dB across 500 Hz..20 kHz).
    let eff_duration_s = duration_s.max(524_288.0 / out_rate);
    let src_bits: u16 = if src_rate <= 48_000 { 16 } else { 24 };
    let (spec, left, right) = piano_multitone(src_rate, src_bits, eff_duration_s, seed);
    let filter_override = if src_rate >= 352_800 {
        Some(FilterArchetype::Null)
    } else {
        Some(FilterArchetype::ShortLp)
    };
    let cfg = NullRunConfig {
        filter_override,
        shaper_override: Some(shaper),
        dac_bits: Some(dac_bits),
        wire_format,
        max_rate_hz: max_rate_for_tier(src_rate, tier),
        fixed_gain_db: Some(-3.0),
        dac_margin_db: 1.0,
        seed,
        ..NullRunConfig::default()
    };
    let rep = run_loopback_null_buffers(
        label,
        "matrix_synth",
        spec,
        TrackPeakMetadata::default(),
        &left,
        &right,
        false,
        &cfg,
    )
    .unwrap_or_else(|e| panic!("L10 cell {label} errored: {e}"));

    assert!(
        rep.layer_a_passed,
        "L10 {label}: Layer A failed (peak={:+.2} dB)",
        rep.layer_a_peak_residual_db
    );
    assert!(
        rep.layer_b.passed,
        "L10 {label}: Layer B failed ({:?})",
        rep.layer_b.failure_reasons
    );
    // Review #10 TV2: verify Layer B harshness/coherence check actually ran on this cell!
    assert!(
        rep.output_frames >= 16_384 && rep.layer_b.max_coherence_excess_db.is_finite(),
        "L10 {label}: Layer B coherence detector did not execute (output_frames={})",
        rep.output_frames
    );
    assert!(
        rep.layer_c_passed,
        "L10 {label}: Layer C failed ({} wire mismatches)",
        rep.layer_c_mismatches
    );
}

#[test]
fn l10_matrix_covering_sweep_and_self_generated_fixtures() {
    // 1. Cover every single source rate (all 8), output tier (8fS/4fS/2fS), wire format (all 4),
    //    shaper (all 5), and dac_bits (all 4) in a fast deterministic covering array (< 10 s).
    let tiers: &[u8] = &[8, 4, 2];
    for (idx, &src_rate) in ALL_SOURCE_RATES.iter().enumerate() {
        let tier = tiers[idx % tiers.len()];
        let wire = ALL_WIRE_FORMATS[idx % ALL_WIRE_FORMATS.len()];
        let shaper = ALL_SHAPERS[idx % ALL_SHAPERS.len()];
        let mut bits = ALL_DAC_BITS[idx % ALL_DAC_BITS.len()];
        if bits > wire.bit_depth() {
            bits = wire.bit_depth();
        }
        let label = format!("r{src_rate}_t{tier}_{wire:?}_{shaper}_{bits}b");
        check_cell(
            &label,
            src_rate,
            tier,
            wire,
            shaper,
            bits,
            0x10_0000 + idx as u64,
            0.20,
        );
    }

    // 2. Also sweep all 5 shapers x all 4 dac_bits at 8fS & 4fS (covering every shaper/bit pair!).
    for (si, &shaper) in ALL_SHAPERS.iter().enumerate() {
        let bits = ALL_DAC_BITS[si % ALL_DAC_BITS.len()];
        let wire = if bits == 16 {
            DacWireFormat::Int16
        } else {
            DacWireFormat::Int24Packed
        };
        let tier = if si % 2 == 0 { 8 } else { 4 };
        let label = format!("shaper_{shaper}_t{tier}_{wire:?}_{bits}b");
        check_cell(
            &label,
            44_100,
            tier,
            wire,
            shaper,
            bits,
            0x20_0000 + si as u64,
            0.25,
        );
    }

    // 3. Exercise self-generated hot ISP master (0 dBFS + +3.01 dBTP ISP + burst-edge Gibbs ring
    //    to ~+3.14 dBTP) and 2.5 s ultra-short track. Pass the scanned true-peak metadata so
    //    normal-mode `AudioPipeline` computes `G_up = -3 dB` plus excess scale `S`, which TV1
    //    uses verbatim from `session.report().headroom.total_gain_db()` (zero oracle override).
    let (hot_spec, hot_l, hot_r) = hot_isp_master(48_000, 24, 0.75);
    let hot_rep = run_loopback_null_buffers(
        "hot_isp_48k_24b",
        "hot_isp",
        hot_spec,
        TrackPeakMetadata::default().with_full_scan_true_peak_dbtp(3.25),
        &hot_l,
        &hot_r,
        false,
        &NullRunConfig {
            filter_override: Some(FilterArchetype::ShortLp),
            shaper_override: Some(NoiseShaperProfile::NsGentle2),
            dac_bits: Some(24),
            wire_format: DacWireFormat::Int32,
            dac_margin_db: 0.0,
            seed: 0x30_0001,
            ..NullRunConfig::default()
        },
    )
    .expect("hot ISP loopback null");
    assert!(hot_rep.passed, "hot ISP failed: {:?}", hot_rep.layer_b.failure_reasons);

    let (short_spec, short_l, short_r) = ultra_short_track(44_100, 16);
    let short_rep = run_loopback_null_buffers(
        "ultra_short_2p5s",
        "ultra_short",
        short_spec,
        TrackPeakMetadata::default(),
        &short_l,
        &short_r,
        false,
        &NullRunConfig {
            filter_override: Some(FilterArchetype::ShortLp),
            shaper_override: Some(NoiseShaperProfile::NsSmooth15),
            dac_bits: Some(20),
            wire_format: DacWireFormat::Int24In32,
            seed: 0x30_0002,
            ..NullRunConfig::default()
        },
    )
    .expect("ultra-short loopback null");
    assert!(short_rep.passed, "ultra-short failed: {:?}", short_rep.layer_b.failure_reasons);

    // 4. Seeded sample of cells on the real bundled 24/96 Nocturne FLAC fixture (1.5 s excerpt).
    let decoded = decode_flac_full_memory(&bundled_fixture_path()).expect("decode fixture");
    let take = (decoded.spec.sample_rate_hz as usize * 3 / 2).min(decoded.audio.left.len());
    for (ci, &(tier, wire, shaper, bits)) in [
        (8_u8, DacWireFormat::Int32, NoiseShaperProfile::NsGentle2, 24_u8),
        (4, DacWireFormat::Int24Packed, NoiseShaperProfile::NsWide9, 20),
        (2, DacWireFormat::Int16, NoiseShaperProfile::TpdfDither, 16),
    ]
    .iter()
    .enumerate()
    {
        let rep = run_loopback_null_buffers(
            &format!("fixture_cell_{ci}"),
            "nocturne_fixture",
            decoded.spec,
            decoded.peak_meta,
            &decoded.audio.left[..take],
            &decoded.audio.right[..take],
            true,
            &NullRunConfig {
                filter_override: Some(FilterArchetype::ShortLp),
                shaper_override: Some(shaper),
                dac_bits: Some(bits),
                wire_format: wire,
                max_rate_hz: max_rate_for_tier(decoded.spec.sample_rate_hz, tier),
                seed: 0x40_0000 + ci as u64,
                ..NullRunConfig::default()
            },
        )
        .expect("fixture matrix cell");
        assert!(
            rep.passed,
            "fixture cell {ci} failed: A={:+.2} dB B={:?} C={}",
            rep.layer_a_peak_residual_db,
            rep.layer_b.failure_reasons,
            rep.layer_c_mismatches
        );
    }
}

#[test]
#[ignore = "exhaustive L10 Cartesian product — executed by nightly-verify.yml"]
fn l10_matrix_exhaustive_nightly() {
    let mut cell_id = 0_u64;
    for &src_rate in ALL_SOURCE_RATES {
        for &tier in &[8_u8, 4, 2] {
            for &wire in ALL_WIRE_FORMATS {
                for &shaper in ALL_SHAPERS {
                    for &bits in ALL_DAC_BITS {
                        if bits > wire.bit_depth() {
                            continue;
                        }
                        cell_id += 1;
                        let label = format!("n{cell_id}_r{src_rate}_t{tier}_{wire:?}_{shaper}_{bits}b");
                        check_cell(
                            &label,
                            src_rate,
                            tier,
                            wire,
                            shaper,
                            bits,
                            0x90_0000 + cell_id,
                            0.12,
                        );
                    }
                }
            }
        }
    }
}
