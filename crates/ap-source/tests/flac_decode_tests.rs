// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! TDD tests for bit-exact full-memory FLAC decoding in `ap-source`.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ap_source::flac::{
    AudioBlockSource, FlacBlockReader, MAX_PREALLOC_FRAMES, UNSUPPORTED_CHANNEL_LAYOUT_PREFIX,
    capped_prealloc_frames, decode_flac_full_memory, decode_flac_full_memory_int32,
    parse_peak_metadata_fields, validate_flac_stream_params,
};
use flac_codec::encode::{FlacSampleWriter, Options};

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
fn streaming_flac_block_reader_matches_full_decode_bit_for_bit() {
    let path = nocturne_fixture_path();
    let full = decode_flac_full_memory(&path).unwrap();

    // Stream in small uneven 17,777-frame chunks and verify exact sample-for-sample equality.
    let mut reader = FlacBlockReader::open(&path).unwrap();
    assert_eq!(reader.spec(), full.spec);
    assert!(!reader.is_streaming());
    assert_eq!(reader.total_frames(), Some(96_000 * 14));

    let mut streamed_l = Vec::new();
    let mut streamed_r = Vec::new();
    let mut blocks = 0usize;
    while let Some(block) = reader.read_next_block(17_777).unwrap() {
        assert!(block.len() <= 17_777);
        assert_eq!(block.sample_rate_hz, 96_000);
        streamed_l.extend_from_slice(&block.left);
        streamed_r.extend_from_slice(&block.right);
        blocks += 1;
    }
    assert!(blocks > 50, "expected many bounded chunks, got {blocks}");
    assert_eq!(streamed_l, full.audio.left);
    assert_eq!(streamed_r, full.audio.right);
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

fn make_temp_flac_path(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "ap_flac_test_{tag}_{}_{}.flac",
        std::process::id(),
        stamp
    ))
}

#[test]
fn decodes_mono_flac_into_dual_mono_stereo_streaming_and_full_memory() {
    let path = make_temp_flac_path("mono_24_48");
    let total_frames = 1_000usize;
    let samples: Vec<i32> = (0..total_frames)
        .map(|i| {
            // Deterministic 24-bit signed waveform in [-2^23, 2^23 - 1]
            let phase = (i as f64) * 0.07;
            (phase.sin() * 4_000_000.0).round() as i32
        })
        .collect();

    let mut enc =
        FlacSampleWriter::create(&path, Options::default(), 48_000, 24, 1, Some(total_frames as u64))
            .expect("create mono FLAC failed");
    enc.write(&samples).expect("write mono samples failed");
    enc.finalize().expect("finalize mono FLAC failed");

    // 1. Streaming reader duplicates mono samples identically into L and R stereo.
    let mut reader = FlacBlockReader::open(&path).expect("open mono FLAC failed");
    assert_eq!(reader.source_channels(), 1);
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate_hz, 48_000);
    assert_eq!(reader.spec().bits_per_sample, 24);
    assert_eq!(reader.total_frames(), Some(total_frames as u64));

    let mut streamed_l = Vec::new();
    let mut streamed_r = Vec::new();
    while let Some(block) = reader.read_next_block(137).unwrap() {
        assert_eq!(block.left, block.right, "mono stream must duplicate L == R");
        streamed_l.extend_from_slice(&block.left);
        streamed_r.extend_from_slice(&block.right);
    }
    assert_eq!(streamed_l.len(), total_frames);
    assert_eq!(streamed_l, streamed_r);

    // 2. Full-memory int32 & f64 decode match bit-for-bit.
    let raw = decode_flac_full_memory_int32(&path).expect("full int32 mono decode failed");
    assert_eq!(raw.spec.channels, 2);
    assert_eq!(raw.pcm.frames.len(), total_frames);
    for (i, &s) in samples.iter().enumerate() {
        let expected_msb = s << 8;
        assert_eq!(raw.pcm.frames[i], [expected_msb, expected_msb]);
    }

    let full_f64 = decode_flac_full_memory(&path).expect("full f64 mono decode failed");
    assert_eq!(full_f64.audio.left, streamed_l);
    assert_eq!(full_f64.audio.right, streamed_r);

    let _ = fs::remove_file(&path);
}

#[test]
fn rejects_multichannel_flac_with_distinct_unsupported_channel_layout_error() {
    let path = make_temp_flac_path("surround_51");
    let channels = 6u8;
    let frames = 256usize;
    let samples = vec![0_i32; frames * usize::from(channels)];

    let mut enc = FlacSampleWriter::create(
        &path,
        Options::default(),
        48_000,
        16,
        channels,
        Some(samples.len() as u64),
    )
    .expect("create 5.1 FLAC failed");
    enc.write(&samples).expect("write 5.1 samples failed");
    enc.finalize().expect("finalize 5.1 FLAC failed");

    let err_stream = FlacBlockReader::open(&path).err().expect("5.1 must be rejected");
    assert!(
        err_stream.starts_with(UNSUPPORTED_CHANNEL_LAYOUT_PREFIX),
        "expected distinct unsupported channel layout prefix, got: {err_stream}"
    );
    assert!(err_stream.contains("6 channels"), "got: {err_stream}");

    let err_int32 = decode_flac_full_memory_int32(&path).unwrap_err();
    assert!(err_int32.starts_with(UNSUPPORTED_CHANNEL_LAYOUT_PREFIX));

    let err_f64 = decode_flac_full_memory(&path).unwrap_err();
    assert!(err_f64.starts_with(UNSUPPORTED_CHANNEL_LAYOUT_PREFIX));

    let _ = fs::remove_file(&path);
}

#[test]
fn caps_untrusted_streaminfo_total_samples_preallocation() {
    assert_eq!(capped_prealloc_frames(None), 0);
    assert_eq!(capped_prealloc_frames(Some(44_100)), 44_100);
    assert_eq!(
        capped_prealloc_frames(Some((1_u64 << 36) - 1)),
        MAX_PREALLOC_FRAMES
    );
    assert_eq!(capped_prealloc_frames(Some(u64::MAX)), MAX_PREALLOC_FRAMES);

    // Patch a tiny valid FLAC's STREAMINFO.total_samples to 2^36 - 1 and verify full-memory
    // decoders cap upfront Vec::with_capacity instead of aborting on a 512 GiB allocation.
    let path = make_temp_flac_path("huge_streaminfo");
    let frames = 64usize;
    let samples: Vec<i32> = (0..frames * 2).map(|i| ((i as i32) * 17) - 500).collect();
    let mut enc = FlacSampleWriter::create(
        &path,
        Options::default(),
        44_100,
        16,
        2,
        Some(samples.len() as u64),
    )
    .unwrap();
    enc.write(&samples).unwrap();
    enc.finalize().unwrap();

    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[0..4], b"fLaC");
    assert_eq!(bytes[4] & 0x7F, 0, "first metadata block must be STREAMINFO");
    // Bytes 21 (low 4 bits) + 22..26 store the 36-bit total_samples field.
    bytes[21] |= 0x0F;
    bytes[22] = 0xFF;
    bytes[23] = 0xFF;
    bytes[24] = 0xFF;
    bytes[25] = 0xFF;
    fs::write(&path, &bytes).unwrap();

    // Before capping pre-allocation, both calls attempted a 512 GiB Vec::with_capacity and aborted
    // the process via handle_alloc_error. With capping, both stay bounded and return a clean Err
    // when the truncated stream hits EOF short of the forged total_samples.
    let err_i32 = decode_flac_full_memory_int32(&path)
        .expect_err("forged 2^36-1 total_samples should fail cleanly at EOF without OOM abort");
    assert!(
        err_i32.contains("FLAC decode error"),
        "unexpected error: {err_i32}"
    );

    let err_f64 = decode_flac_full_memory(&path)
        .expect_err("forged 2^36-1 total_samples should fail cleanly at EOF without OOM abort");
    assert!(
        err_f64.contains("FLAC stream decode error"),
        "unexpected error: {err_f64}"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn parses_true_peak_tags_in_linear_and_dbtp_forms_separately_from_sample_peak() {
    // Mathematically consistent pair: sample peak 0.65 (-3.74 dBFS), true peak -3.5 dBTP (0.6683).
    let fields_db = vec![
        "REPLAYGAIN_TRACK_PEAK=0.65".to_string(),
        "TRUE_PEAK=-3.5 dBTP".to_string(),
    ];
    let m1 = parse_peak_metadata_fields(&fields_db);
    assert_eq!(m1.replaygain_track_peak, Some(0.65));
    assert!(m1.has_verified_true_peak());
    let expected_tp = 10.0_f64.powf(-3.5 / 20.0);
    assert!((m1.true_peak_linear.unwrap() - expected_tp).abs() < 1e-12);
    let (eff, tag) = m1.effective_peak_linear().unwrap();
    assert_eq!(tag, "TRUE_PEAK");
    assert!((eff - expected_tp).abs() < 1e-12);

    // Explicit positive dBTP (+0.3 dBTP) converts cleanly to > 1.0 linear (hot master).
    let m_hot = parse_peak_metadata_fields(&["TRUE_PEAK=+0.3 dBTP".to_string()]);
    assert!((m_hot.true_peak_linear.unwrap() - 10.0_f64.powf(0.3 / 20.0)).abs() < 1e-12);

    // Review #3 Finding #7: ambiguous bare positive TRUE_PEAK ("0.3" or "+0.3") MUST NOT be
    // parsed as quiet linear amplitude 0.3 (-10.46 dBFS)!
    let m_ambig_1 = parse_peak_metadata_fields(&["TRUE_PEAK=0.3".to_string()]);
    assert_eq!(m_ambig_1.true_peak_linear, None);
    assert!(!m_ambig_1.has_verified_true_peak());
    let m_ambig_2 = parse_peak_metadata_fields(&["TRUE_PEAK=+0.3".to_string()]);
    assert_eq!(m_ambig_2.true_peak_linear, None);

    // Unambiguous bare negative TRUE_PEAK in [-120, 0] is accepted as dBTP; Q7.8-like -768 is rejected.
    let m_neg = parse_peak_metadata_fields(&["TRUE_PEAK=-4.2".to_string()]);
    assert!((m_neg.true_peak_linear.unwrap() - 10.0_f64.powf(-4.2 / 20.0)).abs() < 1e-12);
    let m_q78_tp = parse_peak_metadata_fields(&["TRUE_PEAK=-768".to_string()]);
    assert_eq!(m_q78_tp.true_peak_linear, None);

    // Review #4 Follow-up flac.rs:267: bare positive REPLAYGAIN_TRACK_TRUE_PEAK without a
    // companion REPLAYGAIN_TRACK_PEAK tag (e.g. "0.5" meant as +0.5 dBTP) MUST be rejected!
    let m2_ambig = parse_peak_metadata_fields(&["REPLAYGAIN_TRACK_TRUE_PEAK=0.5".to_string()]);
    assert_eq!(m2_ambig.true_peak_linear, None);
    assert!(!m2_ambig.has_verified_true_peak());

    // When accompanied by a confirming REPLAYGAIN_TRACK_PEAK (standard loudgain/foobar2000 pair),
    // bare positive linear REPLAYGAIN_TRACK_TRUE_PEAK is accepted.
    let m2 = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_PEAK=0.600000".to_string(),
        "REPLAYGAIN_TRACK_TRUE_PEAK=0.625000".to_string(),
    ]);
    assert_eq!(m2.true_peak_linear, Some(0.625));
    assert!(m2.has_verified_true_peak());
    let m2_plus = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_PEAK=0.600000".to_string(),
        "REPLAYGAIN_TRACK_TRUE_PEAK=+0.3".to_string(),
    ]);
    assert_eq!(m2_plus.true_peak_linear, None);

    // Explicit dBTP or bare negative dBTP on REPLAYGAIN_TRACK_TRUE_PEAK is unambiguous even alone.
    let m2_db = parse_peak_metadata_fields(&["REPLAYGAIN_TRACK_TRUE_PEAK=-4.0 dBTP".to_string()]);
    assert!((m2_db.true_peak_linear.unwrap() - 10.0_f64.powf(-4.0 / 20.0)).abs() < 1e-12);
    let m2_neg = parse_peak_metadata_fields(&["REPLAYGAIN_TRACK_TRUE_PEAK=-4.0".to_string()]);
    assert!((m2_neg.true_peak_linear.unwrap() - 10.0_f64.powf(-4.0 / 20.0)).abs() < 1e-12);

    // True peak > +6 dBTP counts as unknown (cannot unlock 0 dB or drop ISP allowance).
    let m_over_6 = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_PEAK=0.5".to_string(),
        "TRUE_PEAK=+7.5 dBTP".to_string(),
    ]);
    assert_eq!(m_over_6.classified_peak(), None);
    assert!(!m_over_6.has_verified_true_peak());

    // R128_TRACK_TRUE_PEAK requires explicit dB/dBTP suffix so bare Q7.8 integer '-768' is rejected.
    let m3_bad = parse_peak_metadata_fields(&["R128_TRACK_TRUE_PEAK=-768".to_string()]);
    assert_eq!(m3_bad.true_peak_linear, None);
    let m3_bare = parse_peak_metadata_fields(&["R128_TRACK_TRUE_PEAK=-6.0".to_string()]);
    assert_eq!(m3_bare.true_peak_linear, None);
    let m3_ok = parse_peak_metadata_fields(&["R128_TRACK_TRUE_PEAK=-6.0 dBTP".to_string()]);
    assert!((m3_ok.true_peak_linear.unwrap() - 10.0_f64.powf(-6.0 / 20.0)).abs() < 1e-12);

    // Cross-check against REPLAYGAIN_TRACK_PEAK: mathematically impossible TP < sample_peak is discarded!
    let m_impossible = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_PEAK=0.95".to_string(),
        "TRUE_PEAK=-6.0 dBTP".to_string(), // 0.501 < 0.95!
    ]);
    assert_eq!(m_impossible.true_peak_linear, None);
    assert!(!m_impossible.has_verified_true_peak());
    assert_eq!(
        m_impossible.effective_peak_linear(),
        Some((0.95, "REPLAYGAIN_TRACK_PEAK"))
    );
}

#[test]
fn read_next_block_preserves_valid_prefix_before_mid_block_error() {
    let path = make_temp_flac_path("partial_block_prefix");
    let frames = 128_usize;
    let samples: Vec<i32> = (0..frames * 2).map(|i| ((i as i32) * 31) - 1000).collect();
    let mut enc = FlacSampleWriter::create(
        &path,
        Options::default(),
        44_100,
        16,
        2,
        Some(samples.len() as u64),
    )
    .unwrap();
    enc.write(&samples).unwrap();
    enc.finalize().unwrap();

    // Patch STREAMINFO total_samples upward so flac-codec hits early-EOF error after decoding the
    // 128 valid frames inside a 65,536-frame read request.
    let mut bytes = fs::read(&path).unwrap();
    bytes[24] = 0x10;
    bytes[25] = 0x00;
    fs::write(&path, &bytes).unwrap();

    let mut reader = FlacBlockReader::open(&path).unwrap();
    // First call must return the 128 valid decoded frames instead of discarding them!
    let first = reader
        .read_next_block(65_536)
        .expect("valid prefix before mid-block error must not be discarded")
        .expect("expected partial block with 128 frames");
    assert_eq!(first.len(), frames);
    // Second call surfaces the deferred decode error cleanly.
    let err = reader
        .read_next_block(65_536)
        .expect_err("deferred error must surface on subsequent read_next_block");
    assert!(err.contains("FLAC stream decode error"), "unexpected err: {err}");
    // Subsequent call after error returns clean EOF (Ok(None)).
    assert!(reader.read_next_block(65_536).unwrap().is_none());

    let _ = fs::remove_file(&path);
}

#[test]
fn rejects_zero_hz_streaminfo_flac_cleanly() {
    // Review #3 Finding #24: directly test the open_validated guard so deleting the
    // sample_rate_hz == 0 check cannot hide behind flac-codec's parser error.
    let err_direct = validate_flac_stream_params(2, 0, 16, std::path::Path::new("zero.flac"))
        .expect_err("validate_flac_stream_params must explicitly reject 0 Hz");
    assert!(
        err_direct.contains("invalid FLAC sample rate 0 Hz"),
        "expected explicit 0 Hz error, got: {err_direct}"
    );

    let path = make_temp_flac_path("zero_hz_streaminfo");
    let samples = vec![0_i32; 64 * 2];
    let mut enc = FlacSampleWriter::create(
        &path,
        Options::default(),
        44_100,
        16,
        2,
        Some(samples.len() as u64),
    )
    .unwrap();
    enc.write(&samples).unwrap();
    enc.finalize().unwrap();

    // STREAMINFO bytes 18..20 + top 4 bits of byte 20 store the 20-bit sample rate.
    let mut bytes = fs::read(&path).unwrap();
    bytes[18] = 0;
    bytes[19] = 0;
    bytes[20] &= 0x0F;
    fs::write(&path, &bytes).unwrap();

    let err = FlacBlockReader::open(&path)
        .err()
        .expect("0 Hz FLAC STREAMINFO must be rejected cleanly");
    assert!(
        err.contains("sample rate") || err.contains("failed to parse FLAC"),
        "unexpected error for 0 Hz FLAC: {err}"
    );

    let _ = fs::remove_file(&path);
}

#[test]
fn review5_bare_positive_replaygain_true_peak_does_not_overwrite_unambiguous_true_peak() {
    // Review #5 Finding #24: An earlier valid unambiguous `TRUE_PEAK` or `R128_TRACK_TRUE_PEAK`
    // MUST NOT be erased by a later bare-positive `REPLAYGAIN_TRACK_TRUE_PEAK` candidate that
    // fails the companion sample-peak cross-check!
    let fields = vec![
        "TRUE_PEAK=-4.0 dB".to_string(),
        "REPLAYGAIN_TRACK_TRUE_PEAK=0.5".to_string(),
    ];
    let meta = parse_peak_metadata_fields(&fields);
    let expected_lin = 10.0_f64.powf(-4.0 / 20.0);
    let got = meta
        .true_peak_linear
        .expect("unambiguous TRUE_PEAK must be preserved");
    assert!(
        (got - expected_lin).abs() < 1e-6,
        "expected {expected_lin:.6}, got {got:.6}"
    );

    // Same for R128_ALBUM_TRUE_PEAK followed by bare-positive REPLAYGAIN_ALBUM_TRUE_PEAK.
    let album_fields = vec![
        "R128_ALBUM_TRUE_PEAK=-5.0 dBTP".to_string(),
        "REPLAYGAIN_ALBUM_TRUE_PEAK=0.8".to_string(),
    ];
    let album_meta = parse_peak_metadata_fields(&album_fields);
    let expected_album = 10.0_f64.powf(-5.0 / 20.0);
    let got_album = album_meta
        .true_peak_linear
        .expect("unambiguous R128_ALBUM_TRUE_PEAK must be preserved");
    assert!(
        (got_album - expected_album).abs() < 1e-6,
        "expected {expected_album:.6}, got {got_album:.6}"
    );
}

#[test]
fn headroom_design_issue7_tag_bounds_track_vs_album_and_streaminfo_md5() {
    // 1. Tightened §2 upper bound: (0, 2.0] linear / <= +6.0206 dBTP accepted; > 2.0 rejected to
    // unknown without falling back to a quiet sample-peak tag!
    let at_limit_db = parse_peak_metadata_fields(&["TRUE_PEAK=+6.02 dBTP".to_string()]);
    assert!(at_limit_db.true_peak_linear.is_some());
    assert!(!at_limit_db.true_peak_invalid);

    let at_limit_lin = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_PEAK=0.99".to_string(),
        "REPLAYGAIN_TRACK_TRUE_PEAK=2.0".to_string(),
    ]);
    assert_eq!(at_limit_lin.true_peak_linear, Some(2.0));
    assert!(!at_limit_lin.true_peak_invalid);

    let over_limit_lin = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_PEAK=0.5".to_string(),
        "REPLAYGAIN_TRACK_TRUE_PEAK=2.5".to_string(),
    ]);
    assert!(over_limit_lin.true_peak_invalid);
    assert_eq!(over_limit_lin.true_peak_linear, None);
    assert_eq!(over_limit_lin.classified_peak(), None);

    let over_limit_db = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_PEAK=0.5".to_string(),
        "TRUE_PEAK=+30.0 dBTP".to_string(),
    ]);
    assert!(over_limit_db.true_peak_invalid);
    assert_eq!(over_limit_db.classified_peak(), None);

    // 2. Track true peak > album true peak (beyond 0.5% rounding) -> contradictory -> unknown.
    let bad_tp_pair = parse_peak_metadata_fields(&[
        "REPLAYGAIN_TRACK_TRUE_PEAK=-2.0 dBTP".to_string(),
        "REPLAYGAIN_ALBUM_TRUE_PEAK=-5.0 dBTP".to_string(),
    ]);
    assert!(bad_tp_pair.true_peak_invalid);
    assert_eq!(bad_tp_pair.classified_peak(), None);

    // 3. Album true peak < album sample peak -> discarded cleanly.
    let bad_album_tp = parse_peak_metadata_fields(&[
        "REPLAYGAIN_ALBUM_PEAK=0.95".to_string(),
        "REPLAYGAIN_ALBUM_TRUE_PEAK=-6.0 dBTP".to_string(),
    ]);
    assert_eq!(bad_album_tp.true_peak_linear, None);
    assert_eq!(
        bad_album_tp.effective_peak_linear(),
        Some((0.95, "REPLAYGAIN_ALBUM_PEAK"))
    );

    // 4. FlacBlockReader exposes STREAMINFO MD5 signature for YAML identity verification.
    let path = make_temp_flac_path("streaminfo_md5");
    let samples: Vec<i32> = (0..256).map(|i| (i * 123) % 10_000).collect();
    let mut enc = FlacSampleWriter::create(
        &path,
        Options::default(),
        44_100,
        16,
        2,
        Some(samples.len() as u64),
    )
    .unwrap();
    enc.write(&samples).unwrap();
    enc.finalize().unwrap();

    let reader = FlacBlockReader::open(&path).unwrap();
    let md5 = reader
        .streaminfo_md5()
        .expect("encoded FLAC must expose non-empty STREAMINFO MD5");
    assert_ne!(md5, [0u8; 16]);

    let _ = fs::remove_file(&path);
}

#[test]
fn decodes_zero_residual_first_partition_rfc9639_frame() {
    // Regression test for flac-codec 1.3.3 decode::read_residuals rchunks_mut bug:
    // RFC 9639 §9.2.5.1 allows block_size / 2^partition_order == predictor_order, where
    // partition 0 has 0 residuals (e.g. block_size=16, partition_order=2 -> 4 samples/partition,
    // Fixed predictor order=4 -> partition 0 has 4 - 4 = 0 residuals, partitions 1..3 have 4).
    use flac_codec::decode::FlacSampleReader;
    use flac_codec::stream::{
        Frame, FrameIterator, ResidualPartition, Residuals, Subframe, SubframeWidth,
    };

    // 1. Generate a valid 16-frame 16-bit stereo FLAC to obtain a real STREAMINFO header and
    //    valid FrameHeader matching its exact stream parameters.
    let seed_path = make_temp_flac_path("zero_part0_seed");
    let seed_samples: Vec<i32> = (0..16 * 2).map(|i| 100 + (i as i32)).collect();
    let mut enc = FlacSampleWriter::create(
        &seed_path,
        Options::default(),
        44_100,
        16,
        2,
        Some(seed_samples.len() as u64),
    )
    .unwrap();
    enc.write(&seed_samples).unwrap();
    enc.finalize().unwrap();

    let mut fi = FrameIterator::open(&seed_path).unwrap();
    let metadata_bytes = fs::read(&seed_path).unwrap()[..fi.metadata_len() as usize].to_vec();
    let streaminfo = fi.metadata().streaminfo().clone();
    let (mut frame, _) = fi.next().unwrap().unwrap();
    drop(fi);
    let _ = fs::remove_file(&seed_path);

    // 2. Replace the frame's subframes with Fixed order=4, 4 partitions (partition_order=2,
    //    block_size=16 => partition_size=4 == predictor_order => partition 0 has 0 residuals).
    //    For Fixed order=4 with warm_up=[100, 100, 100, 100] and all-zero residuals, every
    //    decoded sample equals 100 (left) and -200 (right).
    let make_subframe = |val: i32| -> SubframeWidth {
        SubframeWidth::Common(Subframe::Fixed {
            order: 4,
            warm_up: vec![val; 4],
            residuals: Residuals::Method0 {
                partitions: vec![
                    ResidualPartition::Constant { partition_len: 0 },
                    ResidualPartition::Constant { partition_len: 4 },
                    ResidualPartition::Constant { partition_len: 4 },
                    ResidualPartition::Constant { partition_len: 4 },
                ],
            },
            wasted_bps: 0,
        })
    };
    frame.header.channel_assignment = flac_codec::stream::ChannelAssignment::LeftSide;
    let out_path = make_temp_flac_path("zero_part0_valid");
    let mut raw = metadata_bytes;
    let frame_fixed = Frame {
        header: frame.header,
        // LeftSide: left=100, side=300 => right = left - side = -200.
        subframes: vec![make_subframe(100), make_subframe(300)],
    };
    frame_fixed.write(&streaminfo, &mut raw).unwrap();
    fs::write(&out_path, &raw).unwrap();

    // 3. Confirm upstream flac-codec 1.3.3 FlacSampleReader rejects this valid RFC 9639 frame.
    let mut upstream = FlacSampleReader::new(std::io::BufReader::new(
        fs::File::open(&out_path).unwrap(),
    ))
    .unwrap();
    let mut tmp = vec![0i32; 32];
    let upstream_err = upstream
        .read(&mut tmp)
        .expect_err("expected flac-codec 1.3.3 FlacSampleReader to fail with InvalidPartitionOrder");
    assert!(
        upstream_err.to_string().contains("invalid residual partition order"),
        "unexpected upstream error: {upstream_err}"
    );

    // 4. Confirm our FlacBlockReader decodes all 16 frames cleanly without error!
    let decoded = decode_flac_full_memory_int32(&out_path)
        .expect("SafeFlacSampleReader must decode 0-residual first partition cleanly");
    assert_eq!(decoded.pcm.frames.len(), 16);
    for pair in &decoded.pcm.frames {
        assert_eq!(pair[0] >> 16, 100);
        assert_eq!(pair[1] >> 16, -200);
    }

    let _ = fs::remove_file(&out_path);
}
