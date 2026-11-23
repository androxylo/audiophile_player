use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomMode, TrackPeakMetadata};
use ap_dsp::convolution::{
    DEFAULT_REW_CONVOLUTION_DIR, PartitionedStereoConvolver, PreparedStereoIr,
    RewConvolutionConfig, RewConvolverFactory, StereoConvolver, StereoConvolverFactory,
    WavSampleEncoding, discover_latest_rew_wav, load_stereo_wav, probe_wav_header,
};
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};

fn naive_convolve(x: &[f64], h: &[f64]) -> Vec<f64> {
    let out_len = x.len() + h.len() - 1;
    let mut y = vec![0.0_f64; out_len];
    for (i, &xv) in x.iter().enumerate() {
        for (j, &hv) in h.iter().enumerate() {
            y[i + j] += xv * hv;
        }
    }
    y
}

fn write_wav_f32(path: &Path, sample_rate_hz: u32, channels: u16, samples: &[f32], add_junk: bool) {
    let bytes_per_sample = 4u16;
    let block_align = channels * bytes_per_sample;
    let byte_rate = sample_rate_hz * (block_align as u32);
    let data_bytes = (samples.len() * 4) as u32;
    let junk_chunk = if add_junk { 8 + 10 } else { 0 };
    let riff_size = 4 + junk_chunk + (8 + 16) + (8 + data_bytes);

    let mut f = File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&riff_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();

    if add_junk {
        f.write_all(b"JUNK").unwrap();
        f.write_all(&9u32.to_le_bytes()).unwrap(); // odd size tests RIFF 2-byte pad!
        f.write_all(&[0u8; 10]).unwrap();
    }

    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&3u16.to_le_bytes()).unwrap(); // WAVE_FORMAT_IEEE_FLOAT
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&sample_rate_hz.to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&32u16.to_le_bytes()).unwrap();

    f.write_all(b"data").unwrap();
    f.write_all(&data_bytes.to_le_bytes()).unwrap();
    for &s in samples {
        f.write_all(&s.to_le_bytes()).unwrap();
    }
}

fn write_wav_pcm24_extensible(path: &Path, sample_rate_hz: u32, stereo_i24: &[(i32, i32)]) {
    let channels = 2u16;
    let bytes_per_sample = 3u16;
    let block_align = channels * bytes_per_sample;
    let byte_rate = sample_rate_hz * (block_align as u32);
    let data_bytes = (stereo_i24.len() * 6) as u32;
    let fmt_size = 40u32;
    let riff_size = 4 + (8 + fmt_size) + (8 + data_bytes);

    let mut f = File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&riff_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();

    f.write_all(b"fmt ").unwrap();
    f.write_all(&fmt_size.to_le_bytes()).unwrap();
    f.write_all(&0xFFFEu16.to_le_bytes()).unwrap(); // WAVE_FORMAT_EXTENSIBLE
    f.write_all(&channels.to_le_bytes()).unwrap();
    f.write_all(&sample_rate_hz.to_le_bytes()).unwrap();
    f.write_all(&byte_rate.to_le_bytes()).unwrap();
    f.write_all(&block_align.to_le_bytes()).unwrap();
    f.write_all(&24u16.to_le_bytes()).unwrap();
    f.write_all(&22u16.to_le_bytes()).unwrap(); // cbSize
    f.write_all(&24u16.to_le_bytes()).unwrap(); // wValidBitsPerSample
    f.write_all(&3u32.to_le_bytes()).unwrap(); // dwChannelMask (L+R)
    // KSDATAFORMAT_SUBTYPE_PCM
    f.write_all(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ])
    .unwrap();

    f.write_all(b"data").unwrap();
    f.write_all(&data_bytes.to_le_bytes()).unwrap();
    for &(l, r) in stereo_i24 {
        let lb = l.to_le_bytes();
        let rb = r.to_le_bytes();
        f.write_all(&lb[0..3]).unwrap();
        f.write_all(&rb[0..3]).unwrap();
    }
}

#[test]
fn upols_matches_naive_time_domain_convolution_to_float64_precision_across_partitions() {
    // 3,777 taps with block_size = 4,096? Let's test 2,500 taps? Wait:
    // block_size = active_taps.next_power_of_two().clamp(1024, 65536).
    // To force multiple partitions (>= 2 partitions), use > 65,536? No, 1024..65536 clamps to 65536,
    // so let's also test both single-partition (exact fast path) and multi-partition (>65,536 taps)!
    let ir_len = 2_317usize;
    let mut h_l = vec![0.0_f64; ir_len];
    let mut h_r = vec![0.0_f64; ir_len];
    for i in 0..ir_len {
        let env = (-(i as f64) / 220.0).exp();
        h_l[i] = env * ((i as f64) * 0.13).cos() * 0.25;
        h_r[i] = env * ((i as f64) * 0.19 + 0.7).sin() * 0.22;
    }

    let sig_len = 4_913usize;
    let x_l: Vec<f64> = (0..sig_len)
        .map(|i| 0.4 * ((i as f64) * 0.07).sin() + 0.2 * ((i as f64) * 0.31).cos())
        .collect();
    let x_r: Vec<f64> = (0..sig_len)
        .map(|i| 0.35 * ((i as f64) * 0.11).cos() - 0.15 * ((i as f64) * 0.23).sin())
        .collect();

    let expected_l = naive_convolve(&x_l, &h_l);
    let expected_r = naive_convolve(&x_r, &h_r);

    let ir = Arc::new(PreparedStereoIr::from_stereo_ir("test-ir", 384_000, &h_l, &h_r).unwrap());
    let mut conv = PartitionedStereoConvolver::new(ir);

    let mut out_l = Vec::new();
    let mut out_r = Vec::new();
    let b = conv
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: x_l,
            right: x_r,
        })
        .unwrap();
    out_l.extend_from_slice(&b.left);
    out_r.extend_from_slice(&b.right);

    let tail = conv.flush().unwrap();
    out_l.extend_from_slice(&tail.left);
    out_r.extend_from_slice(&tail.right);

    assert_eq!(out_l.len(), expected_l.len());
    assert_eq!(out_r.len(), expected_r.len());

    let max_err_l = out_l
        .iter()
        .zip(expected_l.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    let max_err_r = out_r
        .iter()
        .zip(expected_r.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);

    assert!(max_err_l < 1e-12, "left UPOLS max err {max_err_l:e} too large");
    assert!(max_err_r < 1e-12, "right UPOLS max err {max_err_r:e} too large");
}

#[test]
fn upols_multi_partition_sparse_ir_over_65536_taps_places_echoes_at_exact_sample_indices() {
    // 140,000 taps forces 3 UPOLS partitions (B = 65,536 -> partitions 0, 1, 2).
    let ir_len = 140_000usize;
    let mut h_l = vec![0.0_f64; ir_len];
    let mut h_r = vec![0.0_f64; ir_len];
    h_l[10] = 0.5;
    h_l[70_000] = -0.25; // partition 1
    h_l[135_000] = 0.125; // partition 2

    h_r[25] = 0.75;
    h_r[131_072] = 0.25; // exact start of partition 2

    let ir = Arc::new(PreparedStereoIr::from_stereo_ir("multi-part", 384_000, &h_l, &h_r).unwrap());
    assert_eq!(ir.num_partitions, 3);
    assert_eq!(ir.block_size, 65_536);

    // Feed a unit impulse at sample 333 split across uneven chunks.
    let sig_len = 1_000usize;
    let mut x_l = vec![0.0_f64; sig_len];
    let mut x_r = vec![0.0_f64; sig_len];
    x_l[333] = 1.0;
    x_r[333] = 1.0;

    let mut conv = PartitionedStereoConvolver::new(ir);
    let mut out_l = Vec::new();
    let mut out_r = Vec::new();
    let mut pos = 0usize;
    for sz in [111usize, 289, 600] {
        let res = conv
            .process_block(StereoF64Buffer {
                sample_rate_hz: 384_000,
                left: x_l[pos..pos + sz].to_vec(),
                right: x_r[pos..pos + sz].to_vec(),
            })
            .unwrap();
        out_l.extend_from_slice(&res.left);
        out_r.extend_from_slice(&res.right);
        pos += sz;
    }
    let tail = conv.flush().unwrap();
    out_l.extend_from_slice(&tail.left);
    out_r.extend_from_slice(&tail.right);

    assert!((out_l[333 + 10] - 0.5).abs() < 1e-12);
    assert!((out_l[333 + 70_000] - (-0.25)).abs() < 1e-12);
    assert!((out_l[333 + 135_000] - 0.125).abs() < 1e-12);
    assert!((out_r[333 + 25] - 0.75).abs() < 1e-12);
    assert!((out_r[333 + 131_072] - 0.25).abs() < 1e-12);
}

#[test]
fn upols_streaming_arbitrary_chunks_is_bit_exact_with_oneshot() {
    let ir_len = 1_500usize;
    let h_l: Vec<f64> = (0..ir_len)
        .map(|i| (-(i as f64) / 150.0).exp() * ((i as f64) * 0.2).cos() * 0.3)
        .collect();
    let h_r: Vec<f64> = (0..ir_len)
        .map(|i| (-(i as f64) / 180.0).exp() * ((i as f64) * 0.17).sin() * 0.3)
        .collect();
    let ir = Arc::new(PreparedStereoIr::from_stereo_ir("chunk-test", 352_800, &h_l, &h_r).unwrap());

    let sig_len = 9_123usize;
    let x_l: Vec<f64> = (0..sig_len).map(|i| ((i as f64) * 0.05).sin() * 0.5).collect();
    let x_r: Vec<f64> = (0..sig_len).map(|i| ((i as f64) * 0.09).cos() * 0.5).collect();

    // One-shot
    let mut oneshot = PartitionedStereoConvolver::new(Arc::clone(&ir));
    let mut ref_l = Vec::new();
    let mut ref_r = Vec::new();
    let b = oneshot
        .process_block(StereoF64Buffer {
            sample_rate_hz: 352_800,
            left: x_l.clone(),
            right: x_r.clone(),
        })
        .unwrap();
    ref_l.extend_from_slice(&b.left);
    ref_r.extend_from_slice(&b.right);
    let t = oneshot.flush().unwrap();
    ref_l.extend_from_slice(&t.left);
    ref_r.extend_from_slice(&t.right);

    // Uneven streaming chunks
    for &chunk_size in &[17usize, 137, 1025, 4097] {
        let mut stream = PartitionedStereoConvolver::new(Arc::clone(&ir));
        let mut got_l = Vec::new();
        let mut got_r = Vec::new();
        let mut offset = 0usize;
        while offset < sig_len {
            let end = (offset + chunk_size).min(sig_len);
            let res = stream
                .process_block(StereoF64Buffer {
                    sample_rate_hz: 352_800,
                    left: x_l[offset..end].to_vec(),
                    right: x_r[offset..end].to_vec(),
                })
                .unwrap();
            got_l.extend_from_slice(&res.left);
            got_r.extend_from_slice(&res.right);
            offset = end;
        }
        let tail = stream.flush().unwrap();
        got_l.extend_from_slice(&tail.left);
        got_r.extend_from_slice(&tail.right);

        assert_eq!(got_l.len(), ref_l.len(), "len mismatch for chunk={chunk_size}");
        assert_eq!(got_l, ref_l, "left must be bit-exact for chunk={chunk_size}");
        assert_eq!(got_r, ref_r, "right must be bit-exact for chunk={chunk_size}");
    }
}

#[test]
fn wav_loader_handles_f32_junk_chunk_and_extensible_pcm24_and_rejects_mono() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_wav_loader_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let f32_path = tmp.join("stereo_f32_junk.wav");
    write_wav_f32(&f32_path, 384_000, 2, &[0.5, -0.25, 0.125, 0.75], true);
    let hdr = probe_wav_header(&f32_path).unwrap();
    assert_eq!(hdr.encoding, WavSampleEncoding::IeeeFloat);
    assert_eq!(hdr.channels, 2);
    assert_eq!(hdr.sample_rate_hz, 384_000);
    assert_eq!(hdr.num_frames, 2);

    let wav = load_stereo_wav(&f32_path).unwrap();
    assert_eq!(wav.left, vec![0.5, 0.125]);
    assert_eq!(wav.right, vec![-0.25, 0.75]);

    let pcm24_path = tmp.join("stereo_pcm24_ext.wav");
    write_wav_pcm24_extensible(&pcm24_path, 352_800, &[(4_194_304, -2_097_152)]);
    let wav24 = load_stereo_wav(&pcm24_path).unwrap();
    assert_eq!(wav24.sample_rate_hz, 352_800);
    assert!((wav24.left[0] - 0.5).abs() < 1e-12);
    assert!((wav24.right[0] - (-0.25)).abs() < 1e-12);

    let mono_path = tmp.join("mono_only.wav");
    write_wav_f32(&mono_path, 384_000, 1, &[1.0, 0.0], false);
    assert!(load_stereo_wav(&mono_path).is_err());

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn auto_discovery_picks_newest_rate_matched_stereo_wav_and_ignores_mono_helpers() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_rew_discover_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let old_384 = tmp.join("old_384.wav");
    write_wav_f32(&old_384, 384_000, 2, &[1.0, 1.0], false);
    thread::sleep(Duration::from_millis(35));

    let new_384 = tmp.join("new_384.wav");
    write_wav_f32(&new_384, 384_000, 2, &[0.5, 0.5], false);
    thread::sleep(Duration::from_millis(35));

    // Even newer mono helper file must be ignored!
    let newest_mono = tmp.join("newest_L_mono_384.wav");
    write_wav_f32(&newest_mono, 384_000, 1, &[1.0, 0.0], false);
    thread::sleep(Duration::from_millis(35));

    // Rate-matched 352.8 kHz stereo filter.
    let filter_352 = tmp.join("filter_352.wav");
    write_wav_f32(&filter_352, 352_800, 2, &[0.75, 0.75], false);

    let found_384 = discover_latest_rew_wav(&tmp, 384_000).unwrap().unwrap();
    assert_eq!(found_384.path, new_384);

    let found_352 = discover_latest_rew_wav(&tmp, 352_800).unwrap().unwrap();
    assert_eq!(found_352.path, filter_352);

    // Non-existent rate returns None -> factory falls back cleanly to NullConvolver.
    assert!(discover_latest_rew_wav(&tmp, 192_000).unwrap().is_none());

    let factory = RewConvolverFactory::new(RewConvolutionConfig::AutoDir(tmp.clone()));
    let c384 = factory.create_for_rate(384_000).unwrap();
    assert_eq!(c384.name(), "rew-fft-f64");
    assert!(c384.description().contains("new_384.wav"));

    let c192 = factory.create_for_rate(192_000).unwrap();
    assert_eq!(c192.name(), "null-convolver");

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn rew_boost_triggers_minus_3db_headroom_and_convolution_runs_before_noise_shaper() {
    // +2.0 dB impulse gain (10^(2/20) ~= 1.258925)
    let gain_lin = 10.0_f64.powf(2.0 / 20.0);
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_rew_headroom_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let wav_path = tmp.join("boost_2db_384k.wav");
    write_wav_f32(&wav_path, 384_000, 2, &[gain_lin as f32, gain_lin as f32], false);

    let pipeline = AudioPipeline::with_rew_config(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            shaper_override: Some(NoiseShaperProfile::Bypass),
            ..UpsamplerPolicy::default()
        },
        RewConvolutionConfig::ExplicitFile(wav_path),
    )
    .unwrap();

    let spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };
    // Quiet track at -4.0 dBFS: without boost it would get 0 dB, but +2 dB REW boost pushes
    // effective peak to -2.0 dBFS > -3.0 dBFS -> MUST engage -3 dB headroom!
    let peak = TrackPeakMetadata {
        replaygain_track_peak: Some(10.0_f64.powf(-4.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };

    let audio = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: vec![0.1; 1024],
        right: vec![-0.1; 1024],
    };

    let processed = pipeline.process(audio, spec, &peak, false).unwrap();
    assert_eq!(processed.report.convolver_name, "rew-fft-f64");
    assert_eq!(processed.report.headroom.mode, HeadroomMode::Minus3Db);
    // Because convolution modified the f64 stream BEFORE final quantization, shaper cannot bypass.
    assert_eq!(
        processed.report.effective_shaper,
        NoiseShaperProfile::TpdfDither
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn real_equalizer_apo_rew_filters_load_and_stream_4m_taps_if_present() {
    let dir = Path::new(DEFAULT_REW_CONVOLUTION_DIR);
    if !dir.is_dir() {
        return;
    }

    let Some(found_384) = discover_latest_rew_wav(dir, 384_000).unwrap() else {
        return;
    };
    let Some(found_352) = discover_latest_rew_wav(dir, 352_800).unwrap() else {
        return;
    };

    assert_eq!(found_384.header.channels, 2);
    assert_eq!(found_352.header.channels, 2);

    let t0 = Instant::now();
    let wav = load_stereo_wav(&found_384.path).unwrap();
    let ir = Arc::new(PreparedStereoIr::from_wav(&wav).unwrap());
    let prep_ms = t0.elapsed().as_secs_f64() * 1000.0;

    assert!(ir.max_boost_db < 1.0, "unexpected huge boost: {}", ir.max_boost_db);
    assert!(ir.flush_tail_samples < 384_000 * 3, "flush tail unexpectedly long");

    let mut conv = PartitionedStereoConvolver::new(ir);
    let t1 = Instant::now();
    let block = StereoF64Buffer {
        sample_rate_hz: 384_000,
        left: vec![0.05; 65_536],
        right: vec![-0.05; 65_536],
    };
    let out = conv.process_block(block).unwrap();
    let block_ms = t1.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(out.len(), 65_536);
    assert!(out.left.iter().all(|v| v.is_finite()));

    let tail = conv.flush().unwrap();
    assert!(!tail.is_empty());
    eprintln!(
        "[rew-4m-bench] '{}' ({} taps): prepare={:.1} ms, 65536-frame (170.7 ms audio) block={:.1} ms",
        found_384.path.display(),
        wav.left.len(),
        prep_ms,
        block_ms
    );
}
