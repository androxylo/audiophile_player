// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! TDD unit tests for the decided `-3 dB` / `0 dB` upsampler headroom & excess-only
//! convolution scaling policy (`HEADROOM_DESIGN.md`).

use ap_core::headroom::{
    HeadroomMode, MINUS_3_DB_LINEAR_THRESHOLD, PeakKind, TrackPeakMetadata, select_headroom,
};

#[test]
fn headroom_modes_are_strictly_hardcoded_to_minus_3_db_and_0_db() {
    assert_eq!(HeadroomMode::ZeroDb.gain_db(), 0.0);
    assert_eq!(HeadroomMode::ZeroDb.linear(), 1.0);

    assert_eq!(HeadroomMode::Minus3Db.gain_db(), -3.0);
    let expected = 10.0_f64.powf(-3.0 / 20.0);
    assert!((HeadroomMode::Minus3Db.linear() - expected).abs() < 1e-15);
    assert!(HeadroomMode::Minus3Db.linear() < 1.0);
}

#[test]
fn unknown_or_unrealistic_peak_always_defaults_to_minus_3_db() {
    // Completely untagged FLAC -> unknown (counts as 0 dBFS, A_isp = 3 dB, G_up = -3 dB)
    let d_empty = select_headroom(&TrackPeakMetadata::default(), false, None, None);
    assert_eq!(d_empty.mode, HeadroomMode::Minus3Db);
    assert_eq!(d_empty.convolution_scale_db, 0.0);

    // Gain tag present without a peak tag does NOT bound instantaneous peak -> must stay -3 dB
    let gain_only = TrackPeakMetadata {
        replaygain_track_gain_db: Some(6.0),
        replaygain_album_gain_db: Some(5.0),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&gain_only, false, None, None).mode,
        HeadroomMode::Minus3Db
    );

    // Invalid/malformed peak values (0.0, negative, NaN, inf, absurdly tiny < -120 dBFS) -> -3 dB
    for bad in [0.0, -0.5, f64::NAN, f64::INFINITY, 1e-9] {
        let meta = TrackPeakMetadata {
            replaygain_track_peak: Some(bad),
            ..TrackPeakMetadata::default()
        };
        assert_eq!(
            select_headroom(&meta, false, None, None).mode,
            HeadroomMode::Minus3Db
        );
    }

    // True peak > +6 dBTP counts as unknown (even if a quiet sample peak tag is also present!)
    let over_6dbtp = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(6.5 / 20.0)),
        replaygain_track_peak: Some(0.5),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(over_6dbtp.classified_peak(), None);
    assert!(!over_6dbtp.has_verified_true_peak());
    let d_over = select_headroom(&over_6dbtp, false, Some(2.0), None);
    assert_eq!(d_over.mode, HeadroomMode::Minus3Db);
    // Unknown: P=0, A_isp=3, G_up=-3, G_c=+2 -> excess = +2 dB -> conv scale = -2 dB.
    assert!((d_over.convolution_scale_db - (-2.0)).abs() < 1e-9);

    // Live streaming source (Tidal) has unknown peak -> always -3 dB even if metadata provided
    let quiet = TrackPeakMetadata {
        replaygain_track_peak: Some(0.5),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&quiet, true, None, None).mode,
        HeadroomMode::Minus3Db
    );
}

#[test]
fn quiet_tracks_below_minus_3_dbfs_get_0_db_and_hot_tracks_get_minus_3_db() {
    // Quiet track (-6 dBFS, peak ~0.501) -> 0 dB
    let quiet = TrackPeakMetadata {
        replaygain_track_peak: Some(0.501),
        ..TrackPeakMetadata::default()
    };
    let d_quiet = select_headroom(&quiet, false, None, None);
    assert_eq!(d_quiet.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_quiet.peak_kind, Some(PeakKind::SamplePeak));
    assert_eq!(d_quiet.convolution_scale_db, 0.0);

    // Boundary: exactly at -3.0 dBFS threshold -> 0 dB (sample peak <= -3 dBFS suffices)
    let exact = TrackPeakMetadata {
        replaygain_track_peak: Some(MINUS_3_DB_LINEAR_THRESHOLD),
        ..TrackPeakMetadata::default()
    };
    let d_exact = select_headroom(&exact, false, Some(0.0), None);
    assert_eq!(d_exact.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_exact.convolution_scale_db, 0.0);

    // Slightly above -3.0 dBFS (-2.9 dBFS ~ 0.716) -> -3 dB
    let warm = TrackPeakMetadata {
        replaygain_track_peak: Some(0.716),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&warm, false, None, None).mode,
        HeadroomMode::Minus3Db
    );

    // Sample-peak tag above full scale (1.122 = +1.0 dBFS <= +6 dBTP) counts as TruePeak (A_isp=0)!
    let hot_sp = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(1.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    assert!(hot_sp.has_verified_true_peak());
    assert_eq!(hot_sp.classified_peak().unwrap().kind, PeakKind::TruePeak);
    let d_hot = select_headroom(&hot_sp, false, Some(1.5), None);
    assert_eq!(d_hot.mode, HeadroomMode::Minus3Db);
    // P=+1.0, A_isp=0, G_up=-3.0, G_c=+1.5 -> -0.5 dBFS <= 0 -> no convolution cut!
    assert_eq!(d_hot.convolution_scale_db, 0.0);
    let d_hot_boost = select_headroom(&hot_sp, false, Some(4.0), None);
    // P=+1.0, A_isp=0, G_up=-3.0, G_c=+4.0 -> +2.0 dBFS -> conv scale = -2.0 dB!
    assert!((d_hot_boost.convolution_scale_db - (-2.0)).abs() < 1e-9);
}

#[test]
fn track_peak_takes_priority_over_album_peak_and_falls_back_cleanly() {
    let quiet_on_loud_album = TrackPeakMetadata {
        replaygain_track_peak: Some(0.562),
        replaygain_album_peak: Some(0.99),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&quiet_on_loud_album, false, None, None).mode,
        HeadroomMode::ZeroDb
    );

    let quiet_album_only = TrackPeakMetadata {
        replaygain_album_peak: Some(0.60),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&quiet_album_only, false, None, None).mode,
        HeadroomMode::ZeroDb
    );

    let loud_album_only = TrackPeakMetadata {
        replaygain_album_peak: Some(0.95),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&loud_album_only, false, None, None).mode,
        HeadroomMode::Minus3Db
    );
}

#[test]
fn convolution_is_scaled_only_by_excess_over_0_dbfs_and_never_cuts_when_under_0_dbfs() {
    // Addendum 1 canonical example: true peak -6 dB with +1 dB filter -> -5 dBFS -> 0 dB / 0 dB!
    let tp_minus_6 = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(-6.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let d_example = select_headroom(&tp_minus_6, false, Some(1.0), None);
    assert_eq!(d_example.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_example.convolution_scale_db, 0.0);
    assert_eq!(d_example.convolution_scale_linear(), 1.0);

    // True peak -6 dB with +8 dB uncapped filter -> G_up=0 dB, excess = -6+0+0+8 = +2 dB -> conv -2 dB
    let d_big_boost = select_headroom(&tp_minus_6, false, Some(8.0), None);
    assert_eq!(d_big_boost.mode, HeadroomMode::ZeroDb);
    assert!((d_big_boost.convolution_scale_db - (-2.0)).abs() < 1e-9);

    // Sample peak -4.0 dBFS (A_isp=3 dB) with +2.0 dB filter:
    // G_up = 0 dB (P = -4 <= -3), predicted = -4 + 3 + 0 + 2 = +1 dB -> conv scale = -1.0 dB
    let sp_minus_4 = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-4.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let d_sp = select_headroom(&sp_minus_4, false, Some(2.0), None);
    assert_eq!(d_sp.mode, HeadroomMode::ZeroDb);
    assert!((d_sp.convolution_scale_db - (-1.0)).abs() < 1e-9);

    // Pure-cut filter (G_c = -2.5 dB): max(G_c, 0) = 0 -> never boosts, never adds cut.
    let d_cut = select_headroom(&TrackPeakMetadata::default(), false, Some(-2.5), None);
    assert_eq!(d_cut.mode, HeadroomMode::Minus3Db);
    assert_eq!(d_cut.convolution_scale_db, 0.0);

    // Full-scan post-chain peak M takes precedence over metadata estimate:
    // M = +1.5 dBFS with G_up = 0 dB -> conv scale = -1.5 dB.
    let d_scan = select_headroom(&tp_minus_6, false, Some(10.0), Some(1.5));
    assert_eq!(d_scan.mode, HeadroomMode::ZeroDb);
    assert!((d_scan.convolution_scale_db - (-1.5)).abs() < 1e-9);

    // M = -1.0 dBFS with G_up = 0 dB -> no excess -> conv scale = 0 dB.
    let d_scan_ok = select_headroom(&tp_minus_6, false, Some(10.0), Some(-1.0));
    assert_eq!(d_scan_ok.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_scan_ok.convolution_scale_db, 0.0);
}

#[test]
fn review5_headroom_design_rows_null_convolver_16bit_boundary_14bit_and_tail_s_b() {
    use ap_core::headroom::{apply_gapless_tail_headroom, select_headroom_for_bits};

    // 1. Finding #14 (HEADROOM_DESIGN.md §4 null-convolver row):
    // +4 dBTP true peak with NullConvolver (G_c = 0 / None) -> G_up = -3 dB, S = -1.0 dB!
    let tp_plus_4 = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(4.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let d_null = select_headroom_for_bits(&tp_plus_4, 24, false, None, None);
    assert_eq!(d_null.mode, HeadroomMode::Minus3Db);
    assert!((d_null.convolution_scale_db - (-1.0)).abs() < 1e-9);

    // 2. Finding #16: 16-bit scanner normalizing by 32767 produces 32768/32767 ~= 1.00003052.
    // Must stay SamplePeak (keeping 3 dB A_isp reserve), so with G_c=+3 dB -> S=-3 dB!
    let full_16bit_sp = TrackPeakMetadata {
        replaygain_track_peak: Some(32768.0 / 32767.0),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        full_16bit_sp.classified_peak_for_bits(16).unwrap().kind,
        PeakKind::SamplePeak
    );
    let d_16 = select_headroom_for_bits(&full_16bit_sp, 16, false, Some(3.0), None);
    assert_eq!(d_16.mode, HeadroomMode::Minus3Db);
    assert!((d_16.convolution_scale_db - (-3.0)).abs() < 1e-3);

    // 1-LSB realism floor for 16-bit (-90.3 dBFS): value below 1 LSB (~ -100 dBFS) is rejected.
    let sub_lsb_16 = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-100.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(sub_lsb_16.classified_peak_for_bits(16), None);

    // 3. HEADROOM_DESIGN.md §8 Test 4: 14-bit content (~ -12 dBFS sample peak) + +6 dB filter
    // -> G_up = 0 dB, predicted = -12 + 3 + 0 + 6 = -3 <= 0 -> S = 0 dB, total gain exactly 0 dB!
    let content_14bit = TrackPeakMetadata {
        replaygain_track_peak: Some(0.25), // -12.04 dBFS
        ..TrackPeakMetadata::default()
    };
    let d_14 = select_headroom_for_bits(&content_14bit, 16, false, Some(6.0), None);
    assert_eq!(d_14.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_14.convolution_scale_db, 0.0);
    assert_eq!(d_14.total_gain_db(), 0.0);

    // 4. Finding #3 (all 3 S_B tail-overlap scenarios from Review #5):
    // (a) Untagged B (-3 dB), G_c=+2 dB, T=0.25 -> S_B = -4.50 dB (G_up stays -3 dB).
    let mut d_a = select_headroom_for_bits(&TrackPeakMetadata::default(), 24, false, Some(2.0), None);
    apply_gapless_tail_headroom(
        &mut d_a,
        &TrackPeakMetadata::default(),
        24,
        false,
        2.0,
        None,
        0.25,
        0.0,
    );
    assert_eq!(d_a.mode, HeadroomMode::Minus3Db);
    assert!((d_a.convolution_scale_db - (-4.4988)).abs() < 0.01);

    // (b) REPLAYGAIN_TRACK_PEAK=0.5 (sample, A_isp=3), G_c=0, T=0.4 -> G_up=0, S_B = -1.42 dB.
    let sp_half = TrackPeakMetadata {
        replaygain_track_peak: Some(0.5),
        ..TrackPeakMetadata::default()
    };
    let mut d_b = select_headroom_for_bits(&sp_half, 24, false, Some(0.0), None);
    apply_gapless_tail_headroom(&mut d_b, &sp_half, 24, false, 0.0, None, 0.4, 0.0);
    assert_eq!(d_b.mode, HeadroomMode::ZeroDb);
    assert!((d_b.convolution_scale_db - (-1.4164)).abs() < 0.01);

    // (c) True peak -3.5 dBTP, G_c=0, T=0.6 -> G_up stays 0 dB, S_B = -4.46 dB!
    let tp_35 = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(-3.5 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let mut d_c = select_headroom_for_bits(&tp_35, 24, false, Some(0.0), None);
    apply_gapless_tail_headroom(&mut d_c, &tp_35, 24, false, 0.0, None, 0.6, 0.0);
    assert_eq!(d_c.mode, HeadroomMode::ZeroDb);
    assert!((d_c.convolution_scale_db - (-4.4588)).abs() < 0.01);
}

#[test]
fn headroom_design_issue7_complete_stage1_stage2_truth_tables_and_tag_realism() {
    use ap_core::headroom::{
        MAX_REALISTIC_TRUE_PEAK_LINEAR, apply_gapless_tail_headroom, select_headroom_for_bits,
    };

    let db = |x: f64| 10.0_f64.powf(x / 20.0);

    // --- HEADROOM_DESIGN.md §3 Stage-1 complete truth table ---
    let stage1_cases: &[(&str, TrackPeakMetadata, u16, bool, HeadroomMode)] = &[
        (
            "14-bit content (-12.04 dBFS)",
            TrackPeakMetadata {
                replaygain_track_peak: Some(0.25),
                ..TrackPeakMetadata::default()
            },
            16,
            false,
            HeadroomMode::ZeroDb,
        ),
        (
            "15-bit content (-6.02 dBFS)",
            TrackPeakMetadata {
                replaygain_track_peak: Some(0.5),
                ..TrackPeakMetadata::default()
            },
            16,
            false,
            HeadroomMode::ZeroDb,
        ),
        (
            "quiet master (-3.5 dBFS)",
            TrackPeakMetadata {
                replaygain_track_peak: Some(db(-3.5)),
                ..TrackPeakMetadata::default()
            },
            24,
            false,
            HeadroomMode::ZeroDb,
        ),
        (
            "exact boundary (-3.00 dBFS)",
            TrackPeakMetadata {
                replaygain_track_peak: Some(db(-3.0)),
                ..TrackPeakMetadata::default()
            },
            24,
            false,
            HeadroomMode::ZeroDb,
        ),
        (
            "hot master (-0.3 dBFS)",
            TrackPeakMetadata {
                replaygain_track_peak: Some(db(-0.3)),
                ..TrackPeakMetadata::default()
            },
            24,
            false,
            HeadroomMode::Minus3Db,
        ),
        (
            "untagged local",
            TrackPeakMetadata::default(),
            24,
            false,
            HeadroomMode::Minus3Db,
        ),
        (
            "live stream with quiet tag ignored",
            TrackPeakMetadata {
                true_peak_linear: Some(db(-12.0)),
                ..TrackPeakMetadata::default()
            },
            24,
            true,
            HeadroomMode::Minus3Db,
        ),
    ];
    for (name, meta, bits, streaming, expected_mode) in stage1_cases {
        let d = select_headroom_for_bits(meta, *bits, *streaming, Some(0.0), None);
        assert_eq!(d.mode, *expected_mode, "Stage-1 row failed: {name}");
    }

    // --- HEADROOM_DESIGN.md §4 Stage-2 complete 9-row truth table ---
    struct Stage2Row {
        name: &'static str,
        meta: TrackPeakMetadata,
        g_c_db: f64,
        expected_g_up: f64,
        expected_s: f64,
        expected_total: f64,
    }
    let rows = [
        Stage2Row {
            name: "Owner example: -6 true, +1 fill -> 0 dB total",
            meta: TrackPeakMetadata {
                true_peak_linear: Some(db(-6.0)),
                ..TrackPeakMetadata::default()
            },
            g_c_db: 1.0,
            expected_g_up: 0.0,
            expected_s: 0.0,
            expected_total: 0.0,
        },
        Stage2Row {
            name: "14-bit sample (-12.04), +5 fill -> 0 dB total",
            meta: TrackPeakMetadata {
                replaygain_track_peak: Some(db(-12.04)),
                ..TrackPeakMetadata::default()
            },
            g_c_db: 5.0,
            expected_g_up: 0.0,
            expected_s: 0.0,
            expected_total: 0.0,
        },
        Stage2Row {
            name: "15-bit sample (-6.02), +2 fill -> 0 dB total",
            meta: TrackPeakMetadata {
                replaygain_track_peak: Some(db(-6.02)),
                ..TrackPeakMetadata::default()
            },
            g_c_db: 2.0,
            expected_g_up: 0.0,
            expected_s: 0.0,
            expected_total: 0.0,
        },
        Stage2Row {
            name: "15-bit sample (-6.02), +5 fill -> S=-1.98, total=-1.98",
            meta: TrackPeakMetadata {
                replaygain_track_peak: Some(db(-6.02)),
                ..TrackPeakMetadata::default()
            },
            g_c_db: 5.0,
            expected_g_up: 0.0,
            expected_s: -1.98,
            expected_total: -1.98,
        },
        Stage2Row {
            name: "Quiet master (-4 sample), cut-only (-1) -> 0 dB total",
            meta: TrackPeakMetadata {
                replaygain_track_peak: Some(db(-4.0)),
                ..TrackPeakMetadata::default()
            },
            g_c_db: -1.0,
            expected_g_up: 0.0,
            expected_s: 0.0,
            expected_total: 0.0,
        },
        Stage2Row {
            name: "Hot master (+1 true), +2 fill -> G_up=-3, S=0, total=-3",
            meta: TrackPeakMetadata {
                true_peak_linear: Some(db(1.0)),
                ..TrackPeakMetadata::default()
            },
            g_c_db: 2.0,
            expected_g_up: -3.0,
            expected_s: 0.0,
            expected_total: -3.0,
        },
        Stage2Row {
            name: "Untagged, +2 fill -> G_up=-3, S=-2, total=-5",
            meta: TrackPeakMetadata::default(),
            g_c_db: 2.0,
            expected_g_up: -3.0,
            expected_s: -2.0,
            expected_total: -5.0,
        },
        Stage2Row {
            name: "Untagged, +5 fill -> G_up=-3, S=-5, total=-8",
            meta: TrackPeakMetadata::default(),
            g_c_db: 5.0,
            expected_g_up: -3.0,
            expected_s: -5.0,
            expected_total: -8.0,
        },
        Stage2Row {
            name: "Hot sample (-0.3), +6 fill -> G_up=-3, S=-5.7, total=-8.7",
            meta: TrackPeakMetadata {
                replaygain_track_peak: Some(db(-0.3)),
                ..TrackPeakMetadata::default()
            },
            g_c_db: 6.0,
            expected_g_up: -3.0,
            expected_s: -5.7,
            expected_total: -8.7,
        },
    ];
    for row in &rows {
        let d = select_headroom_for_bits(&row.meta, 16, false, Some(row.g_c_db), None);
        assert!(
            (d.mode.gain_db() - row.expected_g_up).abs() < 1e-6,
            "Stage-2 G_up mismatch for '{}': got {}, expected {}",
            row.name,
            d.mode.gain_db(),
            row.expected_g_up
        );
        assert!(
            (d.convolution_scale_db - row.expected_s).abs() < 1e-3,
            "Stage-2 S mismatch for '{}': got {}, expected {}",
            row.name,
            d.convolution_scale_db,
            row.expected_s
        );
        assert!(
            (d.total_gain_db() - row.expected_total).abs() < 1e-3,
            "Stage-2 total mismatch for '{}': got {}, expected {}",
            row.name,
            d.total_gain_db(),
            row.expected_total
        );
        assert!(d.convolution_scale_db <= 0.0, "S must never be positive");
    }

    // --- §2 Tag realism checks ---
    // 1. Exact linear 2.0 (+6.0206 dBTP) is valid per §2 table `(0, 2.0]`; 2.01 is unknown.
    let tp_exact_2 = TrackPeakMetadata {
        true_peak_linear: Some(MAX_REALISTIC_TRUE_PEAK_LINEAR),
        ..TrackPeakMetadata::default()
    };
    let cp2 = tp_exact_2
        .classified_peak_for_bits(24)
        .expect("linear 2.0 (+6.0206 dBTP) must be accepted");
    assert_eq!(cp2.kind, PeakKind::TruePeak);
    let tp_over_2 = TrackPeakMetadata {
        true_peak_linear: Some(2.01),
        replaygain_track_peak: Some(0.5),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        tp_over_2.classified_peak_for_bits(24),
        None,
        "linear > 2.0 must resolve to unknown without falling back to quiet sample peak"
    );

    // 2. Track sample peak > album sample peak (beyond 0.5% rounding) -> contradictory -> unknown.
    let bad_track_vs_album = TrackPeakMetadata {
        replaygain_track_peak: Some(0.55),
        replaygain_album_peak: Some(0.45),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(bad_track_vs_album.classified_peak_for_bits(24), None);
    assert_eq!(
        select_headroom_for_bits(&bad_track_vs_album, 24, false, None, None).mode,
        HeadroomMode::Minus3Db
    );

    // Within 0.5% rounding tolerance (e.g. 0.501 vs 0.500) stays valid.
    let rounding_ok = TrackPeakMetadata {
        replaygain_track_peak: Some(0.501),
        replaygain_album_peak: Some(0.500),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom_for_bits(&rounding_ok, 24, false, None, None).mode,
        HeadroomMode::ZeroDb
    );

    // 3. Priority #1 full-scan true_peak_dbtp overrides tags (even contradictory or invalid tags).
    let with_full_scan = TrackPeakMetadata {
        replaygain_track_peak: Some(0.95),
        true_peak_invalid: true,
        ..TrackPeakMetadata::default()
    }
    .with_full_scan_true_peak_dbtp(-6.0);
    let d_fs = select_headroom_for_bits(&with_full_scan, 24, false, Some(2.0), None);
    assert_eq!(d_fs.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_fs.peak_kind, Some(PeakKind::TruePeak));
    assert_eq!(d_fs.convolution_scale_db, 0.0);

    // 4. Gapless S_B respects measured post-chain peak M when present (B_out = M + G_up).
    let mut d_m = select_headroom_for_bits(&with_full_scan, 24, false, Some(10.0), Some(-2.0));
    assert_eq!(d_m.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_m.convolution_scale_db, 0.0);
    // Incoming tail T = 0.5 -> room = -6.0206 dB; B_out = M + G_up = -2.0 dB -> S_B = -4.0206 dB.
    apply_gapless_tail_headroom(&mut d_m, &with_full_scan, 24, false, 10.0, Some(-2.0), 0.5, 0.0);
    assert_eq!(d_m.mode, HeadroomMode::ZeroDb);
    assert!((d_m.convolution_scale_db - (-4.0206)).abs() < 0.01);
}

#[test]
fn review6_16bit_rounded_sample_peak_tags_stay_sample_peak_and_keep_isp_reserve() {
    use ap_core::headroom::select_headroom_for_bits;

    // Review #6 Finding #14: 6-7 decimal rounded 32767-normalised 16-bit full-scale tags
    // (`1.000031` / `1.0000306`, from HEADROOM_DESIGN.md §2) must stay `SamplePeak` (A_isp = 3 dB),
    // NOT misclassify as `TruePeak` (A_isp = 0 dB).
    for &rounded in &[1.000_030_6_f64, 1.000_031_f64] {
        let meta = TrackPeakMetadata {
            replaygain_track_peak: Some(rounded),
            ..TrackPeakMetadata::default()
        };
        let cp = meta
            .classified_peak_for_bits(16)
            .expect("rounded 16-bit full-scale sample peak must be valid");
        assert_eq!(
            cp.kind,
            PeakKind::SamplePeak,
            "16-bit tag {rounded} must remain SamplePeak"
        );
        let d = select_headroom_for_bits(&meta, 16, false, Some(3.0), None);
        assert_eq!(d.mode, HeadroomMode::Minus3Db);
        assert!(
            (d.convolution_scale_db - (-3.0)).abs() < 0.01,
            "16-bit sample peak {rounded} with +3 dB REW must receive S=-3 dB (not 0 dB), got {:.3}",
            d.convolution_scale_db
        );
    }

    // Album peak gets the same rounding tolerance.
    let album_rounded = TrackPeakMetadata {
        replaygain_album_peak: Some(1.000_031),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        album_rounded.classified_peak_for_bits(16).unwrap().kind,
        PeakKind::SamplePeak
    );

    // Genuine above-FS true-peak values (e.g. 1.001 = +0.009 dBTP) still promote to TruePeak.
    let genuine_isp = TrackPeakMetadata {
        replaygain_track_peak: Some(1.001),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        genuine_isp.classified_peak_for_bits(16).unwrap().kind,
        PeakKind::TruePeak
    );
}

#[test]
fn issue23_p3_select_ultimate_headroom_v2_binary_step_dac_margin_and_streaming_t10() {
    use ap_core::headroom::{
        PeakEvidence, TP_METER_ERR_8X_64_DB, TP_METER_ERR_TAG_TP_DB, ULTIMATE_DAC_MARGIN_DB,
        quantizer_margin_db, select_ultimate_headroom_v2,
    };

    let dm = ULTIMATE_DAC_MARGIN_DB;
    assert!(dm >= 0.5 && dm <= 1.5, "unexpected ULTIMATE_DAC_MARGIN_DB={dm}");

    // 0. Quantizer margin m_q sanity: ns-gentle-2 (L1 = 2.0) -> 4.5 LSB below full scale.
    let m_q = quantizer_margin_db(24, 2.0);
    let expected_24 = -20.0 * (1.0 - 4.5 / (1_u64 << 23) as f64).log10();
    assert!((m_q - expected_24).abs() < 1e-12);
    let m_q_16 = quantizer_margin_db(16, 2.0);
    let expected_16 = -20.0 * (1.0_f64 - 4.5_f64 / 32768.0_f64).log10();
    assert!((m_q_16 - expected_16).abs() < 1e-12);

    // 1. T2 (Owner Decision #1): ExactPostChain with hot source TP (-1.41 dBFS > -3 dBFS)
    //    MUST keep binary G_up = -3 dB (NOT 0 dB!), with S = 0 dB since -1.41 - 3 + 1 < 0.
    let hot_exact = PeakEvidence::ExactPostChain {
        m_db: -1.41,
        source_tp_db: Some(-1.41),
    };
    let d_hot_exact = select_ultimate_headroom_v2(&hot_exact, false, None, Some(-1.41), 0.15, m_q);
    assert_eq!(d_hot_exact.mode, HeadroomMode::Minus3Db);
    assert!((d_hot_exact.convolution_scale_db).abs() < 1e-9);
    assert!((d_hot_exact.total_gain_db() - (-3.0)).abs() < 1e-9);

    // Review #10 TV8: cached M = -2.0 dBFS with hot source TP = -2.0 stays at -3.0 dB (NOT 0 dB!),
    // and cached M = +1.4 dBFS with hot source TP stays at -3.0 dB (since +1.4 - 3 + 1 < 0).
    let d_cached_m2 = select_ultimate_headroom_v2(
        &PeakEvidence::ExactPostChain {
            m_db: -2.0,
            source_tp_db: Some(-2.0),
        },
        false,
        Some(4.0),
        None,
        0.15,
        m_q,
    );
    assert_eq!(d_cached_m2.mode, HeadroomMode::Minus3Db);
    assert!((d_cached_m2.total_gain_db() - (-3.0)).abs() < 1e-9);

    let d_cached_p14 = select_ultimate_headroom_v2(
        &PeakEvidence::ExactPostChain {
            m_db: 1.4,
            source_tp_db: Some(-0.5),
        },
        false,
        Some(5.0),
        Some(0.2),
        0.15,
        m_q,
    );
    assert_eq!(d_cached_p14.mode, HeadroomMode::Minus3Db);
    assert!((d_cached_p14.total_gain_db() - (-3.0)).abs() < 1e-9);

    // 2. ExactPostChain with quiet source TP (-8.0 dBFS <= -3 dBFS) -> G_up = 0 dB, S = 0 dB.
    let quiet_exact = PeakEvidence::ExactPostChain {
        m_db: -8.0,
        source_tp_db: Some(-8.0),
    };
    let d_quiet_exact =
        select_ultimate_headroom_v2(&quiet_exact, false, None, Some(-8.0), 0.0, 0.0);
    assert_eq!(d_quiet_exact.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_quiet_exact.total_gain_db(), 0.0);

    // 3. ExactPostChain with hot REW boost (M = +4.2 dBFS, source TP = -0.5 dBFS):
    //    G_up = -3.0 dB, S = -(4.2 - 3.0 + m_q + dm) = -(1.2 + m_q + dm), total = -(4.2 + m_q + dm).
    let rew_exact = PeakEvidence::ExactPostChain {
        m_db: 4.2,
        source_tp_db: Some(-0.5),
    };
    let d_rew_exact = select_ultimate_headroom_v2(&rew_exact, false, Some(6.0), None, 0.15, m_q);
    assert_eq!(d_rew_exact.mode, HeadroomMode::Minus3Db);
    assert!((d_rew_exact.total_gain_db() - (-(4.2 + m_q + dm))).abs() < 1e-9);

    // 4. ExactPostChain without source_tp_db decides binary G_up on M itself:
    //    M = -1.5 dBFS (> -3) -> -3 dB; M = -4.0 dBFS (<= -3) -> 0 dB.
    let legacy_hot = PeakEvidence::ExactPostChain {
        m_db: -1.5,
        source_tp_db: None,
    };
    assert_eq!(
        select_ultimate_headroom_v2(&legacy_hot, false, None, None, 0.0, m_q).mode,
        HeadroomMode::Minus3Db
    );
    let legacy_quiet = PeakEvidence::ExactPostChain {
        m_db: -4.0,
        source_tp_db: None,
    };
    assert_eq!(
        select_ultimate_headroom_v2(&legacy_quiet, false, None, None, 0.0, m_q).total_gain_db(),
        0.0
    );

    // 5. SourceTruePeak (E3/E4) applies A_tp, A_up, m_q, and DAC_MARGIN (Owner Decision #2 / K8):
    //    TP = -4.0 dBTP (<= -3 -> G_up = 0 dB), A_tp = 0.1, A_up = 0.15, G_c = +6.0 dB:
    //    M_hat = -4.0 + 0.1 + 0.15 + 6.0 = +2.25 dBFS -> S = -(2.25 + m_q + 1.0) = -(3.25 + m_q).
    let scan_tp = PeakEvidence::SourceTruePeak {
        tp_db: -4.0,
        meter_err_db: TP_METER_ERR_8X_64_DB,
        source: "scan-tp",
    };
    let d_scan = select_ultimate_headroom_v2(&scan_tp, false, Some(6.0), Some(-5.0), 0.15, m_q);
    assert_eq!(d_scan.mode, HeadroomMode::ZeroDb);
    assert!((d_scan.total_gain_db() - (-(2.25 + m_q + dm))).abs() < 1e-9);

    // Compare against sample-peak tag (-4.0 dBFS, A_isp = 3.0): M_hat = +5.15 -> S = -(5.15 + m_q + dm).
    let tag_sp = PeakEvidence::SourceSamplePeak {
        sp_db: -4.0,
        source: "tag-sp",
    };
    let d_sp = select_ultimate_headroom_v2(&tag_sp, false, Some(6.0), Some(-5.0), 0.15, m_q);
    assert_eq!(d_sp.mode, HeadroomMode::ZeroDb);
    assert!((d_sp.total_gain_db() - (-(5.15 + m_q + dm))).abs() < 1e-9);
    assert!(
        d_scan.total_gain_db() > d_sp.total_gain_db() + 2.5,
        "E3 scan-tp must save ~2.9 dB of unnecessary attenuation vs blanket A_isp=3.0"
    );

    // Borderline quiet TP = -3.0 dBTP with Vorbis tag-tp (A_tp=0.7), A_up=0.15, no REW:
    // M_hat = -3.0 + 0.7 + 0.15 = -2.15, + 1.0 DAC margin = -1.15 <= 0 -> G = 0.0 dB!
    let borderline_tp = PeakEvidence::SourceTruePeak {
        tp_db: -3.0,
        meter_err_db: TP_METER_ERR_TAG_TP_DB,
        source: "tag-tp",
    };
    let d_border =
        select_ultimate_headroom_v2(&borderline_tp, false, None, Some(-3.0), 0.15, m_q);
    assert_eq!(d_border.mode, HeadroomMode::ZeroDb);
    assert_eq!(d_border.total_gain_db(), 0.0);

    // 6. T10 + Review #10 HP-05: Streaming & Unknown ALWAYS enforce -3 dB fail-safe PLUS
    //    `m_q + ULTIMATE_DAC_MARGIN_DB` (even when M30 is quiet or absent!).
    let d_stream_quiet =
        select_ultimate_headroom_v2(&quiet_exact, true, Some(2.0), Some(-10.0), 0.0, m_q);
    assert_eq!(d_stream_quiet.mode, HeadroomMode::Minus3Db);
    assert!(
        (d_stream_quiet.total_gain_db() - (-(3.0 + 2.0 + m_q + dm))).abs() < 1e-9,
        "HP-05: streaming fail-safe must include m_q + DAC margin, got {}",
        d_stream_quiet.total_gain_db()
    );
    let d_unknown_no_m30 =
        select_ultimate_headroom_v2(&PeakEvidence::Unknown, false, None, None, 0.0, m_q);
    assert_eq!(d_unknown_no_m30.mode, HeadroomMode::Minus3Db);
    assert!(
        (d_unknown_no_m30.total_gain_db() - (-(3.0 + m_q + dm))).abs() < 1e-9,
        "HP-05: Unknown fail-safe without M30 must reserve DAC margin + m_q, got {}",
        d_unknown_no_m30.total_gain_db()
    );
}
