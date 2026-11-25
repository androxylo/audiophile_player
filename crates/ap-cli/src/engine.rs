//! Headless playback coordinator wiring the bounded-memory streaming pipeline:
//!   `FlacBlockReader` (`DEFAULT_SOURCE_BLOCK_FRAMES` chunks — never loads multi-GB files)
//!   -> Tidy 5-Stage `f64` `StreamingPipelineSession` (Headroom -> Upsampler -> Convolver
//!      -> Delay -> StreamingQuantizer)
//!   -> `BoundedBlockStreamBuffer` (~12 s lead cap, 2 s prebuffer watermark — zero hiccups)
//!   -> Native Rust `WDM-KS` WaveRT `play_block_stream` (zero Windows mixer, Audirvana-friendly Pause)
//!   -> Decoupled Localhost HTTP Control UI (`ap-control`, safe port 8085+).

use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ap_control::security::validate_playable_flac_path;
use ap_control::server::{ControlServer, clear_runtime_url_file, write_runtime_url_file};
use ap_control::state::{ControlCommand, PlayModeLabel, SharedControl};
use ap_core::buffer::DEFAULT_SOURCE_BLOCK_FRAMES;
use ap_core::config::{ConvolutionConfig, PlayerConfig};
use ap_core::format::DacWireFormat;
use ap_dsp::pipeline::{AudioPipeline, GaplessAttachOutcome, GaplessCarryover};
use ap_dsp::selector::{UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsamplerPolicy};
use ap_output_win::hygiene::OptimizationLevel;
use ap_output_win::sink::OutputError;
use ap_output_win::wdm_ks::WdmKsSink;
use ap_source::flac::{
    AudioBlockSource, FlacBlockReader, UNSUPPORTED_CHANNEL_LAYOUT_PREFIX,
};
use ap_source::library::{SequentialFlacCursor, default_user_music_dir, pick_random_flac_walk};

use crate::prefetch::{
    BoundedBlockStreamBuffer, DEFAULT_MAX_BUFFERED_FRAMES, PREBUFFER_SECONDS, PopBlockResult,
    StreamEvent,
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

/// Active playback modes representable in `EngineState::last_play`.
/// Making `Paused` and `Shutdown` unrepresentable here eliminates unreachable match arms.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlayDirective {
    Random {
        root: PathBuf,
    },
    Sequential {
        root: PathBuf,
        seq_gen: u64,
        start_after: Option<PathBuf>,
        /// When true (`SingleFile` -> `Next`), iterate only `root`'s direct `.flac` files
        /// without descending into subdirectories.
        folder_only: bool,
        /// Additional tracks to skip after `start_after` (supports rapid chained flush-`Next`
        /// clicks before the producer buffers the next `TrackStart`).
        skip_count: usize,
    },
    SingleFile {
        file: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProducerDirective {
    Paused,
    Active(PlayDirective),
    Shutdown,
}

struct ProducerControl {
    state: Mutex<(u64, ProducerDirective)>,
    cv: Condvar,
    #[cfg(test)]
    before_pause_enter: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    before_pause_release: Option<Arc<std::sync::Barrier>>,
}

impl ProducerControl {
    fn new(initial_epoch: u64, initial: ProducerDirective) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((initial_epoch, initial)),
            cv: Condvar::new(),
            #[cfg(test)]
            before_pause_enter: None,
            #[cfg(test)]
            before_pause_release: None,
        })
    }

    #[cfg(test)]
    fn new_with_pause_barriers(
        initial_epoch: u64,
        initial: ProducerDirective,
        enter: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((initial_epoch, initial)),
            cv: Condvar::new(),
            before_pause_enter: Some(enter),
            before_pause_release: Some(release),
        })
    }

    /// Publish a new `(epoch, directive)`. `Shutdown` is sticky and can never be overwritten.
    fn set(&self, epoch: u64, directive: ProducerDirective) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(st.1, ProducerDirective::Shutdown) {
            return;
        }
        *st = (epoch, directive);
        self.cv.notify_all();
    }

    /// Producer-side compare-and-set: transition to `Paused` ONLY if `Shutdown` has not been
    /// requested and the control epoch still equals `expected_epoch`. Prevents stale producer
    /// exits from overwriting `Shutdown` (Ctrl+C hang) or a newer `Next`/play directive.
    fn pause_if_epoch(&self, expected_epoch: u64) -> bool {
        #[cfg(test)]
        if let Some(b) = &self.before_pause_enter {
            b.wait();
        }
        #[cfg(test)]
        if let Some(b) = &self.before_pause_release {
            b.wait();
        }
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(st.1, ProducerDirective::Shutdown) || st.0 != expected_epoch {
            return false;
        }
        st.1 = ProducerDirective::Paused;
        self.cv.notify_all();
        true
    }

    fn wait_active(&self) -> Option<(u64, PlayDirective)> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &st.1 {
                ProducerDirective::Shutdown => return None,
                ProducerDirective::Paused => {
                    st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                ProducerDirective::Active(dir) => return Some((st.0, dir.clone())),
            }
        }
    }

    fn epoch(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0
    }
}

/// Consolidated consumer-side playback coordinator state.
struct EngineState<'a> {
    is_paused: bool,
    active_epoch: u64,
    last_play: PlayDirective,
    /// Path from the most recent `TrackStart` started/skipped by the consumer (or the last
    /// attempted path when an error-threshold auto-pause occurred).
    last_started_track: Option<PathBuf>,
    /// True after a `Next` skip until the next track actually starts playing, so rapid chained
    /// `Next` clicks advance past a pre-buffered front `TrackStart` without double-skipping on
    /// a late `Next` right after a natural track end.
    just_skipped_track: bool,
    /// Number of extra tracks to skip past `last_started_track` when consecutive flush-`Next`
    /// clicks arrive before the producer delivers the next `TrackStart`.
    pending_flush_skips: usize,
    /// True while a flush-`Next` restart is in flight and no new `TrackStart` has started yet.
    awaiting_post_skip_start: bool,
    next_seq_gen: u64,
    buf: &'a BoundedBlockStreamBuffer,
    prod_ctrl: &'a ProducerControl,
    shared: &'a SharedControl,
}

impl<'a> EngineState<'a> {
    fn new(
        initial_epoch: u64,
        initial_dir: PlayDirective,
        buf: &'a BoundedBlockStreamBuffer,
        prod_ctrl: &'a ProducerControl,
        shared: &'a SharedControl,
    ) -> Self {
        let next_seq_gen = match &initial_dir {
            PlayDirective::Sequential { seq_gen, .. } => seq_gen.wrapping_add(1),
            _ => 1,
        };
        let awaiting_first = !matches!(initial_dir, PlayDirective::SingleFile { .. });
        Self {
            is_paused: false,
            active_epoch: initial_epoch,
            last_play: initial_dir,
            last_started_track: None,
            just_skipped_track: awaiting_first,
            pending_flush_skips: 0,
            awaiting_post_skip_start: awaiting_first,
            next_seq_gen,
            buf,
            prod_ctrl,
            shared,
        }
    }

    fn alloc_seq_gen(&mut self) -> u64 {
        let g = self.next_seq_gen;
        self.next_seq_gen = self.next_seq_gen.wrapping_add(1);
        g
    }

    fn record_anchor(&mut self, path: PathBuf) {
        self.last_started_track = Some(path);
        self.pending_flush_skips = 0;
        self.awaiting_post_skip_start = false;
    }

    fn start_directive(&mut self, dir: PlayDirective, reset_started_track: bool) {
        let ep = self.buf.reset_and_flush();
        if reset_started_track {
            self.last_started_track = None;
        }
        // Review #3 Finding #20: for PlaySequential / PlayRandom, we have not started playing the
        // first track yet, so a Next arriving before Track 1 plays should advance past Track 1
        // (dropping any buffered TrackStart(01) or restarting with skip_count=1) rather than
        // replaying Track 1. SingleFile handles Next explicitly in dispatch().
        let awaiting_first = !matches!(dir, PlayDirective::SingleFile { .. });
        self.pending_flush_skips = 0;
        self.awaiting_post_skip_start = awaiting_first;
        self.last_play = dir.clone();
        self.active_epoch = ep;
        self.is_paused = false;
        self.just_skipped_track = awaiting_first;
        self.prod_ctrl.set(ep, ProducerDirective::Active(dir));
    }

    fn pause_with_message(&mut self, msg: impl Into<String>) {
        let ep = self.buf.reset_and_flush();
        self.active_epoch = ep;
        self.is_paused = true;
        self.just_skipped_track = false;
        self.pending_flush_skips = 0;
        self.awaiting_post_skip_start = false;
        self.prod_ctrl.set(ep, ProducerDirective::Paused);
        self.shared.set_paused(msg.into());
    }

    /// Rebuild a `Sequential` directive positioned strictly after `last_started_track` (if any),
    /// skipping `skip_count` additional tracks and using a fresh `seq_gen` so the producer's
    /// decode-ahead cursor can never skip an unplayed track or collapse consecutive Nexts.
    fn sequential_after_last_started(
        &mut self,
        root: PathBuf,
        folder_only: bool,
        skip_count: usize,
    ) -> PlayDirective {
        let seq_gen = self.alloc_seq_gen();
        PlayDirective::Sequential {
            root,
            seq_gen,
            start_after: self.last_started_track.clone(),
            folder_only,
            skip_count,
        }
    }

    fn dispatch(&mut self, cmd: ControlCommand) {
        match cmd {
            ControlCommand::PlayRandom(root) => {
                self.start_directive(PlayDirective::Random { root }, true);
            }
            ControlCommand::PlaySequential(root) => {
                let seq_gen = self.alloc_seq_gen();
                self.start_directive(
                    PlayDirective::Sequential {
                        root,
                        seq_gen,
                        start_after: None,
                        folder_only: false,
                        skip_count: 0,
                    },
                    true,
                );
            }
            ControlCommand::PlayFile(file) => {
                let root = self.shared.allowed_root();
                match validate_playable_flac_path(&file, Some(&root)) {
                    Ok(valid) => {
                        self.start_directive(PlayDirective::SingleFile { file: valid }, true);
                    }
                    Err(reason) => {
                        let msg = format!(
                            "Rejected invalid PlayFile path '{}': {reason}",
                            file.display()
                        );
                        eprintln!("  [reject] {msg}");
                        self.shared.record_action(msg);
                    }
                }
            }
            ControlCommand::Next => {
                println!("  [next] Skipping to next track...");
                let _ = io::stdout().flush();

                // From SingleFile, advance sequentially to the next file in that folder only,
                // anchoring `last_started_track = Some(file)` so a rapid second `Next` skips
                // further forward instead of jumping back to the first file in the folder.
                if let PlayDirective::SingleFile { file } = self.last_play.clone() {
                    let parent = file
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.shared.snapshot().active_folder);
                    let seq_gen = self.alloc_seq_gen();
                    let dir = PlayDirective::Sequential {
                        root: parent,
                        seq_gen,
                        start_after: Some(file.clone()),
                        folder_only: true,
                        skip_count: 0,
                    };
                    self.start_directive(dir, false);
                    self.last_started_track = Some(file);
                    self.awaiting_post_skip_start = true;
                    self.just_skipped_track = true;
                    return;
                }

                if !self.is_paused {
                    let (reused_prebuffered, ep, skipped_front) = self
                        .buf
                        .skip_to_next_track_or_flush(self.active_epoch, self.just_skipped_track);
                    if let Some(skipped_path) = skipped_front {
                        self.record_anchor(skipped_path);
                    }
                    self.active_epoch = ep;
                    self.just_skipped_track = true;
                    if reused_prebuffered {
                        return;
                    }
                    // Non-reuse flush: if another flush-Next already ran before a new TrackStart
                    // started, increment `pending_flush_skips` so consecutive Next clicks advance
                    // k tracks instead of collapsing onto the same restart anchor.
                    if self.awaiting_post_skip_start {
                        self.pending_flush_skips = self.pending_flush_skips.saturating_add(1);
                    } else {
                        self.pending_flush_skips = 0;
                        self.awaiting_post_skip_start = true;
                    }
                    let skip_count = self.pending_flush_skips;
                    let restart_dir = match self.last_play.clone() {
                        PlayDirective::Sequential {
                            root, folder_only, ..
                        } => self.sequential_after_last_started(root, folder_only, skip_count),
                        other => other,
                    };
                    self.last_play = restart_dir.clone();
                    self.prod_ctrl.set(ep, ProducerDirective::Active(restart_dir));
                    return;
                }

                // Paused -> Next: resume the next track in the last active mode/folder, anchored
                // at `last_started_track` for Sequential so prefetched tracks discarded on Pause
                // are played instead of skipped (and error-threshold pauses resume past bad runs).
                let resume_dir = match self.last_play.clone() {
                    PlayDirective::Sequential {
                        root, folder_only, ..
                    } => self.sequential_after_last_started(root, folder_only, 0),
                    other => other,
                };
                self.start_directive(resume_dir, false);
                self.awaiting_post_skip_start = true;
                self.just_skipped_track = true;
            }
            ControlCommand::Pause => {
                self.pause_with_message(
                    "Paused — WDM-KS kernel pin closed (DAC available to Audirvana)",
                );
                println!("  [paused] WDM-KS kernel pin closed (DAC released).");
                let _ = io::stdout().flush();
            }
        }
    }
}

/// Run default zero-config random FLAC playback via bounded-memory block streaming,
/// tidy `AudioPipeline`, native Windows Kernel Streaming (`WDM-KS`), and localhost HTTP UI.
pub fn run_random_music_player(
    config_path: Option<PathBuf>,
    music_dir_override: Option<PathBuf>,
    device_filter: Option<String>,
    optimization: OptimizationLevel,
    http_port: Option<u16>,
    upsampler_policy: UpsamplerPolicy,
    rew_override: Option<ConvolutionConfig>,
    controller: &ShutdownController,
) -> Result<(), String> {
    let loaded_cfg = PlayerConfig::discover_and_load(config_path.as_deref())?;
    let effective_conv = rew_override.unwrap_or(loaded_cfg.config.convolution);
    effective_conv.validate_paths()?;

    let pipeline = AudioPipeline::with_rew_config(upsampler_policy, effective_conv)?;
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
    controller.attach_stop_flag(stop_handle.clone());
    controller.attach_shared_control(shared.clone());

    let http_server = ControlServer::start(shared.clone(), http_port)?;
    let http_url = http_server.url();
    write_runtime_url_file(&http_url);

    println!(
        "audiophile-player v{} started (WDM-KS WaveRT, streaming blocks, opt={optimization:?}, DAC='{dac_name}', music='{}').",
        env!("CARGO_PKG_VERSION"),
        music_dir.display()
    );
    match &loaded_cfg.source_path {
        Some(path) => println!("Config file: {}", path.display()),
        None => println!("Config file: none (using built-in defaults; convolution disabled)"),
    }
    println!("Web control: {}", http_server.url());
    println!("Press Ctrl+C or Ctrl+Break to stop gracefully.");
    let _ = io::stdout().flush();

    let stream_buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
    let initial_epoch = stream_buf.current_epoch();
    let initial_dir = PlayDirective::Random {
        root: music_dir.clone(),
    };
    let prod_ctrl =
        ProducerControl::new(initial_epoch, ProducerDirective::Active(initial_dir.clone()));

    // Dedicated background streaming DSP Producer thread (bounded ~12 s / 36 MB cap).
    let buf_bg = Arc::clone(&stream_buf);
    let ctrl_bg = Arc::clone(&prod_ctrl);
    let shared_bg = Arc::clone(&shared);
    let producer_thread = thread::Builder::new()
        .name("ap-dsp-stream".to_string())
        .spawn(move || producer_loop(pipeline, ctrl_bg, buf_bg, shared_bg))
        .map_err(|e| format!("failed spawning DSP streaming thread: {e}"))?;

    let mut state = EngineState::new(
        initial_epoch,
        initial_dir,
        &stream_buf,
        &prod_ctrl,
        &shared,
    );

    while controller.current_signal().is_none() {
        if let Some(cmd) = shared.take_pending_command() {
            state.dispatch(cmd);
            continue;
        }

        if state.is_paused {
            // DAC WDM-KS pin is completely closed & released — Audirvana can play freely!
            match shared.wait_for_command_or_shutdown() {
                Some(cmd) => {
                    state.dispatch(cmd);
                    continue;
                }
                None => break,
            }
        }

        let Some(event) =
            stream_buf.pop_for_epoch_timeout(state.active_epoch, Duration::from_millis(50))
        else {
            if producer_thread.is_finished() && controller.current_signal().is_none() {
                let msg = "DSP producer thread stopped unexpectedly — paused & DAC released";
                eprintln!("  [error] {msg}");
                state.pause_with_message(msg);
            }
            continue;
        };

        if controller.current_signal().is_some() {
            break;
        }
        // If a command arrived right as `pop_for_epoch_timeout` returned, put the popped event
        // back at the front before dispatching so a late `Next` does not double-skip a TrackStart.
        if let Some(cmd) = shared.take_pending_command() {
            let _ = stream_buf.push_front_for_epoch(state.active_epoch, event);
            state.dispatch(cmd);
            continue;
        }

        match event {
            StreamEvent::EndOfQueue(msg) | StreamEvent::FatalError(msg) => {
                state.pause_with_message(msg);
            }
            StreamEvent::ErrorThresholdPause {
                message,
                last_attempted,
            } => {
                state.record_anchor(last_attempted);
                state.pause_with_message(message);
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
                had_gapless_carryover,
            } => {
                let out_rate = report.output_rate_hz;
                let watermark_frames = (out_rate as usize) * (PREBUFFER_SECONDS as usize);

                // Wait until ~2 seconds of PCM are pre-buffered in RAM (or TrackEnd for short clips)
                // while remaining immediately responsive to Pause / Next / Ctrl+C.
                let mut aborted_prebuffer = false;
                let mut pending_cmd: Option<ControlCommand> = None;
                loop {
                    if controller.current_signal().is_some() {
                        aborted_prebuffer = true;
                        break;
                    }
                    if let Some(cmd) = shared.take_pending_command() {
                        pending_cmd = Some(cmd);
                        aborted_prebuffer = true;
                        break;
                    }
                    match stream_buf.wait_prebuffer_watermark(
                        state.active_epoch,
                        watermark_frames,
                        Duration::from_millis(50),
                    ) {
                        Some(true) => break,
                        Some(false) => {
                            if producer_thread.is_finished() {
                                let msg =
                                    "DSP producer thread stopped unexpectedly — paused & DAC released";
                                eprintln!("  [error] {msg}");
                                state.pause_with_message(msg);
                                aborted_prebuffer = true;
                                break;
                            }
                        }
                        None => {
                            aborted_prebuffer = true;
                            break;
                        }
                    }
                }

                // Review #3 Finding #27: re-queue the popped TrackStart before dispatching so
                // skip_to_next_track_or_flush applies identical just_skipped_track protection
                // across line 505, prebuffer, and stop_flag windows (no double-skip after natural EOF).
                if let Some(cmd) = pending_cmd {
                    let _ = stream_buf.push_front_for_epoch(
                        state.active_epoch,
                        StreamEvent::TrackStart {
                            path,
                            mode_label,
                            report,
                            total_duration_s,
                            had_gapless_carryover,
                        },
                    );
                    state.dispatch(cmd);
                    continue;
                }

                if aborted_prebuffer || state.is_paused || controller.current_signal().is_some() {
                    continue;
                }

                // Arm `stop_flag` here BEFORE the final signal/command check (never inside
                // `wdm_ks.rs`) so any Ctrl+C, Pause, or Next arriving right before open is
                // either caught immediately below or leaves `stop_flag == true` for the sink.
                stop_handle.store(false, Ordering::SeqCst);
                if controller.current_signal().is_some() {
                    break;
                }
                if let Some(cmd) = shared.take_pending_command() {
                    let _ = stream_buf.push_front_for_epoch(
                        state.active_epoch,
                        StreamEvent::TrackStart {
                            path,
                            mode_label,
                            report,
                            total_duration_s,
                            had_gapless_carryover,
                        },
                    );
                    state.dispatch(cmd);
                    continue;
                }

                // Record the track actually started by the consumer (not producer decode-ahead).
                state.record_anchor(path.clone());
                state.just_skipped_track = false;

                let fmt_str = report.format_summary(total_duration_s);
                let track_str = path.display().to_string();
                println!("  [playing]  {track_str} ({fmt_str})");
                println!("  [pipeline] {}", report.log_line());
                let _ = io::stdout().flush();
                shared.record_action(report.log_line());
                shared.set_playing(mode_label, track_str, fmt_str);

                let ep_for_stream = state.active_epoch;
                let buf_for_stream = Arc::clone(&stream_buf);
                match sink.play_block_stream(out_rate, DacWireFormat::Int32, move || {
                    match buf_for_stream.try_pop_track_pcm_block(ep_for_stream) {
                        PopBlockResult::Block(b) => ap_output_win::wdm_ks::StreamBlock::Ready(b),
                        PopBlockResult::Underrun => ap_output_win::wdm_ks::StreamBlock::Underrun,
                        PopBlockResult::TrackEnd | PopBlockResult::StaleOrClosed => {
                            ap_output_win::wdm_ks::StreamBlock::EndOfTrack
                        }
                    }
                }) {
                    Ok(_stats) => {}
                    Err(err @ OutputError::DeviceBusy { .. }) => {
                        let msg = format!("DAC busy ({err}) — staying paused");
                        eprintln!("  [busy] {msg}");
                        state.pause_with_message(msg);
                    }
                    Err(err) => {
                        let msg = format!("Output error ({err}) — paused & DAC released");
                        eprintln!("  [error] {msg}");
                        state.pause_with_message(msg);
                    }
                }
            }
        }
    }

    teardown_player_resources(
        &stream_buf,
        &prod_ctrl,
        state.active_epoch,
        http_server,
        &http_url,
        sink,
        producer_thread,
        controller,
        Duration::from_millis(2500),
        Duration::from_millis(1200),
    );

    if let Some(sig) = controller.current_signal() {
        print_shutdown_banner(sig);
    } else {
        println!("audiophile-player stopped cleanly.");
        let _ = io::stdout().flush();
    }

    Ok(())
}

/// Ordered graceful teardown shared by `run_random_music_player` and teardown-order tests:
/// 1. Close stream buffer & signal producer `Shutdown` (unblocks producer backpressure).
/// 2. Shut down HTTP server & remove runtime `server.url`.
/// 3. Drop the audio sink (releases WDM-KS DAC pin immediately).
/// 4. Join the producer thread with a bounded deadline (never hangs on stalled NAS / FFT).
#[allow(clippy::too_many_arguments)]
fn teardown_player_resources<S>(
    stream_buf: &BoundedBlockStreamBuffer,
    prod_ctrl: &ProducerControl,
    active_epoch: u64,
    mut http_server: ControlServer,
    http_url: &str,
    sink: S,
    producer_thread: thread::JoinHandle<()>,
    controller: &ShutdownController,
    normal_timeout: Duration,
    fast_timeout: Duration,
) -> bool {
    stream_buf.close();
    prod_ctrl.set(active_epoch.wrapping_add(1), ProducerDirective::Shutdown);
    http_server.shutdown();
    clear_runtime_url_file(http_url);
    drop(sink);
    join_producer_bounded(producer_thread, controller, normal_timeout, fast_timeout)
}

/// Wait up to `normal_timeout` (or `fast_timeout` if a Windows console-close / logoff / shutdown
/// signal has arrived or escalates mid-wait) for the producer thread to finish, joining cleanly
/// if it exits in time and detaching cleanly if blocked in an uninterruptible OS call.
///
/// Returns `true` if the producer exited and was joined within the bounded deadline.
fn join_producer_bounded(
    producer_thread: thread::JoinHandle<()>,
    controller: &ShutdownController,
    normal_timeout: Duration,
    fast_timeout: Duration,
) -> bool {
    let started = Instant::now();
    loop {
        if producer_thread.is_finished() {
            let _ = producer_thread.join();
            return true;
        }
        let budget = if controller.fast_teardown_requested() {
            fast_timeout
        } else {
            normal_timeout
        };
        if started.elapsed() >= budget {
            return false;
        }
        thread::sleep(Duration::from_millis(15));
    }
}

/// Outcome of decoding and streaming one chosen FLAC track inside `producer_loop`.
#[derive(Debug)]
enum TrackStreamOutcome {
    /// At least one non-empty PCM block was pushed and the track completed cleanly (or reached EOF).
    Played,
    /// Epoch changed or buffer closed mid-track — caller should simply re-check control state.
    Aborted,
    /// Valid FLAC whose channel layout is unsupported (e.g. 5.1 surround); distinct from corruption.
    Unsupported(String),
    /// Track failed to open, failed DSP setup, panicked, or yielded 0 PCM frames.
    Failed(String),
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
    const MAX_CONSECUTIVE_UNSUPPORTED: u32 = 50;

    let mut seq_cursor: Option<(u64, SequentialFlacCursor)> = None;
    let mut single_consumed_epoch: Option<u64> = None;
    let mut last_random: Option<PathBuf> = None;
    let mut last_seen_epoch: Option<u64> = None;
    let mut consecutive_errors: u32 = 0;
    let mut consecutive_unsupported: u32 = 0;
    let mut carried_tail: Option<GaplessCarryover> = None;
    let mut pending_track_end = false;

    let finalize_prev_track = |pipeline: &AudioPipeline,
                               buf: &BoundedBlockStreamBuffer,
                               epoch: u64,
                               carried: &mut Option<GaplessCarryover>,
                               pending_end: &mut bool| {
        if let Some(tail) = carried.take() {
            if let Ok(pcm) = pipeline.drain_gapless_carryover(tail) {
                if !pcm.is_empty() {
                    let _ = buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm));
                }
            }
        }
        if *pending_end {
            *pending_end = false;
            let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
        }
    };

    while let Some((epoch, directive)) = ctrl.wait_active() {
        // Guard against the brief window where `buf.reset_and_flush()` bumped the buffer epoch
        // right before `prod_ctrl.set()` published the matching new directive.
        if buf.current_epoch() != epoch {
            thread::yield_now();
            continue;
        }
        if ctrl.epoch() != epoch {
            continue;
        }
        if last_seen_epoch != Some(epoch) {
            consecutive_errors = 0;
            consecutive_unsupported = 0;
            carried_tail = None;
            pending_track_end = false;
            last_seen_epoch = Some(epoch);
        }

        let is_single_file = matches!(directive, PlayDirective::SingleFile { .. });

        let (chosen, mode_label) = match directive {
            PlayDirective::Random { root } => {
                let mut picked = match pick_random_flac_walk(&root) {
                    Ok(p) => p,
                    Err(err) => {
                        finalize_prev_track(
                            &pipeline,
                            &buf,
                            epoch,
                            &mut carried_tail,
                            &mut pending_track_end,
                        );
                        let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(err));
                        ctrl.pause_if_epoch(epoch);
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
            PlayDirective::Sequential {
                root,
                seq_gen,
                start_after,
                folder_only,
                skip_count,
            } => {
                let needs_init = seq_cursor.as_ref().is_none_or(|(g, _)| *g != seq_gen);
                if needs_init {
                    let cursor_res = if folder_only {
                        SequentialFlacCursor::new_folder_only(
                            root.clone(),
                            start_after.as_deref(),
                        )
                    } else if let Some(after) = start_after.as_deref() {
                        SequentialFlacCursor::new_starting_after(root.clone(), after)
                    } else {
                        SequentialFlacCursor::new(root.clone())
                    };
                    match cursor_res {
                        Ok(mut c) => {
                            for _ in 0..skip_count {
                                if c.next_track().is_none() {
                                    break;
                                }
                            }
                            seq_cursor = Some((seq_gen, c));
                        }
                        Err(err) => {
                            finalize_prev_track(
                                &pipeline,
                                &buf,
                                epoch,
                                &mut carried_tail,
                                &mut pending_track_end,
                            );
                            let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(err));
                            ctrl.pause_if_epoch(epoch);
                            continue;
                        }
                    }
                }
                let next = seq_cursor.as_mut().and_then(|(_, c)| c.next_track());
                match next {
                    Some(p) => (p, PlayModeLabel::Sequential),
                    None => {
                        finalize_prev_track(
                            &pipeline,
                            &buf,
                            epoch,
                            &mut carried_tail,
                            &mut pending_track_end,
                        );
                        let _ = buf.push_for_epoch(
                            epoch,
                            StreamEvent::EndOfQueue(format!(
                                "Finished sequential playback of '{}' — DAC released",
                                root.display()
                            )),
                        );
                        ctrl.pause_if_epoch(epoch);
                        continue;
                    }
                }
            }
            PlayDirective::SingleFile { file } => {
                if single_consumed_epoch == Some(epoch) {
                    finalize_prev_track(
                        &pipeline,
                        &buf,
                        epoch,
                        &mut carried_tail,
                        &mut pending_track_end,
                    );
                    let _ = buf.push_for_epoch(
                        epoch,
                        StreamEvent::EndOfQueue("Finished single track — DAC released".into()),
                    );
                    ctrl.pause_if_epoch(epoch);
                    continue;
                }
                single_consumed_epoch = Some(epoch);
                (file, PlayModeLabel::SingleFile)
            }
        };

        if ctrl.epoch() != epoch || buf.current_epoch() != epoch {
            continue;
        }

        // Isolate per-track decode & DSP in `catch_unwind` so a malformed FLAC panic never kills
        // the producer thread or wedges the UI. Passing `&mut carried_tail` / `&mut pending_track_end`
        // preserves the previous track's carryover if a panic occurs before handoff so `finalize_prev_track`
        // still drains it cleanly onto the previous track.
        if is_single_file {
            finalize_prev_track(
                &pipeline,
                &buf,
                epoch,
                &mut carried_tail,
                &mut pending_track_end,
            );
        }

        let (outcome, next_carryover, next_pending_end) = match catch_unwind(AssertUnwindSafe(|| {
            stream_single_track(
                &pipeline,
                &ctrl,
                &buf,
                &shared,
                epoch,
                &chosen,
                mode_label,
                &mut carried_tail,
                &mut pending_track_end,
                !is_single_file,
            )
        })) {
            Ok(res) => res,
            Err(_) => {
                finalize_prev_track(
                    &pipeline,
                    &buf,
                    epoch,
                    &mut carried_tail,
                    &mut pending_track_end,
                );
                (
                    TrackStreamOutcome::Failed(format!(
                        "Internal panic while decoding/processing '{}'",
                        chosen.display()
                    )),
                    None,
                    false,
                )
            }
        };

        match outcome {
            TrackStreamOutcome::Played => {
                consecutive_errors = 0;
                consecutive_unsupported = 0;
                carried_tail = next_carryover;
                pending_track_end = next_pending_end;
            }
            TrackStreamOutcome::Aborted => {
                carried_tail = None;
                pending_track_end = false;
            }
            TrackStreamOutcome::Unsupported(msg) => {
                carried_tail = next_carryover;
                pending_track_end = next_pending_end;
                eprintln!("  [skip] {msg}");
                shared.record_action(msg.clone());
                if is_single_file {
                    let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                    ctrl.pause_if_epoch(epoch);
                    continue;
                }
                consecutive_unsupported += 1;
                if consecutive_unsupported >= MAX_CONSECUTIVE_UNSUPPORTED {
                    finalize_prev_track(
                        &pipeline,
                        &buf,
                        epoch,
                        &mut carried_tail,
                        &mut pending_track_end,
                    );
                    let _ = buf.push_for_epoch(
                        epoch,
                        StreamEvent::ErrorThresholdPause {
                            message: format!(
                                "Paused after {MAX_CONSECUTIVE_UNSUPPORTED} consecutive unsupported FLACs — DAC released"
                            ),
                            last_attempted: chosen.clone(),
                        },
                    );
                    ctrl.pause_if_epoch(epoch);
                    consecutive_unsupported = 0;
                }
            }
            TrackStreamOutcome::Failed(msg) => {
                carried_tail = next_carryover;
                pending_track_end = next_pending_end;
                eprintln!("  [skip] {msg}");
                shared.record_action(msg.clone());
                if is_single_file {
                    let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                    ctrl.pause_if_epoch(epoch);
                    continue;
                }
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    finalize_prev_track(
                        &pipeline,
                        &buf,
                        epoch,
                        &mut carried_tail,
                        &mut pending_track_end,
                    );
                    let _ = buf.push_for_epoch(
                        epoch,
                        StreamEvent::ErrorThresholdPause {
                            message:
                                "Paused after 10 consecutive unreadable/unplayable FLACs — DAC released"
                                    .into(),
                            last_attempted: chosen.clone(),
                        },
                    );
                    ctrl.pause_if_epoch(epoch);
                    consecutive_errors = 0;
                }
            }
        }
    }
}

/// Decode and stream one FLAC track through `pipeline` into `buf`.
///
/// Defers BOTH gapless carryover handoff AND `TrackStart` emission until the next track produces
/// a real non-empty decoded block, so 0-frame or immediately-corrupt tracks never drop the
/// previous track's REW convolution tail / upsampler history or open the DAC pin unnecessarily.
/// When `*had_pending_prev_end` is true, flushes `*incoming_carryover` onto the previous track
/// before closing it whenever this track fails before producing PCM or changes output rate.
#[allow(clippy::too_many_arguments)]
fn stream_single_track(
    pipeline: &AudioPipeline,
    ctrl: &ProducerControl,
    buf: &BoundedBlockStreamBuffer,
    shared: &SharedControl,
    epoch: u64,
    chosen: &Path,
    mode_label: PlayModeLabel,
    incoming_carryover: &mut Option<GaplessCarryover>,
    had_pending_prev_end: &mut bool,
    gapless_carryover: bool,
) -> (TrackStreamOutcome, Option<GaplessCarryover>, bool) {
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

    let allowed_root = shared.allowed_root();
    if let Err(reason) = validate_playable_flac_path(chosen, Some(&allowed_root)) {
        if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        return (
            TrackStreamOutcome::Failed(format!(
                "Rejected non-playable path '{}': {reason}",
                chosen.display()
            )),
            None,
            false,
        );
    }

    let mut source = match FlacBlockReader::open(chosen) {
        Ok(s) => s,
        Err(err) => {
            if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                return (TrackStreamOutcome::Aborted, None, false);
            }
            if err.starts_with(UNSUPPORTED_CHANNEL_LAYOUT_PREFIX) {
                return (
                    TrackStreamOutcome::Unsupported(format!(
                        "Skipped unsupported FLAC '{}': {err}",
                        chosen.display()
                    )),
                    None,
                    false,
                );
            }
            return (
                TrackStreamOutcome::Failed(format!(
                    "Skipped unreadable FLAC '{}': {err}",
                    chosen.display()
                )),
                None,
                false,
            );
        }
    };

    let spec = source.spec();
    let peak_meta = source.peak_meta();
    let total_duration_s = source
        .total_frames()
        .map(|f| f as f64 / spec.sample_rate_hz as f64);

    let mut session = match pipeline.start_track_for_carryover(
        spec,
        &peak_meta,
        source.is_streaming(),
        incoming_carryover.as_ref(),
    ) {
        Ok(s) => s,
        Err(err) => {
            if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                return (TrackStreamOutcome::Aborted, None, false);
            }
            if err.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX) {
                return (
                    TrackStreamOutcome::Unsupported(format!(
                        "Skipped unsupported sample rate on '{}': {err}",
                        chosen.display()
                    )),
                    None,
                    false,
                );
            }
            return (
                TrackStreamOutcome::Failed(format!(
                    "DSP pipeline error on '{}': {err}",
                    chosen.display()
                )),
                None,
                false,
            );
        }
    };

    // Review #3 Finding #3: decode and validate the first non-empty block BEFORE touching
    // `incoming_carryover` or closing `TrackEnd(prev)`. If this track is 0-frame or corrupt on
    // its first block, `incoming_carryover` is drained intact onto the previous track!
    let first_src = loop {
        if ctrl.epoch() != epoch {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        match source.read_next_block(DEFAULT_SOURCE_BLOCK_FRAMES) {
            Ok(Some(b)) if b.is_empty() => continue,
            Ok(Some(b)) => break b,
            Ok(None) => {
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                return (
                    TrackStreamOutcome::Failed(format!(
                        "Skipped empty (0-frame) FLAC '{}'",
                        chosen.display()
                    )),
                    None,
                    false,
                );
            }
            Err(err) => {
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                return (
                    TrackStreamOutcome::Failed(format!(
                        "FLAC stream error in '{}': {err}",
                        chosen.display()
                    )),
                    None,
                    false,
                );
            }
        }
    };
    if first_src.sample_rate_hz != spec.sample_rate_hz
        || first_src.left.len() != first_src.right.len()
    {
        if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        return (
            TrackStreamOutcome::Failed(format!(
                "Invalid first audio block in '{}'",
                chosen.display()
            )),
            None,
            false,
        );
    }

    // Commit the gapless carryover handoff now that Track B has a real validated first block:
    // - Same output rate + same upsampler geometry -> FullHandoff (0 drain frames)
    // - Same output rate + different upsampler geometry -> ConvolverOnlyHandoff (drain only short
    //   upsampler history onto Track A, keep full REW tail on Track B with zero IR gap!)
    // - Different output rate -> drain full carryover onto Track A before closing TrackEnd(prev).
    let mut had_gapless_carryover = false;
    if let Some(carry) = incoming_carryover.take() {
        let had_active_tail = !carry.is_empty();
        match pipeline.attach_gapless_carryover(&mut session, carry) {
            Ok(GaplessAttachOutcome::FullHandoff) => {
                had_gapless_carryover = had_active_tail;
                if *had_pending_prev_end {
                    *had_pending_prev_end = false;
                    if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                        return (TrackStreamOutcome::Aborted, None, false);
                    }
                }
            }
            Ok(GaplessAttachOutcome::ConvolverOnlyHandoff {
                upsampler_tail_pcm,
                attached_convolver_tail,
            }) => {
                had_gapless_carryover = attached_convolver_tail;
                if !upsampler_tail_pcm.is_empty()
                    && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(upsampler_tail_pcm))
                {
                    *had_pending_prev_end = false;
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                if *had_pending_prev_end {
                    *had_pending_prev_end = false;
                    if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                        return (TrackStreamOutcome::Aborted, None, false);
                    }
                }
            }
            Ok(GaplessAttachOutcome::IncompatibleRate(rejected)) => {
                *incoming_carryover = Some(rejected);
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
            }
            Err(err) => {
                if *had_pending_prev_end {
                    *had_pending_prev_end = false;
                    let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
                }
                return (
                    TrackStreamOutcome::Failed(format!(
                        "Gapless carryover error on '{}': {err}",
                        chosen.display()
                    )),
                    None,
                    false,
                );
            }
        }
    } else if *had_pending_prev_end {
        *had_pending_prev_end = false;
        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
            return (TrackStreamOutcome::Aborted, None, false);
        }
    }

    let mut pending_start = Some(StreamEvent::TrackStart {
        path: chosen.to_path_buf(),
        mode_label,
        report: session.report().clone(),
        total_duration_s,
        had_gapless_carryover,
    });
    let mut pushed_any_pcm = false;
    let mut source_frames_read: usize = 0;
    let mut first_block_error: Option<String> = None;
    let mut pending_first_block = Some(first_src);

    loop {
        if ctrl.epoch() != epoch {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        let next_src = if let Some(b) = pending_first_block.take() {
            b
        } else {
            match source.read_next_block(DEFAULT_SOURCE_BLOCK_FRAMES) {
                Ok(Some(b)) => b,
                Ok(None) => break, // clean EOF
                Err(err) => {
                    let msg = format!("FLAC stream error in '{}': {err}", chosen.display());
                    if !pushed_any_pcm {
                        first_block_error = Some(msg);
                    } else {
                        eprintln!("  [stream err] {msg}");
                        shared.record_action(msg);
                    }
                    break;
                }
            }
        };
        source_frames_read += next_src.len();

        let pcm_block = match session.process_block(next_src) {
            Ok(b) => b,
            Err(err) => {
                let msg = format!("DSP block error on '{}': {err}", chosen.display());
                if !pushed_any_pcm {
                    return (TrackStreamOutcome::Failed(msg), None, false);
                }
                eprintln!("  [dsp] {msg}");
                shared.record_action(msg);
                let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
                return (TrackStreamOutcome::Played, None, false);
            }
        };

        if !pcm_block.is_empty() {
            if let Some(start_ev) = pending_start.take() {
                if !buf.push_for_epoch(epoch, start_ev) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
            }
            if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm_block)) {
                return (TrackStreamOutcome::Aborted, None, false);
            }
            pushed_any_pcm = true;
        }
    }

    if ctrl.epoch() != epoch {
        return (TrackStreamOutcome::Aborted, None, false);
    }

    // Flush DSP tail (only if there wasn't a pre-first-block stream error).
    let mut next_carryover: Option<GaplessCarryover> = None;
    if first_block_error.is_none() && source_frames_read > 0 {
        let flushed = if gapless_carryover {
            session.flush_for_gapless()
        } else {
            session.flush().map(|pcm| (pcm, None))
        };
        if let Ok((tail_pcm, carry)) = flushed {
            next_carryover = carry;
            if !tail_pcm.is_empty() {
                if let Some(start_ev) = pending_start.take() {
                    if !buf.push_for_epoch(epoch, start_ev) {
                        return (TrackStreamOutcome::Aborted, None, false);
                    }
                }
                if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(tail_pcm)) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                pushed_any_pcm = true;
            }
        }
    }

    if !pushed_any_pcm {
        // Ultra-short track (< upsampler half-window) where all samples landed cleanly in carryover:
        // flush immediately so it still plays as its own track rather than being dropped.
        if source_frames_read > 0 {
            if let Some(carry) = next_carryover.take() {
                if let Ok(pcm) = pipeline.drain_gapless_carryover(carry) {
                    if !pcm.is_empty() {
                        if let Some(start_ev) = pending_start.take() {
                            if !buf.push_for_epoch(epoch, start_ev) {
                                return (TrackStreamOutcome::Aborted, None, false);
                            }
                        }
                        if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm)) {
                            return (TrackStreamOutcome::Aborted, None, false);
                        }
                        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                            return (TrackStreamOutcome::Aborted, None, false);
                        }
                        return (TrackStreamOutcome::Played, None, false);
                    }
                }
            }
        }
        return (
            TrackStreamOutcome::Failed(first_block_error.unwrap_or_else(|| {
                format!("Skipped empty (0-frame) FLAC '{}'", chosen.display())
            })),
            None,
            false,
        );
    }

    if next_carryover.is_some() {
        // Keep TrackEnd open until we know whether the next track accepts the gapless carryover;
        // if not (EndOfQueue, rate change, or error), we drain the tail right before TrackEnd.
        return (TrackStreamOutcome::Played, next_carryover, true);
    }

    if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
        return (TrackStreamOutcome::Aborted, None, false);
    }
    (TrackStreamOutcome::Played, None, false)
}

fn print_shutdown_banner(sig: ShutdownSignal) {
    println!("audiophile-player received {sig}; shutting down gracefully...");
    println!("audiophile-player stopped cleanly.");
    let _ = io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_is_sticky_and_pause_if_epoch_cannot_overwrite_shutdown_or_newer_epoch() {
        let ctrl = ProducerControl::new(
            1,
            ProducerDirective::Active(PlayDirective::Random {
                root: PathBuf::from(r"C:\Music"),
            }),
        );

        // Stale epoch cannot pause a newer directive (fixes Next-button race).
        ctrl.set(
            2,
            ProducerDirective::Active(PlayDirective::Sequential {
                root: PathBuf::from(r"C:\Music"),
                seq_gen: 1,
                start_after: None,
                folder_only: false,
                skip_count: 0,
            }),
        );
        assert!(
            !ctrl.pause_if_epoch(1),
            "stale epoch 1 must not pause newer epoch 2"
        );
        assert!(matches!(
            ctrl.wait_active(),
            Some((2, PlayDirective::Sequential { .. }))
        ));

        // Matching epoch pauses cleanly.
        assert!(ctrl.pause_if_epoch(2));

        // Shutdown is sticky: neither `set` nor `pause_if_epoch` can overwrite it.
        ctrl.set(3, ProducerDirective::Shutdown);
        assert!(!ctrl.pause_if_epoch(3));
        ctrl.set(4, ProducerDirective::Paused);
        ctrl.set(
            5,
            ProducerDirective::Active(PlayDirective::Random {
                root: PathBuf::from(r"C:\Music"),
            }),
        );
        assert!(ctrl.wait_active().is_none());
    }

    #[test]
    fn producer_loop_joins_promptly_when_shutdown_races_with_end_of_queue_pause() {
        // Use deterministic barriers to pause the producer RIGHT at `pause_if_epoch(ep)`, then
        // publish `Shutdown` BEFORE releasing it into `pause_if_epoch`'s body. If `pause_if_epoch`
        // were not shutdown-sticky, the producer would overwrite `Shutdown` with `Paused` and hang
        // forever in `wait_active()`.
        let enter = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));

        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.current_epoch();
        let ctrl = ProducerControl::new_with_pause_barriers(
            ep,
            ProducerDirective::Active(PlayDirective::Sequential {
                root: PathBuf::from(r"C:\__definitely_nonexistent_ap_test_dir__"),
                seq_gen: 1,
                start_after: None,
                folder_only: false,
                skip_count: 0,
            }),
            Arc::clone(&enter),
            Arc::clone(&release),
        );
        let shared = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");
        let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();

        let buf_bg = Arc::clone(&buf);
        let ctrl_bg = Arc::clone(&ctrl);
        let shared_bg = Arc::clone(&shared);
        let handle =
            thread::spawn(move || producer_loop(pipeline, ctrl_bg, buf_bg, shared_bg));

        // 1. Wait until producer has actually entered its end-of-queue/error `pause_if_epoch` path!
        enter.wait();
        // 2. Publish Shutdown on the SAME epoch while producer is poised inside `pause_if_epoch`.
        buf.close();
        ctrl.set(ep, ProducerDirective::Shutdown);
        // 3. Release producer into `pause_if_epoch(ep)` — must NOT overwrite Shutdown with Paused!
        release.wait();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "producer_loop hung instead of exiting on Shutdown"
            );
            thread::sleep(Duration::from_millis(5));
        }
        handle.join().unwrap();
    }

    #[test]
    fn consecutive_flush_nexts_and_single_file_double_next_skip_multiple_tracks_without_jumping_back()
    {
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let root = PathBuf::from(r"C:\Music\Album");
        let t1 = root.join("01.flac");
        let init = PlayDirective::Sequential {
            root: root.clone(),
            seq_gen: 1,
            start_after: None,
            folder_only: false,
            skip_count: 0,
        };
        let ctrl = ProducerControl::new(ep0, ProducerDirective::Active(init.clone()));
        let shared = SharedControl::new(root.clone(), "Test DAC");

        let mut state = EngineState::new(ep0, init, &buf, &ctrl, &shared);
        state.record_anchor(t1.clone());

        // Scenario (a): Mid-track 01, buffer empty (no prebuffered TrackStart). Double-click Next!
        state.dispatch(ControlCommand::Next);
        match &state.last_play {
            PlayDirective::Sequential {
                start_after,
                skip_count,
                ..
            } => {
                assert_eq!(start_after.as_ref(), Some(&t1));
                assert_eq!(*skip_count, 0, "first flush-Next plays N+1 (0 extra skips)");
            }
            other => panic!("expected Sequential, got {other:?}"),
        }
        state.dispatch(ControlCommand::Next);
        match &state.last_play {
            PlayDirective::Sequential {
                start_after,
                skip_count,
                ..
            } => {
                assert_eq!(start_after.as_ref(), Some(&t1));
                assert_eq!(*skip_count, 1, "second flush-Next must skip N+1 and play N+2");
            }
            other => panic!("expected Sequential, got {other:?}"),
        }

        // Scenario (b): PlayFile 05.flac -> rapid double Next while 05 is still prebuffering.
        // Must anchor at 05.flac and skip forward (NEVER reset start_after to None -> 01.flac!).
        let f05 = root.join("05.flac");
        state.start_directive(PlayDirective::SingleFile { file: f05.clone() }, true);
        state.dispatch(ControlCommand::Next);
        match &state.last_play {
            PlayDirective::Sequential {
                start_after,
                folder_only,
                skip_count,
                ..
            } => {
                assert_eq!(start_after.as_ref(), Some(&f05));
                assert!(*folder_only);
                assert_eq!(*skip_count, 0);
            }
            other => panic!("expected folder-only Sequential after 05, got {other:?}"),
        }
        state.dispatch(ControlCommand::Next);
        match &state.last_play {
            PlayDirective::Sequential {
                start_after,
                folder_only,
                skip_count,
                ..
            } => {
                assert_eq!(
                    start_after.as_ref(),
                    Some(&f05),
                    "second Next after PlayFile must stay anchored at 05.flac, not None"
                );
                assert!(*folder_only);
                assert_eq!(*skip_count, 1, "second Next after PlayFile must advance to 07.flac");
            }
            other => panic!("expected folder-only Sequential skipping 1, got {other:?}"),
        }
    }

    #[test]
    fn pause_then_next_after_error_threshold_resumes_past_bad_run() {
        use flac_codec::encode::{FlacSampleWriter, Options};
        use std::fs;

        let dir = std::env::temp_dir().join(format!(
            "ap_cli_bad_run_recovery_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let write_good_flac = |path: &Path| {
            let mut w =
                FlacSampleWriter::create(path, Options::default(), 44_100, 16, 2, None).unwrap();
            let samples = vec![1000_i32; 400 * 2];
            w.write(&samples).unwrap();
            w.finalize().unwrap();
        };

        // Folder: 01.flac (good), 02..11.flac (10 corrupt files), 12.flac (good).
        let f01 = dir.join("01.flac");
        let f11 = dir.join("11.flac");
        let f12 = dir.join("12.flac");
        write_good_flac(&f01);
        for i in 2..=11 {
            fs::write(dir.join(format!("{i:02}.flac")), b"not a valid flac").unwrap();
        }
        write_good_flac(&f12);

        let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
        let shared = SharedControl::new(dir.clone(), "Mock DAC");
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.reset_and_flush();
        let init_dir = PlayDirective::Sequential {
            root: dir.clone(),
            seq_gen: 1,
            start_after: None,
            folder_only: false,
            skip_count: 0,
        };
        let ctrl = ProducerControl::new(ep, ProducerDirective::Active(init_dir.clone()));

        let p_ctrl = Arc::clone(&ctrl);
        let p_buf = Arc::clone(&buf);
        let p_shared = Arc::clone(&shared);
        let handle = thread::spawn(move || producer_loop(pipeline, p_ctrl, p_buf, p_shared));

        let mut state = EngineState::new(ep, init_dir, &buf, &ctrl, &shared);

        // 1. Drain 01.flac (TrackStart + PCM + TrackEnd) and then observe ErrorThresholdPause.
        let mut saw_01 = false;
        let mut saw_threshold_pause = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match buf.pop_for_epoch_timeout(state.active_epoch, Duration::from_millis(50)) {
                Some(StreamEvent::TrackStart { path, .. }) => {
                    assert_eq!(path, f01);
                    state.record_anchor(path);
                    saw_01 = true;
                    // Drain 01.flac PCM until TrackEnd.
                    loop {
                        match buf.try_pop_track_pcm_block(state.active_epoch) {
                            PopBlockResult::Block(_) => {}
                            PopBlockResult::Underrun => thread::sleep(Duration::from_millis(2)),
                            PopBlockResult::TrackEnd | PopBlockResult::StaleOrClosed => break,
                        }
                    }
                }
                Some(StreamEvent::ErrorThresholdPause {
                    message,
                    last_attempted,
                }) => {
                    assert_eq!(
                        last_attempted, f11,
                        "error-threshold pause must report the last bad file (11.flac)"
                    );
                    state.record_anchor(last_attempted);
                    state.pause_with_message(message);
                    saw_threshold_pause = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_01, "expected 01.flac to stream");
        assert!(saw_threshold_pause, "expected ErrorThresholdPause after 10 corrupt files");
        assert!(state.is_paused);

        // 2. User clicks Next: must resume AFTER 11.flac and immediately produce 12.flac!
        state.dispatch(ControlCommand::Next);
        let mut saw_12 = false;
        let deadline2 = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline2 {
            match buf.pop_for_epoch_timeout(state.active_epoch, Duration::from_millis(50)) {
                Some(StreamEvent::TrackStart { path, .. }) => {
                    assert_eq!(path, f12, "Next after error pause must advance to 12.flac!");
                    saw_12 = true;
                    break;
                }
                Some(StreamEvent::ErrorThresholdPause { .. }) => {
                    panic!("Next replayed the corrupt run instead of advancing to 12.flac!");
                }
                _ => {}
            }
        }

        buf.close();
        ctrl.set(state.active_epoch.wrapping_add(1), ProducerDirective::Shutdown);
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(saw_12, "expected 12.flac to play after Next");
    }

    #[test]
    fn bounded_producer_join_detaches_stuck_producer_and_honors_console_close_escalation() {
        // 1. Promptly exiting producer joins immediately.
        let ctrl_ok = ShutdownController::new();
        let h_ok = thread::spawn(|| {});
        assert!(join_producer_bounded(
            h_ok,
            &ctrl_ok,
            Duration::from_millis(500),
            Duration::from_millis(100),
        ));

        // 2. Stuck producer on Ctrl+C detaches cleanly at normal_timeout instead of hanging forever.
        let ctrl_stuck = ShutdownController::new();
        ctrl_stuck.trigger(ShutdownSignal::CtrlC);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let h_stuck = thread::spawn(move || {
            let _ = rx.recv_timeout(Duration::from_secs(5));
        });
        let t0 = Instant::now();
        let joined = join_producer_bounded(
            h_stuck,
            &ctrl_stuck,
            Duration::from_millis(120),
            Duration::from_millis(40),
        );
        let elapsed = t0.elapsed();
        let _ = tx.send(());
        assert!(!joined, "stuck producer should detach on bounded timeout");
        assert!(elapsed >= Duration::from_millis(100) && elapsed < Duration::from_millis(600));

        // 3. Ctrl+C followed by ConsoleClose mid-wait switches dynamically to fast_timeout!
        let ctrl_esc = Arc::new(ShutdownController::new());
        ctrl_esc.trigger(ShutdownSignal::CtrlC);
        assert!(!ctrl_esc.fast_teardown_requested());
        let (tx2, rx2) = std::sync::mpsc::channel::<()>();
        let h_stuck2 = thread::spawn(move || {
            let _ = rx2.recv_timeout(Duration::from_secs(5));
        });
        let esc_bg = Arc::clone(&ctrl_esc);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            esc_bg.trigger(ShutdownSignal::ConsoleClose);
        });
        let t1 = Instant::now();
        let joined2 = join_producer_bounded(
            h_stuck2,
            &ctrl_esc,
            Duration::from_secs(5),
            Duration::from_millis(80),
        );
        let elapsed2 = t1.elapsed();
        let _ = tx2.send(());
        assert!(!joined2);
        assert!(ctrl_esc.fast_teardown_requested());
        assert!(
            elapsed2 < Duration::from_millis(500),
            "ConsoleClose escalation should cut 5 s normal budget down to fast timeout, took {elapsed2:?}"
        );
    }

    #[test]
    fn single_file_next_builds_folder_only_sequential_cursor_after_file() {
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let file = PathBuf::from(r"C:\Music\Album\02.flac");
        let init = PlayDirective::SingleFile { file: file.clone() };
        let ctrl = ProducerControl::new(ep0, ProducerDirective::Active(init.clone()));
        let shared = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");

        let mut state = EngineState::new(ep0, init, &buf, &ctrl, &shared);
        state.dispatch(ControlCommand::Next);

        match &state.last_play {
            PlayDirective::Sequential {
                root,
                start_after,
                folder_only,
                ..
            } => {
                assert_eq!(root, &PathBuf::from(r"C:\Music\Album"));
                assert_eq!(start_after.as_ref(), Some(&file));
                assert!(*folder_only);
            }
            other => panic!("expected folder-only Sequential, got {other:?}"),
        }
    }

    #[test]
    fn sequential_pause_then_next_resumes_after_last_started_track_not_decode_ahead() {
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let root = PathBuf::from(r"C:\Music\Album");
        let init = PlayDirective::Sequential {
            root: root.clone(),
            seq_gen: 1,
            start_after: None,
            folder_only: false,
            skip_count: 0,
        };
        let ctrl = ProducerControl::new(ep0, ProducerDirective::Active(init.clone()));
        let shared = SharedControl::new(root.clone(), "Test DAC");

        let mut state = EngineState::new(ep0, init, &buf, &ctrl, &shared);
        // Consumer actually started T1 (even if producer decoded ahead to T2).
        let t1 = root.join("01.flac");
        state.last_started_track = Some(t1.clone());

        state.dispatch(ControlCommand::Pause);
        assert!(state.is_paused);

        state.dispatch(ControlCommand::Next);
        assert!(!state.is_paused);
        match &state.last_play {
            PlayDirective::Sequential {
                root: r,
                seq_gen,
                start_after,
                folder_only,
                skip_count,
            } => {
                assert_eq!(r, &root);
                assert_ne!(*seq_gen, 1, "must allocate fresh seq_gen to rebuild cursor");
                assert_eq!(start_after.as_ref(), Some(&t1));
                assert!(!*folder_only);
                assert_eq!(*skip_count, 0);
            }
            other => panic!("expected Sequential resume, got {other:?}"),
        }
    }

    #[test]
    fn engine_rejects_device_unc_and_outside_root_play_file_before_opening() {
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let root = PathBuf::from(r"C:\Music");
        let init = PlayDirective::Random { root: root.clone() };
        let ctrl = ProducerControl::new(ep0, ProducerDirective::Active(init.clone()));
        let shared = SharedControl::new(root.clone(), "Test DAC");
        let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();

        let mut state = EngineState::new(ep0, init, &buf, &ctrl, &shared);

        for bad in [
            PathBuf::from("CONIN$"),
            PathBuf::from(r"\\.\pipe\ap_nonexistent_pipe"),
            PathBuf::from(r"\\attacker.example\share\track.flac"),
            PathBuf::from(r"C:\Music\CON.flac"),
            PathBuf::from(r"C:\Windows\System32\drivers\etc\hosts"),
        ] {
            state.dispatch(ControlCommand::PlayFile(bad.clone()));
            // Must NOT switch last_play to SingleFile or bump epoch.
            assert!(matches!(state.last_play, PlayDirective::Random { .. }));
            assert_eq!(state.active_epoch, ep0);

            // Direct producer-side guard must also reject without blocking on open().
            let mut no_tail = None;
            let mut no_pending = false;
            let (outcome, next_tail, pending_end) = stream_single_track(
                &pipeline,
                &ctrl,
                &buf,
                &shared,
                ep0,
                &bad,
                PlayModeLabel::SingleFile,
                &mut no_tail,
                &mut no_pending,
                false,
            );
            assert!(next_tail.is_none());
            assert!(!pending_end);
            assert!(
                matches!(outcome, TrackStreamOutcome::Failed(ref m) if m.contains("Rejected")),
                "expected rejection for {}, got {outcome:?}",
                bad.display()
            );
        }
    }

    #[test]
    fn producer_loop_drains_rew_tail_across_rate_change_and_at_end_of_queue() {
        use flac_codec::encode::{FlacSampleWriter, Options};
        use std::fs::{self, File};
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!(
            "ap_cli_engine_tail_drain_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let ir_dir = root.join("ir");
        let music_dir = root.join("music");
        fs::create_dir_all(&ir_dir).unwrap();
        fs::create_dir_all(&music_dir).unwrap();

        // Write 501-tap stereo IRs for 352.8 kHz and 384 kHz (flush_tail_samples = 500).
        let ir_taps = 501_usize;
        let write_ir = |path: &Path, rate: u32| {
            let mut ir = vec![0.0_f32; ir_taps * 2];
            ir[0] = 0.5;
            ir[1] = 0.5;
            ir[500 * 2] = 0.25;
            ir[500 * 2 + 1] = -0.25;
            let data_bytes = (ir.len() * 4) as u32;
            let mut f = File::create(path).unwrap();
            f.write_all(b"RIFF").unwrap();
            f.write_all(&(36u32 + data_bytes).to_le_bytes()).unwrap();
            f.write_all(b"WAVE").unwrap();
            f.write_all(b"fmt ").unwrap();
            f.write_all(&16u32.to_le_bytes()).unwrap();
            f.write_all(&3u16.to_le_bytes()).unwrap();
            f.write_all(&2u16.to_le_bytes()).unwrap();
            f.write_all(&rate.to_le_bytes()).unwrap();
            f.write_all(&(rate * 8).to_le_bytes()).unwrap();
            f.write_all(&8u16.to_le_bytes()).unwrap();
            f.write_all(&32u16.to_le_bytes()).unwrap();
            f.write_all(b"data").unwrap();
            f.write_all(&data_bytes.to_le_bytes()).unwrap();
            for s in ir {
                f.write_all(&s.to_le_bytes()).unwrap();
            }
        };
        write_ir(&ir_dir.join("ir_352800.wav"), 352_800);
        write_ir(&ir_dir.join("ir_384000.wav"), 384_000);

        // Write Track 1 (44.1 kHz, 400 frames) and Track 2 (48 kHz, 400 frames).
        let n_frames = 400_usize;
        let write_flac = |path: &Path, rate: u32| {
            let mut w =
                FlacSampleWriter::create(path, Options::default(), rate, 16, 2, None).unwrap();
            let samples: Vec<i32> = (0..(n_frames * 2))
                .map(|i| if i % 2 == 0 { 10_000 } else { -10_000 })
                .collect();
            w.write(&samples).unwrap();
            w.finalize().unwrap();
        };
        write_flac(&music_dir.join("01_44k.flac"), 44_100);
        write_flac(&music_dir.join("02_48k.flac"), 48_000);

        let pipeline = AudioPipeline::with_rew_config(
            UpsamplerPolicy::default(),
            ConvolutionConfig::auto_dir(ir_dir),
        )
        .unwrap();
        let shared = SharedControl::new(music_dir.clone(), "Mock DAC");
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.reset_and_flush();
        let ctrl = ProducerControl::new(
            ep,
            ProducerDirective::Active(PlayDirective::Sequential {
                root: music_dir.clone(),
                seq_gen: 1,
                start_after: None,
                folder_only: false,
                skip_count: 0,
            }),
        );

        let p_ctrl = Arc::clone(&ctrl);
        let p_buf = Arc::clone(&buf);
        let p_shared = Arc::clone(&shared);
        let handle = thread::spawn(move || producer_loop(pipeline, p_ctrl, p_buf, p_shared));

        let mut track_counts: Vec<(u32, usize)> = Vec::new();
        let mut saw_end_of_queue = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match buf.pop_for_epoch_timeout(ep, Duration::from_millis(50)) {
                Some(StreamEvent::TrackStart { report, .. }) => {
                    let rate = report.output_rate_hz;
                    let mut frames = 0_usize;
                    loop {
                        match buf.try_pop_track_pcm_block(ep) {
                            PopBlockResult::Block(b) => {
                                assert_eq!(b.sample_rate_hz, rate);
                                frames += b.frames.len();
                            }
                            PopBlockResult::Underrun => thread::sleep(Duration::from_millis(2)),
                            PopBlockResult::TrackEnd | PopBlockResult::StaleOrClosed => break,
                        }
                    }
                    track_counts.push((rate, frames));
                }
                Some(StreamEvent::EndOfQueue(_)) => {
                    saw_end_of_queue = true;
                    break;
                }
                Some(StreamEvent::FatalError(err)) => panic!("unexpected FatalError: {err}"),
                _ => {}
            }
        }

        buf.close();
        ctrl.set(ep + 1, ProducerDirective::Shutdown);
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&root);

        assert!(saw_end_of_queue, "expected EndOfQueue after 2 sequential tracks");
        assert_eq!(
            track_counts,
            vec![
                (352_800, n_frames * 8 + (ir_taps - 1)),
                (384_000, n_frames * 8 + (ir_taps - 1)),
            ],
            "both rate-change track and final end-of-queue track must preserve full N*8 + flush_tail"
        );
    }

    #[test]
    fn producer_loop_drains_rew_tail_when_same_rate_next_track_is_corrupt_or_zero_frame() {
        // Review #3 Findings #3 & #17:
        // Valid 44.1k Track 1 followed by (a) a same-rate 44.1k FLAC with valid STREAMINFO whose
        // first audio frame is corrupt, then (b) a same-rate 44.1k 0-frame FLAC, then (c) valid
        // Track 4. Track 1 MUST NOT lose its REW convolution tail when Track 2 fails on block 1!
        use flac_codec::encode::{FlacSampleWriter, Options};
        use std::fs::{self, File};
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!(
            "ap_cli_engine_corrupt_handoff_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let ir_dir = root.join("ir");
        let music_dir = root.join("music");
        fs::create_dir_all(&ir_dir).unwrap();
        fs::create_dir_all(&music_dir).unwrap();

        let ir_taps = 501_usize;
        {
            let mut ir = vec![0.0_f32; ir_taps * 2];
            ir[0] = 0.5;
            ir[1] = 0.5;
            ir[500 * 2] = 0.25;
            ir[500 * 2 + 1] = -0.25;
            let data_bytes = (ir.len() * 4) as u32;
            let mut f = File::create(ir_dir.join("ir_352800.wav")).unwrap();
            f.write_all(b"RIFF").unwrap();
            f.write_all(&(36u32 + data_bytes).to_le_bytes()).unwrap();
            f.write_all(b"WAVE").unwrap();
            f.write_all(b"fmt ").unwrap();
            f.write_all(&16u32.to_le_bytes()).unwrap();
            f.write_all(&3u16.to_le_bytes()).unwrap();
            f.write_all(&2u16.to_le_bytes()).unwrap();
            f.write_all(&352_800u32.to_le_bytes()).unwrap();
            f.write_all(&(352_800u32 * 8).to_le_bytes()).unwrap();
            f.write_all(&8u16.to_le_bytes()).unwrap();
            f.write_all(&32u16.to_le_bytes()).unwrap();
            f.write_all(b"data").unwrap();
            f.write_all(&data_bytes.to_le_bytes()).unwrap();
            for s in ir {
                f.write_all(&s.to_le_bytes()).unwrap();
            }
        }

        let n_frames = 400_usize;
        let write_flac = |path: &Path, frames: usize| {
            let mut w =
                FlacSampleWriter::create(path, Options::default(), 44_100, 16, 2, None).unwrap();
            if frames > 0 {
                let samples: Vec<i32> = (0..(frames * 2))
                    .map(|i| if i % 2 == 0 { 12_000 } else { -12_000 })
                    .collect();
                w.write(&samples).unwrap();
            }
            w.finalize().unwrap();
        };

        let f01 = music_dir.join("01_good.flac");
        let f02 = music_dir.join("02_corrupt_first_block.flac");
        let f03 = music_dir.join("03_zero_frame.flac");
        let f04 = music_dir.join("04_good.flac");
        write_flac(&f01, n_frames);

        // Build 02 with a valid 44.1k STREAMINFO header (so FlacBlockReader::open succeeds!)
        // followed by corrupt audio frame bytes (so the very first read_next_block returns Err!).
        write_flac(&f02, 64);
        {
            let mut bytes = fs::read(&f02).unwrap();
            // Locate end of metadata blocks (after 4-byte "fLaC" + metadata blocks).
            let mut pos = 4_usize;
            loop {
                let header = bytes[pos];
                let is_last = (header & 0x80) != 0;
                let len = ((bytes[pos + 1] as usize) << 16)
                    | ((bytes[pos + 2] as usize) << 8)
                    | (bytes[pos + 3] as usize);
                pos += 4 + len;
                if is_last || pos >= bytes.len() {
                    break;
                }
            }
            assert!(pos < bytes.len(), "expected audio frame payload after metadata");
            for b in &mut bytes[pos..] {
                *b = 0xAA; // invalid FLAC frame sync -> immediate first-block decode error
            }
            fs::write(&f02, &bytes).unwrap();
        }
        // Verify f02 opens cleanly (proving the test actually hits post-open first-block error!).
        assert!(
            FlacBlockReader::open(&f02).is_ok(),
            "02_corrupt_first_block.flac must have valid STREAMINFO so open() succeeds"
        );

        // Build 03 as a valid 0-frame 44.1 kHz FLAC (valid STREAMINFO with total_samples = 0 and no audio frames).
        write_flac(&f03, 64);
        {
            let mut bytes = fs::read(&f03).unwrap();
            let mut pos = 4_usize;
            loop {
                let header = bytes[pos];
                let is_last = (header & 0x80) != 0;
                let len = ((bytes[pos + 1] as usize) << 16)
                    | ((bytes[pos + 2] as usize) << 8)
                    | (bytes[pos + 3] as usize);
                pos += 4 + len;
                if is_last || pos >= bytes.len() {
                    break;
                }
            }
            bytes.truncate(pos);
            bytes[21] &= 0xF0;
            bytes[22] = 0;
            bytes[23] = 0;
            bytes[24] = 0;
            bytes[25] = 0;
            fs::write(&f03, &bytes).unwrap();
        }
        assert!(
            FlacBlockReader::open(&f03).is_ok(),
            "03_zero_frame.flac must open cleanly as a 0-frame FLAC"
        );

        write_flac(&f04, n_frames);

        let pipeline = AudioPipeline::with_rew_config(
            UpsamplerPolicy::default(),
            ConvolutionConfig::auto_dir(ir_dir),
        )
        .unwrap();
        let shared = SharedControl::new(music_dir.clone(), "Mock DAC");
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.reset_and_flush();
        let ctrl = ProducerControl::new(
            ep,
            ProducerDirective::Active(PlayDirective::Sequential {
                root: music_dir.clone(),
                seq_gen: 1,
                start_after: None,
                folder_only: false,
                skip_count: 0,
            }),
        );

        let p_ctrl = Arc::clone(&ctrl);
        let p_buf = Arc::clone(&buf);
        let p_shared = Arc::clone(&shared);
        let handle = thread::spawn(move || producer_loop(pipeline, p_ctrl, p_buf, p_shared));

        let mut track_counts: Vec<(PathBuf, usize, bool)> = Vec::new();
        let mut saw_end_of_queue = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match buf.pop_for_epoch_timeout(ep, Duration::from_millis(50)) {
                Some(StreamEvent::TrackStart {
                    path,
                    had_gapless_carryover,
                    ..
                }) => {
                    let mut frames = 0_usize;
                    loop {
                        match buf.try_pop_track_pcm_block(ep) {
                            PopBlockResult::Block(b) => frames += b.frames.len(),
                            PopBlockResult::Underrun => thread::sleep(Duration::from_millis(2)),
                            PopBlockResult::TrackEnd | PopBlockResult::StaleOrClosed => break,
                        }
                    }
                    track_counts.push((path, frames, had_gapless_carryover));
                }
                Some(StreamEvent::EndOfQueue(_)) => {
                    saw_end_of_queue = true;
                    break;
                }
                Some(StreamEvent::FatalError(err)) => panic!("unexpected FatalError: {err}"),
                _ => {}
            }
        }

        buf.close();
        ctrl.set(ep + 1, ProducerDirective::Shutdown);
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&root);

        assert!(saw_end_of_queue);
        assert_eq!(
            track_counts,
            vec![
                (f01, n_frames * 8 + (ir_taps - 1), false),
                (f04, n_frames * 8 + (ir_taps - 1), false),
            ],
            "Track 1 MUST keep its full N*8 + REW flush_tail when same-rate Track 2 fails on block 1"
        );
    }

    #[test]
    fn next_before_first_track_starts_skips_track_one_in_play_sequential() {
        // Review #3 Finding #20: clicking Next right after PlaySequential before Track 1 starts
        // must skip Track 1 in both Case A (TrackStart(01) already queued) and Case B (still opening).
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let root = PathBuf::from(r"C:\Music\Album");
        let f01 = root.join("01.flac");
        let f02 = root.join("02.flac");
        let ep0 = buf.current_epoch();
        let init = PlayDirective::Random { root: root.clone() };
        let ctrl = ProducerControl::new(ep0, ProducerDirective::Active(init.clone()));
        let shared = SharedControl::new(root.clone(), "Test DAC");
        let mut state = EngineState::new(ep0, init, &buf, &ctrl, &shared);

        // Case B: PlaySequential(root), buffer empty (producer still opening 01.flac), user clicks Next.
        state.dispatch(ControlCommand::PlaySequential(root.clone()));
        state.dispatch(ControlCommand::Next);
        match &state.last_play {
            PlayDirective::Sequential {
                start_after,
                skip_count,
                ..
            } => {
                assert_eq!(*start_after, None);
                assert_eq!(
                    *skip_count, 1,
                    "Next before Track 1 starts must restart with skip_count=1 to skip Track 1"
                );
            }
            other => panic!("expected Sequential, got {other:?}"),
        }

        // Case A: PlaySequential(root), TrackStart(01) + TrackStart(02) buffered before Track 1 starts,
        // user clicks Next -> must pop TrackStart(01) and land on TrackStart(02)!
        state.dispatch(ControlCommand::PlaySequential(root.clone()));
        let ep = state.active_epoch;
        let dummy_report = AudioPipeline::new(UpsamplerPolicy::default())
            .unwrap()
            .start_track(
                ap_core::format::StreamSpec {
                    sample_rate_hz: 44_100,
                    bits_per_sample: 16,
                    channels: 2,
                },
                &ap_core::headroom::TrackPeakMetadata::default(),
                false,
            )
            .unwrap()
            .report()
            .clone();
        assert!(buf.push_for_epoch(
            ep,
            StreamEvent::TrackStart {
                path: f01.clone(),
                mode_label: PlayModeLabel::Sequential,
                report: dummy_report.clone(),
                total_duration_s: Some(1.0),
                had_gapless_carryover: false,
            }
        ));
        assert!(buf.push_for_epoch(ep, StreamEvent::TrackEnd));
        assert!(buf.push_for_epoch(
            ep,
            StreamEvent::TrackStart {
                path: f02.clone(),
                mode_label: PlayModeLabel::Sequential,
                report: dummy_report,
                total_duration_s: Some(1.0),
                had_gapless_carryover: false,
            }
        ));

        state.dispatch(ControlCommand::Next);
        match buf.pop_for_epoch_timeout(state.active_epoch, Duration::from_millis(20)) {
            Some(StreamEvent::TrackStart { path, .. }) => {
                assert_eq!(path, f02, "Next before Track 1 started must advance directly to 02.flac");
            }
            other => panic!("expected TrackStart(02.flac), got {other:?}"),
        }
    }

    #[test]
    fn next_during_prebuffer_after_natural_end_does_not_double_skip_and_mid_track_skip_drops_baked_carryover() {
        // Review #3 Findings #21 & #27.
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let root = PathBuf::from(r"C:\Music\Album");
        let f01 = root.join("01.flac");
        let f02 = root.join("02.flac");
        let ep0 = buf.current_epoch();
        let init = PlayDirective::Sequential {
            root: root.clone(),
            seq_gen: 1,
            start_after: None,
            folder_only: false,
            skip_count: 0,
        };
        let ctrl = ProducerControl::new(ep0, ProducerDirective::Active(init.clone()));
        let shared = SharedControl::new(root.clone(), "Test DAC");
        let mut state = EngineState::new(ep0, init, &buf, &ctrl, &shared);

        let dummy_report = AudioPipeline::new(UpsamplerPolicy::default())
            .unwrap()
            .start_track(
                ap_core::format::StreamSpec {
                    sample_rate_hz: 44_100,
                    bits_per_sample: 16,
                    channels: 2,
                },
                &ap_core::headroom::TrackPeakMetadata::default(),
                false,
            )
            .unwrap()
            .report()
            .clone();

        // 1. Track 01 played and ended naturally -> state has anchor=01, just_skipped_track=false.
        state.record_anchor(f01.clone());
        state.just_skipped_track = false;

        // TrackStart(02) was popped and entered prebuffer; user clicks Next (meaning skip 01).
        // Re-queuing TrackStart(02) via push_front_for_epoch + dispatch(Next) must keep 02 (NOT skip to 03!).
        assert!(buf.push_front_for_epoch(
            state.active_epoch,
            StreamEvent::TrackStart {
                path: f02.clone(),
                mode_label: PlayModeLabel::Sequential,
                report: dummy_report.clone(),
                total_duration_s: Some(1.0),
                had_gapless_carryover: true,
            }
        ));
        state.dispatch(ControlCommand::Next);
        match buf.pop_for_epoch_timeout(state.active_epoch, Duration::from_millis(20)) {
            Some(StreamEvent::TrackStart { path, .. }) => {
                assert_eq!(path, f02, "Next after natural EOF during prebuffer must NOT double-skip 02.flac");
            }
            other => panic!("expected retained TrackStart(02.flac), got {other:?}"),
        }

        // 2. Mid-track skip during Track 01 when Track 02 is already buffered WITH baked gapless
        // carryover from Track 01's ending (Review #3 Finding #21): must FLUSH instead of reusing
        // so Track 02 restarts cleanly without Track 01's skipped ending decay!
        state.record_anchor(f01);
        state.just_skipped_track = false;
        let ep_before = state.active_epoch;
        assert!(buf.push_for_epoch(
            ep_before,
            StreamEvent::PcmBlock(ap_core::buffer::InterleavedInt32Buffer {
                sample_rate_hz: 352_800,
                frames: vec![[1, -1]; 128],
            })
        ));
        assert!(buf.push_for_epoch(ep_before, StreamEvent::TrackEnd));
        assert!(buf.push_for_epoch(
            ep_before,
            StreamEvent::TrackStart {
                path: f02,
                mode_label: PlayModeLabel::Sequential,
                report: dummy_report,
                total_duration_s: Some(1.0),
                had_gapless_carryover: true,
            }
        ));
        state.dispatch(ControlCommand::Next);
        assert_ne!(
            state.active_epoch, ep_before,
            "mid-track Next must flush gapless-carryover-tainted next track instead of reusing it"
        );
        match &state.last_play {
            PlayDirective::Sequential {
                start_after,
                skip_count,
                ..
            } => {
                assert_eq!(start_after.as_ref(), Some(&root.join("01.flac")));
                assert_eq!(*skip_count, 0, "fresh restart plays 02.flac cleanly without 01's tail");
            }
            other => panic!("expected Sequential restart after 01.flac, got {other:?}"),
        }
    }

    #[test]
    fn teardown_player_resources_releases_http_and_sink_before_producer_join() {
        // Follow-up Engine #2: verify exact non-idle teardown order without needing live DAC hardware.
        let dir = std::env::temp_dir().join(format!(
            "ap_cli_teardown_order_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);

        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        struct MockSink {
            log: Arc<Mutex<Vec<&'static str>>>,
        }
        impl Drop for MockSink {
            fn drop(&mut self) {
                self.log.lock().unwrap().push("sink_dropped");
            }
        }

        let shared = SharedControl::new(dir.clone(), "Mock DAC");
        let server = ControlServer::start(shared.clone(), None).unwrap();
        let url = server.url();
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.current_epoch();
        let prod_ctrl = ProducerControl::new(
            ep,
            ProducerDirective::Active(PlayDirective::Random { root: dir.clone() }),
        );
        let controller = ShutdownController::new();
        controller.trigger(ShutdownSignal::CtrlC);

        let log_prod = Arc::clone(&log);
        let prod_ctrl_bg = Arc::clone(&prod_ctrl);
        let producer = thread::spawn(move || {
            // Wait for Shutdown signal, pause briefly so sink drop clearly precedes producer exit.
            while prod_ctrl_bg.wait_active().is_some() {}
            thread::sleep(Duration::from_millis(30));
            log_prod.lock().unwrap().push("producer_exited");
        });

        let joined = teardown_player_resources(
            &buf,
            &prod_ctrl,
            ep,
            server,
            &url,
            MockSink {
                log: Arc::clone(&log),
            },
            producer,
            &controller,
            Duration::from_secs(2),
            Duration::from_secs(1),
        );

        let _ = std::fs::remove_dir_all(&dir);
        assert!(joined, "producer thread should join cleanly on teardown");
        assert_eq!(
            *log.lock().unwrap(),
            vec!["sink_dropped", "producer_exited"],
            "sink must be dropped BEFORE waiting on producer join"
        );
    }
}
