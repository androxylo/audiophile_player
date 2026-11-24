//! TDD unit tests for hardcoded `-3 dB` / `0 dB` ReplayGain & True Peak headroom selection.

use ap_core::headroom::{
    HeadroomMode, MINUS_3_DB_LINEAR_THRESHOLD, TrackPeakMetadata, select_headroom,
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
fn unknown_or_invalid_peak_always_defaults_to_minus_3_db() {
    // Completely untagged FLAC
    let d_empty = select_headroom(&TrackPeakMetadata::default(), false, 0.0);
    assert_eq!(d_empty.mode, HeadroomMode::Minus3Db);

    // Gain tag present without a peak tag does NOT bound instantaneous peak -> must stay -3 dB
    let gain_only = TrackPeakMetadata {
        replaygain_track_gain_db: Some(6.0),
        replaygain_track_peak: None,
        replaygain_album_gain_db: Some(5.0),
        replaygain_album_peak: None,
        true_peak_linear: None,
    };
    assert_eq!(
        select_headroom(&gain_only, false, 0.0).mode,
        HeadroomMode::Minus3Db
    );

    // Invalid/malformed peak values (0.0, negative, NaN, inf) -> fail-safe -3 dB
    for bad in [0.0, -0.5, f64::NAN, f64::INFINITY] {
        let meta = TrackPeakMetadata {
            replaygain_track_peak: Some(bad),
            ..TrackPeakMetadata::default()
        };
        assert_eq!(
            select_headroom(&meta, false, 0.0).mode,
            HeadroomMode::Minus3Db
        );
    }

    // Live streaming source (Tidal) has unknown peak -> always -3 dB even if metadata provided
    let quiet = TrackPeakMetadata {
        replaygain_track_peak: Some(0.5),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&quiet, true, 0.0).mode,
        HeadroomMode::Minus3Db
    );
}

#[test]
fn quiet_tracks_below_minus_3_dbfs_get_0_db_and_hot_tracks_get_minus_3_db() {
    // Quiet track (-6 dBFS, peak ~0.501) -> 0 dB (pleasant leveling advantage, no boost!)
    let quiet = TrackPeakMetadata {
        replaygain_track_peak: Some(0.501),
        ..TrackPeakMetadata::default()
    };
    let d_quiet = select_headroom(&quiet, false, 0.0);
    assert_eq!(d_quiet.mode, HeadroomMode::ZeroDb);
    assert!(d_quiet.reason.contains("0 dB"));

    // Boundary: exactly at -3.0 dBFS threshold -> 0 dB
    let exact = TrackPeakMetadata {
        replaygain_track_peak: Some(MINUS_3_DB_LINEAR_THRESHOLD),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(select_headroom(&exact, false, 0.0).mode, HeadroomMode::ZeroDb);

    // Slightly above -3.0 dBFS (-2.9 dBFS ~ 0.716) -> -3 dB
    let warm = TrackPeakMetadata {
        replaygain_track_peak: Some(0.716),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(select_headroom(&warm, false, 0.0).mode, HeadroomMode::Minus3Db);

    // Hot mastered / ISP track (1.05 linear) -> -3 dB
    let hot = TrackPeakMetadata {
        replaygain_track_peak: Some(1.05),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(select_headroom(&hot, false, 0.0).mode, HeadroomMode::Minus3Db);
}

#[test]
fn track_peak_takes_priority_over_album_peak_and_falls_back_cleanly() {
    // Quiet track (-5 dBFS ~ 0.562) on a loud album (0.99) -> track peak wins -> 0 dB
    let quiet_on_loud_album = TrackPeakMetadata {
        replaygain_track_peak: Some(0.562),
        replaygain_album_peak: Some(0.99),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&quiet_on_loud_album, false, 0.0).mode,
        HeadroomMode::ZeroDb
    );

    // Missing track peak falls back to album peak (which conservatively bounds every track)
    let quiet_album_only = TrackPeakMetadata {
        replaygain_album_peak: Some(0.60),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&quiet_album_only, false, 0.0).mode,
        HeadroomMode::ZeroDb
    );

    let loud_album_only = TrackPeakMetadata {
        replaygain_album_peak: Some(0.95),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&loud_album_only, false, 0.0).mode,
        HeadroomMode::Minus3Db
    );
}

#[test]
fn rew_convolution_boost_raises_required_native_headroom_margin() {
    // Track peak at -4.0 dBFS (~0.631):
    // - Without REW boost: qualifies for 0 dB (4.0 >= 3.0 dB margin).
    // - With +2.0 dB REW boost: needs >= 5.0 dB margin -> drops back to -3 dB!
    let meta = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-4.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(select_headroom(&meta, false, 0.0).mode, HeadroomMode::ZeroDb);
    assert_eq!(
        select_headroom(&meta, false, 2.0).mode,
        HeadroomMode::Minus3Db
    );

    // Very quiet track (-6.0 dBFS) still has >= 5.0 dB margin even with +2.0 dB REW boost -> 0 dB
    let very_quiet = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-6.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&very_quiet, false, 2.0).mode,
        HeadroomMode::ZeroDb
    );

    // Phase-dispersive IR with L1 bound = 1.5 (+3.52 dB) on a -4.58 dBFS (0.59) track ->
    // requires <= -6.52 dBFS for 0 dB, so must drop back to hardcoded -3 dB.
    let l1_margin_db = 20.0 * 1.5_f64.log10();
    let track_059 = TrackPeakMetadata {
        replaygain_track_peak: Some(0.59),
        ..TrackPeakMetadata::default()
    };
    assert_eq!(
        select_headroom(&track_059, false, l1_margin_db).mode,
        HeadroomMode::Minus3Db
    );
}
