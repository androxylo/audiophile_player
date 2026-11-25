//! Offline parallel, incremental, resumable FLAC library scanner (`audiophile-player scan`,
//! `docs/research/07-ultimate-mode.md` U5c).
//!
//! Guarantees:
//! - Strictly read-only on music files (NEVER modifies FLAC audio, metadata, or timestamps).
//! - Strictly per-track (`OQ-40`): every track is analysed and persisted independently with zero
//!   album/folder cross-talk.
//! - Verifies RFC 9639 `STREAMINFO.md5` before persisting whole-track results; corrupt files are
//!   reported and never cached.
//! - Atomically writes Schema-1 YAML via [`TrackDbStore::save_preserving_user`], always preserving
//!   any existing hand-edited `user:` overrides.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ap_core::config::{ConvolutionConfig, PlayerConfig};
use ap_core::track_db::{PostChainKey, TrackDbStore, file_size_and_mtime};
use ap_dsp::analysis::{DETECTOR_VERSION, TrackAnalyzer};
use ap_dsp::convolution::{RewConvolverFactory, StereoConvolverFactory};
use ap_dsp::deemphasis::StereoDeemphasis;
use ap_dsp::fusion::{
    CachedWholeTrackEvidence, build_track_db_entry, fuse_track_evidence,
};
use ap_dsp::selector::{TrackSourceInfo, UpsamplerPicker, UpsamplerPolicy};
use ap_dsp::upsampler::build_upsampler;
use ap_source::flac::{AudioBlockSource, FlacBlockReader};
use ap_source::library::discover_flac_files;
use ap_source::md5::md5_to_hex;

use crate::signals::ShutdownController;

const SCAN_BLOCK_FRAMES: usize = 8_192;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub folder: PathBuf,
    pub report: bool,
    pub sidecar: bool,
    pub post_chain: bool,
    pub force: bool,
    pub trackdb_dir: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub rew_override: Option<ConvolutionConfig>,
    pub upsampler_policy: UpsamplerPolicy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub total_files: usize,
    pub scanned: usize,
    pub skipped_current: usize,
    pub failed: usize,
    pub report_lines: Vec<String>,
}

/// Entrypoint for `audiophile-player scan <folder> ...`.
pub fn run_scan_cli(
    opts: ScanOptions,
    controller: &ShutdownController,
) -> Result<(), String> {
    let summary = scan_folder(&opts, Some(controller))?;
    println!(
        "Scan complete: total={} scanned={} skipped(current)={} failed={}",
        summary.total_files, summary.scanned, summary.skipped_current, summary.failed
    );
    Ok(())
}

/// Run the offline batch scanner over `opts.folder`, returning deterministic summary statistics.
pub fn scan_folder(
    opts: &ScanOptions,
    controller: Option<&ShutdownController>,
) -> Result<ScanSummary, String> {
    let files = discover_flac_files(&opts.folder)?;
    let total_files = files.len();
    if total_files == 0 {
        return Ok(ScanSummary::default());
    }

    let loaded_cfg = PlayerConfig::discover_and_load(opts.config_path.as_deref())?;
    let conv_cfg = opts
        .rew_override
        .clone()
        .unwrap_or_else(|| loaded_cfg.config.convolution.clone());
    let rew_factory = RewConvolverFactory::new(conv_cfg).with_full_tail(true);

    let db_root = opts
        .trackdb_dir
        .clone()
        .unwrap_or_else(TrackDbStore::default_central_dir);
    let store = TrackDbStore::new(db_root, opts.sidecar);

    let workers = std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 4))
        .unwrap_or(2)
        .min(total_files);

    let next_idx = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, TrackScanOutcome)>> = Mutex::new(Vec::with_capacity(total_files));

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                if controller.is_some_and(|c| c.current_signal().is_some()) {
                    break;
                }
                let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                if idx >= files.len() {
                    break;
                }
                let outcome = scan_single_track(
                    &files[idx],
                    &store,
                    &opts.upsampler_policy,
                    &rew_factory,
                    opts.post_chain,
                    opts.force,
                );
                if let Ok(mut guard) = results.lock() {
                    guard.push((idx, outcome));
                }
            });
        }
    });

    let mut collected = results.into_inner().unwrap_or_else(|e| e.into_inner());
    collected.sort_by_key(|(idx, _)| *idx);

    let mut summary = ScanSummary {
        total_files,
        ..ScanSummary::default()
    };

    for (_, outcome) in collected {
        match outcome {
            TrackScanOutcome::Scanned(line) => {
                summary.scanned += 1;
                if opts.report {
                    println!("{line}");
                }
                summary.report_lines.push(line);
            }
            TrackScanOutcome::Skipped(line) => {
                summary.skipped_current += 1;
                if opts.report {
                    println!("{line}");
                }
                summary.report_lines.push(line);
            }
            TrackScanOutcome::Failed(line) => {
                summary.failed += 1;
                eprintln!("{line}");
                summary.report_lines.push(line);
            }
        }
    }

    Ok(summary)
}

enum TrackScanOutcome {
    Scanned(String),
    Skipped(String),
    Failed(String),
}

fn scan_single_track(
    path: &Path,
    store: &TrackDbStore,
    policy: &UpsamplerPolicy,
    rew_factory: &RewConvolverFactory,
    measure_post_chain: bool,
    force: bool,
) -> TrackScanOutcome {
    let display_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());

    let mut reader = match FlacBlockReader::open(path) {
        Ok(r) => r,
        Err(err) => return TrackScanOutcome::Failed(format!("FAIL  {display_name}: {err}")),
    };

    let spec = reader.spec();
    let raw_peak_meta = reader.peak_meta();
    let track_meta = reader.track_meta();
    let header_md5 = reader.streaminfo_md5();
    let header_md5_hex = header_md5.as_ref().map(md5_to_hex);
    let (file_size, file_mtime) = file_size_and_mtime(path).unwrap_or((0, 0));

    let existing = store.load_for_track(Some(path), header_md5_hex.as_deref());
    let existing_current = existing.as_ref().is_some_and(|e| {
        e.is_results_current(
            DETECTOR_VERSION,
            header_md5_hex.as_deref(),
            Some(file_size),
            Some(file_mtime),
        )
    });

    if !force && existing_current && !measure_post_chain {
        return TrackScanOutcome::Skipped(format!(
            "SKIP  {display_name} (current v{DETECTOR_VERSION})"
        ));
    }

    // Stream the entire track once in bounded blocks through TrackAnalyzer (and optional
    // unity-gain post-chain peak measurement when `--post-chain` is requested).
    let mut analyzer = TrackAnalyzer::new(spec.sample_rate_hz, spec.bits_per_sample);

    let mut post_chain_runner = if measure_post_chain {
        prepare_post_chain_runner(spec, &track_meta, policy, rew_factory).ok()
    } else {
        None
    };

    loop {
        match reader.read_next_block(SCAN_BLOCK_FRAMES) {
            Ok(Some(block)) => {
                analyzer.push_block(&block);
                if let Some(ref mut runner) = post_chain_runner {
                    if let Err(err) = runner.process_block(&block) {
                        return TrackScanOutcome::Failed(format!(
                            "FAIL  {display_name}: post-chain DSP error: {err}"
                        ));
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                return TrackScanOutcome::Failed(format!(
                    "FAIL  {display_name}: decode error: {err}"
                ));
            }
        }
    }

    // Verify RFC 9639 STREAMINFO.md5 before persisting whole-track results.
    let computed_md5 = reader.computed_md5();
    if let (Some(expected), Some(actual)) = (header_md5, computed_md5) {
        if expected != actual {
            return TrackScanOutcome::Failed(format!(
                "FAIL  {display_name}: MD5 mismatch (header={} computed={}); refusing to cache",
                md5_to_hex(&expected),
                md5_to_hex(&actual)
            ));
        }
    }
    let final_md5_hex = header_md5_hex.or_else(|| computed_md5.as_ref().map(md5_to_hex));

    let early_digital = track_meta.is_early_digital_era();
    let orig_below = track_meta
        .original_sample_rate_hz
        .is_some_and(|r| r < spec.sample_rate_hz);
    let analysis = analyzer.snapshot(true, early_digital, orig_below, track_meta.is_dsd_hint);

    let cached_user_only = existing
        .as_ref()
        .map(|e| CachedWholeTrackEvidence::from_track_db_entry(e, false));
    let fused = fuse_track_evidence(
        spec.sample_rate_hz,
        spec.bits_per_sample,
        &raw_peak_meta,
        &track_meta,
        Some(&analysis),
        cached_user_only.as_ref(),
    );

    let (decision_filter, decision_rule) =
        summarize_scan_decision(spec.sample_rate_hz, &fused, policy);

    let mut entry = build_track_db_entry(
        path.to_string_lossy().into_owned(),
        file_size,
        file_mtime,
        final_md5_hex,
        &analysis,
        fused.deemphasis.value,
        decision_filter.clone(),
        decision_rule.clone(),
    );

    let mut peak_note = String::new();
    if let Some(runner) = post_chain_runner {
        match runner.finish() {
            Ok((key, peak_dbfs)) if peak_dbfs.is_finite() => {
                peak_note = format!(" M={peak_dbfs:+.2}dBFS");
                entry.upsert_post_chain_peak(key, peak_dbfs, false);
            }
            Ok(_) => {}
            Err(err) => {
                return TrackScanOutcome::Failed(format!(
                    "FAIL  {display_name}: post-chain flush error: {err}"
                ));
            }
        }
    }

    if let Err(err) = store.save_preserving_user(Some(path), entry) {
        return TrackScanOutcome::Failed(format!("FAIL  {display_name}: save error: {err}"));
    }

    let lufs_str = analysis
        .integrated_lufs
        .map(|l| format!("{l:+.1}LUFS"))
        .unwrap_or_else(|| "n/a".to_string());
    TrackScanOutcome::Scanned(format!(
        "SCAN  {display_name} ({}/{}b {:.1}s) TP={:+.2}dBTP {lufs_str} B={}b H={} A={} L={} HF={} -> {decision_filter} [{decision_rule}]{peak_note}",
        spec.sample_rate_hz,
        spec.bits_per_sample,
        analysis.duration_s,
        analysis.true_peak_dbtp,
        analysis.effective_bits,
        analysis.upsampled_from_1x.as_str(),
        analysis.apodizing_needed.as_str(),
        analysis.loud_and_clipped.as_str(),
        analysis.ultrasonic_noise_detected.as_str(),
    ))
}

fn summarize_scan_decision(
    sample_rate_hz: u32,
    fused: &ap_dsp::fusion::FusedTrackEvidence,
    policy: &UpsamplerPolicy,
) -> (String, String) {
    if let Some(ref f) = policy.filter_override {
        return (f.to_string(), "cli_override".to_string());
    }
    if let Some(ref uf) = fused.user_filter {
        return (uf.value.clone(), format!("user({})", uf.source.as_str()));
    }
    if sample_rate_hz >= 352_800 {
        return ("null".to_string(), "native_8fs".to_string());
    }
    if fused.apodizing.value {
        return ("gauss-apod-4m".to_string(), "rule_A_apodizing".to_string());
    }
    if fused.effective_nyquist_hz.value.is_some() {
        return ("sinc-steep-4m".to_string(), "rule_H_fake_hires".to_string());
    }
    if fused.loud_and_clipped.value {
        return ("gauss-apod-4m".to_string(), "rule_L_loud_clipped".to_string());
    }
    if sample_rate_hz <= 48_000 {
        ("sinc-steep-4m".to_string(), "rate_1x_clean".to_string())
    } else if fused.hf_noise_filter.value {
        ("gauss-hires-lp+hf".to_string(), "rate_hires_hf_noise".to_string())
    } else {
        ("gauss-hires-lp".to_string(), "rate_hires_clean".to_string())
    }
}

struct PostChainRunner {
    key: PostChainKey,
    upsampler: Box<dyn ap_dsp::upsampler::Upsampler>,
    deemphasis: Option<StereoDeemphasis>,
    convolver: Box<dyn ap_dsp::convolution::StereoConvolver>,
    peak_linear: f64,
}

impl PostChainRunner {
    fn process_block(&mut self, input: &ap_core::buffer::StereoF64Buffer) -> Result<(), String> {
        let mut buf = self.upsampler.process_block(input.clone())?;
        if let Some(ref mut deemph) = self.deemphasis {
            buf = deemph.process_block(buf)?;
        }
        buf = self.convolver.process_block(buf)?;
        self.observe(&buf);
        Ok(())
    }

    fn finish(mut self) -> Result<(PostChainKey, f64), String> {
        let mut tail = self.upsampler.flush()?;
        if !tail.left.is_empty() {
            if let Some(ref mut deemph) = self.deemphasis {
                tail = deemph.process_block(tail)?;
            }
            let c = self.convolver.process_block(tail)?;
            self.observe(&c);
        }
        let c_tail = self.convolver.flush()?;
        self.observe(&c_tail);

        let peak_dbfs = if self.peak_linear > 0.0 {
            20.0 * self.peak_linear.log10()
        } else {
            -120.0
        };
        Ok((self.key, peak_dbfs))
    }

    fn observe(&mut self, buf: &ap_core::buffer::StereoF64Buffer) {
        for &s in buf.left.iter().chain(buf.right.iter()) {
            let a = s.abs();
            if a > self.peak_linear {
                self.peak_linear = a;
            }
        }
    }
}

fn prepare_post_chain_runner(
    spec: ap_core::format::StreamSpec,
    track_meta: &ap_core::metadata::TrackMetadata,
    policy: &UpsamplerPolicy,
    rew_factory: &RewConvolverFactory,
) -> Result<PostChainRunner, String> {
    let mut src = TrackSourceInfo::from_stream_spec(spec);
    src.deemphasis = track_meta
        .ap_deemphasis
        .unwrap_or(track_meta.cuesheet_pre_emphasis);

    let picker = UpsamplerPicker::new(*policy)?;
    let plan = picker.select(&src);
    let upsampler = build_upsampler(&plan)?;
    let deemphasis = if src.deemphasis {
        Some(StereoDeemphasis::new_50_15us(plan.target_rate_hz)?)
    } else {
        None
    };
    let convolver = rew_factory.create_for_rate(plan.target_rate_hz)?;
    let rew_hash = convolver
        .content_hash()
        .unwrap_or("none")
        .to_string();

    let key = PostChainKey {
        upsampler: plan.filter.to_string(),
        f_eff_hz: plan.effective_nyquist_hz.unwrap_or(spec.sample_rate_hz / 2),
        output_rate_hz: plan.target_rate_hz,
        deemphasis: src.deemphasis,
        hf_noise_filter: plan.hf_noise_filter,
        rew_filter_hash: rew_hash,
    };

    Ok(PostChainRunner {
        key,
        upsampler,
        deemphasis,
        convolver,
        peak_linear: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use flac_codec::encode::{FlacSampleWriter, Options};

    fn make_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ap_scan_test_{label}_{}_{}",
            std::process::id(),
            nanos
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_test_flac(path: &Path, rate: u32, frames: usize, amp: f64) {
        let mut interleaved = Vec::with_capacity(frames * 2);
        let scale = 32767.0 * amp;
        for n in 0..frames {
            let v = ((2.0 * PI * 1000.0 * n as f64 / rate as f64).sin() * scale).round() as i32;
            interleaved.push(v);
            interleaved.push(v);
        }
        let mut writer = FlacSampleWriter::create(
            path,
            Options::default(),
            rate,
            16,
            2,
            Some(interleaved.len() as u64),
        )
        .unwrap();
        writer.write(&interleaved).unwrap();
        writer.finalize().unwrap();
    }

    #[test]
    fn offline_scan_writes_yaml_preserves_user_overrides_is_resumable_and_never_touches_flac() {
        let root = make_temp_dir("basic");
        let music_dir = root.join("music");
        let db_dir = root.join("trackdb");
        fs::create_dir_all(&music_dir).unwrap();

        let flac_path = music_dir.join("01_sine.flac");
        write_test_flac(&flac_path, 44_100, 44_100, 0.5);
        let flac_bytes_before = fs::read(&flac_path).unwrap();

        // 1. First scan writes sidecar YAML + measures post-chain peak M.
        let opts = ScanOptions {
            folder: music_dir.clone(),
            report: false,
            sidecar: true,
            post_chain: true,
            force: false,
            trackdb_dir: Some(db_dir.clone()),
            config_path: None,
            rew_override: Some(ConvolutionConfig::disabled()),
            upsampler_policy: UpsamplerPolicy::default(),
        };
        let s1 = scan_folder(&opts, None).expect("scan 1 failed");
        assert_eq!(s1.total_files, 1);
        assert_eq!(s1.scanned, 1);
        assert_eq!(s1.failed, 0);

        let sidecar_path = TrackDbStore::sidecar_path_for(&flac_path);
        assert!(sidecar_path.is_file(), "sidecar YAML must be created");

        // Verify FLAC file bytes were NEVER modified.
        assert_eq!(fs::read(&flac_path).unwrap(), flac_bytes_before);

        // Parse YAML and verify post_chain_peaks & sample_peak.
        let yaml_text = fs::read_to_string(&sidecar_path).unwrap();
        let mut entry = ap_core::track_db::TrackDbEntry::from_yaml_str(&yaml_text).unwrap();
        assert_eq!(entry.results.post_chain_peaks.len(), 1);
        let measured_m = entry.results.post_chain_peaks[0].peak_dbfs;
        assert!(
            (measured_m - (-6.02)).abs() < 0.2,
            "0.5 amplitude 1 kHz sine unity-gain post-chain M should be ~-6.02 dBFS, got {measured_m}"
        );

        // 2. Hand-edit the `user:` section on disk, then rescan with --force -> `user:` MUST survive!
        entry.user.filter = Some("short-lp".to_string());
        entry.user.note = "keep my custom filter".to_string();
        fs::write(&sidecar_path, entry.to_yaml_string()).unwrap();

        let mut opts_rescan = opts.clone();
        opts_rescan.post_chain = false;
        opts_rescan.force = false;
        let s2 = scan_folder(&opts_rescan, None).expect("resumable skip scan failed");
        assert_eq!(s2.skipped_current, 1);
        assert_eq!(s2.scanned, 0);

        opts_rescan.force = true;
        let s3 = scan_folder(&opts_rescan, None).expect("forced rescan failed");
        assert_eq!(s3.scanned, 1);

        let reloaded =
            ap_core::track_db::TrackDbEntry::from_yaml_str(&fs::read_to_string(&sidecar_path).unwrap())
                .unwrap();
        assert_eq!(reloaded.user.filter.as_deref(), Some("short-lp"));
        assert_eq!(reloaded.user.note, "keep my custom filter");
        assert_eq!(
            reloaded.results.post_chain_peaks.len(),
            1,
            "existing post_chain_peaks must be preserved across rescans"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
