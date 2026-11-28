// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! End-to-end Layers A–D loopback null test runner (`PIPELINE_TESTS_REVIEW.md` §§2.1–2.4).
//!
//! Runs a real FLAC (or synthetic `StereoF64Buffer`) through:
//! - Real `StreamingPipelineSession` fed with seeded random chunk sizes
//! - Real `WaveRtFramePump::fill_slice` wire encoder + `decode_wire_frames` inverse
//! - Independent naive `f64` reference (`reference.rs`) aligned strictly by `report()` latency
//! - Strict Layer A (linear chain `<= -250 dBFS`), Layer B (harshness detector: 0 overs,
//!   error bound, unbiased mean, `|ρ(e,s)| < 1e-3`, 1/3-octave coherence vs silent baseline + 3 dB,
//!   Welch PSD vs analytical NTF within ±1 dB), Layer C (bit-identical wire round-trip), and
//!   Layer D (bit-identical identity on untouched native paths).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ap_core::buffer::{InterleavedInt32Buffer, StereoF64Buffer};
use ap_core::config::ConvolutionConfig;
use ap_core::format::{DacOutputCapabilities, DacWireFormat, StreamSpec};
use ap_core::headroom::{HeadroomDecision, HeadroomMode, TrackPeakMetadata, quantizer_margin_db};
use ap_dsp::pipeline::{AudioPipeline, StreamingPipelineSession};
use ap_dsp::quantize::{DEFAULT_QUANTIZER_SEED, StreamingQuantizer};
use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, TrackSourceInfo, UpsamplerPolicy};
use ap_output_win::wdm_ks::{WaveRtFramePump, decode_wire_frames};
use ap_source::flac::{AudioBlockSource, FlacBlockReader, decode_flac_full_memory};
use ap_source::md5::md5_to_hex;

use crate::metrics::{
    LayerBMetrics, coherence_bands, ntf_max_inband_deviation_db, overs, peak_residual_db,
    rms_residual_db, xcorr,
};
use crate::pick::{extract_excerpt, random_chunk_sizes};
use crate::reference::{
    apply_delay, biquad_deemph, fft_convolve_oneshot, polyphase_decimate_naive, zero_stuff,
};
use crate::report::NullTestReport;

/// Configuration for a single Layers A–D loopback null run.
#[derive(Debug, Clone)]
pub struct NullRunConfig {
    pub filter_override: Option<FilterArchetype>,
    pub shaper_override: Option<NoiseShaperProfile>,
    pub dac_bits: Option<u8>,
    pub wire_format: DacWireFormat,
    pub max_rate_hz: Option<u32>,
    pub deemphasis: bool,
    pub conv_config: ConvolutionConfig,
    pub full_track: bool,
    pub seed: u64,
    /// Optional explicit fixed gain override in dB (`<= 0.0`). When `None`, uses the pipeline's
    /// selected `HeadroomDecision` (with `m_q` + `1.0 dB` safety ceiling when testing hot audio).
    pub fixed_gain_db: Option<f64>,
    /// Override DAC headroom margin check (`1.0 dB` by default; `0.0` for bit-perfect Layer D).
    pub dac_margin_db: f64,
    /// Optional explicit output report directory (when `Some`, writes YAML report on completion).
    pub report_dir: Option<PathBuf>,
}

impl Default for NullRunConfig {
    fn default() -> Self {
        Self {
            filter_override: None,
            shaper_override: None,
            dac_bits: None,
            wire_format: DacWireFormat::Int32,
            max_rate_hz: None,
            deemphasis: false,
            conv_config: ConvolutionConfig::disabled(),
            full_track: false,
            seed: 0x2026_1126_A1B2_C3D4,
            fixed_gain_db: None,
            dac_margin_db: 1.0,
            report_dir: None,
        }
    }
}

/// Build `DacOutputCapabilities` matching `wire_format` and optional `max_rate_hz`.
pub fn build_dac_caps(wire_format: DacWireFormat, max_rate_hz: Option<u32>) -> DacOutputCapabilities {
    let standard_rates: &[u32] = &[
        44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
    ];
    let supported: Vec<(u32, DacWireFormat)> = standard_rates
        .iter()
        .copied()
        .map(|r| (r, wire_format))
        .collect();
    DacOutputCapabilities::from_supported_rates(&supported, max_rate_hz)
}

/// Run Layers A–D loopback null verification directly on a FLAC file on disk.
pub fn run_loopback_null_file(
    path: &Path,
    cfg: &NullRunConfig,
) -> Result<NullTestReport, String> {
    // One-shot decode for the independent reference path + metadata/MD5.
    let decoded = decode_flac_full_memory(path)?;
    let mut reader = FlacBlockReader::open(path)?;
    let md5_hex = reader
        .streaminfo_md5()
        .map(|m| md5_to_hex(&m))
        .unwrap_or_else(|| "none".to_string());

    // Stream-decode via FlacBlockReader using seeded random chunk sizes and assert it matches
    // decode_flac_full_memory bit-for-bit before entering DSP!
    let full_n = decoded.audio.left.len();
    let mut stream_l = Vec::with_capacity(full_n);
    let mut stream_r = Vec::with_capacity(full_n);
    for ask in random_chunk_sizes(full_n.max(1), cfg.seed ^ 0x5555_AAAA) {
        match reader.read_next_block(ask)? {
            Some(b) => {
                stream_l.extend_from_slice(&b.left);
                stream_r.extend_from_slice(&b.right);
            }
            None => break,
        }
    }
    while let Some(b) = reader.read_next_block(8_192)? {
        stream_l.extend_from_slice(&b.left);
        stream_r.extend_from_slice(&b.right);
    }
    if stream_l != decoded.audio.left || stream_r != decoded.audio.right {
        return Err(format!(
            "FlacBlockReader random-chunk decode differed from decode_flac_full_memory on '{}'",
            path.display()
        ));
    }

    let (ex_l, ex_r, excerpted) = extract_excerpt(
        &decoded.audio.left,
        &decoded.audio.right,
        decoded.spec.sample_rate_hz,
        cfg.full_track,
    );

    run_loopback_null_buffers(
        &path.to_string_lossy(),
        &md5_hex,
        decoded.spec,
        decoded.peak_meta,
        &ex_l,
        &ex_r,
        excerpted,
        cfg,
    )
}

/// Run Layers A–D loopback null verification on pre-decoded stereo `f64` buffers.
#[allow(clippy::too_many_arguments)]
pub fn run_loopback_null_buffers(
    track_label: &str,
    md5_hex: &str,
    spec: StreamSpec,
    peak_meta: TrackPeakMetadata,
    src_l: &[f64],
    src_r: &[f64],
    excerpted: bool,
    cfg: &NullRunConfig,
) -> Result<NullTestReport, String> {
    let n_src = src_l.len().min(src_r.len());
    if n_src == 0 {
        return Err("cannot run loopback null on 0-frame audio".to_string());
    }

    let mut policy = UpsamplerPolicy {
        filter_override: cfg.filter_override,
        shaper_override: cfg.shaper_override,
        ..UpsamplerPolicy::default()
    };
    if let Some(bits) = cfg.dac_bits {
        policy.dac_bits = bits;
    }
    let dac_caps = build_dac_caps(cfg.wire_format, cfg.max_rate_hz);
    let pipeline = AudioPipeline::with_rew_config_full_tail(policy, cfg.conv_config.clone(), true)?
        .with_dac_capabilities(dac_caps);

    let mut src_info = TrackSourceInfo::from_stream_spec(spec);
    src_info.deemphasis = cfg.deemphasis;
    let mut session = pipeline.start_track_with_source_info(spec, src_info, &peak_meta)?;
    session.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ cfg.seed);

    // Export read-only stage descriptors BEFORE streaming so the independent reference runs
    // exclusively from the session's exported taps/kernel/IR + report() latency.
    let up_export = session.upsampler_export();
    let ds_export = session.downsampler_kernel();
    let conv_ir_owned: Option<(Vec<f64>, Vec<f64>, usize)> = session
        .convolver_ir()
        .map(|ir| (ir.left.to_vec(), ir.right.to_vec(), ir.flush_tail_samples));
    let deemph_on = session.deemphasis_enabled();
    let report_latency = session.report().upsampler_latency_samples;

    // --- Stream through the REAL pipeline with seeded random chunk sizes (unity tap) ---
    let chunks = random_chunk_sizes(n_src, cfg.seed);
    let mut unity_l = Vec::new();
    let mut unity_r = Vec::new();
    let mut cursor = 0_usize;
    for sz in chunks {
        let blk = StereoF64Buffer {
            sample_rate_hz: spec.sample_rate_hz,
            left: src_l[cursor..cursor + sz].to_vec(),
            right: src_r[cursor..cursor + sz].to_vec(),
        };
        cursor += sz;
        let out = session.process_block_unity_f64(blk)?;
        unity_l.extend_from_slice(&out.left);
        unity_r.extend_from_slice(&out.right);
    }
    let tail = session.flush_unity_f64()?;
    unity_l.extend_from_slice(&tail.left);
    unity_r.extend_from_slice(&tail.right);
    let n_out = unity_l.len();
    assert_eq!(unity_l.len(), unity_r.len());

    // Compute exact safe fixed gain G (using cfg.fixed_gain_db if supplied, else ensuring zero
    // overs with m_q + dac_margin while respecting <= 0 dB).
    let unity_peak = unity_l
        .iter()
        .chain(unity_r.iter())
        .fold(0.0_f64, |acc, &v| acc.max(v.abs()));
    let unity_peak_db = if unity_peak > 0.0 {
        20.0 * unity_peak.log10()
    } else {
        -120.0
    };

    let m_q_db = if session.report().effective_shaper == NoiseShaperProfile::Bypass {
        0.0
    } else {
        quantizer_margin_db(session.report().output_bits, session.shaper_l1_sum())
    };
    // Review #10 TV1: when `cfg.fixed_gain_db` is `None`, use the pipeline's OWN selected
    // `HeadroomDecision::total_gain_db()` verbatim (never silently tighten it from `unity_peak_db`,
    // which would mask real headroom bugs!).
    let fixed_gain_db = match cfg.fixed_gain_db {
        Some(g) => g.min(0.0),
        None => session.report().headroom.total_gain_db(),
    };

    // If Bypass was selected at 0 dB but an explicit negative `fixed_gain_db` was requested,
    // reconfigure quantizer away from Bypass.
    if fixed_gain_db < -1e-9 && session.report().effective_shaper == NoiseShaperProfile::Bypass {
        let hr = HeadroomDecision {
            mode: if fixed_gain_db <= -3.0 + 1e-9 {
                HeadroomMode::Minus3Db
            } else {
                HeadroomMode::ZeroDb
            },
            peak_dbfs: Some(unity_peak_db),
            peak_kind: None,
            convolution_scale_db: if fixed_gain_db <= -3.0 + 1e-9 {
                (fixed_gain_db + 3.0).min(0.0)
            } else {
                fixed_gain_db
            },
            reason: "ap-verify fixed gain".to_string(),
        };
        session.reconfigure_quantizer_for_headroom(&hr)?;
        session.set_quantizer_seed(DEFAULT_QUANTIZER_SEED ^ cfg.seed);
    }
    let _ = m_q_db;

    // --- Quantize unity f64 -> InterleavedInt32Buffer through the real quantizer ---
    let mut no_tail = None;
    let out_rate = session.report().output_rate_hz;
    let mut quantized_frames: Vec<[i32; 2]> = Vec::with_capacity(n_out);
    let mut q_cursor = 0_usize;
    while q_cursor < n_out {
        let take = (n_out - q_cursor).min(65_536);
        let blk = StereoF64Buffer {
            sample_rate_hz: out_rate,
            left: unity_l[q_cursor..q_cursor + take].to_vec(),
            right: unity_r[q_cursor..q_cursor + take].to_vec(),
        };
        let pcm: InterleavedInt32Buffer =
            session.quantize_scaled_block(&blk, fixed_gain_db, &mut no_tail)?;
        quantized_frames.extend_from_slice(&pcm.frames);
        q_cursor += take;
    }

    // --- Real wire encoding (WaveRtFramePump::fill_slice) -> decode_wire_frames ---
    let wire_format = session.report().wire_format;
    let bpf = wire_format.bytes_per_frame();
    let mut wire_bytes = vec![0u8; quantized_frames.len() * bpf];
    let mut pump = WaveRtFramePump::new(&quantized_frames, wire_format);
    let mut byte_off = 0_usize;
    // Pump in varied DMA-like slice sizes to exercise WaveRtFramePump cursor & chunking too.
    let dma_chunk_frames = 4_096_usize;
    while byte_off < wire_bytes.len() {
        let end = (byte_off + dma_chunk_frames * bpf).min(wire_bytes.len());
        let written = pump.fill_slice(&mut wire_bytes[byte_off..end]);
        byte_off += written * bpf;
        if written == 0 {
            break;
        }
    }
    let wire_decoded = decode_wire_frames(&wire_bytes, wire_format)?;

    // --- Independent Reference Model (Layer A) ---
    let (ref_l, ref_r) = compute_independent_reference(
        &src_l[..n_src],
        &src_r[..n_src],
        session.report().internal_rate_hz,
        up_export.as_ref(),
        report_latency,
        deemph_on,
        conv_ir_owned.as_ref(),
        ds_export.as_ref(),
    )?;

    if ref_l.len() != unity_l.len() || ref_r.len() != unity_r.len() {
        return Err(format!(
            "Layer A length mismatch: reference={} vs pipeline={} (latency={report_latency})",
            ref_l.len(),
            unity_l.len()
        ));
    }

    let peak_l_db = peak_residual_db(&unity_l, &ref_l);
    let peak_r_db = peak_residual_db(&unity_r, &ref_r);
    let rms_l_db = rms_residual_db(&unity_l, &ref_l);
    let rms_r_db = rms_residual_db(&unity_r, &ref_r);
    let layer_a_peak_residual_db = peak_l_db.max(peak_r_db);
    let layer_a_rms_residual_db = rms_l_db.max(rms_r_db);
    let layer_a_passed = layer_a_peak_residual_db <= -250.0;

    // --- Layer C: Wire encoding bit-identical round-trip ---
    let layer_c_mismatches = if wire_decoded.len() != quantized_frames.len() {
        quantized_frames.len().abs_diff(wire_decoded.len())
    } else {
        quantized_frames
            .iter()
            .zip(wire_decoded.iter())
            .filter(|&(a, b)| a != b)
            .count()
    };
    let layer_c_passed = layer_c_mismatches == 0;

    // --- Layer B: Gain + Quantizer Harshness Detector (§2.4) ---
    let layer_b = evaluate_layer_b(
        &session,
        &unity_l,
        &unity_r,
        &wire_decoded,
        fixed_gain_db,
        cfg.dac_margin_db,
        DEFAULT_QUANTIZER_SEED ^ cfg.seed,
    )?;

    // --- Layer D: Identity check when bit-perfect ---
    let is_bit_perfect = up_export.is_none()
        && !deemph_on
        && conv_ir_owned.is_none()
        && ds_export.is_none()
        && fixed_gain_db.abs() < 1e-12
        && session.report().effective_shaper == NoiseShaperProfile::Bypass
        && session.report().output_bits >= spec.bits_per_sample as u8;
    let (layer_d_mismatches, layer_d_passed) = if is_bit_perfect {
        let levels = (1_u64 << (spec.bits_per_sample - 1)) as f64;
        let shift = 32 - u32::from(spec.bits_per_sample);
        let mut mm = 0_usize;
        for i in 0..n_src.min(wire_decoded.len()) {
            let exp_l = (src_l[i] * levels).round() as i32;
            let exp_r = (src_r[i] * levels).round() as i32;
            if wire_decoded[i] != [exp_l.wrapping_shl(shift), exp_r.wrapping_shl(shift)] {
                mm += 1;
            }
        }
        (mm, mm == 0 && wire_decoded.len() == n_src)
    } else {
        (0, true)
    };

    let rep = session.report();
    let passed = layer_a_passed && layer_b.passed && layer_c_passed && layer_d_passed;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let report = NullTestReport {
        timestamp_unix_ms: ts,
        track_path: track_label.to_string(),
        seed: cfg.seed,
        excerpted,
        source_frames: n_src,
        output_frames: n_out,
        md5: md5_hex.to_string(),
        filter: rep.dsp_plan.filter.to_string(),
        f_eff_hz: rep
            .dsp_plan
            .effective_nyquist_hz
            .unwrap_or(spec.sample_rate_hz / 2),
        convolver_hash: rep
            .convolver_content_hash
            .clone()
            .unwrap_or_else(|| "none".to_string()),
        source_rate_hz: spec.sample_rate_hz,
        internal_rate_hz: rep.internal_rate_hz,
        output_rate_hz: rep.output_rate_hz,
        deemphasis: deemph_on,
        shaper: rep.effective_shaper.to_string(),
        dac_bits: rep.output_bits,
        wire_format: wire_format.label().to_string(),
        gain_db: fixed_gain_db,
        latency_samples: report_latency,
        layer_a_peak_residual_db,
        layer_a_rms_residual_db,
        layer_a_passed,
        layer_b,
        layer_c_mismatches,
        layer_c_passed,
        layer_d_applicable: is_bit_perfect,
        layer_d_mismatches,
        layer_d_passed,
        passed,
    };

    if let Some(ref dir) = cfg.report_dir {
        let _ = crate::report::write_yaml(dir, &report)?;
    }

    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn compute_independent_reference(
    src_l: &[f64],
    src_r: &[f64],
    internal_rate_hz: u32,
    up_export: Option<&ap_dsp::upsampler::UpsamplerExport>,
    report_latency: usize,
    deemph_on: bool,
    conv_ir: Option<&(Vec<f64>, Vec<f64>, usize)>,
    ds_export: Option<&ap_dsp::downsampler::DownsamplerExport>,
) -> Result<(Vec<f64>, Vec<f64>), String> {
    // 1. Upsampler stage (one-shot zero-stuffed FFT convolution aligned strictly by report_latency).
    let (mut cur_l, mut cur_r) = if let Some(up) = up_export {
        let taps = up.prototype_taps();
        let target_len = src_l.len() * up.ratio;
        let zl = zero_stuff(src_l, up.ratio);
        let zr = zero_stuff(src_r, up.ratio);
        let cl = fft_convolve_oneshot(&zl, &taps);
        let cr = fft_convolve_oneshot(&zr, &taps);
        if report_latency + target_len > cl.len() {
            return Err(format!(
                "reported latency {report_latency} + target_len {target_len} exceeds conv len {}",
                cl.len()
            ));
        }
        (
            cl[report_latency..report_latency + target_len].to_vec(),
            cr[report_latency..report_latency + target_len].to_vec(),
        )
    } else {
        (src_l.to_vec(), src_r.to_vec())
    };

    // 2. De-emphasis stage (independent IEC 60908 50/15 us biquad).
    if deemph_on {
        cur_l = biquad_deemph(&cur_l, internal_rate_hz);
        cur_r = biquad_deemph(&cur_r, internal_rate_hz);
    }

    // 3. REW stereo convolution stage (one-shot independent complex FFT convolution).
    if let Some((ir_l, ir_r, flush_tail_samples)) = conv_ir {
        let target_len = cur_l.len() + *flush_tail_samples;
        let mut cl = fft_convolve_oneshot(&cur_l, ir_l);
        let mut cr = fft_convolve_oneshot(&cur_r, ir_r);
        cl.resize(target_len, 0.0);
        cr.resize(target_len, 0.0);
        cur_l = cl;
        cur_r = cr;
    }

    // 4. Channel delay stage (0 samples for NullChannelDelayer).
    cur_l = apply_delay(&cur_l, 0);
    cur_r = apply_delay(&cur_r, 0);

    // 5. Last-stage polyphase downsampler (direct naive implementation).
    if let Some(ds) = ds_export {
        cur_l = polyphase_decimate_naive(
            &cur_l,
            ds.up_l,
            ds.down_m,
            ds.pad_lead,
            ds.pad_tail,
            &ds.bank,
        );
        cur_r = polyphase_decimate_naive(
            &cur_r,
            ds.up_l,
            ds.down_m,
            ds.pad_lead,
            ds.pad_tail,
            &ds.bank,
        );
    }

    Ok((cur_l, cur_r))
}

fn evaluate_layer_b(
    session: &StreamingPipelineSession,
    unity_l: &[f64],
    unity_r: &[f64],
    wire_decoded: &[[i32; 2]],
    fixed_gain_db: f64,
    dac_margin_db: f64,
    seed: u64,
) -> Result<LayerBMetrics, String> {
    let rep = session.report();
    let dac_bits = rep.output_bits;
    let out_rate = f64::from(rep.output_rate_hz);
    let shaper = rep.effective_shaper;
    let coeffs = session.shaper_coeffs();
    let l1_sum = session.shaper_l1_sum();
    let shift = 32 - u32::from(dac_bits);
    let levels = (1_u64 << (dac_bits - 1)) as f64;
    let g_lin = if (fixed_gain_db - 0.0).abs() < 1e-12 {
        1.0
    } else {
        10.0_f64.powf(fixed_gain_db / 20.0)
    };

    let n = unity_l.len().min(wire_decoded.len());
    let mut s_l = Vec::with_capacity(n);
    let mut s_r = Vec::with_capacity(n);
    let mut e_l = Vec::with_capacity(n);
    let mut e_r = Vec::with_capacity(n);

    let mut max_abs_s = 0.0_f64;
    let mut max_abs_err_lsb = 0.0_f64;
    let mut sum_e_l = 0.0_f64;
    let mut sum_e_r = 0.0_f64;

    for i in 0..n {
        let sl = unity_l[i] * g_lin;
        let sr = unity_r[i] * g_lin;
        max_abs_s = max_abs_s.max(sl.abs()).max(sr.abs());
        s_l.push(sl);
        s_r.push(sr);

        let y_l_int = f64::from(wire_decoded[i][0] >> shift);
        let y_r_int = f64::from(wire_decoded[i][1] >> shift);
        let err_l = y_l_int - sl * levels;
        let err_r = y_r_int - sr * levels;
        max_abs_err_lsb = max_abs_err_lsb.max(err_l.abs()).max(err_r.abs());
        sum_e_l += err_l;
        sum_e_r += err_r;
        e_l.push(err_l);
        e_r.push(err_r);
    }

    let qstats = session.quantizer_stats();
    let overs_ind = overs(unity_l, fixed_gain_db, dac_bits) + overs(unity_r, fixed_gain_db, dac_bits);
    let overs_count = qstats.overs_count.max(overs_ind);
    let err_bound_lsb = if shaper == NoiseShaperProfile::Bypass {
        0.5 + 1e-9
    } else {
        1.5 * l1_sum + 1.5
    };
    let abs_mean_err_lsb = (sum_e_l / (n as f64)).abs().max((sum_e_r / (n as f64)).abs());

    let mut failures = Vec::new();

    // 1. No overs & peak within ceiling (1 - m_q - dac_margin).
    if overs_count > 0 {
        failures.push(format!("overs_count={overs_count} > 0"));
    }
    if shaper != NoiseShaperProfile::Bypass && dac_margin_db > 0.0 {
        let m_q_db = quantizer_margin_db(dac_bits, l1_sum);
        let ceiling_lin = 10.0_f64.powf(-(m_q_db + dac_margin_db) / 20.0);
        if max_abs_s > ceiling_lin * (1.0 + 1e-6) {
            failures.push(format!(
                "pre-quantizer peak {:.8} exceeds ceiling {:.8} (-(m_q+dac_margin))",
                max_abs_s, ceiling_lin
            ));
        }
    }

    // 2. Bounded error: max|e| <= 1.5 * L1 + 1.5 LSB.
    if max_abs_err_lsb > err_bound_lsb + 1e-6 {
        failures.push(format!(
            "max|e|={max_abs_err_lsb:.4} LSB exceeds bound {err_bound_lsb:.4} LSB"
        ));
    }

    // 3. Unbiased error: |mean(e)| < 0.01 LSB (for dithered/shaped paths with enough samples).
    let mean_tol = if n >= 65_536 { 0.01 } else { 0.05 };
    if shaper != NoiseShaperProfile::Bypass && abs_mean_err_lsb >= mean_tol {
        failures.push(format!(
            "|mean(e)|={abs_mean_err_lsb:.6} LSB >= {mean_tol} LSB"
        ));
    }

    // 4. Signal independence (harshness detector):
    //    - |rho(e, s)| < 1e-3 (when signal has non-trivial duration >= 100k samples)
    //    - 1/3-octave coherence (100 Hz..8 kHz) <= silent baseline + 3 dB.
    let rho_l = xcorr(&e_l, &s_l).abs();
    let rho_r = xcorr(&e_r, &s_r).abs();
    let max_abs_xcorr = rho_l.max(rho_r);

    // 5. NTF shape & silent-run coherence baseline (Review #10 TV2 + TV12: run on all cells
    //    with `n >= 16_384` — covering every L10 cell — and check BOTH left and right channels!).
    let mut max_coherence_excess_db = f64::NEG_INFINITY;
    let mut ntf_max_dev_db = 0.0_f64;

    if shaper != NoiseShaperProfile::Bypass && n >= 16_384 {
        // Under the null hypothesis (independent quantization error), flat white TPDF noise has
        // exact Pearson correlation standard error sigma_rho = 1 / sqrt(N). Scale the threshold
        // conservatively at 5.5 sigma for shorter cells while keeping the strict 1e-3 gate on long
        // shaped runs (>= 500k samples).
        let xcorr_limit = if coeffs.is_empty() {
            (5.5 / (n as f64).sqrt()).max(1e-3)
        } else if n >= 500_000 {
            1e-3
        } else {
            (5.5 / (n as f64).sqrt()).max(5e-3)
        };
        if max_abs_xcorr >= xcorr_limit {
            failures.push(format!(
                "|rho(e, s)|={max_abs_xcorr:.6e} >= {xcorr_limit:.2e} (signal-dependent quantization error)"
            ));
        }

        // Silent-run baseline with the exact same shaper & independent seed so coherence floor
        // reflects finite-sample Welch estimator variance.
        let mut silent_q = StreamingQuantizer::with_rate_options(
            shaper,
            dac_bits,
            rep.output_rate_hz,
            false,
        )?;
        silent_q.set_seed(seed ^ 0xDEAD_BEEF_CAFE_BABEu64);
        let zeros = StereoF64Buffer {
            sample_rate_hz: rep.output_rate_hz,
            left: vec![0.0_f64; n],
            right: vec![0.0_f64; n],
        };
        let silent_pcm = silent_q.quantize_block(&zeros);
        let e_silent_l: Vec<f64> = silent_pcm
            .frames
            .iter()
            .map(|f| f64::from(f[0] >> shift))
            .collect();
        let e_silent_r: Vec<f64> = silent_pcm
            .frames
            .iter()
            .map(|f| f64::from(f[1] >> shift))
            .collect();

        // Scale FFT length with sample rate (~0.7 s window on long excerpts; automatically capped
        // to `<= n / 16` so shorter L10 cells still average >= 30 overlapping Welch segments).
        let target_nfft = if rep.output_rate_hz >= 352_800 {
            262_144_usize
        } else if rep.output_rate_hz >= 176_400 {
            131_072_usize
        } else if rep.output_rate_hz >= 88_200 {
            65_536_usize
        } else {
            32_768_usize
        };
        let max_pow2 = ((n / 16).max(256) + 1).next_power_of_two() / 2;
        let coh_nfft = target_nfft.min(max_pow2.max(256));
        let coh_sig_l = coherence_bands(&e_l, &s_l, out_rate, coh_nfft);
        let coh_sig_r = coherence_bands(&e_r, &s_r, out_rate, coh_nfft);
        let coh_base_l = coherence_bands(&e_silent_l, &s_l, out_rate, coh_nfft);
        let coh_base_r = coherence_bands(&e_silent_r, &s_r, out_rate, coh_nfft);
        // Compare BOTH left and right channels (40 bands total) against the silent baseline's peak
        // band + margin (TV12). On runs `< 1M` samples (~3 s @ 352.8k), 100 Hz has only 1-2 FFT
        // bins so finite-sample null-hypothesis max-of-40-bands fluctuates by ~3.5 dB (while true
        // signal-correlated harshness jumps +10..+25 dB); full >= 1M runs enforce the strict +3 dB.
        let coh_tol_db = if n >= 1_000_000 { 3.0 } else { 4.5 };
        let base_peak_db = coh_base_l
            .iter()
            .chain(coh_base_r.iter())
            .map(|b| b.coherence_db)
            .fold(f64::NEG_INFINITY, f64::max);
        let mut worst_excess = f64::NEG_INFINITY;
        for b in coh_sig_l.iter().chain(coh_sig_r.iter()) {
            let excess = b.coherence_db - base_peak_db;
            if excess > worst_excess {
                worst_excess = excess;
            }
        }
        max_coherence_excess_db = worst_excess;
        if max_coherence_excess_db > coh_tol_db {
            failures.push(format!(
                "1/3-oct coherence exceeded silent baseline by {max_coherence_excess_db:+.2} dB (> +{coh_tol_db:.1} dB)"
            ));
        }

        // Welch PSD vs analytical NTF within +-1.0 dB in-band (up to min(20 kHz, 0.4 * fs)).
        // Require >= 24 non-overlapping frames (`>= 47` 50%-overlapped Welch segments) so the
        // lowest 500 Hz 1/3-oct band has tight Welch variance well under +-1.0 dB.
        let psd_nfft = if rep.output_rate_hz >= 176_400 {
            16_384
        } else {
            8_192
        };
        if n >= psd_nfft * 24 {
            let f_hi = 20_000.0_f64.min(0.4 * out_rate);
            let dev_l = ntf_max_inband_deviation_db(&e_l, coeffs, out_rate, f_hi, psd_nfft);
            let dev_r = ntf_max_inband_deviation_db(&e_r, coeffs, out_rate, f_hi, psd_nfft);
            ntf_max_dev_db = dev_l.max(dev_r);
            if ntf_max_dev_db > 1.0 {
                failures.push(format!(
                    "in-band NTF PSD max deviation {ntf_max_dev_db:.3} dB exceeds +-1.0 dB"
                ));
            }
        }
    } else if !max_coherence_excess_db.is_finite() {
        max_coherence_excess_db = 0.0;
    }

    let passed = failures.is_empty();
    Ok(LayerBMetrics {
        overs_count,
        max_abs_err_lsb,
        err_bound_lsb,
        abs_mean_err_lsb,
        max_abs_xcorr,
        max_coherence_excess_db,
        ntf_max_dev_db,
        passed,
        failure_reasons: failures,
    })
}
