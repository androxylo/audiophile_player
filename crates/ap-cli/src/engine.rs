//! Headless playback coordinator for the rudimentary native Rust WDM-KS player.
//!
//! Default mode (zero CLI arguments):
//! 1. Resolves `~/Music` (or `--music-dir`).
//! 2. Auto-selects the best Windows WDM-KS WaveRT DAC (preferring USB UAC2 / Schiit Unison).
//! 3. Loops indefinitely: picks a random FLAC in `O(depth)` time via subfolder walk,
//!    decodes bit-exact into RAM (RFC 9639 16/24/32-bit FLAC), and streams directly
//!    to the DAC via Windows Kernel Streaming (`WDM-KS` WaveRT — zero Windows mixer).
//! 4. On any Windows interrupt signal (`Ctrl+C`, `Ctrl+Break`, `Close`, `Logoff`, `Shutdown`),
//!    immediately triggers a smooth click-free fade-out stop and releases the DAC pin cleanly.

use std::io::{self, Write};
use std::path::PathBuf;

use ap_core::format::DacWireFormat;
use ap_output_win::hygiene::OptimizationLevel;
use ap_output_win::sink::{OutputError, ZeroMixerAudioSink};
use ap_output_win::wdm_ks::WdmKsSink;
use ap_source::flac::decode_flac_full_memory_int32;
use ap_source::library::{default_user_music_dir, pick_random_flac_walk};

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

/// Run default zero-config random FLAC playback via native Windows Kernel Streaming (`WDM-KS`).
pub fn run_random_music_player(
    music_dir_override: Option<PathBuf>,
    device_filter: Option<String>,
    optimization: OptimizationLevel,
    controller: &ShutdownController,
) -> Result<(), String> {
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
    controller.attach_stop_flag(sink.stop_handle());

    println!(
        "audiophile-player v{} started (WDM-KS WaveRT, opt={optimization:?}, DAC='{}', music='{}').",
        env!("CARGO_PKG_VERSION"),
        sink.device().friendly_name,
        music_dir.display()
    );
    println!("Press Ctrl+C or Ctrl+Break to stop gracefully.");
    let _ = io::stdout().flush();

    let mut last_track: Option<PathBuf> = None;
    let mut consecutive_errors: u32 = 0;

    while controller.current_signal().is_none() {
        // Pick a random FLAC in O(depth) time; retry up to 3 times to avoid immediate repeats.
        let mut chosen = match pick_random_flac_walk(&music_dir) {
            Ok(p) => p,
            Err(err) => return Err(err),
        };
        if last_track.as_ref() == Some(&chosen) {
            for _ in 0..3 {
                if let Ok(alt) = pick_random_flac_walk(&music_dir) {
                    if last_track.as_ref() != Some(&alt) {
                        chosen = alt;
                        break;
                    }
                }
            }
        }

        if controller.current_signal().is_some() {
            break;
        }

        let decoded = match decode_flac_full_memory_int32(&chosen) {
            Ok(d) => d,
            Err(err) => {
                eprintln!(
                    "  [skip] could not decode '{}': {err}",
                    chosen.display()
                );
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    return Err("too many consecutive unreadable FLAC files".to_string());
                }
                continue;
            }
        };
        consecutive_errors = 0;

        if controller.current_signal().is_some() {
            break;
        }

        let dur_s = decoded.pcm.frames.len() as f64 / decoded.spec.sample_rate_hz as f64;
        println!(
            "  [playing] {} ({}-bit / {} Hz, {:.1} s, WDM-KS Int32)",
            chosen.display(),
            decoded.spec.bits_per_sample,
            decoded.spec.sample_rate_hz,
            dur_s
        );
        let _ = io::stdout().flush();

        match sink.play_full_memory(&decoded.pcm, DacWireFormat::Int32) {
            Ok(_stats) => {
                last_track = Some(chosen);
            }
            Err(err @ OutputError::DeviceBusy { .. }) => {
                return Err(err.to_string());
            }
            Err(err) => {
                eprintln!("  [output error] {err}");
                return Err(err.to_string());
            }
        }
    }

    if let Some(sig) = controller.current_signal() {
        print_shutdown_banner(sig);
    } else {
        println!("audiophile-player stopped cleanly.");
        let _ = io::stdout().flush();
    }

    Ok(())
}

fn print_shutdown_banner(sig: ShutdownSignal) {
    println!("audiophile-player received {sig}; shutting down gracefully...");
    println!("audiophile-player stopped cleanly.");
    let _ = io::stdout().flush();
}
