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
    PartitionedStereoConvolver, PreparedStereoIr, RewConvolverFactory, StereoConvolver,
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
    assert_eq!(ir.normalization_gain_db, 0.0, "test IR should stay <= 0 dB");
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
    assert_eq!(ir.normalization_gain_db, 0.0);
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
fn rew_boost_triggers_auto_normalization_l1_headroom_and_runs_before_noise_shaper() {
    let tmp = std::env::temp_dir().join(format!(
        "ap_dsp_rew_headroom_test_{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Case A: Phase-dispersive IR h = [0.6, 0.6, -0.6].
    // Exact peak |H(e^{j*pi/2})| = 0.6 * |2 - j| = 0.6 * sqrt(5) ~= 1.34164 (+2.55 dB).
    // Auto-normalization scales h by 1 / (0.6 * sqrt(5)) = 1 / sqrt(5) -> L1 norm = 3 / sqrt(5)
    // ~= 1.34164 (+2.55 dB). On a quiet track tagged at peak 0.59 (-4.58 dBFS), the required
    // 0 dB ceiling drops to -3.0 - 2.55 = -5.55 dBFS, so -3 dB headroom engages, convolution runs
    // BEFORE final TPDF quantization, and the output matches the exact analytical value below FS.
    let dispersive_path = tmp.join("dispersive_384k.wav");
    write_wav_f32(
        &dispersive_path,
        384_000,
        2,
        &[0.6, 0.6, 0.6, 0.6, -0.6, -0.6],
        false,
    );

    let pipeline_a = AudioPipeline::with_rew_config(
        UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            shaper_override: Some(NoiseShaperProfile::Bypass),
            ..UpsamplerPolicy::default()
        },
        ConvolutionConfig::explicit_file(dispersive_path),
    )
    .unwrap();

    let spec = StreamSpec {
        sample_rate_hz: 384_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let peak = TrackPeakMetadata {
        replaygain_track_peak: Some(0.59),
        ..TrackPeakMetadata::default()
    };

    let mut left_in = vec![0.0_f64; 1024];
    let mut right_in = vec![0.0_f64; 1024];
    left_in[0] = -0.59;
    left_in[1] = 0.59;
    left_in[2] = 0.59;
    right_in[0] = -0.59;
    right_in[1] = 0.59;
    right_in[2] = 0.59;

    let processed_a = pipeline_a
        .process(
            StereoF64Buffer {
                sample_rate_hz: 384_000,
                left: left_in,
                right: right_in,
            },
            spec,
            &peak,
            false,
        )
        .unwrap();
    assert_eq!(processed_a.report.convolver_name, "rew-fft-f64");
    assert_eq!(processed_a.report.headroom.mode, HeadroomMode::Minus3Db);
    assert!(
        processed_a.report.convolver_description.contains("norm -"),
        "expected auto-normalization tag in description, got: {}",
        processed_a.report.convolver_description
    );
    // Because convolution modified the f64 stream BEFORE final quantization, shaper cannot bypass.
    assert_eq!(
        processed_a.report.effective_shaper,
        NoiseShaperProfile::TpdfDither
    );

    // At sample index 2: y[2] = x[2]*h_norm[0] + x[1]*h_norm[1] + x[0]*h_norm[2]
    //                         = 0.59 * (3 / sqrt(5)) * 10^(-3/20) ~= 0.560387.
    let expected_y2 = 0.59 * (3.0 / 5.0_f64.sqrt()) * HeadroomMode::Minus3Db.linear();
    let got_y2_l = (processed_a.pcm.frames[2][0] as f64) / 2_147_483_648.0;
    let got_y2_r = (processed_a.pcm.frames[2][1] as f64) / 2_147_483_648.0;
    assert!(
        (got_y2_l - expected_y2).abs() < 1e-5,
        "y[2] left mismatch: got {got_y2_l}, expected {expected_y2}"
    );
    assert!(
        (got_y2_r - expected_y2).abs() < 1e-5,
        "y[2] right mismatch: got {got_y2_r}, expected {expected_y2}"
    );
    let max_abs_a = processed_a
        .pcm
        .frames
        .iter()
        .flat_map(|f| [f[0].unsigned_abs(), f[1].unsigned_abs()])
        .max()
        .unwrap_or(0);
    assert!(max_abs_a < (i32::MAX as u32) - 1024, "must not clip full scale");

    // Case B: +6.02 dB boosting IR (left = 2.0, right = 1.0 -> 2:1 L/R balance) on untagged
    // near-full-scale (0.99) audio. Auto-normalization must bring peak gain to 0 dB while
    // preserving exact 2:1 L/R balance, and -3 dB headroom must keep output cleanly <= full scale.
    let boost_6db_path = tmp.join("boost_6db_384k.wav");
    write_wav_f32(&boost_6db_path, 384_000, 2, &[2.0, 1.0], false);

    let factory_b = RewConvolverFactory::new(ConvolutionConfig::explicit_file(boost_6db_path.clone()));
    let conv_b = factory_b.create_for_rate(384_000).unwrap();
    assert_eq!(conv_b.max_boost_db(), 0.0);
    assert_eq!(
        factory_b.warnings().len(),
        1,
        "first load of >0 dB IR must emit exactly one normalization notice"
    );
    assert!(factory_b.warnings()[0].contains("Auto-normalized"));
    // Second creation uses cached IR and must not spam duplicate notices.
    let _ = factory_b.create_for_rate(384_000).unwrap();
    assert_eq!(factory_b.warnings().len(), 1);

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
            &TrackPeakMetadata::default(), // untagged -> -3 dB fail-safe
            false,
        )
        .unwrap();
    assert_eq!(processed_b.report.headroom.mode, HeadroomMode::Minus3Db);

    let expected_l = 0.99 * 1.0 * HeadroomMode::Minus3Db.linear();
    let expected_r = 0.99 * 0.5 * HeadroomMode::Minus3Db.linear();
    let got_l = (processed_b.pcm.frames[512][0] as f64) / 2_147_483_648.0;
    let got_r = (processed_b.pcm.frames[512][1] as f64) / 2_147_483_648.0;
    assert!(
        (got_l - expected_l).abs() < 1e-5,
        "+6 dB normalized left mismatch: got {got_l}, expected {expected_l}"
    );
    assert!(
        (got_r - expected_r).abs() < 1e-5,
        "+6 dB normalized right (2:1 balance) mismatch: got {got_r}, expected {expected_r}"
    );
    assert!(
        processed_b
            .pcm
            .frames
            .iter()
            .all(|f| f[0] < i32::MAX && f[0] > i32::MIN && f[1] < i32::MAX && f[1] > i32::MIN),
        "+6 dB IR must never clip after auto-normalization"
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
    assert_eq!(ir.normalization_gain_db, 0.0);

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
