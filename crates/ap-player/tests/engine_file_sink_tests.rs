// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! **L11 · Engine End-to-End with `FileWireSink`** (`PIPELINE_TESTS_REVIEW.md` §3 P2).
//!
//! Drives the REAL `producer_loop_with_sources`, `BoundedBlockStreamBuffer`, and
//! `step_consumer_once` consumer across a 3-track FLAC queue (including equal-rate gapless join,
//! sample-rate change, pause/resume, and stop fade-out) into `FileWireSink`, then decodes the
//! captured wire bytes and nulls them against independent Layer A–C references.

use std::f64::consts::PI;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ap_control::state::{ControlCommand, SharedControl};
use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};
use ap_output_win::wdm_ks::decode_wire_frames;
use ap_player::engine::run_sequential_folder_with_sink;
use ap_player::file_wire_sink::FileWireSink;
use ap_player::signals::ShutdownController;
use ap_verify::metrics::peak_residual_db;
use ap_verify::reference::{fft_convolve_oneshot, zero_stuff};
use flac_codec::encode::{FlacSampleWriter, Options};

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ap_l11_{label}_{}_{}",
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a stereo PCM FLAC file from normalized `[-1, 1)` floating-point slices and return the
/// exact quantized source `f64` samples written into the FLAC.
fn write_flac_from_f64(
    path: &Path,
    sample_rate_hz: u32,
    bits: u32,
    left: &[f64],
    right: &[f64],
) -> (Vec<f64>, Vec<f64>) {
    let n = left.len().min(right.len());
    let levels = (1_u64 << (bits - 1)) as f64;
    let min_i = -(1_i64 << (bits - 1));
    let max_i = (1_i64 << (bits - 1)) - 1;

    let mut writer = FlacSampleWriter::create(
        path,
        Options::default(),
        sample_rate_hz,
        bits,
        2,
        Some((n * 2) as u64),
    )
    .expect("create flac writer");

    let mut interleaved = Vec::with_capacity(n * 2);
    let mut exact_l = Vec::with_capacity(n);
    let mut exact_r = Vec::with_capacity(n);
    for i in 0..n {
        let il = (left[i] * levels).round().clamp(min_i as f64, max_i as f64) as i32;
        let ir = (right[i] * levels).round().clamp(min_i as f64, max_i as f64) as i32;
        interleaved.push(il);
        interleaved.push(ir);
        exact_l.push(f64::from(il) / levels);
        exact_r.push(f64::from(ir) / levels);
    }
    writer.write(&interleaved).expect("write flac");
    writer.finalize().expect("finalize flac");
    (exact_l, exact_r)
}

#[test]
fn l11_engine_3_track_queue_gapless_rate_change_and_wire_null() {
    let root = temp_dir("queue3");
    let music_dir = root.join("music");
    fs::create_dir_all(&music_dir).unwrap();

    // Tracks 1 & 2: two halves of a continuous 1.0 s 44.1 kHz / 16-bit chord -> equal-rate gapless join!
    let n_44 = 44_100_usize;
    let split = 22_050_usize;
    let mut full44_l = Vec::with_capacity(n_44);
    let mut full44_r = Vec::with_capacity(n_44);
    for i in 0..n_44 {
        let t = (i as f64) / 44_100.0;
        full44_l.push(0.35 * (2.0 * PI * 440.0 * t).sin() + 0.12 * (2.0 * PI * 1_320.0 * t).sin());
        full44_r.push(0.32 * (2.0 * PI * 554.37 * t).sin() + 0.14 * (2.0 * PI * 1_661.22 * t).sin());
    }
    let (a_l, a_r) = write_flac_from_f64(
        &music_dir.join("01_track_a.flac"),
        44_100,
        16,
        &full44_l[..split],
        &full44_r[..split],
    );
    let (b_l, b_r) = write_flac_from_f64(
        &music_dir.join("02_track_b.flac"),
        44_100,
        16,
        &full44_l[split..],
        &full44_r[split..],
    );

    // Track 3: 0.5 s 48 kHz / 24-bit -> triggers rate change from 352.8 kHz to 384 kHz!
    let n_48 = 24_000_usize;
    let mut c_in_l = Vec::with_capacity(n_48);
    let mut c_in_r = Vec::with_capacity(n_48);
    for i in 0..n_48 {
        let t = (i as f64) / 48_000.0;
        c_in_l.push(0.30 * (2.0 * PI * 997.0 * t).sin());
        c_in_r.push(0.28 * (2.0 * PI * 1_170.0 * t).sin());
    }
    let (c_l, c_r) = write_flac_from_f64(
        &music_dir.join("03_track_c.flac"),
        48_000,
        24,
        &c_in_l,
        &c_in_r,
    );

    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::ShortLp),
        shaper_override: Some(NoiseShaperProfile::NsGentle2),
        dac_bits: 24,
        ..UpsamplerPolicy::default()
    };
    let pipeline = AudioPipeline::new(policy).unwrap();
    let ref_pipeline = AudioPipeline::new(policy).unwrap();

    let shared = SharedControl::new(music_dir.clone(), "FileWireSink");
    let controller = ShutdownController::new();
    let stop_handle = Arc::new(AtomicBool::new(false));
    let wire_path = root.join("captured_wire.bin");
    let mut sink = FileWireSink::create(wire_path.clone(), Arc::clone(&stop_handle)).unwrap();

    run_sequential_folder_with_sink(
        music_dir.clone(),
        pipeline,
        None,
        Arc::clone(&shared),
        &controller,
        Arc::clone(&stop_handle),
        &mut sink,
        |_shared, at_eoq| at_eoq,
    )
    .expect("engine run failed");

    // Review #10 EN2: Consumer keeps the hardware pin open continuously across same-rate,
    // same-wire-format gapless tracks and only reopens on a rate/format change:
    //   Segment 0: Track 1 + Track 2 continuously @ 352,800 Hz (ZERO hardware stop/reopen!)
    //   Segment 1: Track 3 @ 384,000 Hz (clean rate switch + full FIR ring-down tail at end-of-queue)
    assert_eq!(
        sink.segments().len(),
        2,
        "EN2: expected 2 segments (continuous Track 1+2 @ 352.8k, Track 3 @ 384k), got {:?}",
        sink.segments()
    );
    let seg0 = sink.segments()[0].clone();
    let seg1 = sink.segments()[1].clone();
    assert_eq!(seg0.sample_rate_hz, 352_800);
    assert_eq!(seg1.sample_rate_hz, 384_000);
    assert!(!seg0.had_fade_out, "fade-out must NOT occur during rate change Track 2 -> Track 3");
    assert!(!seg1.had_fade_out, "fade-out must NOT occur at natural end-of-queue");

    // Segment 0 wire frames (Track 1 + Track 2 continuous gapless join @ 352.8 kHz)
    // must null EVERY frame (including the Review #6 #13 full FIR ring-down tail!) against the
    // independent continuous reference!
    let bytes_12 = sink.read_segment_bytes(0).unwrap();
    let wire_12 = decode_wire_frames(&bytes_12, seg0.wire_format).unwrap();

    let mut cont44_l = a_l;
    cont44_l.extend_from_slice(&b_l);
    let mut cont44_r = a_r;
    cont44_r.extend_from_slice(&b_r);
    let spec44 = StreamSpec {
        sample_rate_hz: 44_100,
        bits_per_sample: 16,
        channels: 2,
    };
    let s44 = ref_pipeline
        .start_track(spec44, &TrackPeakMetadata::default(), false)
        .unwrap();
    let g44_lin = 10.0_f64.powf(s44.report().headroom.total_gain_db() / 20.0);
    let up44 = s44.upsampler_export().unwrap();
    let lat44 = s44.report().upsampler_latency_samples;
    let ringdown44 = up44.pad_lead * up44.ratio;
    assert_eq!(
        wire_12.len(),
        n_44 * 8 + ringdown44,
        "zero frames lost or repeated across Track 1 -> Track 2 gapless join (+ exact FIR ring-down tail)"
    );
    let taps44 = up44.prototype_taps();
    let conv44_l = fft_convolve_oneshot(&zero_stuff(&cont44_l, 8), &taps44);
    let conv44_r = fft_convolve_oneshot(&zero_stuff(&cont44_r, 8), &taps44);
    let ref12_l = &conv44_l[lat44..lat44 + wire_12.len()];
    let ref12_r = &conv44_r[lat44..lat44 + wire_12.len()];

    // Convert wire_12 i32 (24-bit MSB-aligned in i32) back to f64 and compare against g44 * ref12!
    // Residual must equal shaped dither only (<= 4.5 LSB at 24-bit => <= -125 dBFS peak!),
    // proving bit-exact gapless continuity across the Track 1 -> Track 2 join with zero click!
    let levels = (1_u64 << 23) as f64;
    let wire12_l: Vec<f64> = wire_12.iter().map(|f| f64::from(f[0] >> 8) / levels).collect();
    let wire12_r: Vec<f64> = wire_12.iter().map(|f| f64::from(f[1] >> 8) / levels).collect();
    let exp12_l: Vec<f64> = ref12_l.iter().map(|&v| v * g44_lin).collect();
    let exp12_r: Vec<f64> = ref12_r.iter().map(|&v| v * g44_lin).collect();
    let peak12_db = peak_residual_db(&wire12_l, &exp12_l).max(peak_residual_db(&wire12_r, &exp12_r));
    assert!(
        peak12_db <= -125.0,
        "L11 Segment 0+1 (gapless Track 1+2) wire-vs-reference residual {peak12_db:+.2} dBFS > -125 dBFS"
    );

    // Same Layer A–C null check on Segment 1 (Track 3 @ 384 kHz after clean rate switch)!
    let bytes2 = sink.read_segment_bytes(1).unwrap();
    let wire2 = decode_wire_frames(&bytes2, seg1.wire_format).unwrap();
    let spec48 = StreamSpec {
        sample_rate_hz: 48_000,
        bits_per_sample: 24,
        channels: 2,
    };
    let s48 = ref_pipeline
        .start_track(spec48, &TrackPeakMetadata::default(), false)
        .unwrap();
    let g48_lin = 10.0_f64.powf(s48.report().headroom.total_gain_db() / 20.0);
    let up48 = s48.upsampler_export().unwrap();
    let lat48 = s48.report().upsampler_latency_samples;
    let ringdown48 = up48.pad_lead * up48.ratio;
    assert_eq!(
        wire2.len(),
        n_48 * 8 + ringdown48,
        "zero frames lost or repeated across Track 2 -> Track 3 rate change (+ exact FIR ring-down tail)"
    );
    let taps48 = up48.prototype_taps();
    let conv48_l = fft_convolve_oneshot(&zero_stuff(&c_l, 8), &taps48);
    let conv48_r = fft_convolve_oneshot(&zero_stuff(&c_r, 8), &taps48);
    let ref2_l = &conv48_l[lat48..lat48 + wire2.len()];
    let ref2_r = &conv48_r[lat48..lat48 + wire2.len()];
    let wire2_l: Vec<f64> = wire2.iter().map(|f| f64::from(f[0] >> 8) / levels).collect();
    let wire2_r: Vec<f64> = wire2.iter().map(|f| f64::from(f[1] >> 8) / levels).collect();
    let exp2_l: Vec<f64> = ref2_l.iter().map(|&v| v * g48_lin).collect();
    let exp2_r: Vec<f64> = ref2_r.iter().map(|&v| v * g48_lin).collect();
    let peak2_db = peak_residual_db(&wire2_l, &exp2_l).max(peak_residual_db(&wire2_r, &exp2_r));
    assert!(
        peak2_db <= -125.0,
        "L11 Segment 2 (Track 3 @ 384k) wire-vs-reference residual {peak2_db:+.2} dBFS > -125 dBFS"
    );

    // Also verify exact Layer A unity null (<= -250 dBFS) of Track 3 through a direct session vs ref2.
    let mut s48_u = ref_pipeline
        .start_track(spec48, &TrackPeakMetadata::default(), false)
        .unwrap();
    let mut u48 = s48_u
        .process_block_unity_f64(StereoF64Buffer {
            sample_rate_hz: 48_000,
            left: c_l,
            right: c_r,
        })
        .unwrap();
    let t48 = s48_u.flush_unity_f64().unwrap();
    u48.left.extend_from_slice(&t48.left);
    u48.right.extend_from_slice(&t48.right);
    let unity_res = peak_residual_db(&u48.left, &conv48_l[lat48..lat48 + n_48 * 8])
        .max(peak_residual_db(&u48.right, &conv48_r[lat48..lat48 + n_48 * 8]));
    assert!(unity_res <= -250.0, "L11 Track 3 Layer A unity residual {unity_res:+.2} dB");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn l11_engine_pause_resume_and_stop_fadeout_only_at_stop() {
    let root = temp_dir("pause_stop");
    let music_dir = root.join("music");
    fs::create_dir_all(&music_dir).unwrap();

    // Two 1.0 s tracks so pause/resume happens cleanly mid-Track 1 and stop happens cleanly mid-Track 2.
    let n = 44_100_usize;
    let l: Vec<f64> = (0..n)
        .map(|i| 0.35 * (2.0 * PI * 440.0 * (i as f64) / 44_100.0).sin())
        .collect();
    let r: Vec<f64> = (0..n)
        .map(|i| 0.35 * (2.0 * PI * 660.0 * (i as f64) / 44_100.0).sin())
        .collect();
    write_flac_from_f64(&music_dir.join("01_first.flac"), 44_100, 16, &l, &r);
    write_flac_from_f64(&music_dir.join("02_second.flac"), 44_100, 16, &l, &r);

    let policy = UpsamplerPolicy {
        filter_override: Some(FilterArchetype::SimpleTest),
        shaper_override: Some(NoiseShaperProfile::NsGentle2),
        dac_bits: 24,
        ..UpsamplerPolicy::default()
    };
    let pipeline = AudioPipeline::new(policy).unwrap();
    let shared = SharedControl::new(music_dir.clone(), "FileWireSink");
    let controller = Arc::new(ShutdownController::new());
    let stop_handle = Arc::new(AtomicBool::new(false));
    let wire_path = root.join("pause_stop_wire.bin");

    let did_pause = Arc::new(AtomicBool::new(false));
    let did_pause_hook = Arc::clone(&did_pause);
    let shared_hook = Arc::clone(&shared);
    let stop_hook = Arc::clone(&stop_handle);
    let ctrl_hook = Arc::clone(&controller);

    let mut sink = FileWireSink::create(wire_path, Arc::clone(&stop_handle))
        .unwrap()
        .with_block_hook(Box::new(move |seg_idx, seg_frames, _total| {
            // Segment 0: trigger Pause once we have played at least 32k frames of Track 1.
            if seg_idx == 0 && seg_frames >= 32_768 && !did_pause_hook.swap(true, Ordering::AcqRel)
            {
                shared_hook.submit_command(ControlCommand::Pause);
                stop_hook.store(true, Ordering::Release);
            }
            // Segment 1 is resumed Track 1 continuing seamlessly (EN2) into Track 2 (352,800
            // frames for 1.0 s Track 1 + 32,768 frames into Track 2): trigger Stop mid-Track 2 ->
            // fade-out happens ONLY on stop/pause!
            if seg_idx == 1 && seg_frames >= 352_800 + 32_768 {
                stop_hook.store(true, Ordering::Release);
                ctrl_hook.trigger(ap_player::signals::ShutdownSignal::CtrlC);
            }
        }));

    let resumed = Arc::new(AtomicBool::new(false));
    run_sequential_folder_with_sink(
        music_dir,
        pipeline,
        None,
        Arc::clone(&shared),
        &controller,
        Arc::clone(&stop_handle),
        &mut sink,
        |sh, at_eoq| {
            if did_pause.load(Ordering::Acquire)
                && !sh.snapshot().is_playing
                && !resumed.swap(true, Ordering::AcqRel)
            {
                sh.submit_command(ControlCommand::Resume);
            }
            at_eoq
        },
    )
    .expect("pause/resume/stop engine run failed");

    assert!(did_pause.load(Ordering::Acquire), "expected Pause to fire");
    assert!(resumed.load(Ordering::Acquire), "expected Resume to fire");
    assert_eq!(
        sink.segments().len(),
        2,
        "EN2: expected 2 segments (Track 1 pre-pause, continuous Track 1 resumed -> Track 2 stopped), got {:?}",
        sink.segments()
    );
    assert!(sink.segments()[0].had_fade_out, "Pause must fade out cleanly");
    assert!(sink.segments()[1].had_fade_out, "Stop mid-Track 2 must fade out cleanly");
    let seg1_frames =
        sink.segments()[1].byte_len / sink.segments()[1].wire_format.bytes_per_frame() as u64;
    assert!(
        seg1_frames > 352_800,
        "EN2: Segment 1 must have crossed Track 1 -> Track 2 continuously without reopening ({seg1_frames} frames)"
    );

    let _ = fs::remove_dir_all(&root);
}
