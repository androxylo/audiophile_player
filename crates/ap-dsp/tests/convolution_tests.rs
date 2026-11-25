use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ap_core::buffer::StereoF64Buffer;
use ap_core::config::ConvolutionConfig;
use ap_core::format::StreamSpec;
use ap_core::headroom::{HeadroomMode, TrackPeakMetadata};
use ap_dsp::convolution::{
    MAX_IR_TAPS, PartitionedStereoConvolver, PreparedStereoIr, RewConvolverFactory, StereoConvolver,
    StereoConvolverFactory, WavSampleEncoding, discover_latest_rew_wav, load_stereo_wav,
    probe_wav_header,
};
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};
use realfft::RealFftPlanner;

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

/// Reference exact linear convolution via a single zero-padded `f64` real FFT.
fn reference_fft_convolve(x: &[f64], h: &[f64]) -> Vec<f64> {
    let out_len = x.len() + h.len() - 1;
    let n = out_len.next_power_of_two();
    let mut planner = RealFftPlanner::<f64>::new();
    let r2c = planner.plan_fft_forward(n);
    let c2r = planner.plan_fft_inverse(n);

    let mut bx = vec![0.0_f64; n];
    bx[..x.len()].copy_from_slice(x);
    let mut bh = vec![0.0_f64; n];
    bh[..h.len()].copy_from_slice(h);

    let mut sx = r2c.make_output_vec();
    let mut sh = r2c.make_output_vec();
    let mut scratch_f = r2c.make_scratch_vec();
    r2c.process_with_scratch(&mut bx, &mut sx, &mut scratch_f).unwrap();
    r2c.process_with_scratch(&mut bh, &mut sh, &mut scratch_f).unwrap();

    let inv_n = 1.0 / (n as f64);
    for (cx, ch) in sx.iter_mut().zip(sh.iter()) {
        let re = cx.re * ch.re - cx.im * ch.im;
        let im = cx.re * ch.im + cx.im * ch.re;
        cx.re = re * inv_n;
        cx.im = im * inv_n;
    }
    sx[0].im = 0.0;
    sx[n / 2].im = 0.0;

    let mut out = vec![0.0_f64; n];
    let mut scratch_i = c2r.make_scratch_vec();
    c2r.process_with_scratch(&mut sx, &mut out, &mut scratch_i).unwrap();
    out.truncate(out_len);
    out
}

#[test]
fn upols_single_partition_matches_naive_time_domain_convolution_to_float64_precision() {
    // 2,317 taps -> single UPOLS partition (B = 4,096) verified sample-by-sample against O(L*M)
    // direct time-domain convolution.
    let ir_len = 2_317usize;
    let mut h_l = vec![0.0_f64; ir_len];
    let mut h_r = vec![0.0_f64; ir_len];
    for i in 0..ir_len {
        let env = (-(i as f64) / 220.0).exp();
        h_l[i] = env * ((i as f64) * 0.13).cos() * 0.005;
        h_r[i] = env * ((i as f64) * 0.19 + 0.7).sin() * 0.005;
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
    assert_eq!(ir.max_boost_db, 0.0, "test IR should stay <= 0 dB");
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
fn upols_dense_multi_partition_ir_over_65536_taps_matches_reference_fft_convolution() {
    // 140,000-tap DENSE stereo IR -> 3 UPOLS partitions (B = 65,536) with all FDL slots active
    // across a 200,000-sample multi-block non-zero input streamed in uneven chunks.
    let ir_len = 140_000usize;
    let mut h_l = vec![0.0_f64; ir_len];
    let mut h_r = vec![0.0_f64; ir_len];
    for i in 0..ir_len {
        let env = (-(i as f64) / 35_000.0).exp();
        // Keep gain strictly <= 0 dB while staying well above LONG_IR_FLUSH_THRESHOLD (1e-10)
        // all the way to tap 139,999 so every single tail sample is compared.
        h_l[i] = 1e-5 * env * (((i as f64) * 0.017).cos() + 0.5 * ((i as f64) * 0.113).sin());
        h_r[i] = 1e-5 * env * (((i as f64) * 0.029 + 0.4).sin() - 0.4 * ((i as f64) * 0.071).cos());
    }
    h_l[ir_len - 1] = 2e-6;
    h_r[ir_len - 1] = -2e-6;

    let ir = Arc::new(PreparedStereoIr::from_stereo_ir("dense-140k", 384_000, &h_l, &h_r).unwrap());
    assert_eq!(ir.block_size, 65_536);
    assert_eq!(ir.num_partitions, 3);
    assert_eq!(ir.max_boost_db, 0.0);
    assert_eq!(ir.flush_tail_samples, ir_len - 1);

    let sig_len = 200_000usize; // > 3 full UPOLS blocks
    let x_l: Vec<f64> = (0..sig_len)
        .map(|i| 0.45 * ((i as f64) * 0.013).sin() + 0.25 * ((i as f64) * 0.191).cos())
        .collect();
    let x_r: Vec<f64> = (0..sig_len)
        .map(|i| 0.40 * ((i as f64) * 0.031).cos() - 0.20 * ((i as f64) * 0.089).sin())
        .collect();

    let expected_l = reference_fft_convolve(&x_l, &h_l);
    let expected_r = reference_fft_convolve(&x_r, &h_r);

    let mut conv = PartitionedStereoConvolver::new(ir);
    let mut got_l = Vec::with_capacity(expected_l.len());
    let mut got_r = Vec::with_capacity(expected_r.len());

    let chunk_pattern = [7_777usize, 19_111, 65_537, 3_333, 50_001, 12_345];
    let mut offset = 0usize;
    let mut c_idx = 0usize;
    while offset < sig_len {
        let sz = chunk_pattern[c_idx % chunk_pattern.len()].min(sig_len - offset);
        let res = conv
            .process_block(StereoF64Buffer {
                sample_rate_hz: 384_000,
                left: x_l[offset..offset + sz].to_vec(),
                right: x_r[offset..offset + sz].to_vec(),
            })
            .unwrap();
        got_l.extend_from_slice(&res.left);
        got_r.extend_from_slice(&res.right);
        offset += sz;
        c_idx += 1;
    }
    let tail = conv.flush().unwrap();
    got_l.extend_from_slice(&tail.left);
    got_r.extend_from_slice(&tail.right);

    assert_eq!(got_l.len(), expected_l.len());
    assert_eq!(got_r.len(), expected_r.len());

    let max_err_l = got_l
        .iter()
        .zip(expected_l.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    let max_err_r = got_r
        .iter()
        .zip(expected_r.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);

    assert!(
        max_err_l < 1e-11,
        "dense multi-partition left max err {max_err_l:e} too large"
    );
    assert!(
        max_err_r < 1e-11,
        "dense multi-partition right max err {max_err_r:e} too large"
    );
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

    let factory = RewConvolverFactory::new(ConvolutionConfig::auto_dir(tmp.clone()));
    let c384 = factory.create_for_rate(384_000).unwrap();
    assert_eq!(c384.name(), "rew-fft-f64");
    assert!(c384.description().contains("new_384.wav"));

    let c192 = factory.create_for_rate(192_000).unwrap();
    assert_eq!(c192.name(), "null-convolver");

    // Per-rate explicit file overrides `dir` for that specific rate while `dir` handles the other.
    let mut per_rate_cfg = ConvolutionConfig::auto_dir(tmp.clone());
    per_rate_cfg.file_384000 = Some(old_384.clone());
    let per_rate_factory = RewConvolverFactory::new(per_rate_cfg);
    assert!(
        per_rate_factory
            .create_for_rate(384_000)
            .unwrap()
            .description()
            .contains("old_384.wav")
    );
    assert!(
        per_rate_factory
            .create_for_rate(352_800)
            .unwrap()
            .description()
            .contains("filter_352.wav")
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn rew_boost_is_unnormalized_at_load_scaled_only_by_excess_and_runs_before_noise_shaper() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_rew_headroom_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };

    // Case A (Addendum 1 canonical example): +1.0 dB boosting IR on a -6.0 dBTP track.
    // G_up = 0 dB (P = -6 <= -3), A_isp = 0 dB, predicted = -6 + 0 + 0 + 1 = -5 dBFS <= 0 dBFS.
    // Nothing is cut: IR is never normalized at load, conv_scale = 0 dB, full +1 dB boost passes!
    let boost_1db_lin = 10.0_f64.powf(1.0 / 20.0);
    let boost_1db_path = tmp.join("boost_1db_384k.wav");
    write_wav_f32(
        &boost_1db_path,
        384_000,
        2,
        &[boost_1db_lin as f32, boost_1db_lin as f32],
        false,
    );

    let pipeline_a = AudioPipeline::with_rew_config(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            shaper_override: Some(NoiseShaperProfile::Bypass),
            ..UpsamplerPolicy::default()
        },
        ConvolutionConfig::explicit_file(boost_1db_path),
    )
    .unwrap();
    let peak_minus_6_tp = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(-6.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let amp = 0.5_f64;
    let processed_a = pipeline_a
        .process(
            StereoF64Buffer {
                sample_rate_hz: 384_000,
                left: vec![amp; 512],
                right: vec![amp; 512],
            },
            spec,
            &peak_minus_6_tp,
            false,
        )
        .unwrap();
    assert_eq!(processed_a.report.convolver_name, "rew-fft-f64");
    assert_eq!(processed_a.report.headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(processed_a.report.headroom.convolution_scale_db, 0.0);
    // Because convolution modified the f64 stream BEFORE final quantization, shaper cannot bypass.
    assert_eq!(
        processed_a.report.effective_shaper,
        NoiseShaperProfile::TpdfDither
    );
    let got_a_l = (processed_a.pcm.frames[256][0] as f64) / 2_147_483_648.0;
    let expected_a = amp * boost_1db_lin;
    assert!(
        (got_a_l - expected_a).abs() < 1e-4,
        "+1 dB boost on -6 dBTP track must pass unattenuated: got {got_a_l}, expected {expected_a}"
    );

    // Case B: +6.02 dB boosting IR (left = 2.0, right = 1.0 -> 2:1 L/R balance).
    // Loaded unnormalized (`max_boost_db ~= +6.02 dB`), warning logged once on first load.
    let boost_6db_path = tmp.join("boost_6db_384k.wav");
    write_wav_f32(&boost_6db_path, 384_000, 2, &[2.0, 1.0], false);

    let factory_b = RewConvolverFactory::new(ConvolutionConfig::explicit_file(boost_6db_path.clone()));
    let conv_b = factory_b.create_for_rate(384_000).unwrap();
    assert!(
        (conv_b.max_boost_db() - 6.0206).abs() < 0.02,
        "expected unnormalized max_boost_db ~+6.02 dB, got {:.4}",
        conv_b.max_boost_db()
    );
    assert_eq!(
        factory_b.warnings().len(),
        1,
        "first load of boosting IR must emit exactly one diagnostic warning"
    );
    assert!(factory_b.warnings()[0].contains("WARNING"));
    // Second creation uses cached IR and must not spam duplicate notices.
    let _ = factory_b.create_for_rate(384_000).unwrap();
    assert_eq!(factory_b.warnings().len(), 1);

    // On untagged (unknown peak: P=0, A_isp=3, G_up=-3, G_c=+6.02 -> excess=+6.02 dB):
    // convolution output scales by -6.02 dB (0.5x), preserving 2:1 L/R balance and staying below FS!
    let pipeline_b = AudioPipeline::with_rew_config(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            shaper_override: Some(NoiseShaperProfile::Bypass),
            ..UpsamplerPolicy::default()
        },
        ConvolutionConfig::explicit_file(boost_6db_path),
    )
    .unwrap();
    let processed_b = pipeline_b
        .process(
            StereoF64Buffer {
                sample_rate_hz: 384_000,
                left: vec![0.99; 1024],
                right: vec![0.99; 1024],
            },
            spec,
            &TrackPeakMetadata::default(), // untagged -> G_up=-3 dB, conv excess=-6.02 dB
            false,
        )
        .unwrap();
    assert_eq!(processed_b.report.headroom.mode, HeadroomMode::Minus3Db);
    assert!((processed_b.report.headroom.convolution_scale_db - (-6.0206)).abs() < 0.02);

    let expected_l = 0.99 * 1.0 * HeadroomMode::Minus3Db.linear();
    let expected_r = 0.99 * 0.5 * HeadroomMode::Minus3Db.linear();
    let got_l = (processed_b.pcm.frames[512][0] as f64) / 2_147_483_648.0;
    let got_r = (processed_b.pcm.frames[512][1] as f64) / 2_147_483_648.0;
    assert!(
        (got_l - expected_l).abs() < 1e-4,
        "+6 dB excess-scaled left mismatch: got {got_l}, expected {expected_l}"
    );
    assert!(
        (got_r - expected_r).abs() < 1e-4,
        "+6 dB excess-scaled right (2:1 balance) mismatch: got {got_r}, expected {expected_r}"
    );
    assert!(
        processed_b
            .pcm
            .frames
            .iter()
            .all(|f| f[0] < i32::MAX && f[0] > i32::MIN && f[1] < i32::MAX && f[1] > i32::MIN),
        "+6 dB IR must not clip after excess-only convolution scaling"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn single_file_rate_mismatch_falls_through_to_dir_and_then_null_with_warning() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_rew_fallback_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let single_384 = tmp.join("single_384.wav");
    write_wav_f32(&single_384, 384_000, 2, &[0.8, 0.8], false);

    let dir_352 = tmp.join("dir_filters");
    fs::create_dir_all(&dir_352).unwrap();
    let wav_352 = dir_352.join("room_352.wav");
    write_wav_f32(&wav_352, 352_800, 2, &[0.7, 0.7], false);

    let cfg = ConvolutionConfig {
        enabled: true,
        dir: Some(dir_352.clone()),
        file: Some(single_384.clone()),
        file_352800: None,
        file_384000: None,
    };
    let factory = RewConvolverFactory::new(cfg);

    // 384 kHz matches `file` directly.
    let c384 = factory.create_for_rate(384_000).unwrap();
    assert_eq!(c384.name(), "rew-fft-f64");
    assert!(c384.description().contains("single_384.wav"));

    // 352.8 kHz does NOT match `file` -> logs notice and falls through to `dir`!
    let c352 = factory.create_for_rate(352_800).unwrap();
    assert_eq!(c352.name(), "rew-fft-f64");
    assert!(c352.description().contains("room_352.wav"));

    // 192 kHz matches neither `file` nor `dir` -> falls back to NullConvolver with warning!
    let c192 = factory.create_for_rate(192_000).unwrap();
    assert_eq!(c192.name(), "null-convolver");

    let warnings = factory.warnings();
    assert!(
        warnings.iter().any(|w| w.contains("falling through")),
        "expected rate-mismatch fallthrough notice, got: {warnings:?}"
    );
    assert!(
        warnings.iter().any(|w| w.contains("no 2-channel 192000 Hz WAV")),
        "expected missing-rate warning, got: {warnings:?}"
    );

    // Missing directory also warns cleanly.
    let missing_factory = RewConvolverFactory::new(ConvolutionConfig::auto_dir(
        tmp.join("nonexistent_dir"),
    ));
    let c_missing = missing_factory.create_for_rate(384_000).unwrap();
    assert_eq!(c_missing.name(), "null-convolver");
    assert!(
        missing_factory
            .warnings()
            .iter()
            .any(|w| w.contains("does not exist"))
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn rejects_truncated_wav_data_chunk_and_all_zero_silent_ir() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_rew_validation_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // 1. Valid older WAV + newer truncated WAV in the same folder.
    let valid_path = tmp.join("valid_384.wav");
    write_wav_f32(&valid_path, 384_000, 2, &[0.5, 0.5, 0.25, 0.25], false);
    thread::sleep(Duration::from_millis(35));

    let trunc_path = tmp.join("truncated_384.wav");
    {
        let mut f = File::create(&trunc_path).unwrap();
        // Header claims 64 bytes of data, but we only write 16 bytes (truncated/in-progress).
        f.write_all(b"RIFF").unwrap();
        f.write_all(&100u32.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap(); // IEEE float
        f.write_all(&2u16.to_le_bytes()).unwrap(); // 2 ch
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&64u32.to_le_bytes()).unwrap();
        f.write_all(&[0u8; 16]).unwrap(); // only 16 of 64 bytes present!
    }

    let err = probe_wav_header(&trunc_path).unwrap_err();
    assert!(
        err.contains("truncated WAV"),
        "expected truncated WAV error, got: {err}"
    );
    assert!(load_stereo_wav(&trunc_path).is_err());

    // Discovery must ignore the truncated file and pick the older complete WAV.
    let discovered = discover_latest_rew_wav(&tmp, 384_000).unwrap().unwrap();
    assert_eq!(discovered.path, valid_path);

    // 2. All-zero (silent) IR must be rejected cleanly.
    let silent = vec![0.0_f64; 1024];
    let err = PreparedStereoIr::from_stereo_ir("silent.wav", 384_000, &silent, &silent).unwrap_err();
    assert!(
        err.contains("all zeros"),
        "expected all-zeros error, got: {err}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn gapless_tail_carryover_matches_continuous_convolution_across_three_tracks() {
    let ir_len = 1_500usize;
    let h_l: Vec<f64> = (0..ir_len)
        .map(|i| (-(i as f64) / 200.0).exp() * ((i as f64) * 0.17).cos() * 0.005)
        .collect();
    let h_r: Vec<f64> = (0..ir_len)
        .map(|i| (-(i as f64) / 240.0).exp() * ((i as f64) * 0.23).sin() * 0.005)
        .collect();
    let ir = Arc::new(PreparedStereoIr::from_stereo_ir("gapless-ir", 384_000, &h_l, &h_r).unwrap());
    assert_eq!(ir.max_boost_db, 0.0);

    // Track 1 (5000 frames), Track 2 (500 frames — SHORTER than IR tail!), Track 3 (4000 frames).
    let x1_l: Vec<f64> = (0..5_000).map(|i| 0.4 * ((i as f64) * 0.05).sin()).collect();
    let x1_r: Vec<f64> = (0..5_000).map(|i| 0.3 * ((i as f64) * 0.07).cos()).collect();
    let x2_l: Vec<f64> = (0..500).map(|i| 0.5 * ((i as f64) * 0.11).cos()).collect();
    let x2_r: Vec<f64> = (0..500).map(|i| -0.4 * ((i as f64) * 0.13).sin()).collect();
    let x3_l: Vec<f64> = (0..4_000).map(|i| 0.35 * ((i as f64) * 0.03).sin()).collect();
    let x3_r: Vec<f64> = (0..4_000).map(|i| 0.45 * ((i as f64) * 0.09).cos()).collect();

    let mut full_x_l = Vec::new();
    full_x_l.extend_from_slice(&x1_l);
    full_x_l.extend_from_slice(&x2_l);
    full_x_l.extend_from_slice(&x3_l);
    let mut full_x_r = Vec::new();
    full_x_r.extend_from_slice(&x1_r);
    full_x_r.extend_from_slice(&x2_r);
    full_x_r.extend_from_slice(&x3_r);

    let expected_l = naive_convolve(&full_x_l, &h_l);
    let expected_r = naive_convolve(&full_x_r, &h_r);

    let mut got_l = Vec::new();
    let mut got_r = Vec::new();

    // Track 1 -> split tail
    let mut c1 = PartitionedStereoConvolver::new(Arc::clone(&ir));
    let b1 = c1
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: x1_l,
            right: x1_r,
        })
        .unwrap();
    let (end1, tail1) = c1.flush_split_tail().unwrap();
    assert_eq!(b1.len() + end1.len(), 5_000, "track 1 must emit exact input length");
    got_l.extend_from_slice(&b1.left);
    got_l.extend_from_slice(&end1.left);
    got_r.extend_from_slice(&b1.right);
    got_r.extend_from_slice(&end1.right);

    // Track 2 (shorter than tail1!) -> receives tail1, splits combined tail2
    let mut c2 = PartitionedStereoConvolver::new(Arc::clone(&ir));
    c2.set_incoming_tail(tail1.expect("track 1 must yield tail"));
    let b2 = c2
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: x2_l,
            right: x2_r,
        })
        .unwrap();
    let (end2, tail2) = c2.flush_split_tail().unwrap();
    assert_eq!(b2.len() + end2.len(), 500, "track 2 must emit exact input length");
    got_l.extend_from_slice(&b2.left);
    got_l.extend_from_slice(&end2.left);
    got_r.extend_from_slice(&b2.right);
    got_r.extend_from_slice(&end2.right);

    // Track 3 (last track) -> receives combined tail2, full flush
    let mut c3 = PartitionedStereoConvolver::new(Arc::clone(&ir));
    c3.set_incoming_tail(tail2.expect("track 2 must yield combined tail"));
    let b3 = c3
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: x3_l,
            right: x3_r,
        })
        .unwrap();
    let end3 = c3.flush().unwrap();
    got_l.extend_from_slice(&b3.left);
    got_l.extend_from_slice(&end3.left);
    got_r.extend_from_slice(&b3.right);
    got_r.extend_from_slice(&end3.right);

    assert_eq!(got_l.len(), expected_l.len());
    assert_eq!(got_r.len(), expected_r.len());

    let max_err_l = got_l
        .iter()
        .zip(expected_l.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    let max_err_r = got_r
        .iter()
        .zip(expected_r.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0_f64, f64::max);
    assert!(max_err_l < 1e-12, "gapless left max err {max_err_l:e}");
    assert!(max_err_r < 1e-12, "gapless right max err {max_err_r:e}");
}

#[test]
fn synthetic_4m_tap_ir_prepares_and_streams_cleanly() {
    // Always-running 2^22 (4,194,304) tap stereo IR covering full 64-partition UPOLS,
    // >= 2x zero-padded FFT gain check, and -200 dB flush tail trimming.
    const TAPS: usize = 1 << 22;
    let mut left = vec![0.0_f64; TAPS];
    let mut right = vec![0.0_f64; TAPS];
    left[0] = 0.5;
    left[100_000] = -0.125;
    left[400_000] = 0.0625;
    left[TAPS - 1] = 1e-12; // below LONG_IR_FLUSH_THRESHOLD (1e-10) -> active during playback, trimmed on flush

    right[10] = 0.6;
    right[300_000] = 0.1;
    right[TAPS - 1] = -1e-12;

    let ir = Arc::new(PreparedStereoIr::from_stereo_ir("synthetic-4m", 384_000, &left, &right).unwrap());
    assert_eq!(ir.total_taps, TAPS);
    assert_eq!(ir.active_taps, TAPS);
    assert_eq!(ir.block_size, 65_536);
    assert_eq!(ir.num_partitions, 64);
    assert_eq!(ir.max_boost_db, 0.0);
    assert_eq!(ir.flush_tail_samples, 400_000);

    let mut conv = PartitionedStereoConvolver::new(ir);
    let mut in_l = vec![0.0_f64; 65_536];
    let mut in_r = vec![0.0_f64; 65_536];
    in_l[100] = 1.0;
    in_r[100] = 1.0;

    let b0 = conv
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: in_l,
            right: in_r,
        })
        .unwrap();
    assert_eq!(b0.len(), 65_536);
    assert!((b0.left[100] - 0.5).abs() < 1e-12);
    assert!((b0.right[110] - 0.6).abs() < 1e-12);

    let tail = conv.flush().unwrap();
    assert_eq!(tail.len(), 400_000);
    let mut full_l = b0.left;
    full_l.extend_from_slice(&tail.left);
    let mut full_r = b0.right;
    full_r.extend_from_slice(&tail.right);
    assert!((full_l[100 + 100_000] - (-0.125)).abs() < 1e-12);
    assert!((full_l[100 + 400_000] - 0.0625).abs() < 1e-12);
    assert!((full_r[100 + 300_000] - 0.1).abs() < 1e-12);
    assert!(full_l.iter().all(|v| v.is_finite()));
    assert!(full_r.iter().all(|v| v.is_finite()));
}

#[test]
fn rejects_oversized_ir_wav_and_handles_zero_sized_or_reordered_data_chunks_safely() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_wav_limits_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // 1. Oversized WAV (> MAX_IR_TAPS frames): must be rejected in probe_wav_header BEFORE any allocation!
    let huge_path = tmp.join("huge_384k.wav");
    {
        let huge_frames: u64 = (MAX_IR_TAPS as u64) + 16;
        let data_bytes: u32 = (huge_frames * 4) as u32; // 16-bit stereo = 4 B/frame (~33.5 MiB)
        let mut f = File::create(&huge_path).unwrap();
        let riff_size = 36_u32 + data_bytes;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&2u16.to_le_bytes()).unwrap(); // stereo
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 4).to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_bytes.to_le_bytes()).unwrap();
        f.set_len(44 + u64::from(data_bytes)).unwrap();
    }
    let huge_err = probe_wav_header(&huge_path)
        .err()
        .expect("oversized WAV must be rejected before allocation");
    assert!(
        huge_err.contains("exceeds maximum supported"),
        "unexpected oversized WAV error: {huge_err}"
    );
    // Discovery must skip the oversized WAV rather than failing or OOMing.
    assert!(discover_latest_rew_wav(&tmp, 384_000).unwrap().is_none());
    let _ = fs::remove_file(&huge_path);

    // 2. Genuinely empty data chunk (size == 0) followed by a 200-byte LIST metadata chunk:
    //    must report num_frames == 0 (NEVER parse trailing LIST bytes as IR audio taps!).
    let empty_with_list = tmp.join("empty_list_384k.wav");
    {
        let mut f = File::create(&empty_with_list).unwrap();
        let list_payload = vec![0x42u8; 200];
        let riff_size: u32 = 4 + (8 + 16) + 8 + (8 + 200);
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap(); // IEEE float
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap(); // 0 bytes!
        f.write_all(b"LIST").unwrap();
        f.write_all(&200u32.to_le_bytes()).unwrap();
        f.write_all(&list_payload).unwrap();
    }
    let hdr_empty = probe_wav_header(&empty_with_list).unwrap();
    assert_eq!(hdr_empty.num_frames, 0, "empty data chunk must NOT absorb trailing LIST chunk");
    assert!(load_stereo_wav(&empty_with_list).is_err());

    // 3. Empty data chunk (size == 0) BEFORE fmt chunk: must advance cleanly to fmt without
    //    misinterpreting audio bytes as chunk headers.
    let data_before_fmt = tmp.join("data_before_fmt_384k.wav");
    {
        let mut f = File::create(&data_before_fmt).unwrap();
        let riff_size: u32 = 4 + 8 + (8 + 16);
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
    }
    let hdr_reordered = probe_wav_header(&data_before_fmt).unwrap();
    assert_eq!(hdr_reordered.num_frames, 0);
    assert_eq!(hdr_reordered.sample_rate_hz, 384_000);

    // 4. Streaming 0xFFFFFFFF data placeholder is accepted ONLY when RIFF size is also 0xFFFFFFFF.
    let bad_stream = tmp.join("bad_placeholder.wav");
    {
        let mut f = File::create(&bad_stream).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&100u32.to_le_bytes()).unwrap(); // NOT 0xFFFFFFFF
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        f.write_all(&0.5f32.to_le_bytes()).unwrap();
        f.write_all(&0.5f32.to_le_bytes()).unwrap();
    }
    assert!(probe_wav_header(&bad_stream).is_err());

    let ok_stream = tmp.join("ok_placeholder.wav");
    {
        let mut f = File::create(&ok_stream).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&u32::MAX.to_le_bytes()).unwrap();
        f.write_all(&0.5f32.to_le_bytes()).unwrap();
        f.write_all(&0.25f32.to_le_bytes()).unwrap();
    }
    let hdr_ok = probe_wav_header(&ok_stream).unwrap();
    assert_eq!(hdr_ok.num_frames, 1);

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn gapless_tail_is_preserved_at_end_of_queue_and_across_rate_change() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_tail_drain_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Build two stereo IRs (352.8 kHz and 384 kHz) with main tap at 0 and an echo at tap 500
    // (total 501 taps -> flush_tail_samples == 500).
    let ir_taps = 501_usize;
    let mut ir_samples = vec![0.0_f32; ir_taps * 2];
    ir_samples[0] = 0.5;
    ir_samples[1] = 0.5;
    ir_samples[500 * 2] = 0.25;
    ir_samples[500 * 2 + 1] = -0.25;

    let ir_352 = tmp.join("ir_352800.wav");
    let ir_384 = tmp.join("ir_384000.wav");
    write_wav_f32(&ir_352, 352_800, 2, &ir_samples, false);
    write_wav_f32(&ir_384, 384_000, 2, &ir_samples, false);

    let pipeline = AudioPipeline::with_rew_config(
        UpsamplerPolicy::default(),
        ConvolutionConfig::auto_dir(tmp.clone()),
    )
    .unwrap();

    let spec_44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let spec_48 = StreamSpec {
        sample_rate_hz: 48_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let meta = TrackPeakMetadata::default();
    let n_a = 400_usize;
    let n_b = 400_usize;

    // Track A (44.1 kHz -> 352.8 kHz, 8x): ends with flush_for_gapless holding upsampler + 500-tap tail.
    let mut sess_a = pipeline.start_track(spec_44, &meta, false).unwrap();
    let mut frames_a = sess_a
        .process_block(StereoF64Buffer {
            sample_rate_hz: 44_100,
            left: vec![0.5; n_a],
            right: vec![0.5; n_a],
        })
        .unwrap()
        .frames;
    let (end_a, carry_a) = sess_a.flush_for_gapless().unwrap();
    frames_a.extend(end_a.frames);
    let carry_a = carry_a.expect("Track A must produce gapless carryover");

    // Track B (48 kHz -> 384 kHz) rejects the 352.8 kHz carryover; draining it restores Track A's
    // exact N_A * 8 + 500 frames (zero truncation across the 352.8k -> 384k rate change!).
    let mut sess_b = pipeline.start_track(spec_48, &meta, false).unwrap();
    let rejected_a = sess_b
        .set_incoming_gapless_carryover(carry_a)
        .expect_err("384 kHz session must reject 352.8 kHz carryover");
    let drained_a = pipeline.drain_gapless_carryover(rejected_a).unwrap();
    assert_eq!(drained_a.sample_rate_hz, 352_800);
    frames_a.extend(drained_a.frames);
    assert_eq!(
        frames_a.len(),
        n_a * 8 + (ir_taps - 1),
        "Track A total frames across rate change must equal N*8 + flush_tail"
    );
    // Verify the delayed echo tail near the end of Track A is non-zero (not truncated!).
    let tail_energy_a: i64 = frames_a[(n_a * 8 + 100)..(n_a * 8 + 400)]
        .iter()
        .map(|f| i64::from(f[0]).abs())
        .sum();
    assert!(tail_energy_a > 0, "Track A REW tail energy was lost");

    // Track B ends at end-of-queue: draining its carryover restores exact N_B * 8 + 500 frames!
    let mut frames_b = sess_b
        .process_block(StereoF64Buffer {
            sample_rate_hz: 48_000,
            left: vec![0.5; n_b],
            right: vec![0.5; n_b],
        })
        .unwrap()
        .frames;
    let (end_b, carry_b) = sess_b.flush_for_gapless().unwrap();
    frames_b.extend(end_b.frames);
    let drained_b = pipeline
        .drain_gapless_carryover(carry_b.expect("Track B must produce gapless carryover"))
        .unwrap();
    assert_eq!(drained_b.sample_rate_hz, 384_000);
    frames_b.extend(drained_b.frames);
    assert_eq!(
        frames_b.len(),
        n_b * 8 + (ir_taps - 1),
        "Track B total frames at end-of-queue must equal N*8 + flush_tail"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn all_pass_ir_treats_l1_as_diagnostic_only_under_decided_headroom_policy() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_allpass_l1_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Truncated first-order all-pass IR with a = 0.5:
    //   h[0] = -0.5, h[n] = 0.75 * 0.5^(n-1) for n >= 1.
    // Exact |H(e^{jw})| = 1.0 (0 dB), and diagnostic L1 = sum|h| = 2.0 (+6.02 dB).
    let n_taps = 64_usize;
    let mut h = vec![0.0_f64; n_taps];
    h[0] = -0.5;
    for k in 1..n_taps {
        h[k] = 0.75 * 0.5_f64.powi((k - 1) as i32);
    }
    let mut wav_samples = Vec::with_capacity(n_taps * 2);
    for &v in &h {
        wav_samples.push(v as f32);
        wav_samples.push(v as f32);
    }
    let ap_path = tmp.join("allpass_384k.wav");
    write_wav_f32(&ap_path, 384_000, 2, &wav_samples, false);

    // Per HEADROOM_DESIGN.md: L1 is strictly diagnostic (logged as WARNING once on load) and
    // never used as a gain criterion (`max_boost_db == 0.0`, `peak_gain_bound_db == 0.0`).
    let factory = RewConvolverFactory::new(ConvolutionConfig::explicit_file(ap_path.clone()));
    let conv = factory.create_for_rate(384_000).unwrap();
    assert!(
        conv.max_boost_db() < 0.01,
        "all-pass IR must have ~0 dB max |H(f)|, got {:.4} dB",
        conv.max_boost_db()
    );
    assert!(
        factory
            .warnings()
            .iter()
            .any(|w| w.contains("WARNING") && w.contains("diagnostic L1")),
        "expected diagnostic L1 WARNING in factory.warnings(): {:?}",
        factory.warnings()
    );

    let pipeline = AudioPipeline::with_rew_config(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        },
        ConvolutionConfig::explicit_file(ap_path),
    )
    .unwrap();
    let spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };

    // -5 dBTP track qualifies for 0 dB upsampler gain AND 0 dB conv scale (L1 does not cut it).
    let mid_meta = TrackPeakMetadata {
        true_peak_linear: Some(10.0_f64.powf(-5.0 / 20.0)),
        ..TrackPeakMetadata::default()
    };
    let dc_amp = 0.25_f64;
    let processed_mid = pipeline
        .process(
            StereoF64Buffer {
                sample_rate_hz: 384_000,
                left: vec![dc_amp; 512],
                right: vec![dc_amp; 512],
            },
            spec,
            &mid_meta,
            false,
        )
        .unwrap();
    assert_eq!(processed_mid.report.headroom.mode, HeadroomMode::ZeroDb);
    assert_eq!(processed_mid.report.headroom.convolution_scale_db, 0.0);
    let steady_l = (processed_mid.pcm.frames[256][0] as f64) / 2_147_483_648.0;
    assert!(
        (steady_l - dc_amp).abs() < 1e-4,
        "0 dB mode with 0 dB-peak all-pass IR must preserve unity steady-state gain (~{dc_amp}), got {steady_l:.5}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn discover_latest_rew_wav_warns_when_newer_wav_fails_probe_and_enforces_4m_cap() {
    // Review #3 Finding #13: MAX_IR_TAPS matches REQUIREMENTS.md §5.3 (2^22 = 4,194,304).
    assert_eq!(ap_dsp::convolution::MAX_IR_TAPS, 1 << 22);
    assert_eq!(ap_dsp::convolution::MAX_IR_DATA_BYTES, (1u64 << 22) * 16);

    // Review #3 Finding #14: if a newer .wav fails header probing (e.g. over-cap or truncated),
    // RewConvolverFactory must warn naming the skipped file instead of silently using an older filter.
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_discover_warn_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let older_ok = tmp.join("a_older_384k.wav");
    write_wav_f32(&older_ok, 384_000, 2, &[1.0, 1.0, 0.0, 0.0], false);

    // Ensure distinct newer mtime (or tie-breaking filename 'z_newer_overcap.wav').
    std::thread::sleep(std::time::Duration::from_millis(30));
    let newer_bad = tmp.join("z_newer_overcap.wav");
    {
        // Valid RIFF/fmt header claiming (MAX_IR_TAPS + 1) stereo f32 frames -> exceeds 4M cap.
        let mut f = File::create(&newer_bad).unwrap();
        let data_bytes: u32 = ((ap_dsp::convolution::MAX_IR_TAPS + 1) * 8) as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36u32.saturating_add(data_bytes)).to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap(); // IEEE float
        f.write_all(&2u16.to_le_bytes()).unwrap(); // stereo
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_bytes.to_le_bytes()).unwrap();
    }

    let factory = RewConvolverFactory::new(ConvolutionConfig::auto_dir(tmp.clone()));
    let conv = factory.create_for_rate(384_000).unwrap();
    assert!(
        conv.description().contains("a_older_384k.wav"),
        "expected fallback to older valid filter, got {}",
        conv.description()
    );
    let warnings = factory.warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("z_newer_overcap.wav") && w.contains("skipped")),
        "expected warning naming skipped newer WAV 'z_newer_overcap.wav', got: {warnings:?}"
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
fn discovery_warns_on_zero_frame_and_falls_back_when_newest_wav_fails_prepare() {
    // Review #4 Findings #10 & #11:
    // 1. A newest stereo rate-matched WAV with 0 frames (unfinalized export) MUST push a
    //    SkippedRewWav warning instead of silently falling back.
    // 2. A newest stereo rate-matched WAV that passes header probing but fails prepare (e.g.
    //    all-zero payload) MUST warn and fall back cleanly to the next-newest valid filter
    //    instead of failing every track at that rate.
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_discover_fallback_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let older_ok = tmp.join("a_good_384k.wav");
    write_wav_f32(&older_ok, 384_000, 2, &[0.8, 0.8, 0.0, 0.0], false);

    std::thread::sleep(std::time::Duration::from_millis(25));
    let newer_zero_frames = tmp.join("m_zero_frames_384k.wav");
    {
        let mut f = File::create(&newer_zero_frames).unwrap();
        f.write_all(b"RIFF").unwrap();
        f.write_all(&36u32.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&3u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&384_000u32.to_le_bytes()).unwrap();
        f.write_all(&(384_000u32 * 8).to_le_bytes()).unwrap();
        f.write_all(&8u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&0u32.to_le_bytes()).unwrap(); // 0 frames!
    }

    std::thread::sleep(std::time::Duration::from_millis(25));
    let newest_all_zeros = tmp.join("z_all_zeros_384k.wav");
    write_wav_f32(&newest_all_zeros, 384_000, 2, &[0.0, 0.0, 0.0, 0.0], false);

    let factory = RewConvolverFactory::new(ConvolutionConfig::auto_dir(tmp.clone()));
    let conv = factory
        .create_for_rate(384_000)
        .expect("all-zero newest WAV must fall back to older valid filter, not fail track start");
    assert!(
        conv.description().contains("a_good_384k.wav"),
        "expected fallback to a_good_384k.wav, got {}",
        conv.description()
    );
    let warnings = factory.warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("m_zero_frames_384k.wav") && w.contains("0 frames")),
        "expected 0-frame warning for m_zero_frames_384k.wav, got: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("z_all_zeros_384k.wav") && w.contains("all zeros")),
        "expected load/prepare failure warning for z_all_zeros_384k.wav, got: {warnings:?}"
    );

    // If ONLY invalid/all-zero WAVs remain, fall back cleanly to NullConvolver with a warning.
    fs::remove_file(&older_ok).unwrap();
    let factory_empty = RewConvolverFactory::new(ConvolutionConfig::auto_dir(tmp.clone()));
    let conv_null = factory_empty.create_for_rate(384_000).unwrap();
    assert_eq!(conv_null.name(), "null-convolver");
    assert!(
        factory_empty
            .warnings()
            .iter()
            .any(|w| w.contains("null-convolver")),
        "expected loud fallback-to-null warning, got: {:?}",
        factory_empty.warnings()
    );

    let _ = fs::remove_dir_all(&tmp);
}

#[test]
#[ignore = "requires local C:\\Program Files\\EqualizerAPO\\config REW WAVs"]
fn real_equalizer_apo_rew_filters_load_and_stream_4m_taps_if_present() {
    let dir = Path::new(r"C:\Program Files\EqualizerAPO\config");
    assert!(dir.is_dir(), "EqualizerAPO config directory not found");

    let found_384 = discover_latest_rew_wav(dir, 384_000)
        .unwrap()
        .expect("no 384 kHz stereo WAV found in EqualizerAPO config dir");
    let found_352 = discover_latest_rew_wav(dir, 352_800)
        .unwrap()
        .expect("no 352.8 kHz stereo WAV found in EqualizerAPO config dir");

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
