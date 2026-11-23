//! TDD tests for bit-exact full-memory FLAC decoding in `ap-source`.

use std::path::PathBuf;

use ap_source::flac::{
    decode_flac_full_memory, decode_flac_full_memory_int32, parse_peak_metadata_fields,
};

fn nocturne_fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("audio")
        .join("original_nocturne_24_96.flac")
}

#[test]
fn decodes_original_nocturne_24_96_flac_bit_exact_to_int32_and_f64() {
    let path = nocturne_fixture_path();
    assert!(path.exists(), "missing fixture: {}", path.display());

    let raw = decode_flac_full_memory_int32(&path).expect("int32 FLAC decode failed");
    assert_eq!(raw.spec.sample_rate_hz, 96_000);
    assert_eq!(raw.spec.bits_per_sample, 24);
    assert_eq!(raw.spec.channels, 2);
    assert_eq!(raw.pcm.sample_rate_hz, 96_000);
    assert_eq!(raw.pcm.frames.len(), 96_000 * 14); // 1,344,000 frames exact

    // Bottom 8 bits must be strictly zero for MSB-aligned 24-bit samples
    for &[l, r] in raw.pcm.frames.iter().step_by(997) {
        assert_eq!(l & 0xFF, 0, "non-zero LSB padding in 24-bit MSB-aligned L sample");
        assert_eq!(r & 0xFF, 0, "non-zero LSB padding in 24-bit MSB-aligned R sample");
    }

    // Genuine stereo soundstage (L != R) and safe -12 dBFS peak
    assert!(raw.pcm.frames.iter().any(|&[l, r]| l != r));
    let max_abs = raw
        .pcm
        .frames
        .iter()
        .flat_map(|&[l, r]| [l.unsigned_abs(), r.unsigned_abs()])
        .max()
        .unwrap();
    let peak_dbfs = 20.0 * ((max_abs as f64) / (i32::MAX as f64)).log10();
    assert!(
        (-13.0..=-11.0).contains(&peak_dbfs),
        "unexpected fixture peak: {peak_dbfs:.2} dBFS"
    );

    // Pure f64 conversion must match MSB-aligned int32 with exact power-of-two scaling
    let f64_track = decode_flac_full_memory(&path).expect("f64 FLAC decode failed");
    assert_eq!(f64_track.audio.left.len(), raw.pcm.frames.len());
    let scale = (1_u64 << 31) as f64;
    for i in (0..raw.pcm.frames.len()).step_by(4093) {
        assert_eq!((f64_track.audio.left[i] * scale) as i32, raw.pcm.frames[i][0]);
        assert_eq!((f64_track.audio.right[i] * scale) as i32, raw.pcm.frames[i][1]);
    }
}

#[test]
fn parses_replaygain_and_true_peak_vorbis_comment_fields() {
    let fields = vec![
        "ARTIST=Test".to_string(),
        "REPLAYGAIN_TRACK_GAIN=-4.25 dB".to_string(),
        "REPLAYGAIN_TRACK_PEAK=0.612345".to_string(),
        "REPLAYGAIN_ALBUM_GAIN=-3.10 dB".to_string(),
        "REPLAYGAIN_ALBUM_PEAK=0.890000".to_string(),
    ];
    let meta = parse_peak_metadata_fields(&fields);
    assert_eq!(meta.replaygain_track_gain_db, Some(-4.25));
    assert_eq!(meta.replaygain_track_peak, Some(0.612345));
    assert_eq!(meta.replaygain_album_gain_db, Some(-3.10));
    assert_eq!(meta.replaygain_album_peak, Some(0.89));
}
