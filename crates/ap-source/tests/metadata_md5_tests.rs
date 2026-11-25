//! Phase 3 TDD tests: extended FLAC `TrackMetadata` (`VORBIS_COMMENT` + `CUESHEET` pre-emphasis
//! & `OQ-41` mixed flags) and exact RFC 9639 §8.2 streaming `FlacSampleMd5Hasher` verification
//! against `flac-codec`'s `STREAMINFO.md5` on mono/stereo 16-bit and 24-bit FLAC files.

use std::io::Write;
use std::path::PathBuf;

use ap_core::metadata::{GenreClass, PhaseTagOverride};
use ap_source::flac::{AudioBlockSource, FlacBlockReader};
use flac_codec::encode::{FlacSampleWriter, Options};
use flac_codec::metadata::{Cuesheet, VorbisComment, update};

fn temp_flac_path(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("ap_phase3_{tag}_{nanos}.flac"))
}

fn write_test_flac(
    path: &std::path::Path,
    sample_rate: u32,
    bits_per_sample: u32,
    channels: u8,
    samples: &[i32],
    comments: &[&str],
    cuesheet: Option<Cuesheet>,
) {
    let mut writer = FlacSampleWriter::create(
        path,
        Options::default(),
        sample_rate,
        bits_per_sample,
        channels,
        Some(samples.len() as u64),
    )
    .expect("create FLAC writer");
    writer.write(samples).expect("write FLAC samples");
    writer.finalize().expect("finalize FLAC");

    if !comments.is_empty() || cuesheet.is_some() {
        let owned_fields: Vec<String> = comments.iter().map(|s| (*s).to_string()).collect();
        update(path, move |blocks| {
            if !owned_fields.is_empty() {
                blocks.update::<VorbisComment>(|vc| {
                    vc.fields = owned_fields.clone();
                });
            }
            if let Some(cs) = cuesheet.clone() {
                blocks.insert(cs);
            }
            Ok::<(), std::io::Error>(())
        })
        .expect("update FLAC metadata blocks");
    }
}

#[test]
fn streaming_sample_md5_matches_flac_streaminfo_md5_for_stereo_16_24_and_mono() {
    let cases: &[(&str, u32, u32, u8)] = &[
        ("stereo16", 44_100, 16, 2),
        ("stereo24", 96_000, 24, 2),
        ("mono16", 44_100, 16, 1),
    ];

    for &(label, rate, bits, ch) in cases {
        let path = temp_flac_path(label);
        let frames = 2_345usize;
        let max_amp = ((1_i64 << (bits - 1)) - 1) as f64 * 0.4;
        let mut samples = Vec::with_capacity(frames * usize::from(ch));
        for n in 0..frames {
            let l = (max_amp * (2.0 * std::f64::consts::PI * 997.0 * (n as f64) / (rate as f64)).sin())
                .round() as i32;
            samples.push(l);
            if ch == 2 {
                let r = (max_amp
                    * (2.0 * std::f64::consts::PI * 2_203.0 * (n as f64) / (rate as f64)).cos())
                .round() as i32;
                samples.push(r);
            }
        }

        write_test_flac(&path, rate, bits, ch, &samples, &[], None);

        let mut reader = FlacBlockReader::open(&path).expect("open FLAC");
        let header_md5 = reader
            .streaminfo_md5()
            .expect("encoder must write STREAMINFO.md5");
        assert!(
            reader.computed_md5().is_none(),
            "computed_md5 must stay None before clean EOF"
        );

        let mut decoded_frames = 0usize;
        while let Some(blk) = reader.read_next_block(333).expect("read block") {
            decoded_frames += blk.len();
        }
        assert_eq!(decoded_frames, frames);

        let computed = reader
            .computed_md5()
            .expect("computed_md5 must be Some after clean EOF");
        assert_eq!(
            computed, header_md5,
            "RFC 9639 computed MD5 mismatch for {label}"
        );

        let _ = std::fs::remove_file(&path);
    }
}

#[test]
fn parses_cuesheet_pre_emphasis_mixed_flags_and_extended_vorbis_metadata() {
    let path = temp_flac_path("cuesheet_emph");
    // 2 seconds of 16/44.1 CD-DA stereo audio (multiple of 588 frames per CD sector).
    let frames = 588 * 150;
    let samples = vec![123_i32; frames * 2];

    let cue_text = "\
FILE \"disc.flac\" WAVE
  TRACK 01 AUDIO
    FLAGS PRE
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    INDEX 01 00:01:00
";
    let cuesheet = Cuesheet::parse(frames as u64, cue_text).expect("parse CUESHEET");

    write_test_flac(
        &path,
        44_100,
        16,
        2,
        &samples,
        &[
            "GENRE=Classical; Orchestral",
            "ORIGINALDATE=1984",
            "MEDIA=CD",
            "CONDUCTOR=Herbert von Karajan",
            "AP_PHASE=ip",
        ],
        Some(cuesheet),
    );

    let reader = FlacBlockReader::open(&path).expect("open FLAC with cuesheet");
    let meta = reader.track_meta();
    assert_eq!(meta.genre_class, GenreClass::Classical);
    assert_eq!(meta.original_year, Some(1984));
    assert!(meta.is_early_digital_era());
    assert!(meta.is_pre_emphasis_era_cd());
    assert!(meta.has_classical_credits);
    assert_eq!(meta.ap_phase, Some(PhaseTagOverride::Intermediate));
    assert!(meta.cuesheet_pre_emphasis, "track 1 FLAGS PRE must set cuesheet_pre_emphasis");
    assert!(meta.cuesheet_mixed_emphasis, "track 1 PRE + track 2 non-PRE must set mixed flag");
    assert_eq!(meta.cuesheet_audio_tracks, 2);
    assert!(meta.effective_deemphasis());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn corrupted_truncated_flac_never_emits_computed_md5() {
    let path = temp_flac_path("corrupt_md5");
    let samples: Vec<i32> = (0..16_384).map(|i| ((i * 7919) % 20_000) as i32).collect();
    write_test_flac(&path, 44_100, 16, 2, &samples, &[], None);

    // Corrupt trailing audio bytes so CRC/frame decode fails.
    let mut raw = std::fs::read(&path).expect("read raw");
    let len = raw.len();
    for b in &mut raw[(len - 64)..] {
        *b ^= 0xFF;
    }
    let mut f = std::fs::File::create(&path).expect("rewrite corrupt");
    f.write_all(&raw).expect("write corrupt");

    let mut reader = FlacBlockReader::open(&path).expect("open header");
    let mut saw_err = false;
    loop {
        match reader.read_next_block(1_024) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => {
                saw_err = true;
                break;
            }
        }
    }
    assert!(saw_err, "corrupted FLAC must return decode error");
    assert!(
        reader.computed_md5().is_none(),
        "corrupted FLAC must NEVER produce a verified computed_md5"
    );

    let _ = std::fs::remove_file(&path);
}
