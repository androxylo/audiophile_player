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
use ap_control::server::ControlServer;
use ap_control::state::{ControlCommand, PlayModeLabel, SharedControl};
use ap_core::buffer::DEFAULT_SOURCE_BLOCK_FRAMES;
use ap_core::config::{ConvolutionConfig, PlayerConfig};
use ap_core::format::DacWireFormat;
use ap_dsp::convolution::ConvolutionTail;
use ap_dsp::pipeline::AudioPipeline;
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
}

impl ProducerControl {
    fn new(initial_epoch: u64, initial: ProducerDirective) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((initial_epoch, initial)),
            cv: Condvar::new(),
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
    /// Path from the most recent `TrackStart` actually started by the consumer.
    /// Used as `start_after` when resuming/skipping `Sequential` mode so prefetched-but-unplayed
    /// tracks are never silently skipped.
    last_started_track: Option<PathBuf>,
    /// True after a `Next` skip until the next track actually starts playing, so rapid chained
    /// `Next` clicks advance past a pre-buffered front `TrackStart` without double-skipping on
    /// a late `Next` right after a natural track end.
    just_skipped_track: bool,
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
        Self {
            is_paused: false,
            active_epoch: initial_epoch,
            last_play: initial_dir,
            last_started_track: None,
            just_skipped_track: false,
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

    fn start_directive(&mut self, dir: PlayDirective, reset_started_track: bool) {
        let ep = self.buf.reset_and_flush();
        if reset_started_track {
            self.last_started_track = None;
        }
        self.last_play = dir.clone();
        self.active_epoch = ep;
        self.is_paused = false;
        self.just_skipped_track = false;
        self.prod_ctrl.set(ep, ProducerDirective::Active(dir));
    }

    fn pause_with_message(&mut self, msg: impl Into<String>) {
        let ep = self.buf.reset_and_flush();
        self.active_epoch = ep;
        self.is_paused = true;
        self.just_skipped_track = false;
        self.prod_ctrl.set(ep, ProducerDirective::Paused);
        self.shared.set_paused(msg.into());
    }

    /// Rebuild a `Sequential` directive positioned strictly after `last_started_track` (if any),
    /// using a fresh `seq_gen` so the producer's decode-ahead cursor can never skip an unplayed track.
    fn sequential_after_last_started(&mut self, root: PathBuf, folder_only: bool) -> PlayDirective {
        let seq_gen = self.alloc_seq_gen();
        PlayDirective::Sequential {
            root,
            seq_gen,
            start_after: self.last_started_track.clone(),
            folder_only,
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

                // From SingleFile, advance sequentially to the next file in that folder only.
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
                        start_after: Some(file),
                        folder_only: true,
                    };
                    self.start_directive(dir, false);
                    return;
                }

                if !self.is_paused {
                    let (reused_prebuffered, ep, skipped_front) = self
                        .buf
                        .skip_to_next_track_or_flush(self.active_epoch, self.just_skipped_track);
                    if let Some(skipped_path) = skipped_front {
                        self.last_started_track = Some(skipped_path);
                    }
                    self.active_epoch = ep;
                    self.just_skipped_track = true;
                    if reused_prebuffered {
                        return;
                    }
                    // Non-reuse flush: rebuild Sequential cursor strictly after the last track
                    // actually started by the consumer so decode-ahead never drops N+1.
                    let restart_dir = match self.last_play.clone() {
                        PlayDirective::Sequential {
                            root, folder_only, ..
                        } => self.sequential_after_last_started(root, folder_only),
                        other => other,
                    };
                    self.last_play = restart_dir.clone();
                    self.prod_ctrl.set(ep, ProducerDirective::Active(restart_dir));
                    return;
                }

                // Paused -> Next: resume the next track in the last active mode/folder, anchored
                // at `last_started_track` for Sequential so prefetched tracks discarded on Pause
                // are played instead of skipped.
                let resume_dir = match self.last_play.clone() {
                    PlayDirective::Sequential {
                        root, folder_only, ..
                    } => self.sequential_after_last_started(root, folder_only),
                    other => other,
                };
                self.start_directive(resume_dir, false);
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

    let mut http_server = ControlServer::start(shared.clone(), http_port)?;

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
                // while remaining immediately responsive to Pause / Next / Ctrl+C.
                let mut aborted_prebuffer = false;
                loop {
                    if controller.current_signal().is_some() {
                        aborted_prebuffer = true;
                        break;
                    }
                    if let Some(cmd) = shared.take_pending_command() {
                        state.dispatch(cmd);
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
                    state.dispatch(cmd);
                    continue;
                }

                // Record the track actually started by the consumer (not producer decode-ahead).
                state.last_started_track = Some(path.clone());
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

    // Teardown: on CTRL_CLOSE/LOGOFF/SHUTDOWN (where Windows only grants ~3 s before forced
    // termination), release HTTP and the audio sink FIRST, then give the producer a bounded
    // join window so cleanup banners and `mark_cleanup_complete` always finish in time.
    stream_buf.close();
    prod_ctrl.set(
        state.active_epoch.wrapping_add(1),
        ProducerDirective::Shutdown,
    );
    http_server.shutdown();
    drop(sink);

    let fast_teardown = controller
        .current_signal()
        .is_some_and(|s| s.requires_handler_wait());
    if fast_teardown {
        let deadline = Instant::now() + Duration::from_millis(1500);
        while !producer_thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(15));
        }
        if producer_thread.is_finished() {
            let _ = producer_thread.join();
        }
    } else {
        let _ = producer_thread.join();
    }

    if let Some(sig) = controller.current_signal() {
        print_shutdown_banner(sig);
    } else {
        println!("audiophile-player stopped cleanly.");
        let _ = io::stdout().flush();
    }

    Ok(())
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
    let mut carried_tail: Option<ConvolutionTail> = None;

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
            last_seen_epoch = Some(epoch);
        }

        let is_single_file = matches!(directive, PlayDirective::SingleFile { .. });

        let (chosen, mode_label) = match directive {
            PlayDirective::Random { root } => {
                let mut picked = match pick_random_flac_walk(&root) {
                    Ok(p) => p,
                    Err(err) => {
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
                        Ok(c) => seq_cursor = Some((seq_gen, c)),
                        Err(err) => {
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
        // the producer thread or wedges the UI.
        let incoming_tail = if is_single_file {
            drop(carried_tail.take());
            None
        } else {
            carried_tail.take()
        };
        let (outcome, next_tail) = match catch_unwind(AssertUnwindSafe(|| {
            stream_single_track(
                &pipeline,
                &ctrl,
                &buf,
                &shared,
                epoch,
                &chosen,
                mode_label,
                incoming_tail,
                !is_single_file,
            )
        })) {
            Ok(res) => res,
            Err(_) => (
                TrackStreamOutcome::Failed(format!(
                    "Internal panic while decoding/processing '{}'",
                    chosen.display()
                )),
                None,
            ),
        };

        match outcome {
            TrackStreamOutcome::Played => {
                consecutive_errors = 0;
                consecutive_unsupported = 0;
                carried_tail = next_tail;
            }
            TrackStreamOutcome::Aborted => {
                carried_tail = None;
            }
            TrackStreamOutcome::Unsupported(msg) => {
                carried_tail = None;
                eprintln!("  [skip] {msg}");
                shared.record_action(msg.clone());
                if is_single_file {
                    let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                    ctrl.pause_if_epoch(epoch);
                    continue;
                }
                consecutive_unsupported += 1;
                if consecutive_unsupported >= MAX_CONSECUTIVE_UNSUPPORTED {
                    let _ = buf.push_for_epoch(
                        epoch,
                        StreamEvent::FatalError(format!(
                            "Paused after {MAX_CONSECUTIVE_UNSUPPORTED} consecutive unsupported FLACs — DAC released"
                        )),
                    );
                    ctrl.pause_if_epoch(epoch);
                    consecutive_unsupported = 0;
                }
            }
            TrackStreamOutcome::Failed(msg) => {
                carried_tail = None;
                eprintln!("  [skip] {msg}");
                shared.record_action(msg.clone());
                if is_single_file {
                    let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                    ctrl.pause_if_epoch(epoch);
                    continue;
                }
                consecutive_errors += 1;
                if consecutive_errors >= 10 {
                    let _ = buf.push_for_epoch(
                        epoch,
                        StreamEvent::FatalError(
                            "Paused after 10 consecutive unreadable/unplayable FLACs — DAC released"
                                .into(),
                        ),
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
/// Defers emitting `TrackStart` until the first non-empty `PcmBlock` is produced so 0-frame or
/// immediately-corrupt tracks never open/close the DAC pin and always count toward error limits.
#[allow(clippy::too_many_arguments)]
fn stream_single_track(
    pipeline: &AudioPipeline,
    ctrl: &ProducerControl,
    buf: &BoundedBlockStreamBuffer,
    shared: &SharedControl,
    epoch: u64,
    chosen: &Path,
    mode_label: PlayModeLabel,
    incoming_tail: Option<ConvolutionTail>,
    gapless_carryover: bool,
) -> (TrackStreamOutcome, Option<ConvolutionTail>) {
    let allowed_root = shared.allowed_root();
    if let Err(reason) = validate_playable_flac_path(chosen, Some(&allowed_root)) {
        return (
            TrackStreamOutcome::Failed(format!(
                "Rejected non-playable path '{}': {reason}",
                chosen.display()
            )),
            None,
        );
    }

    let mut source = match FlacBlockReader::open(chosen) {
        Ok(s) => s,
        Err(err) => {
            if err.starts_with(UNSUPPORTED_CHANNEL_LAYOUT_PREFIX) {
                return (
                    TrackStreamOutcome::Unsupported(format!(
                        "Skipped unsupported FLAC '{}': {err}",
                        chosen.display()
                    )),
                    None,
                );
            }
            return (
                TrackStreamOutcome::Failed(format!(
                    "Skipped unreadable FLAC '{}': {err}",
                    chosen.display()
                )),
                None,
            );
        }
    };

    let spec = source.spec();
    let peak_meta = source.peak_meta();
    let total_duration_s = source
        .total_frames()
        .map(|f| f as f64 / spec.sample_rate_hz as f64);

    let mut session = match pipeline.start_track(spec, &peak_meta, source.is_streaming()) {
        Ok(s) => s,
        Err(err) => {
            if err.starts_with(UNSUPPORTED_SAMPLE_RATE_PREFIX) {
                return (
                    TrackStreamOutcome::Unsupported(format!(
                        "Skipped unsupported sample rate on '{}': {err}",
                        chosen.display()
                    )),
                    None,
                );
            }
            return (
                TrackStreamOutcome::Failed(format!(
                    "DSP pipeline error on '{}': {err}",
                    chosen.display()
                )),
                None,
            );
        }
    };
    if let Some(tail) = incoming_tail {
        session.set_incoming_convolver_tail(tail);
    }

    let mut pending_start = Some(StreamEvent::TrackStart {
        path: chosen.to_path_buf(),
        mode_label,
        report: session.report().clone(),
        total_duration_s,
    });
    let mut pushed_any_pcm = false;
    let mut first_block_error: Option<String> = None;

    loop {
        if ctrl.epoch() != epoch {
            return (TrackStreamOutcome::Aborted, None);
        }
        let next_src = match source.read_next_block(DEFAULT_SOURCE_BLOCK_FRAMES) {
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
        };

        let pcm_block = match session.process_block(next_src) {
            Ok(b) => b,
            Err(err) => {
                let msg = format!("DSP block error on '{}': {err}", chosen.display());
                if !pushed_any_pcm {
                    return (TrackStreamOutcome::Failed(msg), None);
                }
                eprintln!("  [dsp] {msg}");
                shared.record_action(msg);
                let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
                return (TrackStreamOutcome::Played, None);
            }
        };

        if !pcm_block.is_empty() {
            if let Some(start_ev) = pending_start.take() {
                if !buf.push_for_epoch(epoch, start_ev) {
                    return (TrackStreamOutcome::Aborted, None);
                }
            }
            if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm_block)) {
                return (TrackStreamOutcome::Aborted, None);
            }
            pushed_any_pcm = true;
        }
    }

    if ctrl.epoch() != epoch {
        return (TrackStreamOutcome::Aborted, None);
    }

    // Flush DSP tail (only if there wasn't a pre-first-block stream error).
    let mut next_tail: Option<ConvolutionTail> = None;
    if first_block_error.is_none() {
        let flushed = if gapless_carryover {
            session.flush_for_gapless()
        } else {
            session.flush().map(|pcm| (pcm, None))
        };
        if let Ok((tail_pcm, conv_tail)) = flushed {
            next_tail = conv_tail;
            if !tail_pcm.is_empty() {
                if let Some(start_ev) = pending_start.take() {
                    if !buf.push_for_epoch(epoch, start_ev) {
                        return (TrackStreamOutcome::Aborted, None);
                    }
                }
                if !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(tail_pcm)) {
                    return (TrackStreamOutcome::Aborted, None);
                }
                pushed_any_pcm = true;
            }
        }
    }

    if !pushed_any_pcm {
        return (
            TrackStreamOutcome::Failed(first_block_error.unwrap_or_else(|| {
                format!("Skipped empty (0-frame) FLAC '{}'", chosen.display())
            })),
            None,
        );
    }

    if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
        return (TrackStreamOutcome::Aborted, None);
    }
    (TrackStreamOutcome::Played, next_tail)
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
        // Point producer at a nonexistent directory so it immediately hits error -> pause_if_epoch.
        // Run concurrently with Shutdown to verify `producer_loop` NEVER hangs in `wait_active`.
        for _ in 0..20 {
            let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
            let ep = buf.current_epoch();
            let ctrl = ProducerControl::new(
                ep,
                ProducerDirective::Active(PlayDirective::Sequential {
                    root: PathBuf::from(r"C:\__definitely_nonexistent_ap_test_dir__"),
                    seq_gen: 1,
                    start_after: None,
                    folder_only: false,
                }),
            );
            let shared = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");
            let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();

            let buf_bg = Arc::clone(&buf);
            let ctrl_bg = Arc::clone(&ctrl);
            let shared_bg = Arc::clone(&shared);
            let handle =
                thread::spawn(move || producer_loop(pipeline, ctrl_bg, buf_bg, shared_bg));

            buf.close();
            ctrl.set(ep.wrapping_add(1), ProducerDirective::Shutdown);

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
            } => {
                assert_eq!(r, &root);
                assert_ne!(*seq_gen, 1, "must allocate fresh seq_gen to rebuild cursor");
                assert_eq!(start_after.as_ref(), Some(&t1));
                assert!(!*folder_only);
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
            let (outcome, next_tail) = stream_single_track(
                &pipeline,
                &ctrl,
                &buf,
                &shared,
                ep0,
                &bad,
                PlayModeLabel::SingleFile,
                None,
                false,
            );
            assert!(next_tail.is_none());
            assert!(
                matches!(outcome, TrackStreamOutcome::Failed(ref m) if m.contains("Rejected")),
                "expected rejection for {}, got {outcome:?}",
                bad.display()
            );
        }
    }
}
