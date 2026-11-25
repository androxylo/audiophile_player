//! Ultimate Mode `W0 -> W1 -> W2 -> W3 -> W4` bounded 30 s look-ahead streaming track engine
//! (`docs/research/07-ultimate-mode.md` §§2–5, Mandates M1–M4, `OQ-33..OQ-41`).
//!
//! Hard invariants enforced here:
//! - **M1 (30 s cap):** `W0` reads strictly `<= 30 s` of source audio before `W1..W3`; the
//!   downstream buffer is capped at `ULTIMATE_MAX_BUFFERED_SECONDS = 30`.
//! - **M3 (`< 30 s` bypass):** If header duration `< 30 s` OR EOF occurs inside `W0`, bypass
//!   ultimate rules cleanly and play via normal-mode defaults labelled `"normal (<30 s)"`.
//! - **M4 (Streaming vs. Local):** Streaming sources NEVER buffer beyond 30 s, NEVER use mega-tap
//!   `gauss-apod-4m`, and ALWAYS apply `-3 dB` upsampling safety (`OQ-33`).
//! - **Strict Per-Track Independence (`OQ-40`):** Track B's detectors, filter choice, `M30`, `M`,
//!   and fixed gain `G_B` depend 100% on Track B alone. Track A's carried gapless tail `z_A[n]`
//!   (already scaled by `G_A`) is strictly additive output-only right before quantization.
//! - **Single-Pass Unity Render (`W2 -> W3`):** Renders `W0` ONCE at unity gain -> measures exact
//!   `M30` -> computes mandatory fixed whole-track gain `G` -> quantizes `W2` and continues `W4`
//!   seamlessly through EOF measuring whole-track `M` -> atomic YAML learn-on-play (`U5b`) ONLY on
//!   clean EOF with verified `STREAMINFO.md5`. Mid-track skip discards cleanly with zero YAML write.

use std::path::Path;
use std::time::Instant;

use ap_control::state::PlayModeLabel;
use ap_core::buffer::StereoF64Buffer;
use ap_core::config::UltimateConfig;
use ap_core::headroom::{
    HeadroomDecision, HeadroomMode, quantizer_margin_db, select_ultimate_headroom,
};
use ap_core::track_db::{PostChainKey, TrackDbStore, file_size_and_mtime};
use ap_dsp::analysis::{DETECTOR_VERSION, TrackAnalyzer};
use ap_dsp::convolution::ConvolutionTail;
use ap_dsp::fusion::{
    CachedWholeTrackEvidence, build_track_db_entry, fuse_track_evidence,
};
use ap_dsp::pipeline::{AudioPipeline, GaplessCarryover, UltimateDecisionReport};
use ap_dsp::selector::{
    ULTIMATE_LOOKAHEAD_SECONDS, UpsamplerPolicy, check_ultimate_throughput, select_ultimate_plan,
};
use ap_source::flac::AudioBlockSource;
use ap_source::md5::md5_to_hex;

use crate::prefetch::{BoundedBlockStreamBuffer, StreamEvent};

/// Runtime configuration & Track DB handle active when `[ultimate] enabled = true` or `--ultimate`.
#[derive(Debug, Clone)]
pub struct UltimateRuntimeContext {
    pub config: UltimateConfig,
    pub store: TrackDbStore,
}

/// Outcome of streaming a single track through the Ultimate `W0..W4` engine.
#[derive(Debug)]
pub enum UltimateStreamResult {
    Played {
        next_carryover: Option<GaplessCarryover>,
        next_pending_end: bool,
        #[allow(dead_code)]
        wrote_yaml: bool,
    },
    Aborted,
    Failed(String),
}

fn peak_linear_of_buffer(buf: &StereoF64Buffer) -> f64 {
    let mut peak = 0.0_f64;
    for &s in buf.left.iter().chain(buf.right.iter()) {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
    }
    peak
}

fn linear_to_dbfs(peak_linear: f64) -> f64 {
    if peak_linear > 0.0 && peak_linear.is_finite() {
        20.0 * peak_linear.log10()
    } else {
        -120.0
    }
}

/// Execute the full `W0 -> W1 -> W2 -> W3 -> W4` Ultimate Mode track streaming lifecycle.
///
/// Works identically for local `FlacBlockReader` (`local_path = Some(...)`) and streaming
/// `HttpAudioSource` (`local_path = None`, `is_streaming = true`).
#[allow(clippy::too_many_arguments)]
pub fn stream_track_ultimate<S, F>(
    pipeline: &AudioPipeline,
    ult: &UltimateRuntimeContext,
    buf: &BoundedBlockStreamBuffer,
    epoch: u64,
    is_aborted: F,
    source: &mut S,
    display_path: &Path,
    local_path: Option<&Path>,
    upnp_uri: Option<String>,
    upnp_next_gen: Option<u64>,
    mode_label: PlayModeLabel,
    duration_hint_s: Option<f64>,
    incoming_carryover: &mut Option<GaplessCarryover>,
    had_pending_prev_end: &mut bool,
    gapless_carryover: bool,
    src_block_frames: usize,
) -> UltimateStreamResult
where
    S: AudioBlockSource,
    F: Fn() -> bool,
{
    let spec = source.spec();
    let is_streaming = source.is_streaming();
    let raw_peak_meta = source.peak_meta();
    let track_meta = source.track_meta();
    let header_md5 = source.streaminfo_md5();
    let header_md5_hex = header_md5.as_ref().map(md5_to_hex);
    let total_duration_s = duration_hint_s.or_else(|| {
        source
            .total_frames()
            .filter(|&f| f > 0)
            .map(|f| f as f64 / f64::from(spec.sample_rate_hz.max(1)))
    });

    // Helper: drain Track A's unconsumed carryover cleanly onto Track A before starting Track B
    // when rates differ or when Track B fails before producing audio.
    let flush_prev_on_incompatible = |buf: &BoundedBlockStreamBuffer,
                                      carry: &mut Option<GaplessCarryover>,
                                      pending_end: &mut bool|
     -> bool {
        if let Some(tail) = carry.take() {
            if let Ok(pcm) = pipeline.drain_gapless_carryover(tail) {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    *pending_end = false;
                    return false;
                }
            }
        }
        if *pending_end {
            *pending_end = false;
            if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                return false;
            }
        }
        true
    };

    // --- M3 Early Short-Track Check (< 30 s known from header / DIDL) ---
    let header_short = total_duration_s
        .is_some_and(|d| d.is_finite() && d < f64::from(ULTIMATE_LOOKAHEAD_SECONDS));
    if header_short {
        return stream_short_track_normal(
            pipeline,
            buf,
            epoch,
            &is_aborted,
            source,
            Vec::new(),
            true,
            display_path,
            upnp_uri,
            upnp_next_gen,
            mode_label,
            total_duration_s,
            incoming_carryover,
            had_pending_prev_end,
            gapless_carryover,
            src_block_frames,
        );
    }

    // --- W0: Bounded <= 30 s Window Acquisition (Mandates M1, M2, M4) ---
    let max_window_frames = u64::from(spec.sample_rate_hz) * u64::from(ULTIMATE_LOOKAHEAD_SECONDS);
    let mut w0_blocks: Vec<StereoF64Buffer> = Vec::new();
    let mut w0_frames: u64 = 0;
    let mut hit_eof_in_w0 = false;
    let mut analyzer = TrackAnalyzer::new(spec.sample_rate_hz, spec.bits_per_sample);

    while w0_frames < max_window_frames {
        if is_aborted() || buf.current_epoch() != epoch {
            return UltimateStreamResult::Aborted;
        }
        let remaining = (max_window_frames - w0_frames) as usize;
        let ask = src_block_frames.min(remaining).max(1);
        match source.read_next_block(ask) {
            Ok(Some(b)) if b.is_empty() => continue,
            Ok(Some(b)) => {
                analyzer.push_block(&b);
                w0_frames += b.left.len() as u64;
                w0_blocks.push(b);
            }
            Ok(None) => {
                hit_eof_in_w0 = true;
                break;
            }
            Err(err) => {
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return UltimateStreamResult::Aborted;
                }
                return UltimateStreamResult::Failed(format!(
                    "FLAC/stream decode error during W0 on '{}': {err}",
                    display_path.display()
                ));
            }
        }
    }

    if w0_blocks.is_empty() {
        if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
            return UltimateStreamResult::Aborted;
        }
        return UltimateStreamResult::Failed(format!(
            "Skipped empty (0-frame) track '{}'",
            display_path.display()
        ));
    }

    // --- M3 Runtime Short-Track Check (e.g. total_samples == 0 header with < 30 s body) ---
    if hit_eof_in_w0 && w0_frames < max_window_frames {
        let actual_dur_s = w0_frames as f64 / f64::from(spec.sample_rate_hz.max(1));
        return stream_short_track_normal(
            pipeline,
            buf,
            epoch,
            &is_aborted,
            source,
            w0_blocks,
            false,
            display_path,
            upnp_uri,
            upnp_next_gen,
            mode_label,
            Some(actual_dur_s),
            incoming_carryover,
            had_pending_prev_end,
            gapless_carryover,
            src_block_frames,
        );
    }

    // --- W1: Strict Per-Track Cache Lookup + Fusion + Selection (OQ-40, U2, U3, U4) ---
    let (file_size, file_mtime) = local_path
        .and_then(file_size_and_mtime)
        .unwrap_or((0, 0));

    let cached_entry = if is_streaming {
        None
    } else {
        ult.store
            .load_for_track(local_path, header_md5_hex.as_deref())
    };
    let cached_current = cached_entry.as_ref().is_some_and(|e| {
        e.is_results_current(
            DETECTOR_VERSION,
            header_md5_hex.as_deref(),
            local_path.map(|_| file_size),
            local_path.map(|_| file_mtime),
        )
    });
    let cached_wt = cached_entry
        .as_ref()
        .map(|e| CachedWholeTrackEvidence::from_track_db_entry(e, cached_current));

    let early_digital = track_meta.is_early_digital_era();
    let orig_below = track_meta
        .original_sample_rate_hz
        .is_some_and(|r| r < spec.sample_rate_hz);
    let w30_snapshot = analyzer.snapshot(
        hit_eof_in_w0,
        early_digital,
        orig_below,
        track_meta.is_dsd_hint,
    );

    let fused = fuse_track_evidence(
        spec.sample_rate_hz,
        spec.bits_per_sample,
        &raw_peak_meta,
        &track_meta,
        Some(&w30_snapshot),
        cached_wt.as_ref(),
    );

    let policy = pipeline.policy();
    let mut selection = match select_ultimate_plan(
        spec,
        is_streaming,
        total_duration_s,
        &fused,
        &policy,
        ult.config.genre_rules,
        cached_current,
    ) {
        Ok(sel) => sel,
        Err(err) => {
            if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                return UltimateStreamResult::Aborted;
            }
            return UltimateStreamResult::Failed(format!(
                "Ultimate selection error on '{}': {err}",
                display_path.display()
            ));
        }
    };

    // --- W2: Single-Pass Unity-Gain Render of W0 + Throughput Self-Check (§4.3) ---
    let placeholder_hr = HeadroomDecision {
        mode: HeadroomMode::ZeroDb,
        peak_dbfs: None,
        peak_kind: None,
        convolution_scale_db: 0.0,
        reason: "ultimate W2 unity-gain measurement".to_string(),
    };

    let render_w2 = |plan: &ap_dsp::selector::SelectedDspPlan| -> Result<
        (
            ap_dsp::pipeline::StreamingPipelineSession,
            Vec<StereoF64Buffer>,
            f64,
            f64,
        ),
        String,
    > {
        let mut sess = pipeline.start_track_with_plan_and_headroom(
            spec,
            plan.clone(),
            placeholder_hr.clone(),
        )?;
        let started = Instant::now();
        let mut rendered = Vec::with_capacity(w0_blocks.len());
        let mut peak = 0.0_f64;
        for blk in &w0_blocks {
            if is_aborted() || buf.current_epoch() != epoch {
                return Err("ABORTED".to_string());
            }
            let out = sess.process_block_unity_f64(blk.clone())?;
            if !out.is_empty() {
                peak = peak.max(peak_linear_of_buffer(&out));
                rendered.push(out);
            }
        }
        let wall_s = started.elapsed().as_secs_f64();
        Ok((sess, rendered, peak, wall_s))
    };

    let (mut session, mut w2_rendered, mut m30_linear, mut w2_wall_s) =
        match render_w2(&selection.plan) {
            Ok(v) => v,
            Err(e) if e == "ABORTED" => return UltimateStreamResult::Aborted,
            Err(err) => {
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return UltimateStreamResult::Aborted;
                }
                return UltimateStreamResult::Failed(format!(
                    "Ultimate W2 render error on '{}': {err}",
                    display_path.display()
                ));
            }
        };

    // Free W0 source blocks if throughput passes; keep until check finishes in case fallback runs.
    let w0_audio_s = w0_frames as f64 / f64::from(spec.sample_rate_hz.max(1));
    let tp_check = check_ultimate_throughput(
        selection.plan.filter,
        spec.sample_rate_hz,
        w0_audio_s,
        w2_wall_s,
        total_duration_s,
    );
    let mut final_speed_x_rt = tp_check.measured_x_rt;

    if !tp_check.passed {
        if let Some(fb_arch) = tp_check.fallback_filter {
            if fb_arch != selection.plan.filter {
                eprintln!("  [ultimate throughput] {}", tp_check.detail);
                selection.warnings.push(tp_check.detail.clone());
                let mut fb_policy = UpsamplerPolicy {
                    filter_override: Some(fb_arch),
                    ..policy
                };
                fb_policy.filter_override = Some(fb_arch);
                if let Ok(fb_sel) = select_ultimate_plan(
                    spec,
                    is_streaming,
                    total_duration_s,
                    &fused,
                    &fb_policy,
                    ult.config.genre_rules,
                    cached_current,
                ) {
                    match render_w2(&fb_sel.plan) {
                        Ok((s2, r2, p2, w2)) => {
                            session = s2;
                            w2_rendered = r2;
                            m30_linear = p2;
                            w2_wall_s = w2;
                            final_speed_x_rt = w0_audio_s / w2_wall_s.max(1e-9);
                            selection.plan = fb_sel.plan;
                            selection.winning_rule = format!("throughput_fallback({fb_arch})");
                        }
                        Err(e) if e == "ABORTED" => return UltimateStreamResult::Aborted,
                        Err(_) => {}
                    }
                }
            }
        }
    }
    drop(w0_blocks);

    // Build exact PostChainKey matching this active linear chain.
    let rew_hash = session
        .report()
        .convolver_content_hash
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let post_key = PostChainKey {
        upsampler: selection.plan.filter.to_string(),
        f_eff_hz: selection
            .plan
            .effective_nyquist_hz
            .unwrap_or(spec.sample_rate_hz / 2),
        output_rate_hz: session.report().output_rate_hz,
        deemphasis: selection.plan.deemphasis,
        hf_noise_filter: selection.plan.hf_noise_filter,
        rew_filter_hash: rew_hash,
    };

    // Look up cached exact whole-track M (local files only; NEVER used for streams per OQ-33!).
    let cached_exact_m_db = if is_streaming || !cached_current {
        None
    } else {
        cached_entry
            .as_ref()
            .and_then(|e| e.find_post_chain_peak(&post_key))
    };

    let m30_dbfs = linear_to_dbfs(m30_linear);
    let m_q_db = quantizer_margin_db(session.report().output_bits, session.shaper_l1_sum());
    let conv_boost_db = session.convolver_max_boost_db();

    // Compute MANDATORY FIXED WHOLE-TRACK GAIN `G` (OQ-33) — locked for the entire track!
    let fixed_headroom = select_ultimate_headroom(
        &fused.peak_meta,
        spec.bits_per_sample,
        is_streaming,
        Some(conv_boost_db),
        cached_exact_m_db,
        Some(m30_dbfs),
        m_q_db,
    );
    let fixed_gain_db = fixed_headroom.total_gain_db();
    session.set_fixed_headroom(fixed_headroom);

    // Attach Showcase Decision Report (U13).
    let decision_report = UltimateDecisionReport {
        mode: if cached_exact_m_db.is_some() {
            "ultimate: cached".to_string()
        } else {
            selection.mode_label.clone()
        },
        winning_rule: selection.winning_rule.clone(),
        fired_rules: selection.fired_rules.clone(),
        cache_hit: cached_current || cached_exact_m_db.is_some(),
        m30_dbfs: Some(m30_dbfs),
        cached_m_dbfs: cached_exact_m_db,
        fixed_gain_db,
        quantizer_margin_db: m_q_db,
        throughput_x_rt: Some(final_speed_x_rt),
        warnings: selection.warnings.clone(),
    };
    session.set_ultimate_report(decision_report);

    // --- W3: Strict Additive Gapless Tail Separation (`OQ-40`) + Quantize W2 & Emit TrackStart ---
    let mut additive_tail: Option<(ConvolutionTail, usize)> = None;
    let mut had_gapless_carryover = false;

    if let Some(carry) = incoming_carryover.take() {
        let rate = session.report().output_rate_hz;
        if carry.output_rate_hz == rate && carry.upsampler.is_none() {
            // Track A already flushed its upsampler and pre-scaled `conv_tail` by G_A.
            if let Some(q_st) = carry.quantizer_state {
                session.restore_quantizer_state_if_compatible(
                    q_st,
                    carry.effective_shaper,
                    carry.output_bits,
                );
            }
            if let Some(ct) = carry.convolver {
                if !ct.left.is_empty() {
                    additive_tail = Some((ct, 0));
                    had_gapless_carryover = true;
                }
            }
            if *had_pending_prev_end {
                *had_pending_prev_end = false;
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
            }
        } else {
            // Incompatible rate or normal-mode carryover with un-flushed upsampler history:
            // drain cleanly onto Track A first so Track B stays 100% independent.
            if let Ok(pcm) = pipeline.drain_gapless_carryover(carry) {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    return UltimateStreamResult::Aborted;
                }
            }
            if *had_pending_prev_end {
                *had_pending_prev_end = false;
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
            }
        }
    } else if *had_pending_prev_end {
        *had_pending_prev_end = false;
        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
            return UltimateStreamResult::Aborted;
        }
    }

    // Emit TrackStart carrying full UltimateDecisionReport.
    if !buf.push_for_epoch(
        epoch,
        StreamEvent::TrackStart {
            path: display_path.to_path_buf(),
            upnp_uri,
            upnp_next_gen,
            mode_label,
            report: session.report().clone(),
            total_duration_s,
            had_gapless_carryover,
        },
    ) {
        return UltimateStreamResult::Aborted;
    }

    // Quantize and push the already-rendered unity-gain W2 blocks (ZERO double-rendering!).
    let mut whole_track_m_linear = m30_linear;
    for unity_blk in w2_rendered {
        if is_aborted() || buf.current_epoch() != epoch {
            return UltimateStreamResult::Aborted;
        }
        match session.quantize_scaled_block(&unity_blk, fixed_gain_db, &mut additive_tail) {
            Ok(pcm) if !pcm.is_empty() => {
                if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    return UltimateStreamResult::Aborted;
                }
            }
            Ok(_) => {}
            Err(err) => {
                return UltimateStreamResult::Failed(format!(
                    "Ultimate W3 quantize error on '{}': {err}",
                    display_path.display()
                ));
            }
        }
    }

    // --- W4: Stream Remainder through EOF (same fixed G, continuous TrackAnalyzer + exact M) ---
    if !hit_eof_in_w0 {
        loop {
            if is_aborted() || buf.current_epoch() != epoch {
                // Mid-track skip: discard cleanly WITHOUT writing YAML!
                return UltimateStreamResult::Aborted;
            }
            match source.read_next_block(src_block_frames) {
                Ok(Some(blk)) if blk.is_empty() => continue,
                Ok(Some(blk)) => {
                    analyzer.push_block(&blk);
                    let unity = match session.process_block_unity_f64(blk) {
                        Ok(u) => u,
                        Err(err) => {
                            return UltimateStreamResult::Failed(format!(
                                "Ultimate W4 DSP error on '{}': {err}",
                                display_path.display()
                            ));
                        }
                    };
                    if !unity.is_empty() {
                        whole_track_m_linear =
                            whole_track_m_linear.max(peak_linear_of_buffer(&unity));
                        match session.quantize_scaled_block(
                            &unity,
                            fixed_gain_db,
                            &mut additive_tail,
                        ) {
                            Ok(pcm) if !pcm.is_empty() => {
                                if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                                    return UltimateStreamResult::Aborted;
                                }
                            }
                            Ok(_) => {}
                            Err(err) => {
                                return UltimateStreamResult::Failed(format!(
                                    "Ultimate W4 quantize error on '{}': {err}",
                                    display_path.display()
                                ));
                            }
                        }
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    return UltimateStreamResult::Failed(format!(
                        "FLAC/stream decode error during W4 on '{}': {err}",
                        display_path.display()
                    ));
                }
            }
        }
    }

    // Clean EOF flush (unity-gain + pre-scaled gapless tail export).
    let (next_carryover, next_pending_end) = if gapless_carryover {
        match session.flush_for_gapless_unity_f64(fixed_gain_db) {
            Ok((ending_unity, tail_peak_unity, carry)) => {
                whole_track_m_linear = whole_track_m_linear.max(tail_peak_unity);
                if !ending_unity.is_empty() {
                    if let Ok(pcm) = session.quantize_scaled_block(
                        &ending_unity,
                        fixed_gain_db,
                        &mut additive_tail,
                    ) {
                        if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm))
                        {
                            return UltimateStreamResult::Aborted;
                        }
                    }
                }
                (carry, true)
            }
            Err(err) => {
                return UltimateStreamResult::Failed(format!(
                    "Ultimate flush error on '{}': {err}",
                    display_path.display()
                ));
            }
        }
    } else {
        match session.flush_unity_f64() {
            Ok(ending_unity) => {
                whole_track_m_linear =
                    whole_track_m_linear.max(peak_linear_of_buffer(&ending_unity));
                if !ending_unity.is_empty() {
                    if let Ok(pcm) = session.quantize_scaled_block(
                        &ending_unity,
                        fixed_gain_db,
                        &mut additive_tail,
                    ) {
                        if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm))
                        {
                            return UltimateStreamResult::Aborted;
                        }
                    }
                }
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                (None, false)
            }
            Err(err) => {
                return UltimateStreamResult::Failed(format!(
                    "Ultimate flush error on '{}': {err}",
                    display_path.display()
                ));
            }
        }
    };

    // --- U5b Learn-on-Play: Persist Verified Whole-Track Results Atomically ONCE at Clean EOF ---
    let mut wrote_yaml = false;
    if ult.config.learn_on_play && !is_streaming && local_path.is_some() {
        let computed_md5 = source.computed_md5();
        let md5_ok = match (header_md5, computed_md5) {
            (Some(expected), Some(actual)) => {
                if expected != actual {
                    eprintln!(
                        "  [ultimate learn-on-play] MD5 mismatch on '{}' (header={} computed={}); refusing to write YAML",
                        display_path.display(),
                        md5_to_hex(&expected),
                        md5_to_hex(&actual),
                    );
                    false
                } else {
                    true
                }
            }
            // If STREAMINFO.md5 is unset (0), allow persistence if computed_md5 succeeded cleanly.
            (None, Some(_)) => true,
            // Stream did not reach clean EOF with zero decode errors -> never write.
            (_, None) => false,
        };

        if md5_ok {
            let wt_analysis =
                analyzer.snapshot(true, early_digital, orig_below, track_meta.is_dsd_hint);
            let final_md5_hex = header_md5_hex.or_else(|| computed_md5.as_ref().map(md5_to_hex));
            let mut entry = build_track_db_entry(
                display_path.to_string_lossy().into_owned(),
                file_size,
                file_mtime,
                final_md5_hex,
                &wt_analysis,
                selection.plan.deemphasis,
                selection.plan.filter.to_string(),
                selection.winning_rule.clone(),
            );
            let whole_m_dbfs = linear_to_dbfs(whole_track_m_linear);
            entry.upsert_post_chain_peak(post_key, whole_m_dbfs, false);
            match ult.store.save_preserving_user(local_path, entry) {
                Ok(_) => wrote_yaml = true,
                Err(err) => {
                    eprintln!(
                        "  [ultimate learn-on-play warn] failed to save YAML for '{}': {err}",
                        display_path.display()
                    );
                }
            }
        }
    }

    UltimateStreamResult::Played {
        next_carryover,
        next_pending_end,
        wrote_yaml,
    }
}

/// Mandate M3: play tracks `< 30 s` through normal-mode defaults and label as `"normal (<30 s)"`.
#[allow(clippy::too_many_arguments)]
fn stream_short_track_normal<S, F>(
    pipeline: &AudioPipeline,
    buf: &BoundedBlockStreamBuffer,
    epoch: u64,
    is_aborted: &F,
    source: &mut S,
    pre_read_blocks: Vec<StereoF64Buffer>,
    read_remaining_from_source: bool,
    display_path: &Path,
    upnp_uri: Option<String>,
    upnp_next_gen: Option<u64>,
    mode_label: PlayModeLabel,
    total_duration_s: Option<f64>,
    incoming_carryover: &mut Option<GaplessCarryover>,
    had_pending_prev_end: &mut bool,
    gapless_carryover: bool,
    src_block_frames: usize,
) -> UltimateStreamResult
where
    S: AudioBlockSource,
    F: Fn() -> bool,
{
    let spec = source.spec();
    let peak_meta = source.peak_meta();
    let track_meta = source.track_meta();

    let mut src_info = ap_dsp::selector::TrackSourceInfo::from_stream_spec(spec);
    src_info.is_streaming_source = source.is_streaming();
    src_info.deemphasis = track_meta
        .ap_deemphasis
        .unwrap_or(track_meta.cuesheet_pre_emphasis);

    let mut session = match pipeline.start_track_with_source_info(spec, src_info, &peak_meta) {
        Ok(s) => s,
        Err(err) => {
            return UltimateStreamResult::Failed(format!(
                "DSP pipeline error on short track '{}': {err}",
                display_path.display()
            ));
        }
    };

    let fixed_g = session.report().headroom.total_gain_db();
    let m_q = quantizer_margin_db(session.report().output_bits, session.shaper_l1_sum());
    session.set_ultimate_report(UltimateDecisionReport {
        mode: "normal (<30 s)".to_string(),
        winning_rule: "short_track_bypass".to_string(),
        fired_rules: vec!["M3(<30 s)".to_string()],
        cache_hit: false,
        m30_dbfs: None,
        cached_m_dbfs: None,
        fixed_gain_db: fixed_g,
        quantizer_margin_db: m_q,
        throughput_x_rt: None,
        warnings: Vec::new(),
    });

    if let Some(carry) = incoming_carryover.take() {
        if let Ok(pcm) = pipeline.drain_gapless_carryover(carry) {
            if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                return UltimateStreamResult::Aborted;
            }
        }
    }
    if *had_pending_prev_end {
        *had_pending_prev_end = false;
        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
            return UltimateStreamResult::Aborted;
        }
    }

    if !buf.push_for_epoch(
        epoch,
        StreamEvent::TrackStart {
            path: display_path.to_path_buf(),
            upnp_uri,
            upnp_next_gen,
            mode_label,
            report: session.report().clone(),
            total_duration_s,
            had_gapless_carryover: false,
        },
    ) {
        return UltimateStreamResult::Aborted;
    }

    let mut push_blk = |blk: StereoF64Buffer| -> Result<bool, String> {
        let pcm = session.process_block(blk)?;
        if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
            return Ok(false);
        }
        Ok(true)
    };

    for blk in pre_read_blocks {
        if is_aborted() || buf.current_epoch() != epoch {
            return UltimateStreamResult::Aborted;
        }
        match push_blk(blk) {
            Ok(true) => {}
            Ok(false) => return UltimateStreamResult::Aborted,
            Err(err) => return UltimateStreamResult::Failed(err),
        }
    }

    if read_remaining_from_source {
        loop {
            if is_aborted() || buf.current_epoch() != epoch {
                return UltimateStreamResult::Aborted;
            }
            match source.read_next_block(src_block_frames) {
                Ok(Some(blk)) if blk.is_empty() => continue,
                Ok(Some(blk)) => match push_blk(blk) {
                    Ok(true) => {}
                    Ok(false) => return UltimateStreamResult::Aborted,
                    Err(err) => return UltimateStreamResult::Failed(err),
                },
                Ok(None) => break,
                Err(err) => return UltimateStreamResult::Failed(err),
            }
        }
    }

    if gapless_carryover {
        match session.flush_for_gapless() {
            Ok((pcm, carry)) => {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    return UltimateStreamResult::Aborted;
                }
                UltimateStreamResult::Played {
                    next_carryover: carry,
                    next_pending_end: true,
                    wrote_yaml: false,
                }
            }
            Err(err) => UltimateStreamResult::Failed(err),
        }
    } else {
        match session.flush() {
            Ok(pcm) => {
                if !pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                    return UltimateStreamResult::Aborted;
                }
                if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                    return UltimateStreamResult::Aborted;
                }
                UltimateStreamResult::Played {
                    next_carryover: None,
                    next_pending_end: false,
                    wrote_yaml: false,
                }
            }
            Err(err) => UltimateStreamResult::Failed(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use ap_core::config::ConvolutionConfig;
    use ap_core::format::StreamSpec;
    use ap_core::headroom::TrackPeakMetadata;
    use ap_core::metadata::TrackMetadata;
    use ap_dsp::selector::FilterArchetype;

    use crate::prefetch::{ULTIMATE_MAX_BUFFERED_FRAMES, ULTIMATE_MAX_BUFFERED_SECONDS};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ap_ult_test_{label}_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Synthetic in-memory `AudioBlockSource` supporting local & streaming fixtures, custom tags,
    /// unknown `total_frames = None`, and forced MD5 mismatch for §7 & §5.5 TDD gates.
    struct SyntheticSource {
        spec: StreamSpec,
        peak_meta: TrackPeakMetadata,
        track_meta: TrackMetadata,
        is_streaming: bool,
        claimed_total_frames: Option<u64>,
        streaminfo_md5: Option<[u8; 16]>,
        computed_md5_at_eof: Option<[u8; 16]>,
        total_frames: usize,
        cursor: usize,
        amp: f64,
        freq_hz: f64,
        clipped: bool,
    }

    impl SyntheticSource {
        fn sine(rate: u32, duration_s: f64, amp: f64, is_streaming: bool) -> Self {
            let total_frames = (rate as f64 * duration_s).round() as usize;
            Self {
                spec: StreamSpec {
                    sample_rate_hz: rate,
                    bits_per_sample: 16,
                    channels: 2,
                },
                peak_meta: TrackPeakMetadata::default(),
                track_meta: TrackMetadata::default(),
                is_streaming,
                claimed_total_frames: Some(total_frames as u64),
                streaminfo_md5: Some([0x11; 16]),
                computed_md5_at_eof: Some([0x11; 16]),
                total_frames,
                cursor: 0,
                amp,
                freq_hz: 1_000.0,
                clipped: false,
            }
        }
    }

    impl AudioBlockSource for SyntheticSource {
        fn spec(&self) -> StreamSpec {
            self.spec
        }
        fn peak_meta(&self) -> TrackPeakMetadata {
            self.peak_meta
        }
        fn is_streaming(&self) -> bool {
            self.is_streaming
        }
        fn total_frames(&self) -> Option<u64> {
            self.claimed_total_frames
        }
        fn track_meta(&self) -> TrackMetadata {
            self.track_meta.clone()
        }
        fn streaminfo_md5(&self) -> Option<[u8; 16]> {
            self.streaminfo_md5
        }
        fn computed_md5(&self) -> Option<[u8; 16]> {
            if self.cursor >= self.total_frames {
                self.computed_md5_at_eof
            } else {
                None
            }
        }
        fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
            if self.cursor >= self.total_frames {
                return Ok(None);
            }
            let take = max_frames.min(self.total_frames - self.cursor);
            let mut left = Vec::with_capacity(take);
            let mut right = Vec::with_capacity(take);
            let rate = self.spec.sample_rate_hz as f64;
            for i in 0..take {
                let n = self.cursor + i;
                let mut v = (2.0 * PI * self.freq_hz * n as f64 / rate).sin() * self.amp;
                if self.clipped {
                    v = v.clamp(-0.9999, 0.9999);
                }
                left.push(v);
                right.push(v);
            }
            self.cursor += take;
            Ok(Some(StereoF64Buffer {
                sample_rate_hz: self.spec.sample_rate_hz,
                left,
                right,
            }))
        }
    }

    fn drain_track_events(
        buf: &BoundedBlockStreamBuffer,
        epoch: u64,
    ) -> (ap_dsp::pipeline::PipelineReport, usize, i32) {
        let mut report = None;
        let mut frames = 0usize;
        let mut max_abs_i32 = 0i32;
        loop {
            match buf.pop_for_epoch_timeout(epoch, Duration::from_secs(2)) {
                Some(StreamEvent::TrackStart { report: r, .. }) => report = Some(r),
                Some(StreamEvent::PcmBlock(b)) => {
                    frames += b.frames.len();
                    for &[l, r] in &b.frames {
                        max_abs_i32 = max_abs_i32.max(l.saturating_abs()).max(r.saturating_abs());
                    }
                }
                Some(StreamEvent::TrackEnd) => break,
                other => panic!("unexpected stream event: {other:?}"),
            }
        }
        (report.expect("missing TrackStart"), frames, max_abs_i32)
    }

    fn make_fast_ultimate_pipeline() -> AudioPipeline {
        // Use Null filter at 352.8k or simple-test at 44.1k for fast debug/release TDD runs while
        // still exercising the full W0..W4 ultimate state machine.
        AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy::default(),
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap()
    }

    #[test]
    fn gate_m3_short_track_header_and_zero_total_samples_use_normal_mode() {
        let db_root = temp_dir("m3");
        let ult = UltimateRuntimeContext {
            config: UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            store: TrackDbStore::new(db_root.clone(), false),
        };
        let pipeline = make_fast_ultimate_pipeline();

        // Case A: header explicitly claims 10 s (< 30 s) -> normal (<30 s), no YAML written.
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();
        let mut src_a = SyntheticSource::sine(352_800, 1.0, 0.5, false);
        let dummy_path = db_root.join("short_a.flac");
        let mut carry = None;
        let mut pend = false;
        let res = stream_track_ultimate(
            &pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_a,
            &dummy_path,
            Some(&dummy_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(1.0),
            &mut carry,
            &mut pend,
            false,
            65_536,
        );
        assert!(matches!(res, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let (rep_a, frames_a, _) = drain_track_events(&buf, ep);
        assert_eq!(frames_a, 352_800);
        assert_eq!(rep_a.ultimate_report.unwrap().mode, "normal (<30 s)");

        // Case B (§5.5): total_samples = 0 / None header with 1 s body -> EOF inside W0 -> normal (<30 s)!
        let mut src_b = SyntheticSource::sine(352_800, 1.0, 0.5, false);
        src_b.claimed_total_frames = None;
        let res_b = stream_track_ultimate(
            &pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_b,
            &dummy_path,
            Some(&dummy_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            None,
            &mut carry,
            &mut pend,
            false,
            65_536,
        );
        assert!(matches!(res_b, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let (rep_b, frames_b, _) = drain_track_events(&buf, ep);
        assert_eq!(frames_b, 352_800);
        assert_eq!(rep_b.ultimate_report.unwrap().mode, "normal (<30 s)");

        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn gate_learn_on_play_exact_m_zero_wasted_headroom_and_md5_mismatch_refusal() {
        let db_root = temp_dir("exact_m");
        let track_path = db_root.join("track_32s.flac");
        fs::write(&track_path, b"dummy").unwrap();
        let ult = UltimateRuntimeContext {
            config: UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            store: TrackDbStore::new(db_root.clone(), false),
        };
        let pipeline = make_fast_ultimate_pipeline();

        // Use native 8fS (352.8 kHz) at 32 s? No, 8 kHz upsampled or 44.1 kHz with --filter null?
        // Using 44.1 kHz with --filter null keeps 31 s tiny (~1.37M frames -> < 30 ms in test!)
        // while exercising full W0..W4, exact M measurement, learn-on-play YAML, and cached replay!
        let fast_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let fast_pipeline = AudioPipeline::with_rew_config_full_tail(
            fast_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();

        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();

        // Track amplitude = 0.85 (-1.41 dBFS, untagged):
        // - Play 1 (uncached): G <= G_HR (-3.0 dB), writes YAML ONCE with exact M ~= -1.41 dBFS.
        let mut src1 = SyntheticSource::sine(44_100, 31.0, 0.85, false);
        let mut carry = None;
        let mut pend = false;
        let buf_c = Arc::clone(&buf);
        let drain_h = std::thread::spawn(move || drain_track_events(&buf_c, ep));
        let r1 = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src1,
            &track_path,
            Some(&track_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r1, UltimateStreamResult::Played { wrote_yaml: true, .. }));
        let (rep1, frames1, _) = drain_h.join().unwrap();
        assert_eq!(frames1, 44_100 * 31);
        assert!(
            rep1.headroom.total_gain_db() <= -3.0 + 1e-6,
            "uncached untagged track must apply G <= G_HR (-3 dB), got {}",
            rep1.headroom.total_gain_db()
        );
        assert_eq!(rep1.ultimate_report.as_ref().unwrap().mode, "ultimate: live");

        // - Play 2 (cached exact M = -1.41 dBFS): G = 0.0 dB (zero wasted headroom!), 0 overs!
        let mut src2 = SyntheticSource::sine(44_100, 31.0, 0.85, false);
        let buf_c2 = Arc::clone(&buf);
        let drain_h2 = std::thread::spawn(move || drain_track_events(&buf_c2, ep));
        let r2 = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src2,
            &track_path,
            Some(&track_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r2, UltimateStreamResult::Played { wrote_yaml: true, .. }));
        let (rep2, _, _) = drain_h2.join().unwrap();
        let u2 = rep2.ultimate_report.as_ref().unwrap();
        assert_eq!(u2.mode, "ultimate: cached");
        assert!(u2.cache_hit);
        assert!(
            rep2.headroom.total_gain_db().abs() < 1e-6,
            "cached exact M=-1.41 dBFS must apply 0.0 dB (no wasted -3 dB!), got {}",
            rep2.headroom.total_gain_db()
        );

        // - MD5 mismatch fixture (§5.5): refuses to write YAML!
        let corrupt_path = db_root.join("corrupt.flac");
        fs::write(&corrupt_path, b"corrupt").unwrap();
        let mut src_bad = SyntheticSource::sine(44_100, 31.0, 0.5, false);
        src_bad.streaminfo_md5 = Some([0xAA; 16]);
        src_bad.computed_md5_at_eof = Some([0xBB; 16]);
        let buf_c3 = Arc::clone(&buf);
        let drain_h3 = std::thread::spawn(move || drain_track_events(&buf_c3, ep));
        let r3 = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_bad,
            &corrupt_path,
            Some(&corrupt_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r3, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let _ = drain_h3.join().unwrap();
        assert!(ult.store.load_for_track(Some(&corrupt_path), None).is_none());

        // - Mid-window skip discards cleanly with ZERO YAML write!
        let skip_path = db_root.join("skip.flac");
        fs::write(&skip_path, b"skip").unwrap();
        let mut src_skip = SyntheticSource::sine(44_100, 31.0, 0.5, false);
        src_skip.streaminfo_md5 = Some([0xCC; 16]);
        let abort_flag = AtomicBool::new(true);
        let r_skip = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || abort_flag.load(Ordering::Relaxed),
            &mut src_skip,
            &skip_path,
            Some(&skip_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(r_skip, UltimateStreamResult::Aborted));
        assert!(ult.store.load_for_track(Some(&skip_path), None).is_none());

        let _ = pipeline;
        let _ = fs::remove_dir_all(&db_root);
    }

    #[test]
    fn gate_streaming_m4_oq33_stale_tag_disproof_and_strict_per_track_independence_oq40() {
        let db_root = temp_dir("stream_oq40");
        let ult = UltimateRuntimeContext {
            config: UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: true,
            },
            store: TrackDbStore::new(db_root.clone(), false),
        };
        let fast_policy = UpsamplerPolicy {
            filter_override: Some(FilterArchetype::Null),
            ..UpsamplerPolicy::default()
        };
        let fast_pipeline = AudioPipeline::with_rew_config_full_tail(
            fast_policy,
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::with_limits(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
        );
        let ep = buf.current_epoch();

        // 1. Streaming source with quiet tag MUST STILL enforce -3 dB safety (OQ-33) & write 0 YAML!
        let mut src_stream = SyntheticSource::sine(44_100, 31.0, 0.25, true);
        src_stream.peak_meta.true_peak_linear = Some(0.25);
        let stream_label = PathBuf::from("tidal://track/1");
        let mut carry = None;
        let mut pend = false;
        let buf_c = Arc::clone(&buf);
        let h1 = std::thread::spawn(move || drain_track_events(&buf_c, ep));
        let rs = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_stream,
            &stream_label,
            None,
            Some("http://127.0.0.1/stream.flac".to_string()),
            None,
            PlayModeLabel::UpnpRenderer,
            Some(31.0),
            &mut carry,
            &mut pend,
            false,
            16_384,
        );
        assert!(matches!(rs, UltimateStreamResult::Played { wrote_yaml: false, .. }));
        let (rep_s, _, _) = h1.join().unwrap();
        assert_eq!(rep_s.headroom.mode, HeadroomMode::Minus3Db);
        assert!((rep_s.headroom.total_gain_db() - (-3.0)).abs() < 1e-6);

        // 2. Stale peak tag disproof (§5.5): tag claims 0.25 (-12 dBFS) but W0 peaks at 0.90!
        let stale_path = db_root.join("stale.flac");
        fs::write(&stale_path, b"stale").unwrap();
        let mut src_stale = SyntheticSource::sine(44_100, 31.0, 0.90, false);
        src_stale.peak_meta.replaygain_track_peak = Some(0.25);
        src_stale.streaminfo_md5 = Some([0xDD; 16]);
        src_stale.computed_md5_at_eof = Some([0xDD; 16]);
        let buf_c2 = Arc::clone(&buf);
        let h2 = std::thread::spawn(move || drain_track_events(&buf_c2, ep));
        let _ = stream_track_ultimate(
            &fast_pipeline,
            &ult,
            &buf,
            ep,
            || false,
            &mut src_stale,
            &stale_path,
            Some(&stale_path),
            None,
            None,
            PlayModeLabel::SingleFile,
            Some(31.0),
            &mut carry,
            &mut pend,
            false,
            16_384,
        );
        let (rep_stale, _, _) = h2.join().unwrap();
        assert_eq!(rep_stale.headroom.mode, HeadroomMode::Minus3Db);
        assert!(
            rep_stale
                .ultimate_report
                .as_ref()
                .unwrap()
                .warnings
                .iter()
                .any(|w| w.contains("stale peak tag")),
            "expected stale peak tag warning"
        );

        // 3. Strict per-track independence (OQ-40): Track B's fixed gain G_B and M30 are 100%
        // identical whether or not Track A carried a loud gapless ConvolutionTail!
        let indep_path = db_root.join("indep_b.flac");
        fs::write(&indep_path, b"b").unwrap();
        let run_b = |with_tail: bool| -> (f64, f64) {
            let mut src_b = SyntheticSource::sine(44_100, 31.0, 0.40, false);
            src_b.peak_meta.true_peak_linear = Some(0.40);
            src_b.streaminfo_md5 = Some([0xEE; 16]);
            src_b.computed_md5_at_eof = Some([0xEE; 16]);
            let mut inc = if with_tail {
                Some(GaplessCarryover {
                    output_rate_hz: 44_100,
                    output_bits: 16,
                    effective_shaper: ap_dsp::selector::NoiseShaperProfile::Bypass,
                    upsampler: None,
                    deemphasis: None,
                    convolver: Some(ConvolutionTail {
                        sample_rate_hz: 44_100,
                        left: vec![0.95; 512],
                        right: vec![0.95; 512],
                    }),
                    quantizer_state: None,
                })
            } else {
                None
            };
            let mut p = false;
            let buf_x = Arc::clone(&buf);
            let hx = std::thread::spawn(move || drain_track_events(&buf_x, ep));
            let _ = stream_track_ultimate(
                &fast_pipeline,
                &ult,
                &buf,
                ep,
                || false,
                &mut src_b,
                &indep_path,
                Some(&indep_path),
                None,
                None,
                PlayModeLabel::SingleFile,
                Some(31.0),
                &mut inc,
                &mut p,
                false,
                16_384,
            );
            let (rep, _, _) = hx.join().unwrap();
            let u = rep.ultimate_report.unwrap();
            (u.fixed_gain_db, u.m30_dbfs.unwrap())
        };
        let (g_alone, m30_alone) = run_b(false);
        let (g_after_a, m30_after_a) = run_b(true);
        assert_eq!(g_alone, g_after_a, "OQ-40: Track A must NEVER alter Track B's fixed gain!");
        assert!(
            (m30_alone - m30_after_a).abs() < 1e-12,
            "OQ-40: Track A must NEVER alter Track B's M30!"
        );

        let _ = fs::remove_dir_all(&db_root);
    }
}
