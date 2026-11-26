//! Phase 1 (§1 & U7) lossless FFT/UPOLS upsampler equivalence & full-tail REW verification.
//!
//! Acceptance gates (`docs/research/07-ultimate-mode.md` §1 & U7):
//! - FFT/UPOLS upsampler matches direct-form `PolyphaseFirUpsampler` within `<= -280 dBFS` RMS
//!   and `<= -260 dBFS` peak across all supported rates, non-power-of-two ratios, multi-partition
//!   mega-tap filters, asymmetric/minimum-phase banks, arbitrary chunk splits, and gapless seams.
//! - `PreparedStereoIr::from_stereo_ir_with_options(..., full_tail = true)` preserves 100% of
//!   active tails below `-200 dB` (both `flush()` and gapless `flush_split_tail()`), skips only
//!   literally all-zero partitions, and produces bit-identical deterministic output across parallel
//!   multi-partition runs.

use std::f64::consts::PI;
use std::sync::Arc;

use ap_core::buffer::StereoF64Buffer;
use ap_dsp::convolution::{PartitionedStereoConvolver, PreparedStereoIr, StereoConvolver};
use ap_dsp::selector::{FilterArchetype, FilterDesignMetrics, FilterIdentity, FilterPhase};
use ap_dsp::upsampler::{
    GaussianFilterSpec, PolyphaseFirUpsampler, Upsampler, design_gaussian_filter,
};
use ap_dsp::upsampler_fft::{FftUpolsUpsampler, PreparedUpsamplerKernel};

fn synth_stereo_signal(rate_hz: u32, frames: usize) -> StereoF64Buffer {
    let fs = rate_hz as f64;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for n in 0..frames {
        let t = (n as f64) / fs;
        // Rich broadband multitone + DC offset + transient impulse to stress every bin & phase.
        let impulse = if n == frames / 3 { 0.45 } else { 0.0 };
        let l = 0.05
            + 0.25 * (2.0 * PI * 997.0 * t).sin()
            + 0.20 * (2.0 * PI * 6_311.0 * t).cos()
            + 0.15 * (2.0 * PI * (0.39 * fs) * t).sin()
            + impulse;
        let r = -0.04
            + 0.22 * (2.0 * PI * 1_481.0 * t).cos()
            + 0.18 * (2.0 * PI * 9_127.0 * t).sin()
            + 0.14 * (2.0 * PI * (0.43 * fs) * t).cos()
            - 0.5 * impulse;
        left.push(l);
        right.push(r);
    }
    StereoF64Buffer {
        sample_rate_hz: rate_hz,
        left,
        right,
    }
}

fn run_oneshot(up: &mut dyn Upsampler, input: &StereoF64Buffer) -> StereoF64Buffer {
    let mut out = up.process_block(input.clone()).expect("process_block");
    let tail = up.flush().expect("flush");
    out.left.extend_from_slice(&tail.left);
    out.right.extend_from_slice(&tail.right);
    out
}

fn run_chunked(
    up: &mut dyn Upsampler,
    input: &StereoF64Buffer,
    chunk_sizes: &[usize],
) -> StereoF64Buffer {
    let mut out_l = Vec::new();
    let mut out_r = Vec::new();
    let mut pos = 0usize;
    let mut idx = 0usize;
    while pos < input.len() {
        let step = chunk_sizes[idx % chunk_sizes.len()].min(input.len() - pos);
        idx += 1;
        let blk = StereoF64Buffer {
            sample_rate_hz: input.sample_rate_hz,
            left: input.left[pos..pos + step].to_vec(),
            right: input.right[pos..pos + step].to_vec(),
        };
        let emitted = up.process_block(blk).expect("chunk process_block");
        out_l.extend_from_slice(&emitted.left);
        out_r.extend_from_slice(&emitted.right);
        pos += step;
    }
    let tail = up.flush().expect("chunk flush");
    let rate = tail.sample_rate_hz;
    out_l.extend_from_slice(&tail.left);
    out_r.extend_from_slice(&tail.right);
    StereoF64Buffer {
        sample_rate_hz: rate,
        left: out_l,
        right: out_r,
    }
}

fn rms_and_peak_diff_db(a: &StereoF64Buffer, b: &StereoF64Buffer) -> (f64, f64) {
    assert_eq!(a.sample_rate_hz, b.sample_rate_hz, "sample rate mismatch");
    assert_eq!(a.left.len(), b.left.len(), "left length mismatch");
    assert_eq!(a.right.len(), b.right.len(), "right length mismatch");
    let mut sum_sq = 0.0_f64;
    let mut peak = 0.0_f64;
    let count = (a.left.len() * 2) as f64;
    for (&x, &y) in a.left.iter().zip(b.left.iter()).chain(a.right.iter().zip(b.right.iter())) {
        let d = (x - y).abs();
        sum_sq += d * d;
        if d > peak {
            peak = d;
        }
    }
    let rms = (sum_sq / count.max(1.0)).sqrt();
    let rms_db = if rms > 0.0 { 20.0 * rms.log10() } else { -400.0 };
    let peak_db = if peak > 0.0 { 20.0 * peak.log10() } else { -400.0 };
    (rms_db, peak_db)
}

#[test]
fn section1_fft_upsampler_matches_direct_polyphase_within_minus_280_dbfs_across_all_rates() {
    let cases = [
        (FilterArchetype::GaussLongLp, 44_100_u32, 352_800_u32, 3_500_usize),
        (FilterArchetype::GaussLongLp, 48_000_u32, 384_000_u32, 3_500_usize),
        (FilterArchetype::GaussHiresLp, 88_200_u32, 352_800_u32, 4_000_usize),
        (FilterArchetype::GaussHiresLp, 96_000_u32, 384_000_u32, 4_000_usize),
        (FilterArchetype::GaussHiresLp, 192_000_u32, 384_000_u32, 4_000_usize),
        // Non-power-of-two ratio (12x: 32 kHz -> 384 kHz)
        (FilterArchetype::GaussLongLp, 32_000_u32, 384_000_u32, 2_400_usize),
    ];

    for (arch, src_rate, dst_rate, frames) in cases {
        let spec = GaussianFilterSpec::for_default_archetype(arch, src_rate, dst_rate)
            .expect("valid gaussian spec");
        let designed = design_gaussian_filter(spec);
        let input = synth_stereo_signal(src_rate, frames);

        let mut direct = PolyphaseFirUpsampler::from_gaussian(designed.clone());
        let mut fft = FftUpolsUpsampler::from_gaussian(designed).expect("fft upsampler");

        let ref_out = run_oneshot(&mut direct, &input);
        let fft_out = run_oneshot(&mut fft, &input);

        let (rms_db, peak_db) = rms_and_peak_diff_db(&ref_out, &fft_out);
        assert!(
            rms_db <= -280.0 && peak_db <= -260.0,
            "{arch} {src_rate}->{dst_rate} Hz failed §1 equivalence: rms={rms_db:.2} dBFS, peak={peak_db:.2} dBFS"
        );
    }
}

#[test]
fn section1_fft_upsampler_chunk_invariance_and_gapless_carryover_equivalence() {
    let spec = GaussianFilterSpec::for_default_archetype(
        FilterArchetype::GaussLongLp,
        44_100,
        352_800,
    )
    .expect("spec");
    let designed = design_gaussian_filter(spec);
    let full_input = synth_stereo_signal(44_100, 6_000);

    // 1. Direct reference one-shot.
    let mut direct_ref = PolyphaseFirUpsampler::from_gaussian(designed.clone());
    let ref_out = run_oneshot(&mut direct_ref, &full_input);

    // 2. Arbitrary prime/weird chunk sizes through FFT/UPOLS.
    let mut fft_chunked = FftUpolsUpsampler::from_gaussian(designed.clone()).expect("fft");
    let chunked_out = run_chunked(&mut fft_chunked, &full_input, &[17, 513, 1, 2049, 89, 1024, 333]);
    let (rms_c, peak_c) = rms_and_peak_diff_db(&ref_out, &chunked_out);
    assert!(
        rms_c <= -280.0 && peak_c <= -260.0,
        "chunked FFT failed §1 gate: rms={rms_c:.2} dBFS, peak={peak_c:.2} dBFS"
    );

    // 3. Gapless split A + B via FftUpolsUpsampler carryover.
    let split = 2_777;
    let track_a = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: full_input.left[..split].to_vec(),
        right: full_input.right[..split].to_vec(),
    };
    let track_b = StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: full_input.left[split..].to_vec(),
        right: full_input.right[split..].to_vec(),
    };

    let mut up_a = FftUpolsUpsampler::from_gaussian(designed.clone()).expect("fft A");
    let mut combined_l = Vec::new();
    let mut combined_r = Vec::new();

    let a_blk = up_a.process_block(track_a.clone()).expect("a process");
    combined_l.extend_from_slice(&a_blk.left);
    combined_r.extend_from_slice(&a_blk.right);
    let (a_tail, carry) = up_a.flush_for_gapless().expect("a gapless flush");
    combined_l.extend_from_slice(&a_tail.left);
    combined_r.extend_from_slice(&a_tail.right);
    let carry = carry.expect("carryover must be produced");

    // Verify FilterIdentity mismatch rejection before valid attach.
    let wrong_spec = GaussianFilterSpec::for_default_archetype(
        FilterArchetype::GaussLongLp,
        48_000,
        384_000,
    )
    .expect("wrong spec");
    let mut wrong_up =
        FftUpolsUpsampler::from_gaussian(design_gaussian_filter(wrong_spec)).expect("wrong fft");
    let carry = wrong_up
        .set_incoming_carryover(carry)
        .expect_err("mismatched identity must reject carryover");

    let mut up_b = FftUpolsUpsampler::from_gaussian(designed.clone()).expect("fft B");
    up_b.set_incoming_carryover(carry.clone())
        .expect("matching identity must accept carryover");
    let b_blk = up_b.process_block(track_b.clone()).expect("b process");
    combined_l.extend_from_slice(&b_blk.left);
    combined_r.extend_from_slice(&b_blk.right);
    let b_tail = up_b.flush().expect("b flush");
    combined_l.extend_from_slice(&b_tail.left);
    combined_r.extend_from_slice(&b_tail.right);

    let gapless_out = StereoF64Buffer {
        sample_rate_hz: 352_800,
        left: combined_l,
        right: combined_r,
    };
    let (rms_g, peak_g) = rms_and_peak_diff_db(&ref_out, &gapless_out);
    assert!(
        rms_g <= -280.0 && peak_g <= -260.0,
        "gapless FFT carryover failed §1 gate: rms={rms_g:.2} dBFS, peak={peak_g:.2} dBFS"
    );

    // 4. Cross-executor interoperability: FFT carryover -> PolyphaseFirUpsampler Track B!
    let mut poly_b = PolyphaseFirUpsampler::from_gaussian(designed);
    poly_b
        .set_incoming_carryover(carry)
        .expect("PolyphaseFirUpsampler must accept FFT carryover cleanly");
    let pb_blk = poly_b.process_block(track_b).expect("poly b");
    let pb_tail = poly_b.flush().expect("poly b flush");
    let mut cross_l = a_blk.left;
    cross_l.extend_from_slice(&a_tail.left);
    cross_l.extend_from_slice(&pb_blk.left);
    cross_l.extend_from_slice(&pb_tail.left);
    let mut cross_r = a_blk.right;
    cross_r.extend_from_slice(&a_tail.right);
    cross_r.extend_from_slice(&pb_blk.right);
    cross_r.extend_from_slice(&pb_tail.right);
    let cross_out = StereoF64Buffer {
        sample_rate_hz: 352_800,
        left: cross_l,
        right: cross_r,
    };
    let (rms_x, peak_x) = rms_and_peak_diff_db(&ref_out, &cross_out);
    assert!(
        rms_x <= -280.0 && peak_x <= -260.0,
        "cross-executor carryover failed §1 gate: rms={rms_x:.2} dBFS, peak={peak_x:.2} dBFS"
    );
}

#[test]
fn section1_fft_upsampler_handles_multipartition_and_causal_minimum_phase_banks() {
    // 1. Causal asymmetric (minimum-phase-like) bank: pad_lead = m - 1, pad_tail = 0.
    let ratio = 4usize;
    let m = 48usize;
    let mut bank = vec![vec![0.0_f64; m]; ratio];
    for p in 0..ratio {
        let mut sum = 0.0;
        for i in 0..m {
            // Causal exponentially damped oscillatory prototype (non-symmetric!).
            let d = (m - 1 - i) as f64 + (p as f64) / (ratio as f64);
            let v = (-0.18 * d).exp() * (0.37 * d + 0.1 * (p as f64)).cos();
            bank[p][i] = v;
            sum += v;
        }
        for v in &mut bank[p] {
            *v /= sum;
        }
    }

    let metrics = FilterDesignMetrics {
        phase: FilterPhase::Minimum,
        num_taps: ratio * m,
        taps_per_phase: m,
        length_seconds: 0.0,
        group_delay_seconds: 0.0,
        passband_edge_001db_hz: 20_000.0,
        level_at_20khz_db: Some(0.0),
        stopband_edge_hz: 44_100.0,
        stopband_attenuation_db: -150.0,
        l1_norm: 1.0,
        l1_norm_db: 0.0,
    };
    let identity = FilterIdentity {
        archetype: FilterArchetype::MinphaseLong,
        source_rate_hz: 88_200,
        target_rate_hz: 352_800,
        ratio,
        num_taps: ratio * m,
        taps_per_phase: m,
        fc_hz_bits: 0,
        sigma_f_hz_bits: 0,
        effective_nyquist_hz: None,
    };

    let kernel = Arc::new(
        PreparedUpsamplerKernel::from_polyphase_bank(
            "test-causal-mp",
            88_200,
            352_800,
            ratio,
            m,
            m - 1, // causal minimum-phase alignment
            0,
            Arc::new(bank.clone()),
            metrics,
            identity,
        )
        .expect("causal kernel"),
    );

    let input = synth_stereo_signal(88_200, 1_500);
    let mut fft = FftUpolsUpsampler::new(kernel);
    let fft_out = run_chunked(&mut fft, &input, &[113, 401, 256, 730]);

    // Direct time-domain reference with pad_lead = m - 1, pad_tail = 0.
    let mut pad_l = vec![0.0_f64; m - 1];
    pad_l.extend_from_slice(&input.left);
    let mut pad_r = vec![0.0_f64; m - 1];
    pad_r.extend_from_slice(&input.right);
    let mut ref_l = vec![0.0_f64; input.len() * ratio];
    let mut ref_r = vec![0.0_f64; input.len() * ratio];
    for n in 0..input.len() {
        for p in 0..ratio {
            let mut al = 0.0;
            let mut ar = 0.0;
            for i in 0..m {
                al += pad_l[n + i] * bank[p][i];
                ar += pad_r[n + i] * bank[p][i];
            }
            ref_l[n * ratio + p] = al;
            ref_r[n * ratio + p] = ar;
        }
    }
    let ref_buf = StereoF64Buffer {
        sample_rate_hz: 352_800,
        left: ref_l,
        right: ref_r,
    };
    let (rms_db, peak_db) = rms_and_peak_diff_db(&ref_buf, &fft_out);
    assert!(
        rms_db <= -280.0 && peak_db <= -260.0,
        "causal min-phase FFT upsampler failed §1 gate: rms={rms_db:.2} dBFS, peak={peak_db:.2} dBFS"
    );

    // 2. Multi-partition (>= 9 partitions) mega-tap synthetic kernel triggering parallel bin-slice MAC.
    //    8x ratio with m = 70,000 taps/phase -> 560,000 total taps -> 9 partitions @ B_out = 65,536.
    //    Use a sparse impulse bank (exact known output = weighted delayed copies) to verify exactness
    //    without spending O(N * 560k) in direct FIR!
    let ratio_big = 8usize;
    let m_big = 70_001usize; // odd, m_half = 35,000
    let m_half_big = m_big / 2;
    let mut sparse_bank = vec![vec![0.0_f64; m_big]; ratio_big];
    // Place known taps near start, middle, and far tail across multiple partitions.
    let tap_positions = [3usize, 12_345, m_half_big, 55_555, m_big - 2];
    for p in 0..ratio_big {
        let mut s = 0.0;
        for (idx, &pos) in tap_positions.iter().enumerate() {
            let w = (0.5_f64).powi((idx as i32) + 1) * (1.0 + 0.05 * (p as f64));
            sparse_bank[p][pos] = w;
            s += w;
        }
        for &pos in &tap_positions {
            sparse_bank[p][pos] /= s;
        }
    }
    let big_id = FilterIdentity {
        archetype: FilterArchetype::GaussApod4M,
        source_rate_hz: 44_100,
        target_rate_hz: 352_800,
        ratio: ratio_big,
        num_taps: ratio_big * m_big,
        taps_per_phase: m_big,
        fc_hz_bits: 0,
        sigma_f_hz_bits: 0,
        effective_nyquist_hz: None,
    };
    let big_kernel = Arc::new(
        PreparedUpsamplerKernel::from_polyphase_bank(
            "test-multipartition",
            44_100,
            352_800,
            ratio_big,
            m_big,
            m_half_big,
            m_half_big,
            Arc::new(sparse_bank.clone()),
            metrics,
            big_id,
        )
        .expect("multipartition kernel"),
    );
    assert!(
        big_kernel.num_partitions >= 9,
        "expected >= 9 partitions to exercise parallel bin-slice MAC, got {}",
        big_kernel.num_partitions
    );

    let big_in = synth_stereo_signal(44_100, 10_000);
    let mut big_fft1 = FftUpolsUpsampler::new(Arc::clone(&big_kernel));
    let out1 = run_oneshot(&mut big_fft1, &big_in);

    // Determinism: second parallel run must be 100% bit-identical.
    let mut big_fft2 = FftUpolsUpsampler::new(big_kernel);
    let out2 = run_oneshot(&mut big_fft2, &big_in);
    assert_eq!(out1.left, out2.left, "parallel FFT upsampler must be bit-deterministic (L)");
    assert_eq!(out1.right, out2.right, "parallel FFT upsampler must be bit-deterministic (R)");

    // Exact sparse direct reference.
    let mut ref_big_l = vec![0.0_f64; big_in.len() * ratio_big];
    let mut ref_big_r = vec![0.0_f64; big_in.len() * ratio_big];
    for n in 0..big_in.len() {
        for p in 0..ratio_big {
            let mut al = 0.0;
            let mut ar = 0.0;
            for &pos in &tap_positions {
                let x_idx = (n as isize) + (pos as isize) - (m_half_big as isize);
                if x_idx >= 0 && (x_idx as usize) < big_in.len() {
                    let c = sparse_bank[p][pos];
                    al += big_in.left[x_idx as usize] * c;
                    ar += big_in.right[x_idx as usize] * c;
                }
            }
            ref_big_l[n * ratio_big + p] = al;
            ref_big_r[n * ratio_big + p] = ar;
        }
    }
    let ref_big = StereoF64Buffer {
        sample_rate_hz: 352_800,
        left: ref_big_l,
        right: ref_big_r,
    };
    let (rms_big, peak_big) = rms_and_peak_diff_db(&ref_big, &out1);
    assert!(
        rms_big <= -280.0 && peak_big <= -260.0,
        "multi-partition FFT upsampler failed §1 gate: rms={rms_big:.2} dBFS, peak={peak_big:.2} dBFS"
    );
}

#[test]
fn u7_full_tail_rew_preserves_sub_minus_200db_tail_and_is_bit_deterministic() {
    // Construct a 131,072-tap IR (> MAX_PARTITION_BLOCK = 65,536) with:
    // - unity impulse at tap 0
    // - sub-(-200 dB) tiny pulse (1e-12 = -240 dB) at tap 130,000
    // - literally-zero tail taps after 130,000
    let ir_len = 131_072usize;
    let tail_pos = 130_000usize;
    let tiny_val = 1e-12_f64;
    let mut ir_l = vec![0.0_f64; ir_len];
    let mut ir_r = vec![0.0_f64; ir_len];
    ir_l[0] = 1.0;
    ir_r[0] = 1.0;
    ir_l[tail_pos] = tiny_val;
    ir_r[tail_pos] = -tiny_val;

    // 1. Normal mode (`full_tail = false`): truncates flush_tail_samples before the -240 dB tap.
    let normal_ir =
        PreparedStereoIr::from_stereo_ir("normal-long", 384_000, &ir_l, &ir_r).expect("normal ir");
    assert!(!normal_ir.full_tail);
    assert_eq!(normal_ir.flush_tail_samples, 0);

    // 2. Ultimate mode (`full_tail = true`): preserves 100% of active_taps - 1 (up to tap 130,000).
    let ult_ir = Arc::new(
        PreparedStereoIr::from_stereo_ir_with_options("ult-long", 384_000, &ir_l, &ir_r, true)
            .expect("ult ir"),
    );
    assert!(ult_ir.full_tail);
    assert_eq!(ult_ir.active_taps, tail_pos + 1);
    assert_eq!(ult_ir.flush_tail_samples, tail_pos);

    // Feed a unit impulse through full-tail convolver and verify exact -240 dB tap survives flush()!
    let mut conv = PartitionedStereoConvolver::new(Arc::clone(&ult_ir));
    let mut in_l = vec![0.0_f64; 1_024];
    let mut in_r = vec![0.0_f64; 1_024];
    in_l[0] = 1.0;
    in_r[0] = 1.0;
    let b1 = conv
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: in_l.clone(),
            right: in_r.clone(),
        })
        .expect("process");
    let fl = conv.flush().expect("flush");
    let mut total_l = b1.left;
    total_l.extend_from_slice(&fl.left);
    let mut total_r = b1.right;
    total_r.extend_from_slice(&fl.right);
    assert_eq!(total_l.len(), 1_024 + tail_pos);
    assert!(
        (total_l[tail_pos] - tiny_val).abs() < 1e-15,
        "U7 full_tail must preserve -240 dB tail tap exactly, got {}",
        total_l[tail_pos]
    );
    assert!(
        (total_r[tail_pos] + tiny_val).abs() < 1e-15,
        "U7 full_tail must preserve -240 dB right tail tap exactly, got {}",
        total_r[tail_pos]
    );

    // Also verify gapless flush_split_tail carries the complete -240 dB tail to Track B.
    let mut conv_gap = PartitionedStereoConvolver::new(Arc::clone(&ult_ir));
    let _ = conv_gap
        .process_block(StereoF64Buffer {
            sample_rate_hz: 384_000,
            left: in_l,
            right: in_r,
        })
        .expect("gap process");
    let (_end_buf, carried_tail) = conv_gap.flush_split_tail().expect("gap split");
    let carried_tail = carried_tail.expect("full tail must be carried");
    assert_eq!(carried_tail.left.len(), tail_pos);
    // Tap at index `tail_pos` from sample 0 lands at offset `tail_pos - 1024` inside carried_tail.
    assert!((carried_tail.left[tail_pos - 1_024] - tiny_val).abs() < 1e-15);

    // 3. Multi-partition (>= 16 partitions) parallel REW convolution determinism & exactness check.
    let big_rew_len = 65_536 * 18; // 18 partitions -> exercises REW parallel bin-slice MAC!
    let mut big_l = vec![0.0_f64; big_rew_len];
    let mut big_r = vec![0.0_f64; big_rew_len];
    for p in 0..18 {
        big_l[p * 65_536 + p * 7] = 0.04 / ((p + 1) as f64);
        big_r[p * 65_536 + p * 11] = -0.03 / ((p + 1) as f64);
    }
    let big_rew = Arc::new(
        PreparedStereoIr::from_stereo_ir_with_options("big-rew-18p", 384_000, &big_l, &big_r, true)
            .expect("big rew"),
    );
    let sig = synth_stereo_signal(384_000, 70_000);

    let mut c1 = PartitionedStereoConvolver::new(Arc::clone(&big_rew));
    let o1 = c1.process_block(sig.clone()).expect("c1");
    let mut c2 = PartitionedStereoConvolver::new(big_rew);
    let o2 = c2.process_block(sig).expect("c2");
    assert_eq!(o1.left, o2.left, "parallel REW UPOLS must be bit-deterministic (L)");
    assert_eq!(o1.right, o2.right, "parallel REW UPOLS must be bit-deterministic (R)");
}

#[test]
fn parallel_pool_fft_upsampler_bit_identical_to_sequential_f12() {
    // 16-partition 44.1 -> 352.8 kHz FFT upsampler (exercises both `join_stereo` and
    // `for_each_mac_slice` on the persistent worker pool): every single output sample MUST be
    // 0-ULP (`to_bits()`) identical to forced-sequential execution (Review #7 F12).
    let spec = GaussianFilterSpec::for_default_archetype(
        FilterArchetype::GaussLongLp,
        44_100,
        352_800,
    )
    .expect("spec");
    let designed = design_gaussian_filter(spec);
    let input = synth_stereo_signal(44_100, 20_000);

    let dispatches_before_par = ap_dsp::worker_pool::parallel_dispatch_count();
    let mut par_up = FftUpolsUpsampler::from_gaussian(designed.clone()).expect("par fft");
    let par_out = run_chunked(&mut par_up, &input, &[4_096, 8_192, 7_712]);
    let dispatches_after_par = ap_dsp::worker_pool::parallel_dispatch_count();
    assert!(
        dispatches_after_par > dispatches_before_par,
        "C8: parallel FFT upsampler run must dispatch work onto the worker pool ({dispatches_before_par} -> {dispatches_after_par})"
    );

    let seq_out = ap_dsp::worker_pool::with_force_sequential(|| {
        let mut seq_up = FftUpolsUpsampler::from_gaussian(designed).expect("seq fft");
        run_chunked(&mut seq_up, &input, &[4_096, 8_192, 7_712])
    });

    assert_eq!(par_out.left.len(), seq_out.left.len());
    assert_eq!(par_out.right.len(), seq_out.right.len());
    for (idx, (&p, &s)) in par_out.left.iter().zip(seq_out.left.iter()).enumerate() {
        assert_eq!(
            p.to_bits(),
            s.to_bits(),
            "F12: FFT upsampler L sample {idx} differs between parallel pool and sequential"
        );
    }
    for (idx, (&p, &s)) in par_out.right.iter().zip(seq_out.right.iter()).enumerate() {
        assert_eq!(
            p.to_bits(),
            s.to_bits(),
            "F12: FFT upsampler R sample {idx} differs between parallel pool and sequential"
        );
    }
}
