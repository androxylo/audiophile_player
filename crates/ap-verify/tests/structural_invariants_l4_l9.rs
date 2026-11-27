// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! **L4–L9 · Structural Invariants on Real Music** (`PIPELINE_TESTS_REVIEW.md` §3 P1).
//!
//! - **L4:** Chunk independence end-to-end (unity f64 & quantized, plus Ultimate W0–W4 vs normal).
//! - **L5:** Gapless = continuous with last-stage downsampler enabled (`÷2`, `÷4`, `÷8`, and
//!   cross-family rational `352.8 -> 48 kHz`) for both normal gapless and Ultimate additive tail.
//! - **L6:** Seam after 30 s look-ahead (`W2` + `W4` continuation == single-session render).
//! - **L7:** Linearity (`render(a*x) == a*render(x)`) and time invariance
//!   (`render(delay_k(x)) == delay_{k*L}(render(x))`) on real audio.
//! - **L8:** Downsampler anti-aliasing vs ideal FFT brick-wall downsampler (`<= -180 dB`).
//! - **L9:** Determinism (same seed -> byte-identical wire bytes; parallel pool vs sequential
//!   -> byte-identical wire bytes).

use std::f64::consts::PI;
use std::fs;
use std::io::Write;
use std::path::Path;

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::config::ConvolutionConfig;
use ap_core::format::{DacWireFormat, StreamSpec};
use ap_core::headroom::{HeadroomDecision, HeadroomMode, TrackPeakMetadata};
use ap_dsp::downsampler::build_downsampler;
use ap_dsp::pipeline::{AudioPipeline, GaplessAttachOutcome};
use ap_dsp::quantize::DEFAULT_QUANTIZER_SEED;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, TrackSourceInfo, UpsamplerPolicy};
use ap_dsp::worker_pool::with_force_sequential;
use ap_output_win::wdm_ks::WaveRtFramePump;
use ap_source::flac::decode_flac_full_memory;
use ap_verify::metrics::band_residual_db;
use ap_verify::pick::{bundled_fixture_path, random_chunk_sizes};
use ap_verify::reference::brickwall_fft;
use ap_verify::runner::build_dac_caps;

fn load_fixture_excerpt(seconds: f64) -> (StreamSpec, Vec<f64>, Vec<f64>) {
    let decoded = decode_flac_full_memory(&bundled_fixture_path()).expect("decode fixture");
    let take = ((f64::from(decoded.spec.sample_rate_hz) * seconds) as usize)
        .min(decoded.audio.left.len());
    (
        decoded.spec,
        decoded.audio.left[..take].to_vec(),
        decoded.audio.right[..take].to_vec(),
    )
}

fn write_stereo_f32_wav(path: &Path, sample_rate_hz: u32, left: &[f64], right: &[f64]) {
    let n = left.len().min(right.len());
    let data_bytes = (n * 2 * 4) as u32;
    let riff_size = 4 + (8 + 16) + (8 + data_bytes);
    let mut f = fs::File::create(path).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&riff_size.to_le_bytes()).unwrap();
    f.write_all(b"WAVE").unwrap();
    f.write_all(b"fmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&3u16.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&sample_rate_hz.to_le_bytes()).unwrap();
    f.write_all(&(sample_rate_hz * 8).to_le_bytes()).unwrap();
    f.write_all(&8u16.to_le_bytes()).unwrap();
    f.write_all(&32u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_bytes.to_le_bytes()).unwrap();
    for i in 0..n {
        f.write_all(&(left[i] as f32).to_le_bytes()).unwrap();
        f.write_all(&(right[i] as f32).to_le_bytes()).unwrap();
    }
}

fn encode_wire_bytes(frames: &[[i32; 2]], fmt: DacWireFormat) -> Vec<u8> {
    let bpf = fmt.bytes_per_frame();
    let mut out = vec![0u8; frames.len() * bpf];
    let mut pump = WaveRtFramePump::new(frames, fmt);
    let written = pump.fill_slice(&mut out);
    assert_eq!(written, frames.len());
    out
}

#[test]
fn l4_chunk_independence_end_to_end_and_ultimate_vs_normal() {
    let (spec, left, right) = load_fixture_excerpt(1.5);
    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::ShortLp),
        shaper_override: Some(NoiseShaperProfile::NsGentle2),
        dac_bits: 24,
        ..UpsamplerPolicy::default()
    };
    let dac_caps = build_dac_caps(DacWireFormat::Int32, Some(192_000));
    let pipeline = AudioPipeline::new(policy)
        .unwrap()
        .with_dac_capabilities(dac_caps);
    let hr = HeadroomDecision {
        mode: HeadroomMode::Minus3Db,
        peak_dbfs: None,
        peak_kind: None,
        convolution_scale_db: 0.0,
        reason: "L4 fixed -3 dB".to_string(),
    };
    let plan = pipeline.policy();
    let picker = ap_dsp::selector::UpsamplerPicker::new(plan).unwrap();
    let selected_plan = picker.select(&TrackSourceInfo::from_stream_spec(spec));

    // 1. One-shot unity + quantized.
    let mut s_one = pipeline
        .start_track_with_plan_and_headroom(spec, selected_plan.clone(), hr.clone())
        .unwrap();
    s_one.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ 0x4444);
    let mut one_u = s_one
        .process_block_unity_f64(StereoF64Buffer {
            sample_rate_hz: spec.sample_rate_hz,
            left: left.clone(),
            right: right.clone(),
        })
        .unwrap();
    let one_tail = s_one.flush_unity_f64().unwrap();
    one_u.left.extend_from_slice(&one_tail.left);
    one_u.right.extend_from_slice(&one_tail.right);
    let mut no_tail = None;
    let one_q = s_one
        .quantize_scaled_block(&one_u, hr.total_gain_db(), &mut no_tail)
        .unwrap();

    // 2. Seeded random chunk sizes through the exact same session API (Ultimate W0-W4 style).
    let mut s_rnd = pipeline
        .start_track_with_plan_and_headroom(spec, selected_plan.clone(), hr.clone())
        .unwrap();
    s_rnd.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ 0x4444);
    let mut rnd_l = Vec::new();
    let mut rnd_r = Vec::new();
    let mut rnd_q_frames = Vec::new();
    let mut cursor = 0_usize;
    for sz in random_chunk_sizes(left.len(), 0x4444_CAFEu64.wrapping_add(1337)) {
        let blk = StereoF64Buffer {
            sample_rate_hz: spec.sample_rate_hz,
            left: left[cursor..cursor + sz].to_vec(),
            right: right[cursor..cursor + sz].to_vec(),
        };
        cursor += sz;
        let u = s_rnd.process_block_unity_f64(blk).unwrap();
        if !u.is_empty() {
            rnd_l.extend_from_slice(&u.left);
            rnd_r.extend_from_slice(&u.right);
            let q = s_rnd
                .quantize_scaled_block(&u, hr.total_gain_db(), &mut no_tail)
                .unwrap();
            rnd_q_frames.extend_from_slice(&q.frames);
        }
    }
    let rnd_tail = s_rnd.flush_unity_f64().unwrap();
    if !rnd_tail.is_empty() {
        rnd_l.extend_from_slice(&rnd_tail.left);
        rnd_r.extend_from_slice(&rnd_tail.right);
        let q = s_rnd
            .quantize_scaled_block(&rnd_tail, hr.total_gain_db(), &mut no_tail)
            .unwrap();
        rnd_q_frames.extend_from_slice(&q.frames);
    }

    assert_eq!(one_u.left.len(), rnd_l.len());
    for i in 0..one_u.left.len() {
        assert_eq!(
            one_u.left[i].to_bits(),
            rnd_l[i].to_bits(),
            "L4 unity L mismatch at frame {i}"
        );
        assert_eq!(
            one_u.right[i].to_bits(),
            rnd_r[i].to_bits(),
            "L4 unity R mismatch at frame {i}"
        );
    }
    assert_eq!(
        one_q.frames, rnd_q_frames,
        "L4 quantized frames differ between one-shot and random chunks"
    );

    // 3. Normal-mode `process_block` + `flush` with identical plan & gain & seed matches Ultimate
    //    unity+quantize path within IEEE 754 f64 gain commutativity (linear chain is LTI).
    let mut s_norm = pipeline
        .start_track_with_plan_and_headroom(spec, selected_plan, hr)
        .unwrap();
    s_norm.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ 0x4444);
    let mut norm_frames = Vec::new();
    let mut cursor = 0_usize;
    for sz in random_chunk_sizes(left.len(), 0x7777) {
        let blk = StereoF64Buffer {
            sample_rate_hz: spec.sample_rate_hz,
            left: left[cursor..cursor + sz].to_vec(),
            right: right[cursor..cursor + sz].to_vec(),
        };
        cursor += sz;
        norm_frames.extend_from_slice(&s_norm.process_block(blk).unwrap().frames);
    }
    norm_frames.extend_from_slice(&s_norm.flush().unwrap().frames);
    assert_eq!(norm_frames.len(), one_q.frames.len());
    // Gain applied pre-upsampler vs post-downsampler differs by <= 1e-15 linear (<= 1 LSB in 24-bit
    // only on rare half-integer dither ties); assert peak difference <= 1 LSB at 24-bit!
    let mut max_lsb_diff = 0_i64;
    for (a, b) in norm_frames.iter().zip(one_q.frames.iter()) {
        for ch in 0..2 {
            let d = (i64::from(a[ch] >> 8) - i64::from(b[ch] >> 8)).abs();
            if d > max_lsb_diff {
                max_lsb_diff = d;
            }
        }
    }
    assert!(
        max_lsb_diff <= 1,
        "L4 normal vs ultimate quantizer path differed by {max_lsb_diff} LSB"
    );
}

#[test]
fn l5_gapless_equals_continuous_with_downsampler_integer_and_rational() {
    let (_, fix_l, fix_r) = load_fixture_excerpt(1.0);

    // Test ÷2 (384->192k), ÷4 (384->96k), ÷8 (384->48k), AND rational 352.8->48k (D1/D6/D10)!
    let cases: &[(u32, u32, usize)] = &[
        (96_000, 192_000, 31_777), // 384k -> 192k (÷2)
        (96_000, 96_000, 41_233),  // 384k -> 96k  (÷4)
        (96_000, 48_000, 53_119),  // 384k -> 48k  (÷8)
        (44_100, 48_000, 19_333),  // 352.8k -> 48k (rational cross-family D10!)
    ];

    for &(src_rate, out_rate, split_frame) in cases {
        let spec = StreamSpec {
            sample_rate_hz: src_rate,
            bits_per_sample: 24,
            channels: 2,
        };
        let n = if src_rate == 96_000 {
            fix_l.len()
        } else {
            // 1.0 s rich multi-tone signal at 44.1 kHz
            let len = 44_100;
            assert!(split_frame < len);
            len
        };
        let (src_l, src_r): (Vec<f64>, Vec<f64>) = if src_rate == 96_000 {
            (fix_l[..n].to_vec(), fix_r[..n].to_vec())
        } else {
            let mut l = Vec::with_capacity(n);
            let mut r = Vec::with_capacity(n);
            for i in 0..n {
                let t = (i as f64) / 44_100.0;
                l.push(0.25 * (2.0 * PI * 440.0 * t).sin() + 0.15 * (2.0 * PI * 3_117.0 * t).sin());
                r.push(0.25 * (2.0 * PI * 660.0 * t).sin() + 0.15 * (2.0 * PI * 5_431.0 * t).sin());
            }
            (l, r)
        };

        let policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::ShortLp),
            shaper_override: Some(NoiseShaperProfile::TpdfDither),
            dac_bits: 24,
            ..UpsamplerPolicy::default()
        };
        let dac_caps = if src_rate == 44_100 && out_rate == 48_000 {
            // Simulate a 48 kHz-only HDMI/SPDIF DAC to force cross-family rational 352.8 -> 48 kHz.
            ap_core::format::DacOutputCapabilities::from_supported_rates(
                &[(48_000, DacWireFormat::Int32)],
                None,
            )
        } else {
            build_dac_caps(DacWireFormat::Int32, Some(out_rate))
        };
        let pipeline = AudioPipeline::new(policy)
            .unwrap()
            .with_dac_capabilities(dac_caps);
        let peak_meta = TrackPeakMetadata {
            true_peak_linear: Some(0.4),
            ..TrackPeakMetadata::default()
        };

        // 1. Continuous single-track reference (normal mode, quantized).
        let mut s_cont = pipeline.start_track(spec, &peak_meta, false).unwrap();
        s_cont.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ 0x5555);
        assert_eq!(s_cont.report().output_rate_hz, out_rate);
        let mut cont_frames = s_cont
            .process_block(StereoF64Buffer {
                sample_rate_hz: src_rate,
                left: src_l.clone(),
                right: src_r.clone(),
            })
            .unwrap()
            .frames;
        cont_frames.extend_from_slice(&s_cont.flush().unwrap().frames);

        // 2. Split into A = [0..split_frame], B = [split_frame..n], played gaplessly!
        let mut s_a = pipeline.start_track(spec, &peak_meta, false).unwrap();
        s_a.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ 0x5555);
        let mut gap_frames = s_a
            .process_block(StereoF64Buffer {
                sample_rate_hz: src_rate,
                left: src_l[..split_frame].to_vec(),
                right: src_r[..split_frame].to_vec(),
            })
            .unwrap()
            .frames;
        let (tail_a, carry_a) = s_a.flush_for_gapless().unwrap();
        gap_frames.extend_from_slice(&tail_a.frames);
        let carry_a = carry_a.expect("gapless carryover must exist");

        let mut s_b = pipeline.start_track(spec, &peak_meta, false).unwrap();
        let outcome = pipeline
            .attach_gapless_carryover(&mut s_b, carry_a)
            .unwrap();
        assert!(
            matches!(outcome, GaplessAttachOutcome::FullHandoff),
            "expected FullHandoff for {src_rate}->{out_rate}, got {outcome:?}"
        );
        gap_frames.extend_from_slice(
            &s_b.process_block(StereoF64Buffer {
                sample_rate_hz: src_rate,
                left: src_l[split_frame..].to_vec(),
                right: src_r[split_frame..].to_vec(),
            })
            .unwrap()
            .frames,
        );
        gap_frames.extend_from_slice(&s_b.flush().unwrap().frames);

        assert_eq!(
            gap_frames.len(),
            cont_frames.len(),
            "L5 gapless length mismatch for {src_rate}->{out_rate}: gap={} cont={}",
            gap_frames.len(),
            cont_frames.len()
        );
        assert_eq!(
            gap_frames, cont_frames,
            "L5 gapless quantized output must be BIT-IDENTICAL to continuous for {src_rate}->{out_rate}"
        );
    }
}

#[test]
fn l6_seam_after_30s_lookahead_equals_single_session() {
    // 41 s track at 44.1 kHz (with ShortLp 8x -> 352.8 kHz):
    // W2 renders first 30 s unity -> locks G -> quantizes W2 -> W4 continues remaining 11 s + flush.
    // Must be bit-identical to a single continuous session render of all 41 s with the same G & seed!
    let src_rate = 44_100_u32;
    let total_frames = (src_rate as usize) * 41;
    let w0_frames = (src_rate as usize) * 30;
    let mut left = Vec::with_capacity(total_frames);
    let mut right = Vec::with_capacity(total_frames);
    for i in 0..total_frames {
        let t = (i as f64) / f64::from(src_rate);
        let env = 0.4 + 0.2 * (2.0 * PI * 0.1 * t).sin();
        left.push(env * (2.0 * PI * 523.25 * t).sin());
        right.push(env * (2.0 * PI * 659.25 * t).sin());
    }
    let spec = StreamSpec {
        sample_rate_hz: src_rate,
        bits_per_sample: 24,
        channels: 2,
    };
    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SimpleTest),
        shaper_override: Some(NoiseShaperProfile::NsGentle2),
        dac_bits: 24,
        ..UpsamplerPolicy::default()
    };
    let pipeline = AudioPipeline::new(policy).unwrap();
    let picker = ap_dsp::selector::UpsamplerPicker::new(pipeline.policy()).unwrap();
    let plan = picker.select(&TrackSourceInfo::from_stream_spec(spec));
    let hr = HeadroomDecision {
        mode: HeadroomMode::ZeroDb,
        peak_dbfs: None,
        peak_kind: None,
        convolution_scale_db: -1.5,
        reason: "L6 fixed gain".to_string(),
    };
    let fixed_g = hr.total_gain_db();

    // Path 1: W2 (30 s unity buffered, then quantized) + W4 (11 s streamed) + flush.
    let mut s_ult = pipeline
        .start_track_with_plan_and_headroom(spec, plan.clone(), hr.clone())
        .unwrap();
    s_ult.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ 0x6666);
    let mut w2_bufs = Vec::new();
    let mut pos = 0_usize;
    while pos < w0_frames {
        let take = (w0_frames - pos).min(16_384);
        let u = s_ult
            .process_block_unity_f64(StereoF64Buffer {
                sample_rate_hz: src_rate,
                left: left[pos..pos + take].to_vec(),
                right: right[pos..pos + take].to_vec(),
            })
            .unwrap();
        if !u.is_empty() {
            w2_bufs.push(u);
        }
        pos += take;
    }
    let mut ult_frames = Vec::new();
    let mut no_tail = None;
    for u in w2_bufs {
        ult_frames.extend_from_slice(
            &s_ult
                .quantize_scaled_block(&u, fixed_g, &mut no_tail)
                .unwrap()
                .frames,
        );
    }
    while pos < total_frames {
        let take = (total_frames - pos).min(11_111); // deliberately different W4 chunk size!
        let u = s_ult
            .process_block_unity_f64(StereoF64Buffer {
                sample_rate_hz: src_rate,
                left: left[pos..pos + take].to_vec(),
                right: right[pos..pos + take].to_vec(),
            })
            .unwrap();
        if !u.is_empty() {
            ult_frames.extend_from_slice(
                &s_ult
                    .quantize_scaled_block(&u, fixed_g, &mut no_tail)
                    .unwrap()
                    .frames,
            );
        }
        pos += take;
    }
    let flush_u = s_ult.flush_unity_f64().unwrap();
    ult_frames.extend_from_slice(
        &s_ult
            .quantize_scaled_block(&flush_u, fixed_g, &mut no_tail)
            .unwrap()
            .frames,
    );

    // Path 2: Single continuous session render across all 41 s with uniform 32,768-frame chunks.
    let mut s_ref = pipeline
        .start_track_with_plan_and_headroom(spec, plan, hr)
        .unwrap();
    s_ref.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ 0x6666);
    let mut ref_frames = Vec::new();
    let mut rpos = 0_usize;
    while rpos < total_frames {
        let take = (total_frames - rpos).min(32_768);
        let u = s_ref
            .process_block_unity_f64(StereoF64Buffer {
                sample_rate_hz: src_rate,
                left: left[rpos..rpos + take].to_vec(),
                right: right[rpos..rpos + take].to_vec(),
            })
            .unwrap();
        if !u.is_empty() {
            ref_frames.extend_from_slice(
                &s_ref
                    .quantize_scaled_block(&u, fixed_g, &mut no_tail)
                    .unwrap()
                    .frames,
            );
        }
        rpos += take;
    }
    let rflush = s_ref.flush_unity_f64().unwrap();
    ref_frames.extend_from_slice(
        &s_ref
            .quantize_scaled_block(&rflush, fixed_g, &mut no_tail)
            .unwrap()
            .frames,
    );

    assert_eq!(ult_frames.len(), ref_frames.len());
    assert_eq!(
        ult_frames, ref_frames,
        "L6 W2->W4 30 s look-ahead seam must be bit-identical to single continuous session"
    );
}

#[test]
fn l7_linearity_and_time_invariance_on_real_fixture() {
    let (spec, left, right) = load_fixture_excerpt(0.75);
    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::GaussHiresLp),
        ..UpsamplerPolicy::default()
    };
    let pipeline = AudioPipeline::new(policy).unwrap();
    let render_unity = |in_l: &[f64], in_r: &[f64]| -> (Vec<f64>, Vec<f64>, usize) {
        let mut s = pipeline
            .start_track(spec, &TrackPeakMetadata::default(), false)
            .unwrap();
        let ratio = (s.report().internal_rate_hz / spec.sample_rate_hz) as usize;
        let mut out = s
            .process_block_unity_f64(StereoF64Buffer {
                sample_rate_hz: spec.sample_rate_hz,
                left: in_l.to_vec(),
                right: in_r.to_vec(),
            })
            .unwrap();
        let tail = s.flush_unity_f64().unwrap();
        out.left.extend_from_slice(&tail.left);
        out.right.extend_from_slice(&tail.right);
        (out.left, out.right, ratio)
    };

    let (base_l, base_r, ratio) = render_unity(&left, &right);
    let base_peak = base_l
        .iter()
        .chain(base_r.iter())
        .fold(0.0_f64, |acc, &v| acc.max(v.abs()));

    // 1. Linearity: render(a * x) == a * render(x) within 1e-15 relative!
    for &a in &[0.37_f64, -0.61_f64] {
        let sl: Vec<f64> = left.iter().map(|&v| v * a).collect();
        let sr: Vec<f64> = right.iter().map(|&v| v * a).collect();
        let (yl, yr, _) = render_unity(&sl, &sr);
        let mut max_rel = 0.0_f64;
        for i in 0..base_l.len() {
            let dl = (yl[i] - a * base_l[i]).abs() / (base_peak * a.abs());
            let dr = (yr[i] - a * base_r[i]).abs() / (base_peak * a.abs());
            max_rel = max_rel.max(dl).max(dr);
        }
        assert!(
            max_rel <= 1e-14,
            "L7 linearity (a={a}) relative error {max_rel:.3e} > 1e-14"
        );
    }

    // 2. Time invariance: render(delay_k(x)) == delay_{k*L}(render(x)) in interior!
    let k_delay = 113_usize;
    let mut del_l = vec![0.0_f64; k_delay];
    del_l.extend_from_slice(&left);
    del_l.resize(del_l.len() + k_delay, 0.0);
    let mut del_r = vec![0.0_f64; k_delay];
    del_r.extend_from_slice(&right);
    del_r.resize(del_r.len() + k_delay, 0.0);

    let (dl_out, dr_out, _) = render_unity(&del_l, &del_r);
    let margin = 4_096 * ratio; // skip zero-padded boundary transients
    let shift_out = k_delay * ratio;
    let mut max_ti_rel = 0.0_f64;
    for i in margin..(base_l.len() - margin) {
        let el = (dl_out[i + shift_out] - base_l[i]).abs() / base_peak;
        let er = (dr_out[i + shift_out] - base_r[i]).abs() / base_peak;
        max_ti_rel = max_ti_rel.max(el).max(er);
    }
    assert!(
        max_ti_rel <= 1e-14,
        "L7 time invariance (k={k_delay}, L={ratio}) relative error {max_ti_rel:.3e} > 1e-14"
    );
}

#[test]
fn l8_downsampler_antialiasing_below_minus_180db_on_real_content() {
    let (spec, fix_l, fix_r) = load_fixture_excerpt(0.5);
    // Upsample real fixture to 384 kHz first, then inject strong out-of-band ultrasonic tones
    // above the target downsampler Nyquist and verify output-band residual vs ideal FFT brickwall
    // stays <= -180 dB!
    let pipeline = AudioPipeline::new(UpsamplerPolicy {
        filter_override: Some(FilterArchetype::GaussHiresLp),
        ..UpsamplerPolicy::default()
    })
    .unwrap();
    let mut sess = pipeline
        .start_track(spec, &TrackPeakMetadata::default(), false)
        .unwrap();
    let mut up = sess
        .process_block_unity_f64(StereoF64Buffer {
            sample_rate_hz: spec.sample_rate_hz,
            left: fix_l,
            right: fix_r,
        })
        .unwrap();
    let tail = sess.flush_unity_f64().unwrap();
    up.left.extend_from_slice(&tail.left);
    up.right.extend_from_slice(&tail.right);

    // Ensure exact length divisible by 8*147 so integer AND rational FFT brickwall align cleanly.
    let n_in = (up.left.len() / 1_176) * 1_176;
    let in_rate = 384_000_u32;

    for &out_rate in &[192_000_u32, 96_000, 48_000] {
        // Inject two strong ultrasonic sinusoids well above output Nyquist (0.62*fs_out and 0.85*fs_out)
        // that would fold directly into the audio passband if the decimator leaked aliases.
        let f_alias_1 = 0.62 * f64::from(out_rate);
        let f_alias_2 = 0.85 * f64::from(out_rate);
        let mut x_l = Vec::with_capacity(n_in);
        let mut x_r = Vec::with_capacity(n_in);
        for i in 0..n_in {
            let t = (i as f64) / f64::from(in_rate);
            let ultra = 0.35 * (2.0 * PI * f_alias_1 * t).sin()
                + 0.35 * (2.0 * PI * f_alias_2 * t).sin();
            x_l.push(up.left[i] + ultra);
            x_r.push(up.right[i] + ultra);
        }

        let mut ds = build_downsampler(in_rate, out_rate).unwrap();
        let mut y = ds
            .process_block(StereoF64Buffer {
                sample_rate_hz: in_rate,
                left: x_l.clone(),
                right: x_r.clone(),
            })
            .unwrap();
        let dt = ds.flush().unwrap();
        y.left.extend_from_slice(&dt.left);
        y.right.extend_from_slice(&dt.right);

        let ref_l = brickwall_fft(&x_l, in_rate, out_rate);
        let ref_r = brickwall_fft(&x_r, in_rate, out_rate);
        let n_cmp = y.left.len().min(ref_l.len());
        // Skip filter edge transient (~2048 output samples at both ends) and measure in-band
        // [100 Hz, 0.40 * fs_out] residual against ideal FFT brickwall!
        let trim = 2_048.min(n_cmp / 6);
        let f_hi = 0.40 * f64::from(out_rate);
        let res_l = band_residual_db(
            &y.left[trim..n_cmp - trim],
            &ref_l[trim..n_cmp - trim],
            f64::from(out_rate),
            100.0,
            f_hi,
        );
        let res_r = band_residual_db(
            &y.right[trim..n_cmp - trim],
            &ref_r[trim..n_cmp - trim],
            f64::from(out_rate),
            100.0,
            f_hi,
        );
        let worst = res_l.max(res_r);
        assert!(
            worst <= -180.0,
            "L8 downsampler {in_rate}->{out_rate} in-band alias residual {worst:+.2} dB > -180 dB"
        );
    }
}

#[test]
fn l9_determinism_same_seed_and_parallel_vs_sequential() {
    let (spec, left, right) = load_fixture_excerpt(0.75);
    let tmp_ir = std::env::temp_dir().join(format!(
        "ap_verify_l9_ir_{}.wav",
        std::process::id()
    ));
    // 16,384-tap IR + ExtSharpLp (FFT UPOLS) exercises both parallel worker pools!
    let mut ir = vec![0.0_f64; 16_384];
    ir[0] = 0.85;
    ir[137] = 0.10;
    ir[4_096] = -0.04;
    write_stereo_f32_wav(&tmp_ir, 384_000, &ir, &ir);

    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::ExtSharpLp),
        shaper_override: Some(NoiseShaperProfile::NsSharp5),
        dac_bits: 24,
        max_taps: 65_536,
        ..UpsamplerPolicy::default()
    };
    let conv_cfg = ConvolutionConfig {
        enabled: true,
        file: Some(tmp_ir.clone()),
        ..ConvolutionConfig::default()
    };
    let pipeline = AudioPipeline::with_rew_config_full_tail(policy, conv_cfg, true).unwrap();

    let run_wire = |seed: u64| -> Vec<u8> {
        let mut s = pipeline
            .start_track(spec, &TrackPeakMetadata::default(), false)
            .unwrap();
        s.set_quantizer_seed(seed);
        let mut frames = Vec::new();
        let mut pos = 0_usize;
        for sz in random_chunk_sizes(left.len(), 0x9999) {
            let pcm: InterleavedInt32Buffer = s
                .process_block(StereoF64Buffer {
                    sample_rate_hz: spec.sample_rate_hz,
                    left: left[pos..pos + sz].to_vec(),
                    right: right[pos..pos + sz].to_vec(),
                })
                .unwrap();
            frames.extend_from_slice(&pcm.frames);
            pos += sz;
        }
        frames.extend_from_slice(&s.flush().unwrap().frames);
        encode_wire_bytes(&frames, DacWireFormat::Int32)
    };

    let seed = DEFAULT_QUANTIZER_SEED ^ 0x9999_DE7E_1234_u64;
    let par_run_1 = run_wire(seed);
    let par_run_2 = run_wire(seed);
    let seq_run = with_force_sequential(|| run_wire(seed));
    let _ = fs::remove_file(&tmp_ir);

    assert!(!par_run_1.is_empty());
    assert_eq!(
        par_run_1, par_run_2,
        "L9: two runs with identical seed must produce byte-identical wire output"
    );
    assert_eq!(
        par_run_1, seq_run,
        "L9: parallel worker pool and forced-sequential execution must produce byte-identical wire output"
    );
}
