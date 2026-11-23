//! Headless playback coordinator for the native Rust WDM-KS player + rudimentary HTTP UI.
//!
//! Default mode (zero CLI arguments):
//! 1. Resolves `~/Music` (or `--music-dir`).
//! 2. Auto-selects the best Windows WDM-KS WaveRT DAC (preferring USB UAC2 / Schiit Unison).
//! 3. Starts the rudimentary local HTTP control server on the first free port in `17441..=17499`
//!    (never port 80 or 8080).
//! 4. Immediately starts playing random FLACs via `O(depth)` subfolder walk & WDM-KS WaveRT.
//! 5. Supports `Play Random`, `Play Sequential`, `Play File`, and unified `Pause` (which fades
//!    out smoothly and immediately closes/releases the WDM-KS DAC pin so Audirvana can play).

use std::io::{self, Write};
use std::path::PathBuf;

use ap_control::server::ControlServer;
use ap_control::state::{ControlCommand, PlayModeLabel, SharedControl};
use ap_core::format::DacWireFormat;
use ap_core::headroom::UNKNOWN_PEAK_HEADROOM_DB;
use ap_dsp::quantize::{apply_safe_gain_f64, quantize_to_int32};
use ap_dsp::selector::{FilterArchetype, TrackSourceInfo, UpsamplerPicker, UpsamplerPolicy};
use ap_dsp::upsampler::build_upsampler;
use ap_output_win::hygiene::OptimizationLevel;
use ap_output_win::sink::{OutputError, ZeroMixerAudioSink};
use ap_output_win::wdm_ks::WdmKsSink;
use ap_source::flac::decode_flac_full_memory;
use ap_source::library::{SequentialFlacCursor, default_user_music_dir, pick_random_flac_walk};

use crate::signals::{ShutdownController, ShutdownSignal};

/// Run idle mode (`--idle`): print start, park at 0% CPU until signal, print graceful stop.
pub fn run_idle(controller: &ShutdownController) {
    println!(
        "audiophile-player v{} started (idle mode — waiting for interrupt signal).",
        env!("CARGO_PKG_VERSION"),
    );
    let _ = io::stdout().flush();

    let sig = controller.wait_for_shutdown();
    print_shutdown_banner(sig);
}

enum ActivePlayState {
    Paused,
    Random {
        root: PathBuf,
        last_track: Option<PathBuf>,
    },
    Sequential {
        cursor: SequentialFlacCursor,
    },
    SingleFile(Option<PathBuf>),
}

/// Run default zero-config random FLAC playback via native Windows Kernel Streaming (`WDM-KS`)
/// paired with the rudimentary local HTTP control interface and dynamic per-track upsampler picker.
pub fn run_random_music_player(
    music_dir_override: Option<PathBuf>,
    device_filter: Option<String>,
    optimization: OptimizationLevel,
    http_port: Option<u16>,
    upsampler_policy: UpsamplerPolicy,
    controller: &ShutdownController,
) -> Result<(), String> {
    let upsampler_picker = UpsamplerPicker::new(upsampler_policy)?;
    let music_dir = match music_dir_override {
        Some(dir) => dir,
        None => default_user_music_dir()
            .ok_or_else(|| "could not resolve default ~/Music directory".to_string())?,
    };
    if !music_dir.is_dir() {
        return Err(format!(
            "music directory does not exist or is not a folder: '{}'",
            music_dir.display()
        ));
    }

    let mut sink = WdmKsSink::open_best(device_filter.as_deref(), optimization)
        .map_err(|e| e.to_string())?;
    let dac_name = sink.device().friendly_name.clone();
    let stop_handle = sink.stop_handle();

    let shared = SharedControl::new(music_dir.clone(), dac_name.clone());
    shared.attach_sink_stop_flag(stop_handle.clone());
    controller.attach_stop_flag(stop_handle);
    controller.attach_shared_control(shared.clone());

    let mut http_server = ControlServer::start(shared.clone(), http_port)?;

    println!(
        "audiophile-player v{} started (WDM-KS WaveRT, opt={optimization:?}, DAC='{dac_name}', music='{}').",
        env!("CARGO_PKG_VERSION"),
        music_dir.display()
    );
    println!("Web control: {}", http_server.url());
    println!("Press Ctrl+C or Ctrl+Break to stop gracefully.");
    let _ = io::stdout().flush();

    // Start immediately in highest-quality Play Random mode from ~/Music (zero-config default).
    let mut play_state = ActivePlayState::Random {
        root: music_dir,
        last_track: None,
    };
    let mut consecutive_errors: u32 = 0;

    while controller.current_signal().is_none() {
        // Apply any queued command from the HTTP control page before selecting the next track.
        if let Some(cmd) = shared.take_pending_command() {
            play_state = apply_command(cmd, &shared);
            consecutive_errors = 0;
        }

        if matches!(play_state, ActivePlayState::Paused) {
            // DAC pin is completely closed & released here — Audirvana can play freely!
            match shared.wait_for_command_or_shutdown() {
                Some(cmd) => {
                    play_state = apply_command(cmd, &shared);
                    consecutive_errors = 0;
                    continue;
                }
                None => break, // process shutdown signaled
            }
        }

        // Select the next track for the active mode.
        let (chosen, mode_label) = match &mut play_state {
            ActivePlayState::Random { root, last_track } => {
                let mut picked = match pick_random_flac_walk(root) {
                    Ok(p) => p,
                    Err(err) => {
                        shared.set_paused(format!("Paused: {err}"));
                        play_state = ActivePlayState::Paused;
                        continue;
                    }
                };
                if last_track.as_ref() == Some(&picked) {
                    for _ in 0..3 {
                        if let Ok(alt) = pick_random_flac_walk(root) {
                            if last_track.as_ref() != Some(&alt) {
                                picked = alt;
                                break;
                            }
                        }
                    }
                }
                *last_track = Some(picked.clone());
                (picked, PlayModeLabel::Random)
            }
            ActivePlayState::Sequential { cursor } => match cursor.next_track() {
                Some(p) => (p, PlayModeLabel::Sequential),
                None => {
                    shared.set_paused(format!(
                        "Finished sequential playback of '{}' — DAC released",
                        cursor.root().display()
                    ));
                    play_state = ActivePlayState::Paused;
                    continue;
                }
            },
            ActivePlayState::SingleFile(slot) => match slot.take() {
                Some(p) => (p, PlayModeLabel::SingleFile),
                None => {
                    shared.set_paused("Finished single track — DAC released");
                    play_state = ActivePlayState::Paused;
                    continue;
                }
            },
            ActivePlayState::Paused => continue,
        };

        if controller.current_signal().is_some() {
            break;
        }
        if let Some(cmd) = shared.take_pending_command() {
            play_state = apply_command(cmd, &shared);
            continue;
        }

        let mut decoded = match decode_flac_full_memory(&chosen) {
            Ok(d) => d,
            Err(err) => {
                let msg = format!("Skipped unreadable FLAC '{}': {err}", chosen.display());
                eprintln!("  [skip] {msg}");
                shared.record_action(msg);
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    shared.set_paused("Paused after 10 consecutive unreadable FLACs — DAC released");
                    play_state = ActivePlayState::Paused;
                }
                continue;
            }
        };
        consecutive_errors = 0;

        if controller.current_signal().is_some() {
            break;
        }
        if let Some(cmd) = shared.take_pending_command() {
            play_state = apply_command(cmd, &shared);
            continue;
        }

        // Re-select the upsampler & 8fS noise shaper freshly for EVERY track (expects
        // different source rates/bit-depths/filters from track to track, honoring CLI overrides).
        let source_info = TrackSourceInfo::from_stream_spec(decoded.spec);
        let dsp_plan = upsampler_picker.select(&source_info);
        let upsampler = match build_upsampler(&dsp_plan) {
            Ok(u) => u,
            Err(err) => {
                eprintln!("  [dsp error] {err}");
                shared.set_paused(format!("DSP configuration error: {err}"));
                play_state = ActivePlayState::Paused;
                continue;
            }
        };

        // Pure IEEE 754 f64 pipeline: optional -3 dB safety headroom -> f64 upsampling (or
        // zero-copy NullUpsampler for native 352.8/384 kHz files) -> single final f64 -> i32
        // quantization for the DAC.
        let dsp_start = std::time::Instant::now();
        if dsp_plan.filter != FilterArchetype::Null {
            if let Err(err) = apply_safe_gain_f64(&mut decoded.audio, UNKNOWN_PEAK_HEADROOM_DB) {
                shared.set_paused(format!("Headroom error: {err}"));
                play_state = ActivePlayState::Paused;
                continue;
            }
        }
        let upsampled_f64 = match upsampler.upsample(decoded.audio, &dsp_plan) {
            Ok(buf) => buf,
            Err(err) => {
                let msg = format!("Upsampling failed for '{}': {err}", chosen.display());
                eprintln!("  [dsp skip] {msg}");
                shared.record_action(msg);
                continue;
            }
        };
        let dac_bits = if dsp_plan.filter == FilterArchetype::Null {
            (decoded.spec.bits_per_sample as u8).clamp(16, 32)
        } else {
            24
        };
        let final_pcm = quantize_to_int32(&upsampled_f64, dsp_plan.shaper, dac_bits);
        let dsp_ms = dsp_start.elapsed().as_secs_f64() * 1000.0;

        if controller.current_signal().is_some() {
            break;
        }
        if let Some(cmd) = shared.take_pending_command() {
            play_state = apply_command(cmd, &shared);
            continue;
        }

        let dur_s = final_pcm.frames.len() as f64 / final_pcm.sample_rate_hz as f64;
        let fmt_str = format!(
            "{}/{} Hz -> {} Hz f64 ({}, {:.1} s, DSP {:.0} ms) | WDM-KS Int32",
            decoded.spec.bits_per_sample,
            decoded.spec.sample_rate_hz,
            final_pcm.sample_rate_hz,
            dsp_plan.filter,
            dur_s,
            dsp_ms,
        );
        let track_str = chosen.display().to_string();

        println!("  [playing] {track_str} ({fmt_str})");
        println!("  [picker]  {}", dsp_plan.summary());
        let _ = io::stdout().flush();
        shared.record_action(dsp_plan.summary());
        shared.set_playing(mode_label, track_str, fmt_str);

        // WdmKsSink::play_full_memory opens the kernel pin strictly for the duration of this call
        // and closes/releases it via RAII immediately on return (including on Pause!).
        match sink.play_full_memory(&final_pcm, DacWireFormat::Int32) {
            Ok(_stats) => {}
            Err(err @ OutputError::DeviceBusy { .. }) => {
                let msg = format!("DAC busy ({err}) — staying paused");
                eprintln!("  [busy] {msg}");
                shared.set_paused(msg);
                play_state = ActivePlayState::Paused;
            }
            Err(err) => {
                let msg = format!("Output error ({err}) — paused & DAC released");
                eprintln!("  [error] {msg}");
                shared.set_paused(msg);
                play_state = ActivePlayState::Paused;
            }
        }
    }

    http_server.shutdown();

    if let Some(sig) = controller.current_signal() {
        print_shutdown_banner(sig);
    } else {
        println!("audiophile-player stopped cleanly.");
        let _ = io::stdout().flush();
    }

    Ok(())
}

fn apply_command(cmd: ControlCommand, shared: &SharedControl) -> ActivePlayState {
    match cmd {
        ControlCommand::PlayRandom(root) => ActivePlayState::Random {
            root,
            last_track: None,
        },
        ControlCommand::PlaySequential(root) => match SequentialFlacCursor::new(root) {
            Ok(cursor) => ActivePlayState::Sequential { cursor },
            Err(err) => {
                shared.set_paused(format!("Cannot play sequentially: {err}"));
                ActivePlayState::Paused
            }
        },
        ControlCommand::PlayFile(file) => ActivePlayState::SingleFile(Some(file)),
        ControlCommand::Pause => {
            shared.set_paused("Paused — WDM-KS kernel pin closed (DAC available to Audirvana)");
            println!("  [paused] WDM-KS kernel pin closed (DAC released).");
            let _ = io::stdout().flush();
            ActivePlayState::Paused
        }
    }
}

fn print_shutdown_banner(sig: ShutdownSignal) {
    println!("audiophile-player received {sig}; shutting down gracefully...");
    println!("audiophile-player stopped cleanly.");
    let _ = io::stdout().flush();
}
