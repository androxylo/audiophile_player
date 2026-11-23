//! Headless playback coordinator wiring the bounded-memory streaming pipeline:
//!   `FlacBlockReader` (`DEFAULT_SOURCE_BLOCK_FRAMES` chunks — never loads multi-GB files)
//!   -> Tidy 5-Stage `f64` `StreamingPipelineSession` (Headroom -> Upsampler -> NullConvolver
//!      -> NullDelay -> StreamingQuantizer)
//!   -> `BoundedBlockStreamBuffer` (~12 s RAM cap, 2 s prebuffer watermark — zero hiccups)
//!   -> Native Rust `WDM-KS` WaveRT `play_block_stream` (zero Windows mixer, Audirvana-friendly Pause)
//!   -> Decoupled Localhost HTTP Control UI (`ap-control`, safe port 8085+).

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use ap_control::server::ControlServer;
use ap_control::state::{ControlCommand, PlayModeLabel, SharedControl};
use ap_core::buffer::DEFAULT_SOURCE_BLOCK_FRAMES;
use ap_core::format::DacWireFormat;
use ap_dsp::pipeline::AudioPipeline;
use ap_dsp::selector::UpsamplerPolicy;
use ap_output_win::hygiene::OptimizationLevel;
use ap_output_win::sink::OutputError;
use ap_output_win::wdm_ks::WdmKsSink;
use ap_source::flac::{AudioBlockSource, FlacBlockReader};
use ap_source::library::{SequentialFlacCursor, default_user_music_dir, pick_random_flac_walk};

use crate::prefetch::{
    BoundedBlockStreamBuffer, DEFAULT_MAX_BUFFERED_FRAMES, PREBUFFER_SECONDS, StreamEvent,
};
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

/// Run default zero-config random FLAC playback via bounded-memory block streaming,
/// tidy `AudioPipeline`, native Windows Kernel Streaming (`WDM-KS`), and localhost HTTP UI.
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
        "audiophile-player v{} started (WDM-KS WaveRT, streaming blocks, opt={optimization:?}, DAC='{dac_name}', music='{}').",
        env!("CARGO_PKG_VERSION"),
        music_dir.display()
    );
    println!("Web control: {}", http_server.url());
    println!("Press Ctrl+C or Ctrl+Break to stop gracefully.");
    let _ = io::stdout().flush();

    let stream_buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
    let mut active_epoch = stream_buf.current_epoch();
    let prod_ctrl = ProducerControl::new(
        active_epoch,
        ProducerDirective::Random {
            root: music_dir.clone(),
        },
    );

    // Dedicated background streaming DSP Producer thread (bounded ~36 MB buffer cap).
    let buf_bg = Arc::clone(&stream_buf);
    let ctrl_bg = Arc::clone(&prod_ctrl);
    let shared_bg = Arc::clone(&shared);
    let producer_thread = thread::Builder::new()
        .name("ap-dsp-stream".to_string())
        .spawn(move || producer_loop(pipeline, ctrl_bg, buf_bg, shared_bg))
        .map_err(|e| format!("failed spawning DSP streaming thread: {e}"))?;

    let mut is_paused = false;

    while controller.current_signal().is_none() {
        if let Some(cmd) = shared.take_pending_command() {
            let (paused, next_ep) = dispatch_command(cmd, &stream_buf, &prod_ctrl, &shared);
            is_paused = paused;
            active_epoch = next_ep;
        }

        if is_paused {
            // DAC WDM-KS pin is completely closed & released — Audirvana can play freely!
            match shared.wait_for_command_or_shutdown() {
                Some(cmd) => {
                    let (paused, next_ep) =
                        dispatch_command(cmd, &stream_buf, &prod_ctrl, &shared);
                    is_paused = paused;
                    active_epoch = next_ep;
                    continue;
                }
                None => break,
            }
        }

        let Some(event) = stream_buf.pop_for_epoch_timeout(active_epoch, Duration::from_millis(50))
        else {
            continue;
        };

        if controller.current_signal().is_some() {
            break;
        }
        if let Some(cmd) = shared.take_pending_command() {
            let (paused, next_ep) = dispatch_command(cmd, &stream_buf, &prod_ctrl, &shared);
            is_paused = paused;
            active_epoch = next_ep;
            continue;
        }

        match event {
            StreamEvent::EndOfQueue(msg) | StreamEvent::FatalError(msg) => {
                shared.set_paused(msg);
                active_epoch = stream_buf.reset_and_flush();
                prod_ctrl.set(active_epoch, ProducerDirective::Paused);
                is_paused = true;
            }
            StreamEvent::PcmBlock(_) | StreamEvent::TrackEnd => {
                // Orphaned block/marker from an interrupted track — ignore safely.
                continue;
            }
            StreamEvent::TrackStart {
                path,
                mode_label,
                report,
                total_duration_s,
            } => {
                let out_rate = report.output_rate_hz;
                let watermark_frames = (out_rate as usize) * (PREBUFFER_SECONDS as usize);

                // Wait until ~2 seconds of PCM are pre-buffered in RAM (or TrackEnd for short clips)
                // while remaining immediately responsive to Pause / Ctrl+C.
                loop {
                    if controller.current_signal().is_some() {
                        break;
                    }
                    if let Some(cmd) = shared.take_pending_command() {
                        let (paused, next_ep) =
                            dispatch_command(cmd, &stream_buf, &prod_ctrl, &shared);
                        is_paused = paused;
                        active_epoch = next_ep;
                        break;
                    }
                    match stream_buf.wait_prebuffer_watermark(
                        active_epoch,
                        watermark_frames,
                        Duration::from_millis(50),
                    ) {
                        Some(true) => break,
                        Some(false) => continue,
                        None => break,
                    }
                }

                if is_paused || controller.current_signal().is_some() {
                    continue;
                }

                let fmt_str = report.format_summary(total_duration_s);
                let track_str = path.display().to_string();
                println!("  [playing]  {track_str} ({fmt_str})");
                println!("  [pipeline] {}", report.log_line());
                let _ = io::stdout().flush();
                shared.record_action(report.log_line());
                shared.set_playing(mode_label, track_str, fmt_str);

                let ep_for_stream = active_epoch;
                let buf_for_stream = Arc::clone(&stream_buf);
                match sink.play_block_stream(out_rate, DacWireFormat::Int32, move || {
                    buf_for_stream.pop_track_pcm_block(ep_for_stream, Duration::from_secs(5))
                }) {
                    Ok(_stats) => {}
                    Err(err @ OutputError::DeviceBusy { .. }) => {
                        let msg = format!("DAC busy ({err}) — staying paused");
                        eprintln!("  [busy] {msg}");
                        shared.set_paused(msg);
                        active_epoch = stream_buf.reset_and_flush();
                        prod_ctrl.set(active_epoch, ProducerDirective::Paused);
                        is_paused = true;
                    }
                    Err(err) => {
                        let msg = format!("Output error ({err}) — paused & DAC released");
                        eprintln!("  [error] {msg}");
                        shared.set_paused(msg);
                        active_epoch = stream_buf.reset_and_flush();
                        prod_ctrl.set(active_epoch, ProducerDirective::Paused);
                        is_paused = true;
                    }
                }
            }
        }
    }

    stream_buf.close();
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
    buf: &BoundedBlockStreamBuffer,
    prod_ctrl: &ProducerControl,
    shared: &SharedControl,
) -> (bool, u64) {
    let ep = buf.reset_and_flush();
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

/// Background streaming DSP Producer: reads bounded blocks from `FlacBlockReader`,
/// processes them incrementally through `StreamingPipelineSession`, and feeds
/// `BoundedBlockStreamBuffer` with automatic backpressure (strictly bounded RAM).
fn producer_loop(
    pipeline: AudioPipeline,
    ctrl: Arc<ProducerControl>,
    buf: Arc<BoundedBlockStreamBuffer>,
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
                        let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(err));
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
                            let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(err));
                            ctrl.set(epoch, ProducerDirective::Paused);
                            continue;
                        }
                    }
                }
                let next = seq_cursor.as_mut().and_then(|(_, c)| c.next_track());
                match next {
                    Some(p) => (p, PlayModeLabel::Sequential),
                    None => {
                        let _ = buf.push_for_epoch(
                            epoch,
                            StreamEvent::EndOfQueue(format!(
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
                    let _ = buf.push_for_epoch(
                        epoch,
                        StreamEvent::EndOfQueue("Finished single track — DAC released".into()),
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

        let mut source = match FlacBlockReader::open(&chosen) {
            Ok(s) => s,
            Err(err) => {
                let msg = format!("Skipped unreadable FLAC '{}': {err}", chosen.display());
                eprintln!("  [skip] {msg}");
                shared.record_action(msg);
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    let _ = buf.push_for_epoch(
                        epoch,
                        StreamEvent::FatalError(
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

        let spec = source.spec();
        let peak_meta = source.peak_meta();
        let total_duration_s = source
            .total_frames()
            .map(|f| f as f64 / spec.sample_rate_hz as f64);

        let mut session = match pipeline.start_track(spec, &peak_meta, source.is_streaming()) {
            Ok(s) => s,
            Err(err) => {
                let msg = format!("DSP pipeline error on '{}': {err}", chosen.display());
                eprintln!("  [dsp] {msg}");
                shared.record_action(msg.clone());
                let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                ctrl.set(epoch, ProducerDirective::Paused);
                continue;
            }
        };

        if !buf.push_for_epoch(
            epoch,
            StreamEvent::TrackStart {
                path: chosen.clone(),
                mode_label,
                report: session.report().clone(),
                total_duration_s,
            },
        ) {
            continue;
        }

        // Stream bounded source blocks incrementally through the 5-stage f64 DSP session.
        let mut aborted = false;
        loop {
            if ctrl.epoch() != epoch {
                aborted = true;
                break;
            }
            let next_src = match source.read_next_block(DEFAULT_SOURCE_BLOCK_FRAMES) {
                Ok(Some(b)) => b,
                Ok(None) => break, // clean EOF
                Err(err) => {
                    let msg = format!("FLAC stream error in '{}': {err}", chosen.display());
                    eprintln!("  [stream err] {msg}");
                    shared.record_action(msg);
                    break;
                }
            };

            let pcm_block = match session.process_block(next_src) {
                Ok(b) => b,
                Err(err) => {
                    let msg = format!("DSP block error on '{}': {err}", chosen.display());
                    eprintln!("  [dsp] {msg}");
                    shared.record_action(msg);
                    aborted = true;
                    break;
                }
            };

            if !pcm_block.is_empty()
                && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm_block))
            {
                aborted = true;
                break;
            }
        }

        if !aborted && ctrl.epoch() == epoch {
            if let Ok(tail) = session.flush() {
                if !tail.is_empty() {
                    let _ = buf.push_for_epoch(epoch, StreamEvent::PcmBlock(tail));
                }
            }
            let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
        }
    }
}

fn print_shutdown_banner(sig: ShutdownSignal) {
    println!("audiophile-player received {sig}; shutting down gracefully...");
    println!("audiophile-player stopped cleanly.");
    let _ = io::stdout().flush();
}
