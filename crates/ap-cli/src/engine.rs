//! Headless playback coordinator wiring:
//!   Source (`ap-source`) -> Tidy 5-Stage `f64` Pipeline (`ap-dsp::pipeline::AudioPipeline`)
//!   -> Bounded Look-Ahead `TrackPrefetchQueue` (zero between-track hiccups)
//!   -> Native Rust `WDM-KS` WaveRT Output (`ap-output-win`, `VirtualLock` + MMCSS + 100 ms ring)
//!   -> Decoupled Localhost HTTP Control UI (`ap-control`, safe port 8085+).

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use ap_control::server::ControlServer;
use ap_control::state::{ControlCommand, PlayModeLabel, SharedControl};
use ap_core::format::DacWireFormat;
use ap_dsp::pipeline::{AudioPipeline, ProcessedTrack};
use ap_dsp::selector::UpsamplerPolicy;
use ap_output_win::hygiene::OptimizationLevel;
use ap_output_win::sink::{OutputError, ZeroMixerAudioSink};
use ap_output_win::wdm_ks::WdmKsSink;
use ap_source::flac::decode_flac_full_memory;
use ap_source::library::{SequentialFlacCursor, default_user_music_dir, pick_random_flac_walk};

use crate::prefetch::{PrefetchEvent, TrackPrefetchQueue};
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

#[derive(Debug, Clone)]
enum ProducerDirective {
    Paused,
    Random { root: PathBuf },
    Sequential { root: PathBuf },
    SingleFile { file: PathBuf },
    Shutdown,
}

struct ProducerControl {
    state: Mutex<(u64, ProducerDirective)>,
    cv: Condvar,
}

impl ProducerControl {
    fn new(initial_epoch: u64, initial: ProducerDirective) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((initial_epoch, initial)),
            cv: Condvar::new(),
        })
    }

    fn set(&self, epoch: u64, directive: ProducerDirective) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *st = (epoch, directive);
        self.cv.notify_all();
    }

    fn wait_active(&self) -> Option<(u64, ProducerDirective)> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &st.1 {
                ProducerDirective::Shutdown => return None,
                ProducerDirective::Paused => {
                    st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                other => return Some((st.0, other.clone())),
            }
        }
    }

    fn epoch(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0
    }
}

struct PreparedTrack {
    path: PathBuf,
    mode_label: PlayModeLabel,
    processed: ProcessedTrack,
}

/// Run default zero-config random FLAC playback via native Windows Kernel Streaming (`WDM-KS`),
/// tidy `AudioPipeline`, hiccup-free 1-track look-ahead prefetching, and localhost HTTP UI.
pub fn run_random_music_player(
    music_dir_override: Option<PathBuf>,
    device_filter: Option<String>,
    optimization: OptimizationLevel,
    http_port: Option<u16>,
    upsampler_policy: UpsamplerPolicy,
    controller: &ShutdownController,
) -> Result<(), String> {
    let pipeline = AudioPipeline::new(upsampler_policy)?;
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

    let prefetch_queue: Arc<TrackPrefetchQueue<PreparedTrack>> = TrackPrefetchQueue::new();
    let mut active_epoch = prefetch_queue.current_epoch();
    let prod_ctrl = ProducerControl::new(
        active_epoch,
        ProducerDirective::Random {
            root: music_dir.clone(),
        },
    );

    // Spawn dedicated background DSP Producer thread (pre-decodes & processes Track N+1 into RAM
    // while Track N streams out via WDM-KS so there are zero hiccups between tracks).
    let q_bg = Arc::clone(&prefetch_queue);
    let ctrl_bg = Arc::clone(&prod_ctrl);
    let shared_bg = Arc::clone(&shared);
    let producer_thread = thread::Builder::new()
        .name("ap-dsp-prefetch".to_string())
        .spawn(move || producer_loop(pipeline, ctrl_bg, q_bg, shared_bg))
        .map_err(|e| format!("failed spawning DSP prefetch thread: {e}"))?;

    let mut is_paused = false;

    while controller.current_signal().is_none() {
        // Apply any pending command from the HTTP UI.
        if let Some(cmd) = shared.take_pending_command() {
            let (paused, next_ep) =
                dispatch_command(cmd, &prefetch_queue, &prod_ctrl, &shared);
            is_paused = paused;
            active_epoch = next_ep;
        }

        if is_paused {
            // DAC WDM-KS pin is completely closed & released — Audirvana can play freely!
            match shared.wait_for_command_or_shutdown() {
                Some(cmd) => {
                    let (paused, next_ep) =
                        dispatch_command(cmd, &prefetch_queue, &prod_ctrl, &shared);
                    is_paused = paused;
                    active_epoch = next_ep;
                    continue;
                }
                None => break,
            }
        }

        // Wait for the next pre-buffered track (instantaneous once Track N+1 is ready in RAM;
        // 50 ms wake window keeps Pause / Ctrl+C responsive even while Track 1 is decoding).
        let Some(event) =
            prefetch_queue.pop_for_epoch_timeout(active_epoch, Duration::from_millis(50))
        else {
            continue;
        };

        if controller.current_signal().is_some() {
            break;
        }
        if let Some(cmd) = shared.take_pending_command() {
            let (paused, next_ep) =
                dispatch_command(cmd, &prefetch_queue, &prod_ctrl, &shared);
            is_paused = paused;
            active_epoch = next_ep;
            continue;
        }

        match event {
            PrefetchEvent::EndOfQueue(msg) | PrefetchEvent::FatalError(msg) => {
                shared.set_paused(msg);
                active_epoch = prefetch_queue.reset_and_flush();
                prod_ctrl.set(active_epoch, ProducerDirective::Paused);
                is_paused = true;
            }
            PrefetchEvent::Track(prepared) => {
                let pcm = &prepared.processed.pcm;
                let report = &prepared.processed.report;
                let dur_s = pcm.frames.len() as f64 / pcm.sample_rate_hz as f64;
                let fmt_str = report.format_summary(dur_s);
                let track_str = prepared.path.display().to_string();

                println!("  [playing]  {track_str} ({fmt_str})");
                println!("  [pipeline] {}", report.log_line());
                let _ = io::stdout().flush();
                shared.record_action(report.log_line());
                shared.set_playing(prepared.mode_label, track_str, fmt_str);

                match sink.play_full_memory(pcm, DacWireFormat::Int32) {
                    Ok(_stats) => {}
                    Err(err @ OutputError::DeviceBusy { .. }) => {
                        let msg = format!("DAC busy ({err}) — staying paused");
                        eprintln!("  [busy] {msg}");
                        shared.set_paused(msg);
                        active_epoch = prefetch_queue.reset_and_flush();
                        prod_ctrl.set(active_epoch, ProducerDirective::Paused);
                        is_paused = true;
                    }
                    Err(err) => {
                        let msg = format!("Output error ({err}) — paused & DAC released");
                        eprintln!("  [error] {msg}");
                        shared.set_paused(msg);
                        active_epoch = prefetch_queue.reset_and_flush();
                        prod_ctrl.set(active_epoch, ProducerDirective::Paused);
                        is_paused = true;
                    }
                }
            }
        }
    }

    // Graceful teardown: stop producer thread, flush RAM buffer, shut down HTTP server.
    prefetch_queue.close();
    prod_ctrl.set(active_epoch.wrapping_add(1), ProducerDirective::Shutdown);
    let _ = producer_thread.join();
    http_server.shutdown();

    if let Some(sig) = controller.current_signal() {
        print_shutdown_banner(sig);
    } else {
        println!("audiophile-player stopped cleanly.");
        let _ = io::stdout().flush();
    }

    Ok(())
}

fn dispatch_command(
    cmd: ControlCommand,
    queue: &TrackPrefetchQueue<PreparedTrack>,
    prod_ctrl: &ProducerControl,
    shared: &SharedControl,
) -> (bool, u64) {
    let ep = queue.reset_and_flush();
    match cmd {
        ControlCommand::PlayRandom(root) => {
            prod_ctrl.set(ep, ProducerDirective::Random { root });
            (false, ep)
        }
        ControlCommand::PlaySequential(root) => {
            prod_ctrl.set(ep, ProducerDirective::Sequential { root });
            (false, ep)
        }
        ControlCommand::PlayFile(file) => {
            prod_ctrl.set(ep, ProducerDirective::SingleFile { file });
            (false, ep)
        }
        ControlCommand::Pause => {
            prod_ctrl.set(ep, ProducerDirective::Paused);
            shared.set_paused("Paused — WDM-KS kernel pin closed (DAC available to Audirvana)");
            println!("  [paused] WDM-KS kernel pin closed (DAC released).");
            let _ = io::stdout().flush();
            (true, ep)
        }
    }
}

/// Background DSP Producer thread: selects, decodes, runs the full `AudioPipeline`, and
/// deposits one track ahead into `TrackPrefetchQueue` for hiccup-free transitions.
fn producer_loop(
    pipeline: AudioPipeline,
    ctrl: Arc<ProducerControl>,
    queue: Arc<TrackPrefetchQueue<PreparedTrack>>,
    shared: Arc<SharedControl>,
) {
    let mut seq_cursor: Option<(u64, SequentialFlacCursor)> = None;
    let mut single_consumed_epoch: Option<u64> = None;
    let mut last_random: Option<PathBuf> = None;
    let mut consecutive_errors: u32 = 0;

    while let Some((epoch, directive)) = ctrl.wait_active() {
        if ctrl.epoch() != epoch {
            continue;
        }

        let (chosen, mode_label) = match directive {
            ProducerDirective::Random { root } => {
                let mut picked = match pick_random_flac_walk(&root) {
                    Ok(p) => p,
                    Err(err) => {
                        let _ = queue.push_for_epoch(epoch, PrefetchEvent::FatalError(err));
                        ctrl.set(epoch, ProducerDirective::Paused);
                        continue;
                    }
                };
                if last_random.as_ref() == Some(&picked) {
                    for _ in 0..3 {
                        if let Ok(alt) = pick_random_flac_walk(&root) {
                            if last_random.as_ref() != Some(&alt) {
                                picked = alt;
                                break;
                            }
                        }
                    }
                }
                last_random = Some(picked.clone());
                (picked, PlayModeLabel::Random)
            }
            ProducerDirective::Sequential { root } => {
                let needs_init = seq_cursor.as_ref().is_none_or(|(ep, _)| *ep != epoch);
                if needs_init {
                    match SequentialFlacCursor::new(root.clone()) {
                        Ok(c) => seq_cursor = Some((epoch, c)),
                        Err(err) => {
                            let _ = queue.push_for_epoch(epoch, PrefetchEvent::FatalError(err));
                            ctrl.set(epoch, ProducerDirective::Paused);
                            continue;
                        }
                    }
                }
                let next = seq_cursor.as_mut().and_then(|(_, c)| c.next_track());
                match next {
                    Some(p) => (p, PlayModeLabel::Sequential),
                    None => {
                        let _ = queue.push_for_epoch(
                            epoch,
                            PrefetchEvent::EndOfQueue(format!(
                                "Finished sequential playback of '{}' — DAC released",
                                root.display()
                            )),
                        );
                        ctrl.set(epoch, ProducerDirective::Paused);
                        continue;
                    }
                }
            }
            ProducerDirective::SingleFile { file } => {
                if single_consumed_epoch == Some(epoch) {
                    let _ = queue.push_for_epoch(
                        epoch,
                        PrefetchEvent::EndOfQueue("Finished single track — DAC released".into()),
                    );
                    ctrl.set(epoch, ProducerDirective::Paused);
                    continue;
                }
                single_consumed_epoch = Some(epoch);
                (file, PlayModeLabel::SingleFile)
            }
            ProducerDirective::Paused | ProducerDirective::Shutdown => continue,
        };

        if ctrl.epoch() != epoch {
            continue;
        }

        let decoded = match decode_flac_full_memory(&chosen) {
            Ok(d) => d,
            Err(err) => {
                let msg = format!("Skipped unreadable FLAC '{}': {err}", chosen.display());
                eprintln!("  [skip] {msg}");
                shared.record_action(msg);
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    let _ = queue.push_for_epoch(
                        epoch,
                        PrefetchEvent::FatalError(
                            "Paused after 10 consecutive unreadable FLACs — DAC released".into(),
                        ),
                    );
                    ctrl.set(epoch, ProducerDirective::Paused);
                    consecutive_errors = 0;
                }
                continue;
            }
        };
        consecutive_errors = 0;

        if ctrl.epoch() != epoch {
            continue;
        }

        let processed = match pipeline.process(decoded.audio, decoded.spec, &decoded.peak_meta, false)
        {
            Ok(p) => p,
            Err(err) => {
                let msg = format!("DSP pipeline error on '{}': {err}", chosen.display());
                eprintln!("  [dsp] {msg}");
                shared.record_action(msg.clone());
                let _ = queue.push_for_epoch(epoch, PrefetchEvent::FatalError(msg));
                ctrl.set(epoch, ProducerDirective::Paused);
                continue;
            }
        };

        // Blocks cleanly if 1 next track is already buffered in RAM; discards immediately if
        // the user clicked Pause / switched mode while this track was being prepared.
        let _ = queue.push_for_epoch(
            epoch,
            PrefetchEvent::Track(PreparedTrack {
                path: chosen,
                mode_label,
                processed,
            }),
        );
    }
}

fn print_shutdown_banner(sig: ShutdownSignal) {
    println!("audiophile-player received {sig}; shutting down gracefully...");
    println!("audiophile-player stopped cleanly.");
    let _ = io::stdout().flush();
}
