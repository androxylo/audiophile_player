//! Phase 4 TDD tests: Detectors H, A, L, B, 4× BS.1770 true-peak meter, `OQ-37` ultrasonic noise
//! shelf detection, and all §5.3 Metadata × Analyser fusion rules (including stale peak tag
//! disproof and strict per-track independence).

use std::f64::consts::PI;

use ap_core::buffer::StereoF64Buffer;
use ap_core::headroom::{HeadroomMode, TrackPeakMetadata, select_headroom_for_bits};
use ap_core::metadata::{GenreClass, TrackMetadata};
use ap_dsp::analysis::{DetectorVerdict, TrackAnalyzer};
use ap_dsp::fusion::{CachedWholeTrackEvidence, EvidenceSource, fuse_track_evidence};

/// Deterministic xorshift64 PRNG returning uniform white noise in `[-1, 1]`.
struct Rng64(u64);
impl Rng64 {
    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        ((x >> 11) as f64) * (2.0 / ((1_u64 << 53) as f64)) - 1.0
    }
}

/// Apply a symmetric linear-phase brickwall windowed-sinc low-pass filter at `fc_hz` on `sig`.
fn brickwall_lowpass(sig: &[f64], fs_hz: f64, fc_hz: f64, half_len: usize) -> Vec<f64> {
    let n = 2 * half_len + 1;
    let mut h = vec![0.0_f64; n];
    let mut sum = 0.0;
    for k in 0..n {
        let t = (k as isize - half_len as isize) as f64;
        let sinc = if t == 0.0 {
            2.0 * fc_hz / fs_hz
        } else {
            (2.0 * PI * fc_hz * t / fs_hz).sin() / (PI * t)
        };
        let win = 0.54 - 0.46 * (2.0 * PI * (k as f64) / ((n - 1) as f64)).cos();
        h[k] = sinc * win;
        sum += h[k];
    }
    for v in &mut h {
        *v /= sum;
    }
    let mut out = vec![0.0_f64; sig.len()];
    for i in 0..sig.len() {
        let mut acc = 0.0;
        for (k, &c) in h.iter().enumerate() {
            let j = i as isize + k as isize - half_len as isize;
            if j >= 0 && (j as usize) < sig.len() {
                acc += sig[j as usize] * c;
            }
        }
        out[i] = acc;
    }
    out
}

#[test]
fn detector_h_identifies_fake_hires_genuine_hires_lossy_and_oq37_ultrasonic_noise() {
    let mut rng = Rng64(0x1234_5678_9ABC_DEF0);

    // 1. Fake hi-res: 96 kHz broadband white noise low-passed hard at 21 kHz (simulating 44.1->96k).
    let frames_96 = 96_000 * 6; // 6 s (>= 5 s minimum)
    let raw_96: Vec<f64> = (0..frames_96).map(|_| 0.25 * rng.next_f64()).collect();
    let fake_96 = brickwall_lowpass(&raw_96, 96_000.0, 20_500.0, 96);
    let mut an_fake = TrackAnalyzer::new(96_000, 24);
    an_fake.push_block(&StereoF64Buffer {
        sample_rate_hz: 96_000,
        left: fake_96.clone(),
        right: fake_96,
    });
    let res_fake = an_fake.snapshot(false, false, false, false);
    assert_eq!(res_fake.upsampled_from_1x, DetectorVerdict::Positive);
    assert_eq!(res_fake.effective_nyquist_hz, Some(24_000));

    // 2. Genuine 96 kHz broadband content flat all the way to 45 kHz -> Negative.
    let mut an_real = TrackAnalyzer::new(96_000, 24);
    an_real.push_block(&StereoF64Buffer {
        sample_rate_hz: 96_000,
        left: raw_96.clone(),
        right: raw_96,
    });
    let res_real = an_real.snapshot(false, false, false, false);
    assert_eq!(res_real.upsampled_from_1x, DetectorVerdict::Negative);
    assert_eq!(res_real.effective_nyquist_hz, None);

    // 3. 1x lossy origin: 44.1 kHz white noise brickwalled at 16 kHz -> lossy_origin_suspected.
    let frames_44 = 44_100 * 6;
    let raw_44: Vec<f64> = (0..frames_44).map(|_| 0.25 * rng.next_f64()).collect();
    let mp3_like = brickwall_lowpass(&raw_44, 44_100.0, 15_800.0, 96);
    let mut an_lossy = TrackAnalyzer::new(44_100, 16);
    an_lossy.push_block(&StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: mp3_like.clone(),
        right: mp3_like,
    });
    let res_lossy = an_lossy.snapshot(false, false, false, false);
    assert!(
        res_lossy.lossy_origin_suspected,
        "16 kHz brickwall on 44.1 kHz must trigger lossy_origin_suspected"
    );

    // 4. OQ-37 4fS (192 kHz) ultrasonic DSD/ADC noise shelf above 45 kHz -> Positive!
    let frames_192 = 192_000 * 6;
    let mut sig_dsd = Vec::with_capacity(frames_192);
    // Low-pass musical noise below 20 kHz + strong ultrasonic noise shaped above 50 kHz
    // (differentiated white noise scaled to simulate DSD64 ultrasonic noise rise).
    let mut prev1 = 0.0;
    let mut prev2 = 0.0;
    for n in 0..frames_192 {
        let t = (n as f64) / 192_000.0;
        let music = 0.2 * (2.0 * PI * 1_500.0 * t).sin() + 0.1 * (2.0 * PI * 6_000.0 * t).cos();
        let w = 0.015 * rng.next_f64();
        // 2nd-order high-pass (1 - z^-1)^2 emphasizes > 50 kHz strongly while leaving 22-36 kHz quieter
        let hp = w - 2.0 * prev1 + prev2;
        prev2 = prev1;
        prev1 = w;
        sig_dsd.push(music + hp);
    }
    let mut an_dsd = TrackAnalyzer::new(192_000, 24);
    an_dsd.push_block(&StereoF64Buffer {
        sample_rate_hz: 192_000,
        left: sig_dsd.clone(),
        right: sig_dsd,
    });
    let res_dsd = an_dsd.snapshot(false, false, false, false);
    assert_eq!(
        res_dsd.ultrasonic_noise_detected,
        DetectorVerdict::Positive,
        "OQ-37 DSD ultrasonic noise rise must be detected: rise={:+.1} dB, lvl={:.1} dBFS",
        res_dsd.ultrasonic_noise_rise_db,
        res_dsd.ultrasonic_noise_level_dbfs
    );
}

#[test]
fn detector_a_distinguishes_linear_phase_preringing_from_causal_transients() {
    let fs = 44_100usize;
    let frames = fs * 6; // 6 s -> ~25 clicks spaced every 220 ms

    // 1. Sharp clicks passed through a long linear-phase brickwall at 21 kHz -> strong pre-ringing!
    let mut clicks = vec![0.0_f64; frames];
    let step = (fs * 220) / 1_000;
    for idx in (step..(frames - step)).step_by(step) {
        clicks[idx] = 0.85;
    }
    let prering = brickwall_lowpass(&clicks, fs as f64, 21_200.0, 256);

    let mut an_lin = TrackAnalyzer::new(fs as u32, 16);
    an_lin.push_block(&StereoF64Buffer {
        sample_rate_hz: fs as u32,
        left: prering.clone(),
        right: prering,
    });
    let res_lin = an_lin.snapshot(false, false, false, false);
    assert_eq!(
        res_lin.apodizing_needed,
        DetectorVerdict::Positive,
        "linear-phase brickwall clicks must fire Detector A (onsets={}, events={})",
        res_lin.onset_count,
        res_lin.apod_events
    );

    // 2. Causal exponentially decaying transients (minimum-phase-like, zero pre-ringing) -> Negative!
    let mut causal = vec![0.0_f64; frames];
    for idx in (step..(frames - step)).step_by(step) {
        for d in 0..256 {
            let env = (-0.04 * (d as f64)).exp();
            // Broadband post-ringing only, strictly zero before `idx`
            causal[idx + d] = 0.85 * env * if d % 2 == 0 { 1.0 } else { -0.8 };
        }
    }
    let mut an_mp = TrackAnalyzer::new(fs as u32, 16);
    an_mp.push_block(&StereoF64Buffer {
        sample_rate_hz: fs as u32,
        left: causal.clone(),
        right: causal,
    });
    let res_mp = an_mp.snapshot(false, false, false, false);
    assert_eq!(
        res_mp.apodizing_needed,
        DetectorVerdict::Negative,
        "causal minimum-phase transients must NOT fire Detector A (onsets={}, events={})",
        res_mp.onset_count,
        res_mp.apod_events
    );
}

#[test]
fn detector_l_and_b_measure_loudness_clipping_true_peak_and_16_in_24_padding() {
    let fs = 44_100usize;
    let frames = fs * 6;
    let mut rng = Rng64(0xDEAD_BEEF_CAFE_BABE);

    // 1. Hot clipped signal (>= -10 LUFS with >= 3-sample clipped runs) -> Rule L Positive!
    let mut hot = Vec::with_capacity(frames);
    for i in 0..frames {
        let base = 0.8 * (2.0 * PI * 440.0 * (i as f64) / (fs as f64)).sin() + 0.5 * rng.next_f64();
        hot.push(base.clamp(-1.0, 1.0));
    }
    let mut an_hot = TrackAnalyzer::new(fs as u32, 16);
    an_hot.push_block(&StereoF64Buffer {
        sample_rate_hz: fs as u32,
        left: hot.clone(),
        right: hot,
    });
    let res_hot = an_hot.snapshot(false, false, false, false);
    assert!(
        res_hot.integrated_lufs.is_some_and(|l| l >= -10.0),
        "expected hot signal >= -10 LUFS, got {:?}",
        res_hot.integrated_lufs
    );
    assert!(res_hot.clipped_runs >= 10, "expected clipped runs, got {}", res_hot.clipped_runs);
    assert_eq!(res_hot.loud_and_clipped, DetectorVerdict::Positive);

    // 2. Quiet intro in 30 s window (-24 LUFS, 0 clips) -> Inconclusive (never falsely Negative!).
    let quiet: Vec<f64> = (0..frames)
        .map(|i| 0.05 * (2.0 * PI * 440.0 * (i as f64) / (fs as f64)).sin())
        .collect();
    let mut an_q = TrackAnalyzer::new(fs as u32, 24);
    an_q.push_block(&StereoF64Buffer {
        sample_rate_hz: fs as u32,
        left: quiet.clone(),
        right: quiet,
    });
    let res_q = an_q.snapshot(false, false, false, false);
    assert_eq!(res_q.loud_and_clipped, DetectorVerdict::Inconclusive);

    // 3. Detector B: exact 16-bit samples stored in 24-bit container -> effective_bits == 16!
    let scale_16 = (1_u64 << 15) as f64;
    let padded_16_in_24: Vec<f64> = (0..4_096)
        .map(|i| {
            let s16 = (20_000.0 * (2.0 * PI * 997.0 * (i as f64) / 44_100.0).sin()).round() as i16;
            (s16 as f64) / scale_16
        })
        .collect();
    let mut an_b16 = TrackAnalyzer::new(44_100, 24);
    an_b16.push_block(&StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: padded_16_in_24.clone(),
        right: padded_16_in_24,
    });
    assert_eq!(an_b16.snapshot(false, false, false, false).effective_bits, 16);

    // Real 24-bit content -> effective_bits == 24!
    let scale_24 = (1_u64 << 23) as f64;
    let real_24: Vec<f64> = (0..4_096)
        .map(|i| ((i as i32 * 13 + 1) % 8_000_000) as f64 / scale_24)
        .collect();
    let mut an_b24 = TrackAnalyzer::new(44_100, 24);
    an_b24.push_block(&StereoF64Buffer {
        sample_rate_hz: 44_100,
        left: real_24.clone(),
        right: real_24,
    });
    assert_eq!(an_b24.snapshot(false, false, false, false).effective_bits, 24);
}

#[test]
fn fusion_disproves_stale_peak_tags_and_enforces_strict_evidence_priority() {
    // Construct a 6 s window whose actual peak is 0.92 (-0.72 dBFS) while a stale ReplayGain tag
    // falsely claims peak = 0.40 (-7.96 dBFS, which would otherwise trigger unsafe 0 dB headroom!).
    let fs = 44_100usize;
    let frames = fs * 6;
    let hot: Vec<f64> = (0..frames)
        .map(|i| 0.92 * (2.0 * PI * 997.0 * (i as f64) / (fs as f64)).sin())
        .collect();
    let mut an = TrackAnalyzer::new(fs as u32, 16);
    an.push_block(&StereoF64Buffer {
        sample_rate_hz: fs as u32,
        left: hot.clone(),
        right: hot,
    });
    let w30 = an.snapshot(false, false, false, false);

    let stale_tags = TrackPeakMetadata {
        replaygain_track_peak: Some(0.40),
        ..TrackPeakMetadata::default()
    };
    let track_meta = TrackMetadata {
        genre_class: GenreClass::Classical,
        cuesheet_pre_emphasis: true,
        mqa_encoder: Some("MQAEncode".to_string()),
        ..TrackMetadata::default()
    };

    let fused = fuse_track_evidence(44_100, 16, &stale_tags, &track_meta, Some(&w30), None);
    assert!(fused.stale_peak_tag_disproved, "W30 peak 0.92 must disprove stale 0.40 tag");
    assert!(
        fused
            .stale_peak_tag_detail
            .as_deref()
            .is_some_and(|s| s.contains("stale peak tag"))
    );
    // After disproof, headroom MUST fall back safely to -3 dB!
    let hr = select_headroom_for_bits(&fused.peak_meta, 16, false, Some(0.0), None);
    assert_eq!(hr.mode, HeadroomMode::Minus3Db);

    // CUESHEET pre-emphasis activates de-emphasis (Header fact); MQA notice logged.
    assert!(fused.deemphasis.value);
    assert_eq!(fused.deemphasis.source, EvidenceSource::Header);
    assert!(fused.notices.iter().any(|n| n.contains("Rule M")));

    // Cached whole-track evidence beats window evidence; user override beats cache.
    let cached = CachedWholeTrackEvidence {
        user_filter: Some("minphase-long".to_string()),
        user_deemphasis: Some(false),
        apod_events: 18,
        apodizing_needed: DetectorVerdict::Positive,
        true_peak_dbtp: Some(-4.5),
        ..CachedWholeTrackEvidence::default()
    };
    let fused_wt = fuse_track_evidence(44_100, 16, &stale_tags, &track_meta, Some(&w30), Some(&cached));
    assert!(!fused_wt.stale_peak_tag_disproved, "WT full-scan peak overrides tags");
    assert!(!fused_wt.deemphasis.value, "YAML user.deemphasis=false must beat CUESHEET");
    assert_eq!(fused_wt.deemphasis.source, EvidenceSource::User);
    assert!(fused_wt.apodizing.value);
    assert_eq!(fused_wt.apodizing.source, EvidenceSource::Cache);
    assert_eq!(
        fused_wt.user_filter.as_ref().map(|e| e.value.as_str()),
        Some("minphase-long")
    );
}
