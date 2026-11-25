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
        replaygain_track_peak: None,
        replaygain_album_gain_db: Some(5.0),
        replaygain_album_peak: None,
        true_peak_linear: None,
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
    apply_gapless_tail_headroom(&mut d_a, &TrackPeakMetadata::default(), 24, false, 2.0, 0.25, 0.0);
    assert_eq!(d_a.mode, HeadroomMode::Minus3Db);
    assert!((d_a.convolution_scale_db - (-4.4988)).abs() < 0.01);

    // (b) REPLAYGAIN_TRACK_PEAK=0.5 (sample, A_isp=3), G_c=0, T=0.4 -> G_up=0, S_B = -1.42 dB.
    let sp_half = TrackPeakMetadata {
        replaygain_track_peak: Some(0.5),
        ..TrackPeakMetadata::default()
    };
    let mut d_b = select_headroom_for_bits(&sp_half, 24, false, Some(0.0), None);
    apply_gapless_tail_headroom(&mut d_b, &sp_half, 24, false, 0.0, 0.4, 0.0);
    assert_eq!(d_b.mode, HeadroomMode::ZeroDb);
    assert!((d_b.convolution_scale_db - (-1.4164)).abs() < 0.01);

    // (c) True peak -3.5 dBTP, G_c=0, T=0.6 -> G_up stays 0 dB, S_B = -4.46 dB!
    let tp_35 = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(-3.5 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let mut d_c = select_headroom_for_bits(&tp_35, 24, false, Some(0.0), None);
    apply_gapless_tail_headroom(&mut d_c, &tp_35, 24, false, 0.0, 0.6, 0.0);
    assert_eq!(d_c.mode, HeadroomMode::ZeroDb);
    assert!((d_c.convolution_scale_db - (-4.4588)).abs() < 0.01);
}
