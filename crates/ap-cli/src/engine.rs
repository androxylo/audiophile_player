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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ap_control::security::validate_playable_flac_path;
use ap_control::server::{
    ControlServer, clear_runtime_url_file_at, runtime_url_file_path, write_runtime_url_file_at,
};
use ap_control::state::{
    ControlCommand, PlayModeLabel, SharedControl, StreamCaptureRequest, UpnpPlayRequest,
};
use ap_core::buffer::DEFAULT_SOURCE_BLOCK_FRAMES;
use std::str::FromStr;

use ap_core::config::{
    ConvolutionConfig, PlayerConfig, QuantizerConfig, TrackDbConfig, UltimateConfig,
};
use ap_core::format::DacWireFormat;
use ap_core::track_db::TrackDbStore;
use ap_dsp::pipeline::{AudioPipeline, GaplessAttachOutcome, GaplessCarryover};
use ap_dsp::selector::{NoiseShaperProfile, UNSUPPORTED_SAMPLE_RATE_PREFIX, UpsamplerPolicy};
use ap_output_win::hygiene::OptimizationLevel;
use ap_output_win::sink::{OutputError, PlaybackStats};
use ap_output_win::wdm_ks::WdmKsSink;
use ap_source::flac::{
    AudioBlockSource, FlacBlockReader, UNSUPPORTED_CHANNEL_LAYOUT_PREFIX,
};
use ap_source::http_stream::{HttpAudioSource, HttpStreamHints};
use ap_source::library::{SequentialFlacCursor, default_user_music_dir, pick_random_flac_walk};
use ap_source::stream_capture::{
    CaptureEndReason, SharedCaptureTransport, StreamCaptureMonitor, StreamCaptureSource,
    verify_status_exclusive,
};
use ap_upnp::{UpnpRendererConfig, UpnpRendererServer, UpnpRendererState};

use crate::cli_args::{QuantizerCliOverride, UltimateCliOverride, UpnpCliOverride};
use crate::prefetch::{
    BoundedBlockStreamBuffer, DEFAULT_MAX_BUFFERED_FRAMES, PREBUFFER_SECONDS, PopBlockResult,
    StreamEvent, ULTIMATE_MAX_BUFFERED_FRAMES, ULTIMATE_MAX_BUFFERED_SECONDS,
};
use crate::signals::{ShutdownController, ShutdownSignal};
use crate::ultimate::{UltimateRuntimeContext, UltimateStreamResult, stream_track_ultimate};

/// Prebuffer cushion for live HTTP/UPnP streams (`2 s` at output rate — absorbs LAN/WAN jitter
/// and heavy 4M-tap REW/upsampler bursts while starting playback rapidly).
const UPNP_PREBUFFER_SECONDS: u32 = 2;

/// Prebuffer cushion for live exclusive kernel capture (`0.75 s` at output rate — ~16 Tidal
/// WaveRT packets / ~4 UPOLS FFT partitions for zero underruns with crisp startup latency).
const STREAM_CAPTURE_PREBUFFER_MS: usize = 750;

/// Tighter engine output buffer duration cap during live exclusive capture (`3 s`) so clock drift
/// never accumulates into a 24–30 s queue (Review #8 B4).
const STREAM_CAPTURE_MAX_BUFFERED: Duration = Duration::from_secs(3);

/// Target output frames per streaming DSP chunk (`65,536` = exact 1 UPOLS FFT partition,
/// `~171 ms` @ 384 kHz / `~186 ms` @ 352.8 kHz). Eliminates multi-second monolithic DSP gulps
/// at `8x–48x` upsampling ratios and keeps end-of-pipeline buffer fill `> 99%` across all rates.
const TARGET_OUTPUT_BLOCK_FRAMES: usize = 65_536;

/// Compute source read chunk size so each `session.process_block()` produces ~1 UPOLS partition
/// (`~65,536` output frames) at upsampled rates, or `~0.25 s` on non-8fS `--filter null`.
fn source_block_frames_for_rates(source_rate_hz: u32, output_rate_hz: u32) -> usize {
    let src = u64::from(source_rate_hz.max(1));
    let dst = u64::from(output_rate_hz.max(1));
    if dst <= src {
        ((src / 4) as usize).clamp(4_096, DEFAULT_SOURCE_BLOCK_FRAMES)
    } else {
        let scaled = ((TARGET_OUTPUT_BLOCK_FRAMES as u64) * src).div_ceil(dst);
        (scaled as usize).clamp(1_024, DEFAULT_SOURCE_BLOCK_FRAMES)
    }
}

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
    UpnpStream {
        request: UpnpPlayRequest,
    },
    StreamCapture {
        request: StreamCaptureRequest,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProducerDirective {
    Paused,
    Active(PlayDirective),
    Shutdown,
}

struct ProducerControl {
    state: Mutex<(u64, ProducerDirective, Arc<AtomicBool>)>,
    cv: Condvar,
    #[cfg(test)]
    before_pause_enter: Option<Arc<std::sync::Barrier>>,
    #[cfg(test)]
    before_pause_release: Option<Arc<std::sync::Barrier>>,
}

impl ProducerControl {
    fn new(initial_epoch: u64, initial: ProducerDirective) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((initial_epoch, initial, Arc::new(AtomicBool::new(false)))),
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
            state: Mutex::new((initial_epoch, initial, Arc::new(AtomicBool::new(false)))),
            cv: Condvar::new(),
            before_pause_enter: Some(enter),
            before_pause_release: Some(release),
        })
    }

    /// Publish a new `(epoch, directive)`. `Shutdown` is sticky and can never be overwritten.
    /// Signals the previous epoch's cancellation flag so any blocked HTTP stream aborts immediately.
    fn set(&self, epoch: u64, directive: ProducerDirective) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(st.1, ProducerDirective::Shutdown) {
            return;
        }
        st.2.store(true, Ordering::SeqCst);
        let next_cancel = Arc::new(AtomicBool::new(matches!(
            directive,
            ProducerDirective::Shutdown
        )));
        *st = (epoch, directive, next_cancel);
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

    #[cfg(test)]
    fn wait_active(&self) -> Option<(u64, PlayDirective)> {
        self.wait_active_with_cancel().map(|(e, d, _)| (e, d))
    }

    fn wait_active_with_cancel(&self) -> Option<(u64, PlayDirective, Arc<AtomicBool>)> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match &st.1 {
                ProducerDirective::Shutdown => return None,
                ProducerDirective::Paused => {
                    st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
                }
                ProducerDirective::Active(dir) => {
                    return Some((st.0, dir.clone(), Arc::clone(&st.2)));
                }
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
    upnp_state: Option<Arc<UpnpRendererState>>,
    capture_transport: Option<SharedCaptureTransport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchOutcome {
    Applied,
    /// Command was rejected (e.g. TOCTOU-invalidated `PlayFile`) with zero state/epoch change.
    Rejected,
}

impl<'a> EngineState<'a> {
    #[cfg(test)]
    fn new(
        initial_epoch: u64,
        initial_dir: PlayDirective,
        buf: &'a BoundedBlockStreamBuffer,
        prod_ctrl: &'a ProducerControl,
        shared: &'a SharedControl,
    ) -> Self {
        Self::with_upnp(initial_epoch, initial_dir, buf, prod_ctrl, shared, None)
    }

    #[cfg(test)]
    fn with_upnp(
        initial_epoch: u64,
        initial_dir: PlayDirective,
        buf: &'a BoundedBlockStreamBuffer,
        prod_ctrl: &'a ProducerControl,
        shared: &'a SharedControl,
        upnp_state: Option<Arc<UpnpRendererState>>,
    ) -> Self {
        Self::with_sources(
            initial_epoch,
            initial_dir,
            buf,
            prod_ctrl,
            shared,
            upnp_state,
            None,
        )
    }

    fn with_sources(
        initial_epoch: u64,
        initial_dir: PlayDirective,
        buf: &'a BoundedBlockStreamBuffer,
        prod_ctrl: &'a ProducerControl,
        shared: &'a SharedControl,
        upnp_state: Option<Arc<UpnpRendererState>>,
        capture_transport: Option<SharedCaptureTransport>,
    ) -> Self {
        let next_seq_gen = match &initial_dir {
            PlayDirective::Sequential { seq_gen, .. } => seq_gen.wrapping_add(1),
            _ => 1,
        };
        let awaiting_first = !matches!(
            initial_dir,
            PlayDirective::SingleFile { .. }
                | PlayDirective::UpnpStream { .. }
                | PlayDirective::StreamCapture { .. }
        );
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
            upnp_state,
            capture_transport,
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
        let is_capture = matches!(dir, PlayDirective::StreamCapture { .. });
        self.buf.set_epoch_duration_cap(if is_capture {
            Some(STREAM_CAPTURE_MAX_BUFFERED)
        } else {
            None
        });
        if !is_capture {
            // User explicitly switched to local/UPnP while a capture stream may be running:
            // remember its generation so the monitor retry loop does not fight the user (B5).
            if let Some(ref cap) = self.capture_transport {
                let g = cap.active_generation();
                if g != 0 {
                    cap.mark_user_paused(g);
                }
            }
        }
        let ep = self.buf.reset_and_flush();
        if reset_started_track {
            self.last_started_track = None;
        }
        // Review #3 Finding #20: for PlaySequential / PlayRandom, we have not started playing the
        // first track yet, so a Next arriving before Track 1 plays should advance past Track 1
        // (dropping any buffered TrackStart(01) or restarting with skip_count=1) rather than
        // replaying Track 1. SingleFile handles Next explicitly in dispatch().
        let awaiting_first = !matches!(
            dir,
            PlayDirective::SingleFile { .. }
                | PlayDirective::UpnpStream { .. }
                | PlayDirective::StreamCapture { .. }
        );
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

    fn notify_upnp_preempted(&self) {
        if matches!(self.last_play, PlayDirective::UpnpStream { .. }) {
            if let Some(ref upnp) = self.upnp_state {
                upnp.engine_mark_preempted();
            }
        }
    }

    fn dispatch(&mut self, cmd: ControlCommand) -> DispatchOutcome {
        match cmd {
            ControlCommand::PlayRandom(root) => {
                self.notify_upnp_preempted();
                self.start_directive(PlayDirective::Random { root }, true);
                DispatchOutcome::Applied
            }
            ControlCommand::PlaySequential(root) => {
                self.notify_upnp_preempted();
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
                DispatchOutcome::Applied
            }
            ControlCommand::PlayFile(file) => {
                let root = self.shared.allowed_root();
                match validate_playable_flac_path(&file, Some(&root)) {
                    Ok(valid) => {
                        // Review #5 Finding #19: only preempt UPnP after validation succeeds!
                        self.notify_upnp_preempted();
                        self.start_directive(PlayDirective::SingleFile { file: valid }, true);
                        DispatchOutcome::Applied
                    }
                    Err(reason) => {
                        let msg = format!(
                            "Rejected invalid PlayFile path '{}': {reason}",
                            file.display()
                        );
                        eprintln!("  [reject] {msg}");
                        self.shared.record_action(msg);
                        DispatchOutcome::Rejected
                    }
                }
            }
            ControlCommand::PlayUpnp(request) => {
                self.start_directive(PlayDirective::UpnpStream { request }, true);
                DispatchOutcome::Applied
            }
            ControlCommand::PlayStreamCapture(request) => {
                // Review #8 B1: when already actively playing exclusive capture, DO NOT flush the
                // engine buffer or interrupt WDM-KS! The producer follows generations seamlessly
                // inside the same epoch (preserving the previous track's full REW/upsampler tail).
                if !self.is_paused && matches!(self.last_play, PlayDirective::StreamCapture { .. }) {
                    self.last_play = PlayDirective::StreamCapture { request };
                    return DispatchOutcome::Rejected;
                }
                if let Some(ref cap) = self.capture_transport {
                    cap.clear_user_paused();
                }
                self.notify_upnp_preempted();
                self.start_directive(PlayDirective::StreamCapture { request }, true);
                DispatchOutcome::Applied
            }
            ControlCommand::Next => {
                if let PlayDirective::StreamCapture { ref request } = self.last_play {
                    // Review #8 B5: when paused in AP during an active exclusive stream, allow
                    // Next to resume the current running capture generation from AP!
                    if self.is_paused {
                        if let Some(ref cap) = self.capture_transport {
                            if let Ok(st) = cap.get_status() {
                                if st.stream_running != 0 && verify_status_exclusive(&st).is_ok() {
                                    let mut resumed = request.clone();
                                    resumed.generation = st.stream_generation;
                                    resumed.sample_rate_hz = st.format.sample_rate;
                                    resumed.channels = st.format.channels;
                                    resumed.bits_per_sample = st.format.bits_per_sample;
                                    resumed.valid_bits_per_sample = if st.format.valid_bits_per_sample > 0 {
                                        st.format.valid_bits_per_sample
                                    } else {
                                        st.format.bits_per_sample
                                    };
                                    cap.clear_user_paused();
                                    self.start_directive(
                                        PlayDirective::StreamCapture { request: resumed },
                                        true,
                                    );
                                    return DispatchOutcome::Applied;
                                }
                            }
                        }
                    }
                    // While actively streaming live capture, tracks are controlled in the host app.
                    return DispatchOutcome::Rejected;
                }
                if matches!(self.last_play, PlayDirective::UpnpStream { .. }) {
                    if let Some(next_req) = self
                        .upnp_state
                        .as_deref()
                        .and_then(UpnpRendererState::engine_take_next_for_gapless)
                    {
                        self.start_directive(
                            PlayDirective::UpnpStream { request: next_req },
                            true,
                        );
                        return DispatchOutcome::Applied;
                    }
                    // Review #6 Finding #7: when no NextURI is queued during UPnP playback,
                    // return Rejected so the consumer's resume loop clears `stop_handle` and
                    // continues playing the active stream instead of discarding remaining PCM.
                    return DispatchOutcome::Rejected;
                }
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
                    return DispatchOutcome::Applied;
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
                        return DispatchOutcome::Applied;
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
                    return DispatchOutcome::Applied;
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
                DispatchOutcome::Applied
            }
            ControlCommand::Pause => {
                // Record user-paused capture generation so monitor does not auto-resume over
                // an explicit AP Pause (Review #8 B5).
                if let Some(ref cap) = self.capture_transport {
                    let g = cap.active_generation();
                    if g != 0 {
                        cap.mark_user_paused(g);
                    } else if let PlayDirective::StreamCapture { ref request } = self.last_play {
                        cap.mark_user_paused(request.generation);
                    }
                }
                self.notify_upnp_preempted();
                self.pause_with_message(
                    "Paused — WDM-KS kernel pin closed (DAC available to Audirvana)",
                );
                println!("  [paused] WDM-KS kernel pin closed (DAC released).");
                let _ = io::stdout().flush();
                DispatchOutcome::Applied
            }
        }
    }
}

/// RAII guard ensuring `server.url` is always removed on ANY early startup error (Review #5 #21).
struct RuntimeUrlGuard {
    path: Option<PathBuf>,
    url: String,
    active: bool,
}

impl RuntimeUrlGuard {
    fn write(path: Option<PathBuf>, url: String) -> Self {
        if let Some(ref p) = path {
            write_runtime_url_file_at(p, &url);
        }
        Self {
            path,
            url,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for RuntimeUrlGuard {
    fn drop(&mut self) {
        if self.active {
            if let Some(ref p) = self.path {
                clear_runtime_url_file_at(p, &self.url);
            }
        }
    }
}

/// Merge `[quantizer]` config settings with explicit CLI `--dac-bits` / `--shaper` overrides
/// (CLI always takes precedence over `config.toml`).
fn resolve_upsampler_policy(
    mut cli_policy: UpsamplerPolicy,
    cli_quant: QuantizerCliOverride,
    cfg_quant: &QuantizerConfig,
) -> Result<UpsamplerPolicy, String> {
    cli_policy.dac_bits = cli_quant.dac_bits.unwrap_or(cfg_quant.dac_bits);
    cli_policy.shaper_override = match cli_quant.shaper {
        Some(explicit) => explicit,
        None => match cfg_quant.shaper.as_deref() {
            None => None,
            Some(name) if name.eq_ignore_ascii_case("auto") => None,
            Some(name) => Some(NoiseShaperProfile::from_str(name)?),
        },
    };
    cli_policy.validate()?;
    Ok(cli_policy)
}

#[cfg(test)]
mod quantizer_policy_tests {
    use super::*;

    #[test]
    fn cli_quantizer_flags_override_toml_config_and_auto_clears_toml_shaper() {
        let cfg = QuantizerConfig {
            dac_bits: 16,
            shaper: Some("ns5".to_string()),
        };

        // No CLI flags -> config wins.
        let p_cfg =
            resolve_upsampler_policy(UpsamplerPolicy::default(), QuantizerCliOverride::default(), &cfg)
                .unwrap();
        assert_eq!(p_cfg.dac_bits, 16);
        assert_eq!(p_cfg.shaper_override, Some(NoiseShaperProfile::Ns5Multibit));

        // Explicit --dac-bits 20 --shaper auto overrides both TOML fields.
        let p_cli = resolve_upsampler_policy(
            UpsamplerPolicy::default(),
            QuantizerCliOverride {
                dac_bits: Some(20),
                shaper: Some(None),
            },
            &cfg,
        )
        .unwrap();
        assert_eq!(p_cli.dac_bits, 20);
        assert_eq!(p_cli.shaper_override, None);
    }

    #[test]
    fn output_aligned_source_chunk_keeps_every_rate_near_one_upols_partition() {
        // 8fS upsampled rates align cleanly to ~65,536 output frames (exact 1 UPOLS FFT partition).
        assert_eq!(source_block_frames_for_rates(384_000, 384_000), 65_536);
        assert_eq!(source_block_frames_for_rates(192_000, 384_000), 32_768);
        assert_eq!(source_block_frames_for_rates(96_000, 384_000), 16_384);
        assert_eq!(source_block_frames_for_rates(44_100, 352_800), 8_192);
        assert_eq!(source_block_frames_for_rates(22_050, 352_800), 4_096);
        // 48x (8 kHz -> 384 kHz): ~1,366 source -> ~65,568 output (~171 ms, NOT 8.19 s!).
        let chunk_8k = source_block_frames_for_rates(8_000, 384_000);
        assert_eq!(chunk_8k, 1_366);
        assert!((chunk_8k * 48) <= 66_000);
        // Non-8fS --filter null pass-through stays responsive (~0.25 s).
        assert_eq!(source_block_frames_for_rates(44_100, 44_100), 11_025);
    }
}

/// Run default zero-config random FLAC playback via bounded-memory block streaming,
/// tidy `AudioPipeline`, native Windows Kernel Streaming (`WDM-KS`), localhost HTTP UI,
/// and built-in UPnP AV MediaRenderer (for foobar2000 / BubbleUPnP).
#[allow(clippy::too_many_arguments)]
pub fn run_random_music_player(
    config_path: Option<PathBuf>,
    music_dir_override: Option<PathBuf>,
    device_filter: Option<String>,
    optimization: OptimizationLevel,
    optimization_explicit: bool,
    http_port: Option<u16>,
    upsampler_policy: UpsamplerPolicy,
    quantizer_override: QuantizerCliOverride,
    rew_override: Option<ConvolutionConfig>,
    upnp_override: UpnpCliOverride,
    capture_disabled: bool,
    ultimate_override: UltimateCliOverride,
    controller: &ShutdownController,
) -> Result<(), String> {
    let loaded_cfg = PlayerConfig::discover_and_load(config_path.as_deref())?;
    let effective_conv = rew_override.unwrap_or(loaded_cfg.config.convolution);
    effective_conv.validate_paths()?;
    let effective_policy = resolve_upsampler_policy(
        upsampler_policy,
        quantizer_override,
        &loaded_cfg.config.quantizer,
    )?;

    let effective_ultimate = UltimateConfig {
        enabled: ultimate_override
            .enabled
            .unwrap_or(loaded_cfg.config.ultimate.enabled),
        genre_rules: ultimate_override
            .genre_rules
            .unwrap_or(loaded_cfg.config.ultimate.genre_rules),
        learn_on_play: ultimate_override
            .learn_on_play
            .unwrap_or(loaded_cfg.config.ultimate.learn_on_play),
    };
    let effective_trackdb = TrackDbConfig {
        dir: ultimate_override
            .trackdb_dir
            .or(loaded_cfg.config.trackdb.dir),
        sidecar: ultimate_override
            .trackdb_sidecar
            .unwrap_or(loaded_cfg.config.trackdb.sidecar),
    };
    let optimization = if effective_ultimate.enabled && !optimization_explicit {
        OptimizationLevel::Extreme
    } else {
        optimization
    };
    let ultimate_ctx = if effective_ultimate.enabled {
        let db_root = effective_trackdb
            .dir
            .clone()
            .unwrap_or_else(TrackDbStore::default_central_dir);
        Some(UltimateRuntimeContext {
            config: effective_ultimate,
            store: TrackDbStore::new(db_root, effective_trackdb.sidecar),
        })
    } else {
        None
    };

    let upnp_explicit = upnp_override.port.is_some()
        || upnp_override.bind_ip.is_some()
        || loaded_cfg.config.upnp.port.is_some()
        || loaded_cfg.config.upnp.bind_ip.is_some();
    let upnp_enabled = loaded_cfg.config.upnp.enabled && !upnp_override.disabled;
    let cfg_bind_ip = loaded_cfg
        .config
        .upnp
        .bind_ip
        .as_deref()
        .map(|s| {
            s.parse::<std::net::Ipv4Addr>()
                .map_err(|_| format!("invalid [upnp] bind_ip '{s}'"))
        })
        .transpose()?;
    let effective_upnp_cfg = UpnpRendererConfig {
        friendly_name: upnp_override
            .friendly_name
            .unwrap_or(loaded_cfg.config.upnp.friendly_name),
        port: upnp_override.port.or(loaded_cfg.config.upnp.port),
        bind_ip: upnp_override.bind_ip.or(cfg_bind_ip),
        enable_ssdp: true,
    };

    let pipeline = AudioPipeline::with_rew_config_full_tail(
        effective_policy,
        effective_conv,
        effective_ultimate.enabled,
    )?;
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
    let url_file_path = runtime_url_file_path();
    let mut url_guard = RuntimeUrlGuard::write(url_file_path.clone(), http_url.clone());

    // Review #5 Finding #21: explicit UPnP port/bind failures abort cleanly (and RuntimeUrlGuard
    // removes server.url); default zero-config UPnP bind failure logs a warning and continues!
    let upnp_server = if upnp_enabled {
        match UpnpRendererServer::start(shared.clone(), effective_upnp_cfg) {
            Ok(srv) => Some(srv),
            Err(err) if upnp_explicit => return Err(err),
            Err(err) => {
                eprintln!("  [upnp warning] could not start UPnP renderer ({err}); continuing without UPnP");
                shared.record_action(format!("UPnP renderer disabled after bind error: {err}"));
                None
            }
        }
    } else {
        None
    };
    let upnp_state = upnp_server.as_ref().map(|s| Arc::clone(s.state()));

    // Start opt-in exclusive kernel stream capture watcher (\\.\ApExclusiveAudio).
    // Quietly parks if the virtual ACX driver is not installed; preempts automatically when Tidal
    // Desktop opens in Exclusive Mode (hard zero-mixer gate enforced in kernel & user mode).
    let capture_enabled = loaded_cfg.config.capture.enabled && !capture_disabled;
    let capture_monitor = if capture_enabled {
        let shared_for_cap = Arc::clone(&shared);
        let shared_for_blk = Arc::clone(&shared);
        Some(StreamCaptureMonitor::start(
            move |ev| {
                shared_for_cap.submit_command(ControlCommand::PlayStreamCapture(
                    StreamCaptureRequest {
                        generation: ev.generation,
                        sample_rate_hz: ev.sample_rate_hz,
                        channels: ev.channels,
                        bits_per_sample: ev.bits_per_sample,
                        valid_bits_per_sample: ev.valid_bits_per_sample,
                        caller_name: ev.caller_name,
                        caller_pid: ev.caller_pid,
                    },
                ));
            },
            move |delta, proc_name, pid, stage| {
                let msg = format!(
                    "Blocked {delta} Windows mixer (shared-mode) attempt(s) from {proc_name} (PID {pid}) at {stage}"
                );
                eprintln!("  [capture zero-mixer] {msg}");
                shared_for_blk.record_action(msg);
            },
        ))
    } else {
        None
    };
    let capture_transport = capture_monitor.as_ref().map(|m| m.transport());

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
    if let Some(ref upnp) = upnp_server {
        println!(
            "UPnP renderer: '{}' ({}, {})",
            upnp.friendly_name(),
            upnp.location_url(),
            upnp.udn()
        );
    } else {
        println!("UPnP renderer: disabled");
    }
    match capture_monitor.as_ref() {
        Some(mon) if mon.is_driver_connected() => {
            println!(
                "Exclusive capture: READY (\\\\.\\ApExclusiveAudio -> 'Line (Audiophile Player)' in Tidal Exclusive Mode; Windows mixer blocked)"
            );
        }
        Some(_) => {
            println!("Exclusive capture: waiting for opt-in ACX driver (\\\\.\\ApExclusiveAudio)");
        }
        None => {
            println!("Exclusive capture: disabled");
        }
    }
    if let Some(ref ult) = ultimate_ctx {
        println!(
            "Ultimate mode: ENABLED ({}s look-ahead, full-tail REW, VirtualLock RAM, genre_rules={}, learn_on_play={}, trackdb='{}')",
            ULTIMATE_MAX_BUFFERED_SECONDS,
            ult.config.genre_rules,
            ult.config.learn_on_play,
            ult.store.root_dir().display()
        );
    }
    println!("Press Ctrl+C or Ctrl+Break to stop gracefully.");
    let _ = io::stdout().flush();

    let stream_buf = if ultimate_ctx.is_some() {
        BoundedBlockStreamBuffer::with_limits_and_pinning(
            ULTIMATE_MAX_BUFFERED_FRAMES,
            Duration::from_secs(ULTIMATE_MAX_BUFFERED_SECONDS),
            true,
        )
    } else {
        BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES)
    };
    let initial_epoch = stream_buf.current_epoch();
    let initial_dir = PlayDirective::Random {
        root: music_dir.clone(),
    };
    let prod_ctrl =
        ProducerControl::new(initial_epoch, ProducerDirective::Active(initial_dir.clone()));

    // Dedicated background streaming DSP Producer thread (24 s normal / 30 s VirtualLock ultimate).
    let buf_bg = Arc::clone(&stream_buf);
    let ctrl_bg = Arc::clone(&prod_ctrl);
    let shared_bg = Arc::clone(&shared);
    let upnp_bg = upnp_state.clone();
    let capture_bg = capture_transport.clone();
    let producer_thread = thread::Builder::new()
        .name("ap-dsp-stream".to_string())
        .spawn(move || {
            producer_loop_with_sources(
                pipeline,
                ctrl_bg,
                buf_bg,
                shared_bg,
                upnp_bg,
                capture_bg,
                ultimate_ctx,
            )
        })
        .map_err(|e| format!("failed spawning DSP streaming thread: {e}"))?;

    let mut state = EngineState::with_sources(
        initial_epoch,
        initial_dir,
        &stream_buf,
        &prod_ctrl,
        &shared,
        upnp_state,
        capture_transport,
    );

    while controller.current_signal().is_none() {
        match step_consumer_once(
            &mut state,
            &stream_buf,
            &shared,
            controller,
            &stop_handle,
            &mut sink,
            || producer_thread.is_finished(),
        ) {
            ConsumerStepResult::Continue => {}
            ConsumerStepResult::Exit => break,
        }
    }

    url_guard.disarm();
    if let Some(mon) = capture_monitor {
        mon.stop();
    }
    teardown_player_resources_with_upnp(
        &stream_buf,
        &prod_ctrl,
        state.active_epoch,
        http_server,
        upnp_server,
        url_file_path.as_deref(),
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

/// Narrow streaming-sink abstraction shared by `WdmKsSink` and mock-sink consumer tests
/// (Review #4 Finding #15).
trait BlockStreamAudioSink {
    fn play_block_stream<F>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> ap_output_win::wdm_ks::StreamBlock;

    fn play_block_stream_with_recycler<F, R>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
        _recycle_block: R,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> ap_output_win::wdm_ks::StreamBlock,
        R: FnMut(ap_core::buffer::InterleavedInt32Buffer) + 'static,
    {
        self.play_block_stream(sample_rate_hz, wire_format, next_block)
    }
}

impl BlockStreamAudioSink for WdmKsSink {
    fn play_block_stream<F>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> ap_output_win::wdm_ks::StreamBlock,
    {
        WdmKsSink::play_block_stream(self, sample_rate_hz, wire_format, next_block)
    }

    fn play_block_stream_with_recycler<F, R>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
        recycle_block: R,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> ap_output_win::wdm_ks::StreamBlock,
        R: FnMut(ap_core::buffer::InterleavedInt32Buffer) + 'static,
    {
        WdmKsSink::play_block_stream_with_recycler(
            self,
            sample_rate_hz,
            wire_format,
            next_block,
            recycle_block,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsumerStepResult {
    Continue,
    Exit,
}

/// Execute one iteration of the playback consumer loop (command handling, TrackStart prebuffer
/// with re-queue protection, `stop_flag` arming, streaming playback, and seamless resume on
/// rejected commands).
#[allow(clippy::too_many_arguments)]
fn step_consumer_once<S: BlockStreamAudioSink>(
    state: &mut EngineState<'_>,
    stream_buf: &Arc<BoundedBlockStreamBuffer>,
    shared: &SharedControl,
    controller: &ShutdownController,
    stop_handle: &AtomicBool,
    sink: &mut S,
    producer_finished: impl Fn() -> bool,
) -> ConsumerStepResult {
    if controller.current_signal().is_some() {
        return ConsumerStepResult::Exit;
    }

    if let Some(cmd) = shared.take_pending_command() {
        let _ = state.dispatch(cmd);
        return ConsumerStepResult::Continue;
    }

    if state.is_paused {
        // DAC WDM-KS pin is completely closed & released — Audirvana can play freely!
        return match shared.wait_for_command_or_shutdown() {
            Some(cmd) => {
                let _ = state.dispatch(cmd);
                ConsumerStepResult::Continue
            }
            None => ConsumerStepResult::Exit,
        };
    }

    let Some(event) =
        stream_buf.pop_for_epoch_timeout(state.active_epoch, Duration::from_millis(50))
    else {
        if producer_finished() && controller.current_signal().is_none() {
            let msg = "DSP producer thread stopped unexpectedly — paused & DAC released";
            eprintln!("  [error] {msg}");
            state.pause_with_message(msg);
        }
        return ConsumerStepResult::Continue;
    };

    if controller.current_signal().is_some() {
        return ConsumerStepResult::Exit;
    }
    // If a command arrived right as `pop_for_epoch_timeout` returned, put the popped event
    // back at the front before dispatching so a late `Next` does not double-skip a TrackStart.
    if let Some(cmd) = shared.take_pending_command() {
        let _ = stream_buf.push_front_for_epoch(state.active_epoch, event);
        let _ = state.dispatch(cmd);
        return ConsumerStepResult::Continue;
    }

    match event {
        StreamEvent::EndOfQueue(msg) => {
            if matches!(state.last_play, PlayDirective::UpnpStream { .. }) {
                // Review #5 Finding #6: if a late SetNextAVTransportURI arrived after the
                // producer already hit EOF and pushed EndOfQueue, promote & start it now!
                if let Some(next_req) = state
                    .upnp_state
                    .as_deref()
                    .and_then(UpnpRendererState::engine_take_next_for_gapless)
                {
                    state.start_directive(PlayDirective::UpnpStream { request: next_req }, true);
                    return ConsumerStepResult::Continue;
                }
                if let Some(ref upnp) = state.upnp_state {
                    upnp.engine_mark_finished();
                }
            }
            state.pause_with_message(msg);
        }
        StreamEvent::FatalError(msg) => {
            if matches!(state.last_play, PlayDirective::UpnpStream { .. }) {
                if let Some(ref upnp) = state.upnp_state {
                    upnp.engine_mark_error(&msg);
                }
            }
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
        }
        StreamEvent::TrackStart {
            path,
            upnp_uri,
            upnp_next_gen,
            mode_label,
            report,
            total_duration_s,
            had_gapless_carryover,
        } => {
            let out_rate = report.output_rate_hz;
            let watermark_frames = match mode_label {
                PlayModeLabel::StreamCapture => {
                    ((out_rate as usize) * STREAM_CAPTURE_PREBUFFER_MS) / 1_000
                }
                PlayModeLabel::UpnpRenderer => {
                    (out_rate as usize) * (UPNP_PREBUFFER_SECONDS as usize)
                }
                _ => (out_rate as usize) * (PREBUFFER_SECONDS as usize),
            };

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
                        if producer_finished() {
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

            // Re-queue the popped TrackStart before dispatching so skip_to_next_track_or_flush
            // applies identical just_skipped_track protection across all arrival windows.
            if let Some(cmd) = pending_cmd {
                let _ = stream_buf.push_front_for_epoch(
                    state.active_epoch,
                    StreamEvent::TrackStart {
                        path,
                        upnp_uri,
                        upnp_next_gen,
                        mode_label,
                        report,
                        total_duration_s,
                        had_gapless_carryover,
                    },
                );
                let _ = state.dispatch(cmd);
                return ConsumerStepResult::Continue;
            }

            if aborted_prebuffer || state.is_paused || controller.current_signal().is_some() {
                return ConsumerStepResult::Continue;
            }

            // Arm `stop_flag` here BEFORE the final signal/command check (never inside
            // `wdm_ks.rs`) so any Ctrl+C, Pause, or Next arriving right before open is
            // either caught immediately below or leaves `stop_flag == true` for the sink.
            stop_handle.store(false, Ordering::SeqCst);
            if controller.current_signal().is_some() {
                return ConsumerStepResult::Exit;
            }
            if let Some(cmd) = shared.take_pending_command() {
                let _ = stream_buf.push_front_for_epoch(
                    state.active_epoch,
                    StreamEvent::TrackStart {
                        path,
                        upnp_uri,
                        upnp_next_gen,
                        mode_label,
                        report,
                        total_duration_s,
                        had_gapless_carryover,
                    },
                );
                let _ = state.dispatch(cmd);
                return ConsumerStepResult::Continue;
            }

            // Record the track actually started by the consumer (not producer decode-ahead).
            state.record_anchor(path.clone());
            state.just_skipped_track = false;

            let fmt_str = report.format_summary(total_duration_s);
            let track_str = path.display().to_string();
            let decision_str = report.ultimate_report.as_ref().map(|u| u.summary());
            let known_peak = report
                .ultimate_report
                .as_ref()
                .and_then(|u| {
                    u.cached_m_dbfs
                        .map(|m| format!("{m:+.2} dBFS (whole-track)"))
                        .or_else(|| u.m30_dbfs.map(|m| format!("{m:+.2} dBFS (30 s window)")))
                })
                .or_else(|| {
                    report
                        .headroom
                        .peak_dbfs
                        .map(|p| match report.headroom.peak_kind {
                            Some(ap_core::headroom::PeakKind::TruePeak) => {
                                format!("{p:+.2} dBTP (tag)")
                            }
                            Some(ap_core::headroom::PeakKind::SamplePeak) => {
                                format!("{p:+.2} dBFS (tag)")
                            }
                            None => format!("{p:+.2} dBFS"),
                        })
                });
            println!("  [playing]  {track_str} ({fmt_str})");
            println!("  [pipeline] {}", report.log_line());
            if let Some(ref ult_rep) = report.ultimate_report {
                for w in &ult_rep.warnings {
                    println!("  [ultimate] {w}");
                    shared.record_action(format!("Ultimate: {w}"));
                }
            }
            let _ = io::stdout().flush();
            shared.record_action(report.log_line());
            shared.set_playing_with_decision_and_peak(
                mode_label,
                track_str,
                fmt_str,
                decision_str,
                known_peak,
            );
            if mode_label == PlayModeLabel::UpnpRenderer {
                if let Some(ref upnp) = state.upnp_state {
                    // Review #5 #5 & Review #6 #8/#17: promote prefetched NextURI -> CurrentURI
                    // strictly by generation token when audio becomes audible on the DAC; explicit
                    // Play/Seek restarts pass `upnp_next_gen = None` and never consume `st.next`.
                    if let Some(ref uri) = upnp_uri {
                        upnp.engine_mark_playing_with_gen(uri, total_duration_s, upnp_next_gen);
                    } else {
                        upnp.engine_mark_playing_current(total_duration_s);
                    }
                }
            }

            // Review #4 Finding #20 & Review #5 Finding #19: if playback is interrupted by a
            // rejected command (e.g. TOCTOU-invalidated PlayFile), drain any remaining queued
            // commands (e.g. a trailing Next or Pause) before clearing stop_handle, and re-check
            // immediately after clearing stop_handle so no command ever loses its stop signal.
            stream_buf.reset_track_low_water();
            loop {
                let ep_for_stream = state.active_epoch;
                let buf_for_stream = Arc::clone(stream_buf);
                let buf_for_recycle = Arc::clone(stream_buf);
                let live_peak_for_rt = shared.live_output_peak_handle();
                let hit_track_end = Arc::new(AtomicBool::new(false));
                let hit_end_cb = Arc::clone(&hit_track_end);
                let play_res = sink.play_block_stream_with_recycler(
                    out_rate,
                    DacWireFormat::Int32,
                    move || match buf_for_stream.try_pop_track_pcm_block_with_peak(ep_for_stream) {
                        (PopBlockResult::Block(b), peak_u32) => {
                            if peak_u32 > 0 {
                                live_peak_for_rt.fetch_max(peak_u32, Ordering::Relaxed);
                            }
                            ap_output_win::wdm_ks::StreamBlock::Ready(b)
                        }
                        (PopBlockResult::Underrun, _) => ap_output_win::wdm_ks::StreamBlock::Underrun,
                        (PopBlockResult::TrackEnd | PopBlockResult::StaleOrClosed, _) => {
                            hit_end_cb.store(true, Ordering::SeqCst);
                            ap_output_win::wdm_ks::StreamBlock::EndOfTrack
                        }
                    },
                    move |consumed| buf_for_recycle.recycle_consumed_block(consumed),
                );
                stream_buf.drain_deferred_drops();
                match play_res {
                    Ok(stats) => {
                        if stats.underrun_halves > 0 {
                            let warn_msg = format!(
                                "Buffer underrun on '{}' ({} Hz): {} DMA half-buffer(s) padded with silence",
                                path.display(),
                                out_rate,
                                stats.underrun_halves
                            );
                            eprintln!("  [underrun warn] {warn_msg}");
                            shared.record_action(warn_msg);
                        }
                        if !hit_track_end.load(Ordering::SeqCst)
                            && controller.current_signal().is_none()
                        {
                            let mut saw_rejected = false;
                            let mut applied_any = false;
                            while let Some(cmd) = shared.take_pending_command() {
                                if state.dispatch(cmd) == DispatchOutcome::Rejected {
                                    saw_rejected = true;
                                } else {
                                    applied_any = true;
                                    break;
                                }
                            }
                            if saw_rejected
                                && !applied_any
                                && !state.is_paused
                                && controller.current_signal().is_none()
                            {
                                stop_handle.store(false, Ordering::SeqCst);
                                // Re-check queue immediately after clearing stop_handle so a
                                // concurrent command cannot be stranded with stop_handle == false.
                                let mut applied_after = false;
                                while let Some(cmd) = shared.take_pending_command() {
                                    if state.dispatch(cmd) != DispatchOutcome::Rejected {
                                        applied_after = true;
                                        break;
                                    }
                                }
                                if !applied_after
                                    && !state.is_paused
                                    && controller.current_signal().is_none()
                                {
                                    stop_handle.store(false, Ordering::SeqCst);
                                    continue;
                                }
                            }
                        }
                        break;
                    }
                    Err(err @ OutputError::DeviceBusy { .. }) => {
                        let msg = format!("DAC busy ({err}) — staying paused");
                        eprintln!("  [busy] {msg}");
                        state.pause_with_message(msg);
                        break;
                    }
                    Err(err) => {
                        let msg = format!("Output error ({err}) — paused & DAC released");
                        eprintln!("  [error] {msg}");
                        state.pause_with_message(msg);
                        break;
                    }
                }
            }
        }
    }

    ConsumerStepResult::Continue
}

/// Ordered graceful teardown shared by `run_random_music_player` and teardown-order tests:
/// 1. Close stream buffer & signal producer `Shutdown` (unblocks producer backpressure & cancels HTTP).
/// 2. Shut down UPnP MediaRenderer (`ssdp:byebye`), HTTP server & remove runtime `server.url`.
/// 3. Drop the audio sink (releases WDM-KS DAC pin immediately).
/// 4. Join the producer thread with a bounded deadline (never hangs on stalled NAS / FFT).
#[allow(clippy::too_many_arguments)]
fn teardown_player_resources_with_upnp<S>(
    stream_buf: &BoundedBlockStreamBuffer,
    prod_ctrl: &ProducerControl,
    active_epoch: u64,
    mut http_server: ControlServer,
    mut upnp_server: Option<UpnpRendererServer>,
    url_file_path: Option<&Path>,
    http_url: &str,
    sink: S,
    producer_thread: thread::JoinHandle<()>,
    controller: &ShutdownController,
    normal_timeout: Duration,
    fast_timeout: Duration,
) -> bool {
    stream_buf.close();
    prod_ctrl.set(active_epoch.wrapping_add(1), ProducerDirective::Shutdown);
    if let Some(ref mut upnp) = upnp_server {
        upnp.shutdown();
    }
    http_server.shutdown();
    if let Some(p) = url_file_path {
        clear_runtime_url_file_at(p, http_url);
    }
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

/// Background streaming DSP Producer: reads bounded blocks from `FlacBlockReader` or
/// `HttpAudioSource` (UPnP), processes them incrementally through `StreamingPipelineSession`,
/// and feeds `BoundedBlockStreamBuffer` with automatic backpressure (strictly bounded RAM).
#[cfg(test)]
fn producer_loop(
    pipeline: AudioPipeline,
    ctrl: Arc<ProducerControl>,
    buf: Arc<BoundedBlockStreamBuffer>,
    shared: Arc<SharedControl>,
) {
    producer_loop_with_upnp(pipeline, ctrl, buf, shared, None, None);
}

fn finalize_prev_track(
    pipeline: &AudioPipeline,
    buf: &BoundedBlockStreamBuffer,
    epoch: u64,
    carried: &mut Option<GaplessCarryover>,
    pending_end: &mut bool,
) {
    if let Some(tail) = carried.take() {
        if let Ok((pcm, drain_stats)) = pipeline.drain_gapless_carryover_with_stats(tail) {
            if drain_stats.overs_count > 0 {
                eprintln!(
                    "  [clip warn] drained gapless tail: {} pre-quantizer sample overs (max {:.2} dBFS)",
                    drain_stats.overs_count,
                    drain_stats.max_pre_quant_peak_dbfs()
                );
            }
            if !pcm.is_empty() {
                let _ = buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm));
            }
        }
    }
    if *pending_end {
        *pending_end = false;
        let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
    }
}

#[cfg(test)]
fn producer_loop_with_upnp(
    pipeline: AudioPipeline,
    ctrl: Arc<ProducerControl>,
    buf: Arc<BoundedBlockStreamBuffer>,
    shared: Arc<SharedControl>,
    upnp_state: Option<Arc<UpnpRendererState>>,
    ultimate_ctx: Option<UltimateRuntimeContext>,
) {
    producer_loop_with_sources(pipeline, ctrl, buf, shared, upnp_state, None, ultimate_ctx);
}

fn producer_loop_with_sources(
    pipeline: AudioPipeline,
    ctrl: Arc<ProducerControl>,
    buf: Arc<BoundedBlockStreamBuffer>,
    shared: Arc<SharedControl>,
    upnp_state: Option<Arc<UpnpRendererState>>,
    capture_transport: Option<SharedCaptureTransport>,
    ultimate_ctx: Option<UltimateRuntimeContext>,
) {
    const MAX_CONSECUTIVE_UNSUPPORTED: u32 = 50;

    let mut seq_cursor: Option<(u64, SequentialFlacCursor)> = None;
    let mut single_consumed_epoch: Option<u64> = None;
    let mut upnp_consumed_epoch: Option<u64> = None;
    let mut capture_consumed_epoch: Option<u64> = None;
    let mut last_random: Option<PathBuf> = None;
    let mut last_seen_epoch: Option<u64> = None;
    let mut consecutive_errors: u32 = 0;
    let mut consecutive_unsupported: u32 = 0;
    let mut carried_tail: Option<GaplessCarryover> = None;
    let mut pending_track_end = false;

    while let Some((epoch, directive, cancel_flag)) = ctrl.wait_active_with_cancel() {
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

        // Handle live exclusive kernel stream capture (e.g. vanilla Tidal Desktop Exclusive Mode).
        // Never uses the 30 s W0 look-ahead window (real-time 1x push stream); feeds directly
        // through the pure-f64 5-stage AudioPipeline with -3 dB unknown-peak headroom, rate-following
        // Gaussian upsampling (352.8/384 kHz), full-tail REW convolution, and WDM-KS output.
        //
        // Review #8 B1/B2/B4/B5/B6: follows stream generations seamlessly inside ONE epoch (zero
        // buffer flush on rate change/track boundary; gapless on same-rate switches, full REW tail
        // drain on rate switches), throttles kernel draining against engine cushion fill, and
        // reports any kernel ring overrun deltas.
        if let PlayDirective::StreamCapture { request } = directive {
            if capture_consumed_epoch == Some(epoch) {
                finalize_prev_track(
                    &pipeline,
                    &buf,
                    epoch,
                    &mut carried_tail,
                    &mut pending_track_end,
                );
                let _ = buf.push_for_epoch(
                    epoch,
                    StreamEvent::EndOfQueue("Exclusive stream paused — DAC released".into()),
                );
                ctrl.pause_if_epoch(epoch);
                continue;
            }
            let Some(ref transport) = capture_transport else {
                let msg = "Exclusive capture transport not available".to_string();
                let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                ctrl.pause_if_epoch(epoch);
                continue;
            };

            struct ActiveGenGuard {
                transport: SharedCaptureTransport,
            }
            impl Drop for ActiveGenGuard {
                fn drop(&mut self) {
                    self.transport.set_active_generation(0);
                }
            }
            let _active_guard = ActiveGenGuard {
                transport: transport.clone(),
            };

            let mut cur_gen = request.generation;
            let mut cur_caller = request.caller_name.clone();
            let mut aborted = false;

            loop {
                if ctrl.epoch() != epoch || buf.current_epoch() != epoch || cancel_flag.load(Ordering::Acquire) {
                    aborted = true;
                    break;
                }

                // Wait briefly for `cur_gen` to report verified status if transitioning mid-stream.
                let st = match transport.get_status() {
                    Ok(s) => s,
                    Err(err) => {
                        let msg = format!("Exclusive stream status error: {err}");
                        transport.mark_dispatch_failed(cur_gen);
                        finalize_prev_track(&pipeline, &buf, epoch, &mut carried_tail, &mut pending_track_end);
                        eprintln!("  [capture error] {msg}");
                        shared.record_action(msg.clone());
                        let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                        ctrl.pause_if_epoch(epoch);
                        aborted = true;
                        break;
                    }
                };

                // Review #8 B5: if generation already advanced while dispatching, follow it cleanly!
                if st.stream_generation != cur_gen {
                    if st.stream_running != 0 && verify_status_exclusive(&st).is_ok() {
                        cur_gen = st.stream_generation;
                    } else {
                        // Stream already paused before open -> benign end (no FatalError).
                        break;
                    }
                }

                let valid_bits = if st.format.valid_bits_per_sample > 0 {
                    st.format.valid_bits_per_sample
                } else {
                    st.format.bits_per_sample
                };
                let caller_str = if st.run_pid != 0 {
                    ap_stream_protocol::proc_name_str(&st.run_proc).to_string()
                } else if !cur_caller.is_empty() {
                    cur_caller.clone()
                } else {
                    "Live Exclusive Stream".to_string()
                };
                cur_caller = caller_str.clone();
                let label = format!(
                    "{caller_str} (Exclusive {} Hz {}-in-{}b)",
                    st.format.sample_rate, valid_bits, st.format.bits_per_sample
                );

                transport.set_active_generation(cur_gen);

                let buf_fill_probe = Arc::clone(&buf);
                let fill_fn: Arc<dyn Fn() -> Duration + Send + Sync> = Arc::new(move || {
                    Duration::from_secs_f64(buf_fill_probe.buffered_duration_seconds().max(0.0))
                });
                let shared_ov = Arc::clone(&shared);
                let ov_fn: Arc<dyn Fn(u64) + Send + Sync> = Arc::new(move |delta| {
                    let msg = format!(
                        "Kernel capture ring overrun x{delta} — audio dropped (check CPU/DSP load)"
                    );
                    eprintln!("  [capture warn] {msg}");
                    shared_ov.record_action(msg);
                });

                let mut panic_backup_carry: Option<GaplessCarryover> = None;
                let (outcome, next_carry, next_pend, end_reason) =
                    match catch_unwind(AssertUnwindSafe(|| {
                        match StreamCaptureSource::open_generation_with_hooks(
                            transport.clone(),
                            cur_gen,
                            Arc::clone(&cancel_flag),
                            Some(fill_fn),
                            Some(ov_fn),
                        ) {
                            Ok(mut src) => {
                                let (out, c, p) = stream_open_audio_source(
                                    &pipeline,
                                    &ctrl,
                                    &buf,
                                    &shared,
                                    epoch,
                                    &mut src,
                                    &PathBuf::from(&label),
                                    None,
                                    None,
                                    PlayModeLabel::StreamCapture,
                                    None,
                                    None,
                                    &mut carried_tail,
                                    &mut panic_backup_carry,
                                    &mut pending_track_end,
                                    true,
                                );
                                (out, c, p, src.end_reason())
                            }
                            Err(err) => (
                                TrackStreamOutcome::Failed(format!(
                                    "Exclusive stream open failed for '{label}': {err}"
                                )),
                                None,
                                false,
                                CaptureEndReason::Error,
                            ),
                        }
                    })) {
                        Ok(res) => res,
                        Err(_) => {
                            if carried_tail.is_none() {
                                carried_tail = panic_backup_carry.take();
                            }
                            finalize_prev_track(
                                &pipeline,
                                &buf,
                                epoch,
                                &mut carried_tail,
                                &mut pending_track_end,
                            );
                            (
                                TrackStreamOutcome::Failed(format!(
                                    "Internal panic while streaming exclusive capture '{label}'"
                                )),
                                None,
                                false,
                                CaptureEndReason::Error,
                            )
                        }
                    };

                match outcome {
                    TrackStreamOutcome::Played => {
                        carried_tail = next_carry;
                        pending_track_end = next_pend;
                        if let CaptureEndReason::GenerationChanged(next_gen) = end_reason {
                            // Review #8 B1: wait briefly for next_gen to stabilize, then follow
                            // seamlessly inside the SAME epoch without flushing previous tail!
                            let mut ready = false;
                            let deadline = Instant::now() + Duration::from_millis(500);
                            while Instant::now() < deadline
                                && ctrl.epoch() == epoch
                                && !cancel_flag.load(Ordering::Acquire)
                            {
                                if let Ok(nst) = transport.get_status() {
                                    if nst.stream_running != 0
                                        && verify_status_exclusive(&nst).is_ok()
                                    {
                                        cur_gen = nst.stream_generation;
                                        ready = true;
                                        break;
                                    }
                                }
                                thread::sleep(Duration::from_millis(10));
                            }
                            if ready {
                                continue;
                            }
                            // Next gen didn't start running -> finish cleanly.
                            let _ = next_gen;
                        }
                        break;
                    }
                    TrackStreamOutcome::Aborted => {
                        carried_tail = None;
                        pending_track_end = false;
                        aborted = true;
                        break;
                    }
                    TrackStreamOutcome::Unsupported(msg) | TrackStreamOutcome::Failed(msg) => {
                        carried_tail = next_carry;
                        pending_track_end = next_pend;
                        // Review #8 B5: benign 0-frame or generation-advanced race -> check if a
                        // newer generation is running and follow it instead of emitting FatalError!
                        if let CaptureEndReason::GenerationChanged(next_gen) = end_reason {
                            cur_gen = next_gen;
                            continue;
                        }
                        if msg.contains("ended with 0 audio frames")
                            || msg.contains("generation already advanced")
                        {
                            if let Ok(nst) = transport.get_status() {
                                if nst.stream_running != 0
                                    && nst.stream_generation != cur_gen
                                    && verify_status_exclusive(&nst).is_ok()
                                {
                                    cur_gen = nst.stream_generation;
                                    continue;
                                }
                            }
                            break;
                        }
                        transport.mark_dispatch_failed(cur_gen);
                        finalize_prev_track(
                            &pipeline,
                            &buf,
                            epoch,
                            &mut carried_tail,
                            &mut pending_track_end,
                        );
                        eprintln!("  [capture error] {msg}");
                        shared.record_action(msg.clone());
                        let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                        ctrl.pause_if_epoch(epoch);
                        aborted = true;
                        break;
                    }
                }
            }

            if !aborted {
                capture_consumed_epoch = Some(epoch);
                finalize_prev_track(
                    &pipeline,
                    &buf,
                    epoch,
                    &mut carried_tail,
                    &mut pending_track_end,
                );
                let _ = buf.push_for_epoch(
                    epoch,
                    StreamEvent::EndOfQueue("Exclusive stream paused — DAC released".into()),
                );
                ctrl.pause_if_epoch(epoch);
            }
            continue;
        }

        // Handle UPnP MediaRenderer streaming queue (including chained Ogg FLAC & gapless
        // `SetNextAVTransportURI` handoff).
        if let PlayDirective::UpnpStream { request } = directive {
            if upnp_consumed_epoch == Some(epoch) {
                finalize_prev_track(
                    &pipeline,
                    &buf,
                    epoch,
                    &mut carried_tail,
                    &mut pending_track_end,
                );
                let _ = buf.push_for_epoch(
                    epoch,
                    StreamEvent::EndOfQueue("Finished UPnP stream — DAC released".into()),
                );
                ctrl.pause_if_epoch(epoch);
                continue;
            }

            let mut cur_req = request;
            let mut last_prefetched_gen: Option<u64> = cur_req.prefetched_next_gen;
            let mut aborted = false;
            loop {
                if ctrl.epoch() != epoch || buf.current_epoch() != epoch {
                    aborted = true;
                    break;
                }
                let mut panic_backup_carry: Option<GaplessCarryover> = None;
                let (outcome, next_carry, next_pend) = match catch_unwind(AssertUnwindSafe(|| {
                    stream_upnp_request(
                        &pipeline,
                        &ctrl,
                        &buf,
                        &shared,
                        epoch,
                        &cur_req,
                        &cancel_flag,
                        &mut carried_tail,
                        &mut panic_backup_carry,
                        &mut pending_track_end,
                        ultimate_ctx.as_ref(),
                    )
                })) {
                    Ok(res) => res,
                    Err(_) => {
                        if carried_tail.is_none() {
                            carried_tail = panic_backup_carry.take();
                        }
                        finalize_prev_track(
                            &pipeline,
                            &buf,
                            epoch,
                            &mut carried_tail,
                            &mut pending_track_end,
                        );
                        (
                            TrackStreamOutcome::Failed(format!(
                                "Internal panic while streaming UPnP '{}'",
                                cur_req.uri
                            )),
                            None,
                            false,
                        )
                    }
                };

                match outcome {
                    TrackStreamOutcome::Played => {
                        carried_tail = next_carry;
                        pending_track_end = next_pend;
                        // Review #5 #5 & Review #6 #8: peek NextURI by generation token WITHOUT
                        // mutating CurrentURI or resetting RelTime; consumer promotes it strictly
                        // when TrackStart becomes audible on the DAC (and repeat-one prefetches!).
                        if let Some(next_req) = upnp_state.as_deref().and_then(|u| {
                            u.engine_peek_next_for_prefetch(last_prefetched_gen)
                        }) {
                            last_prefetched_gen = next_req.prefetched_next_gen;
                            cur_req = next_req;
                            continue;
                        }
                        break;
                    }
                    TrackStreamOutcome::Aborted => {
                        carried_tail = None;
                        pending_track_end = false;
                        aborted = true;
                        break;
                    }
                    TrackStreamOutcome::Unsupported(msg) | TrackStreamOutcome::Failed(msg) => {
                        carried_tail = next_carry;
                        pending_track_end = next_pend;
                        finalize_prev_track(
                            &pipeline,
                            &buf,
                            epoch,
                            &mut carried_tail,
                            &mut pending_track_end,
                        );
                        eprintln!("  [upnp error] {msg}");
                        shared.record_action(msg.clone());
                        let _ = buf.push_for_epoch(epoch, StreamEvent::FatalError(msg));
                        ctrl.pause_if_epoch(epoch);
                        aborted = true;
                        break;
                    }
                }
            }

            if !aborted {
                upnp_consumed_epoch = Some(epoch);
                finalize_prev_track(
                    &pipeline,
                    &buf,
                    epoch,
                    &mut carried_tail,
                    &mut pending_track_end,
                );
                let _ = buf.push_for_epoch(
                    epoch,
                    StreamEvent::EndOfQueue("Finished UPnP stream — DAC released".into()),
                );
                ctrl.pause_if_epoch(epoch);
            }
            continue;
        }

        let is_single_file = matches!(directive, PlayDirective::SingleFile { .. });

        let (chosen, mode_label) = match directive {
            PlayDirective::UpnpStream { .. } | PlayDirective::StreamCapture { .. } => {
                unreachable!()
            }
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

        let mut panic_backup_carry: Option<GaplessCarryover> = None;
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
                &mut panic_backup_carry,
                &mut pending_track_end,
                !is_single_file,
                ultimate_ctx.as_ref(),
            )
        })) {
            Ok(res) => res,
            Err(_) => {
                // Review #5 Finding #20: if Track B panicked after attaching A's carryover but
                // before emitting any PCM, restore the backup clone so A's full REW tail drains!
                if carried_tail.is_none() {
                    carried_tail = panic_backup_carry.take();
                }
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
    panic_backup_carry: &mut Option<GaplessCarryover>,
    had_pending_prev_end: &mut bool,
    gapless_carryover: bool,
    ultimate_ctx: Option<&UltimateRuntimeContext>,
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

    if let Some(ult) = ultimate_ctx {
        let out_rate_hint = if spec.sample_rate_hz % 44_100 == 0 {
            352_800
        } else {
            384_000
        };
        let src_block_frames = source_block_frames_for_rates(spec.sample_rate_hz, out_rate_hint);
        return match stream_track_ultimate(
            pipeline,
            ult,
            buf,
            epoch,
            || ctrl.epoch() != epoch,
            &mut source,
            chosen,
            Some(chosen),
            None,
            None,
            mode_label,
            total_duration_s,
            incoming_carryover,
            panic_backup_carry,
            had_pending_prev_end,
            gapless_carryover,
            src_block_frames,
        ) {
            UltimateStreamResult::Played {
                next_carryover,
                next_pending_end,
                warning,
                ..
            } => {
                if let Some(msg) = warning {
                    shared.record_action(msg);
                }
                (TrackStreamOutcome::Played, next_carryover, next_pending_end)
            }
            UltimateStreamResult::Aborted => (TrackStreamOutcome::Aborted, None, false),
            UltimateStreamResult::Failed(msg) => (TrackStreamOutcome::Failed(msg), None, false),
        };
    }

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

    let src_block_frames =
        source_block_frames_for_rates(spec.sample_rate_hz, session.report().output_rate_hz);

    // Review #3 Finding #3: decode and validate the first non-empty block BEFORE touching
    // `incoming_carryover` or closing `TrackEnd(prev)`. If this track is 0-frame or corrupt on
    // its first block, `incoming_carryover` is drained intact onto the previous track!
    let first_src = loop {
        if ctrl.epoch() != epoch {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        match source.read_next_block(src_block_frames) {
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

    // Commit the gapless carryover handoff now that Track B has a real validated first block,
    // while keeping `*had_pending_prev_end` open until Track B actually yields its first non-empty
    // PCM block (Review #4 Finding #2). If Track B fails after handoff but before producing any
    // PCM (e.g., short valid prefix < UPOLS B=65536 followed by decode error, or DSP block error),
    // we flush `session` directly onto Track A before closing `TrackEnd(prev)` so Track A's REW
    // reverb tail and upsampler history are NEVER dropped!
    let mut had_gapless_carryover = false;
    let mut attached_carryover_into_session = false;
    if let Some(carry) = incoming_carryover.take() {
        let had_active_tail = !carry.is_empty();
        // Review #5 Finding #20: keep backup clone until Track B pushes its first PCM block.
        *panic_backup_carry = Some(carry.clone());
        match pipeline.attach_gapless_carryover(&mut session, carry) {
            Ok(GaplessAttachOutcome::FullHandoff) => {
                had_gapless_carryover = had_active_tail;
                attached_carryover_into_session = had_active_tail;
            }
            Ok(GaplessAttachOutcome::ConvolverOnlyHandoff {
                upsampler_tail_pcm,
                attached_convolver_tail,
                upsampler_tail_stats,
            }) => {
                if upsampler_tail_stats.overs_count > 0 {
                    eprintln!(
                        "  [clip warn] previous track gapless tail: {} pre-quantizer sample overs (max {:.2} dBFS)",
                        upsampler_tail_stats.overs_count,
                        upsampler_tail_stats.max_pre_quant_peak_dbfs()
                    );
                }
                had_gapless_carryover = attached_convolver_tail;
                attached_carryover_into_session = attached_convolver_tail;
                if !upsampler_tail_pcm.is_empty()
                    && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(upsampler_tail_pcm))
                {
                    *panic_backup_carry = None;
                    *had_pending_prev_end = false;
                    return (TrackStreamOutcome::Aborted, None, false);
                }
            }
            Ok(GaplessAttachOutcome::IncompatibleRate(rejected)) => {
                *panic_backup_carry = None;
                *incoming_carryover = Some(rejected);
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
            }
            Err(err) => {
                // Review #5 Finding #20: drain A's backup tail cleanly and still play Track B!
                eprintln!(
                    "  [gapless warn] carryover attach failed on '{}': {err}; draining previous tail and continuing cleanly",
                    chosen.display()
                );
                *incoming_carryover = panic_backup_carry.take();
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
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
        upnp_uri: None,
        upnp_next_gen: None,
        mode_label,
        report: session.report().clone(),
        total_duration_s,
        had_gapless_carryover,
    });
    let mut pushed_any_pcm = false;
    let mut source_frames_read: usize = 0;
    let mut first_block_error: Option<String> = None;
    let mut pending_first_block = Some(first_src);

    // Helper closing TrackEnd(prev) right before TrackStart(B) + PcmBlock(B).
    let emit_track_b_pcm = |buf: &BoundedBlockStreamBuffer,
                            pending_prev_end: &mut bool,
                            pending_start: &mut Option<StreamEvent>,
                            pcm: ap_core::buffer::InterleavedInt32Buffer|
     -> bool {
        if *pending_prev_end {
            *pending_prev_end = false;
            if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                return false;
            }
        }
        if let Some(start_ev) = pending_start.take() {
            if !buf.push_for_epoch(epoch, start_ev) {
                return false;
            }
        }
        buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm))
    };

    // Helper draining an already-attached carryover from `session` onto Track A when Track B
    // errors before emitting any PCM (closes TrackEnd(prev) without opening TrackStart(B)).
    let drain_attached_tail_onto_prev = |session: &mut ap_dsp::pipeline::StreamingPipelineSession,
                                         buf: &BoundedBlockStreamBuffer,
                                         pending_prev_end: &mut bool,
                                         attached: bool|
     -> bool {
        if attached {
            if let Ok(tail_pcm) = session.flush() {
                if !tail_pcm.is_empty() && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(tail_pcm))
                {
                    *pending_prev_end = false;
                    return false;
                }
            }
        }
        if *pending_prev_end {
            *pending_prev_end = false;
            if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                return false;
            }
        }
        true
    };

    loop {
        if ctrl.epoch() != epoch {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        let next_src = if let Some(b) = pending_first_block.take() {
            b
        } else {
            match source.read_next_block(src_block_frames) {
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
                    if !drain_attached_tail_onto_prev(
                        &mut session,
                        buf,
                        had_pending_prev_end,
                        attached_carryover_into_session,
                    ) {
                        return (TrackStreamOutcome::Aborted, None, false);
                    }
                    return (TrackStreamOutcome::Failed(msg), None, false);
                }
                eprintln!("  [dsp] {msg}");
                shared.record_action(msg);
                let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
                return (TrackStreamOutcome::Played, None, false);
            }
        };

        if !pcm_block.is_empty() {
            if !emit_track_b_pcm(buf, had_pending_prev_end, &mut pending_start, pcm_block) {
                return (TrackStreamOutcome::Aborted, None, false);
            }
            pushed_any_pcm = true;
            *panic_backup_carry = None;
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
                if !emit_track_b_pcm(buf, had_pending_prev_end, &mut pending_start, tail_pcm) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                pushed_any_pcm = true;
            }
        }
    }

    if !pushed_any_pcm {
        // Ultra-short track (< upsampler half-window) where all samples landed cleanly in carryover:
        // flush immediately so it still plays as its own track rather than being dropped.
        if first_block_error.is_none() && source_frames_read > 0 {
            if let Some(carry) = next_carryover.take() {
                if let Ok(pcm) = pipeline.drain_gapless_carryover(carry) {
                    if !pcm.is_empty() {
                        if !emit_track_b_pcm(buf, had_pending_prev_end, &mut pending_start, pcm) {
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
        // Track B errored (or produced zero output) before emitting any PCM: drain any already-
        // attached REW tail & upsampler history onto Track A and close TrackEnd(prev) cleanly!
        if !drain_attached_tail_onto_prev(
            &mut session,
            buf,
            had_pending_prev_end,
            attached_carryover_into_session,
        ) {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        return (
            TrackStreamOutcome::Failed(first_block_error.unwrap_or_else(|| {
                format!("Skipped empty (0-frame) FLAC '{}'", chosen.display())
            })),
            None,
            false,
        );
    }

    let qstats = session.quantizer_stats();
    if qstats.overs_count > 0 {
        let warn_msg = format!(
            "Quantizer clipped {} sample(s) on '{}' (pre-quant peak {:+.2} dBFS)",
            qstats.overs_count,
            chosen.display(),
            qstats.max_pre_quant_peak_dbfs()
        );
        eprintln!("  [clip warn] {warn_msg}");
        shared.record_action(warn_msg);
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

/// Open and stream one UPnP HTTP audio request (including any chained Ogg FLAC segments
/// with sample-rate switches) through the pure-`f64` `AudioPipeline`.
#[allow(clippy::too_many_arguments)]
fn stream_upnp_request(
    pipeline: &AudioPipeline,
    ctrl: &ProducerControl,
    buf: &BoundedBlockStreamBuffer,
    shared: &SharedControl,
    epoch: u64,
    req: &UpnpPlayRequest,
    cancel: &Arc<AtomicBool>,
    incoming_carryover: &mut Option<GaplessCarryover>,
    panic_backup_carry: &mut Option<GaplessCarryover>,
    had_pending_prev_end: &mut bool,
    ultimate_ctx: Option<&UltimateRuntimeContext>,
) -> (TrackStreamOutcome, Option<GaplessCarryover>, bool) {
    let hints = HttpStreamHints {
        protocol_info: req.protocol_info.clone(),
        sample_rate_hz: req.sample_rate_hz,
        channels: req.channels,
        bits_per_sample: req.bits_per_sample,
    };

    let mut cur_source = match HttpAudioSource::open_at_offset(
        &req.uri,
        &hints,
        req.seek_offset_s,
        Arc::clone(cancel),
    ) {
        Ok(s) => s,
        Err(err) => {
            if ctrl.epoch() != epoch || cancel.load(Ordering::Acquire) {
                return (TrackStreamOutcome::Aborted, None, false);
            }
            return (
                TrackStreamOutcome::Failed(format!(
                    "UPnP stream open failed for '{}': {err}",
                    req.uri
                )),
                incoming_carryover.take(),
                *had_pending_prev_end,
            );
        }
    };

    let mut seg_idx = 0usize;
    loop {
        if ctrl.epoch() != epoch || cancel.load(Ordering::Acquire) {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        let seg_duration = cur_source
            .total_frames()
            .map(|f| f as f64 / cur_source.spec().sample_rate_hz as f64)
            .or(if seg_idx == 0 { req.duration_s } else { None });
        let seg_label = if seg_idx == 0 {
            format!(
                "{} [UPnP {}]",
                req.display_name,
                cur_source.container_format()
            )
        } else {
            format!(
                "{} [UPnP {} seg #{}]",
                req.display_name,
                cur_source.container_format(),
                seg_idx + 1
            )
        };
        let (outcome, next_carry, next_pend) = if let Some(ult) = ultimate_ctx {
            let sp = cur_source.spec();
            let out_rate_hint = if sp.sample_rate_hz % 44_100 == 0 {
                352_800
            } else {
                384_000
            };
            let src_block_frames = source_block_frames_for_rates(sp.sample_rate_hz, out_rate_hint);
            match stream_track_ultimate(
                pipeline,
                ult,
                buf,
                epoch,
                || ctrl.epoch() != epoch || cancel.load(Ordering::Acquire),
                &mut cur_source,
                &PathBuf::from(&seg_label),
                None,
                Some(req.uri.clone()),
                if seg_idx == 0 {
                    req.prefetched_next_gen
                } else {
                    None
                },
                PlayModeLabel::UpnpRenderer,
                seg_duration,
                incoming_carryover,
                panic_backup_carry,
                had_pending_prev_end,
                true,
                src_block_frames,
            ) {
                UltimateStreamResult::Played {
                    next_carryover,
                    next_pending_end,
                    warning,
                    ..
                } => {
                    if let Some(msg) = warning {
                        shared.record_action(msg);
                    }
                    (TrackStreamOutcome::Played, next_carryover, next_pending_end)
                }
                UltimateStreamResult::Aborted => (TrackStreamOutcome::Aborted, None, false),
                UltimateStreamResult::Failed(msg) => (TrackStreamOutcome::Failed(msg), None, false),
            }
        } else {
            stream_open_audio_source(
                pipeline,
                ctrl,
                buf,
                shared,
                epoch,
                &mut cur_source,
                &PathBuf::from(&seg_label),
                Some(&req.uri),
                if seg_idx == 0 {
                    req.prefetched_next_gen
                } else {
                    None
                },
                PlayModeLabel::UpnpRenderer,
                seg_duration,
                None,
                incoming_carryover,
                panic_backup_carry,
                had_pending_prev_end,
                true,
            )
        };
        match outcome {
            TrackStreamOutcome::Played => {
                *incoming_carryover = next_carry;
                *had_pending_prev_end = next_pend;
                if let Some(next_seg) = cur_source.take_next_chained_segment() {
                    cur_source = next_seg;
                    seg_idx += 1;
                    continue;
                }
                return (
                    TrackStreamOutcome::Played,
                    incoming_carryover.take(),
                    std::mem::take(had_pending_prev_end),
                );
            }
            other => {
                return (other, next_carry, next_pend);
            }
        }
    }
}

/// Generic streaming helper for an already-opened [`AudioBlockSource`] (used by UPnP HTTP
/// streams and chained Ogg FLAC segments). Preserves full `GaplessCarryover` /
/// `ConvolverOnlyHandoff` semantics identical to [`stream_single_track`].
#[allow(clippy::too_many_arguments)]
fn stream_open_audio_source<S: AudioBlockSource>(
    pipeline: &AudioPipeline,
    ctrl: &ProducerControl,
    buf: &BoundedBlockStreamBuffer,
    shared: &SharedControl,
    epoch: u64,
    source: &mut S,
    label_path: &Path,
    upnp_uri: Option<&str>,
    upnp_next_gen: Option<u64>,
    mode_label: PlayModeLabel,
    total_duration_s: Option<f64>,
    block_frames_override: Option<usize>,
    incoming_carryover: &mut Option<GaplessCarryover>,
    panic_backup_carry: &mut Option<GaplessCarryover>,
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

    let spec = source.spec();
    let peak_meta = source.peak_meta();
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
                        "Unsupported sample rate on '{}': {err}",
                        label_path.display()
                    )),
                    None,
                    false,
                );
            }
            return (
                TrackStreamOutcome::Failed(format!(
                    "DSP pipeline error on '{}': {err}",
                    label_path.display()
                )),
                None,
                false,
            );
        }
    };

    let block_frames = block_frames_override.unwrap_or_else(|| {
        source_block_frames_for_rates(spec.sample_rate_hz, session.report().output_rate_hz)
    });

    let first_src = loop {
        if ctrl.epoch() != epoch {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        match source.read_next_block(block_frames) {
            Ok(Some(b)) if b.is_empty() => continue,
            Ok(Some(b)) => break b,
            Ok(None) => {
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                return (
                    TrackStreamOutcome::Failed(format!(
                        "Stream '{}' ended with 0 audio frames",
                        label_path.display()
                    )),
                    None,
                    false,
                );
            }
            Err(err) => {
                if ctrl.epoch() != epoch {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                return (
                    TrackStreamOutcome::Failed(format!(
                        "Stream decode error in '{}': {err}",
                        label_path.display()
                    )),
                    None,
                    false,
                );
            }
        }
    };

    let mut had_gapless_carryover = false;
    let mut attached_carryover_into_session = false;
    if let Some(carry) = incoming_carryover.take() {
        let had_active_tail = !carry.is_empty();
        *panic_backup_carry = Some(carry.clone());
        match pipeline.attach_gapless_carryover(&mut session, carry) {
            Ok(GaplessAttachOutcome::FullHandoff) => {
                had_gapless_carryover = had_active_tail;
                attached_carryover_into_session = had_active_tail;
            }
            Ok(GaplessAttachOutcome::ConvolverOnlyHandoff {
                upsampler_tail_pcm,
                attached_convolver_tail,
                upsampler_tail_stats,
            }) => {
                if upsampler_tail_stats.overs_count > 0 {
                    eprintln!(
                        "  [clip warn] previous track gapless tail: {} pre-quantizer sample overs (max {:.2} dBFS)",
                        upsampler_tail_stats.overs_count,
                        upsampler_tail_stats.max_pre_quant_peak_dbfs()
                    );
                }
                had_gapless_carryover = attached_convolver_tail;
                attached_carryover_into_session = attached_convolver_tail;
                if !upsampler_tail_pcm.is_empty()
                    && !buf.push_for_epoch(epoch, StreamEvent::PcmBlock(upsampler_tail_pcm))
                {
                    *panic_backup_carry = None;
                    *had_pending_prev_end = false;
                    return (TrackStreamOutcome::Aborted, None, false);
                }
            }
            Ok(GaplessAttachOutcome::IncompatibleRate(rejected)) => {
                *panic_backup_carry = None;
                *incoming_carryover = Some(rejected);
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
            }
            Err(err) => {
                eprintln!(
                    "  [gapless warn] carryover attach failed on '{}': {err}; draining previous tail and continuing cleanly",
                    label_path.display()
                );
                *incoming_carryover = panic_backup_carry.take();
                if !flush_prev_on_incompatible(buf, incoming_carryover, had_pending_prev_end) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
            }
        }
    } else if *had_pending_prev_end {
        *had_pending_prev_end = false;
        if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
            return (TrackStreamOutcome::Aborted, None, false);
        }
    }

    let mut pending_start = Some(StreamEvent::TrackStart {
        path: label_path.to_path_buf(),
        upnp_uri: upnp_uri.map(str::to_string),
        upnp_next_gen,
        mode_label,
        report: session.report().clone(),
        total_duration_s,
        had_gapless_carryover,
    });
    let mut pushed_any_pcm = false;
    let mut source_frames_read: usize = 0;
    let mut first_block_error: Option<String> = None;
    let mut pending_first_block = Some(first_src);

    let emit_pcm = |buf: &BoundedBlockStreamBuffer,
                    pending_prev_end: &mut bool,
                    pending_start: &mut Option<StreamEvent>,
                    pcm: ap_core::buffer::InterleavedInt32Buffer|
     -> bool {
        if *pending_prev_end {
            *pending_prev_end = false;
            if !buf.push_for_epoch(epoch, StreamEvent::TrackEnd) {
                return false;
            }
        }
        if let Some(start_ev) = pending_start.take() {
            if !buf.push_for_epoch(epoch, start_ev) {
                return false;
            }
        }
        buf.push_for_epoch(epoch, StreamEvent::PcmBlock(pcm))
    };

    loop {
        if ctrl.epoch() != epoch {
            return (TrackStreamOutcome::Aborted, None, false);
        }
        let next_src = if let Some(b) = pending_first_block.take() {
            b
        } else {
            match source.read_next_block(block_frames) {
                Ok(Some(b)) => b,
                Ok(None) => break,
                Err(err) => {
                    if ctrl.epoch() != epoch {
                        return (TrackStreamOutcome::Aborted, None, false);
                    }
                    let msg = format!("Stream error in '{}': {err}", label_path.display());
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
                let msg = format!("DSP block error on '{}': {err}", label_path.display());
                if !pushed_any_pcm {
                    if attached_carryover_into_session {
                        if let Ok(tail_pcm) = session.flush() {
                            if !tail_pcm.is_empty() {
                                let _ = buf.push_for_epoch(epoch, StreamEvent::PcmBlock(tail_pcm));
                            }
                        }
                    }
                    if *had_pending_prev_end {
                        *had_pending_prev_end = false;
                        let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
                    }
                    return (TrackStreamOutcome::Failed(msg), None, false);
                }
                eprintln!("  [dsp] {msg}");
                shared.record_action(msg);
                let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
                return (TrackStreamOutcome::Played, None, false);
            }
        };

        if !pcm_block.is_empty() {
            if !emit_pcm(buf, had_pending_prev_end, &mut pending_start, pcm_block) {
                return (TrackStreamOutcome::Aborted, None, false);
            }
            pushed_any_pcm = true;
            *panic_backup_carry = None;
        }
    }

    if ctrl.epoch() != epoch {
        return (TrackStreamOutcome::Aborted, None, false);
    }

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
                if !emit_pcm(buf, had_pending_prev_end, &mut pending_start, tail_pcm) {
                    return (TrackStreamOutcome::Aborted, None, false);
                }
                pushed_any_pcm = true;
            }
        }
    }

    if !pushed_any_pcm {
        if first_block_error.is_none() && source_frames_read > 0 {
            if let Some(carry) = next_carryover.take() {
                if let Ok(pcm) = pipeline.drain_gapless_carryover(carry) {
                    if !pcm.is_empty() {
                        if !emit_pcm(buf, had_pending_prev_end, &mut pending_start, pcm) {
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
        if attached_carryover_into_session {
            if let Ok(tail_pcm) = session.flush() {
                if !tail_pcm.is_empty() {
                    let _ = buf.push_for_epoch(epoch, StreamEvent::PcmBlock(tail_pcm));
                }
            }
        }
        if *had_pending_prev_end {
            *had_pending_prev_end = false;
            let _ = buf.push_for_epoch(epoch, StreamEvent::TrackEnd);
        }
        return (
            TrackStreamOutcome::Failed(first_block_error.unwrap_or_else(|| {
                format!("Stream '{}' produced 0 PCM frames", label_path.display())
            })),
            None,
            false,
        );
    }

    let qstats = session.quantizer_stats();
    if qstats.overs_count > 0 {
        let warn_msg = format!(
            "Quantizer clipped {} sample(s) on '{}' (pre-quant peak {:+.2} dBFS)",
            qstats.overs_count,
            label_path.display(),
            qstats.max_pre_quant_peak_dbfs()
        );
        eprintln!("  [clip warn] {warn_msg}");
        shared.record_action(warn_msg);
    }

    if next_carryover.is_some() {
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
            let mut no_backup = None;
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
                &mut no_backup,
                &mut no_pending,
                false,
                None,
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

        // Review #6 #13: full zero-input FIR decay adds `m_half * ratio = 317 * 8 = 2536` samples
        // for `gauss-long-lp` at 8x when draining unconsumed carryover across rate changes / EOF.
        let expected_full = n_frames * 8 + 317 * 8 + (ir_taps - 1);
        assert!(saw_end_of_queue, "expected EndOfQueue after 2 sequential tracks");
        assert_eq!(
            track_counts,
            vec![
                (352_800, expected_full),
                (384_000, expected_full),
            ],
            "both rate-change track and final end-of-queue track must preserve full N*8 + FIR decay + flush_tail"
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

        let expected_full = n_frames * 8 + 317 * 8 + (ir_taps - 1);
        assert!(saw_end_of_queue);
        assert_eq!(
            track_counts,
            vec![
                (f01, expected_full, false),
                (f04, expected_full, false),
            ],
            "Track 1 MUST keep its full N*8 + FIR decay + REW flush_tail when same-rate Track 2 fails on block 1"
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
                upnp_uri: None,
        upnp_next_gen: None,
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
                upnp_uri: None,
        upnp_next_gen: None,
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
                upnp_uri: None,
        upnp_next_gen: None,
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
                upnp_uri: None,
        upnp_next_gen: None,
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
        // Review #4 Finding #19 & Review #5 Finding #22: deterministic condvar-synchronized
        // teardown order test using an isolated temp server.url path (never clobbering the live
        // %LOCALAPPDATA%\audiophile-player\server.url) AND verifying UPnP teardown too.
        use std::net::{Ipv4Addr, SocketAddr, TcpStream};

        let dir = std::env::temp_dir().join(format!(
            "ap_cli_teardown_order_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let temp_url_file = dir.join("test_server.url");

        let shared = SharedControl::new(dir.clone(), "Mock DAC");
        let server = ControlServer::start(shared.clone(), None).unwrap();
        let upnp_srv = UpnpRendererServer::start(
            shared.clone(),
            UpnpRendererConfig {
                friendly_name: "Teardown Test Renderer".to_string(),
                port: None,
                bind_ip: Some(Ipv4Addr::LOCALHOST),
                enable_ssdp: false,
            },
        )
        .unwrap();
        let upnp_port = upnp_srv.port();
        let url = server.url();
        let port: u16 = url
            .rsplit(':')
            .next()
            .and_then(|p| p.trim_end_matches('/').parse().ok())
            .expect("valid port in ControlServer URL");
        write_runtime_url_file_at(&temp_url_file, &url);

        let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
        let sink_dropped_gate = Arc::new((Mutex::new(false), Condvar::new()));

        struct MockSink {
            log: Arc<Mutex<Vec<&'static str>>>,
            gate: Arc<(Mutex<bool>, Condvar)>,
            http_port: u16,
            upnp_port: u16,
            url_file: PathBuf,
            http_url: String,
        }
        impl Drop for MockSink {
            fn drop(&mut self) {
                // Verify HTTP and UPnP servers are ALREADY shut down before sink drops.
                let http_closed = TcpStream::connect_timeout(
                    &SocketAddr::from(([127, 0, 0, 1], self.http_port)),
                    Duration::from_millis(200),
                )
                .is_err();
                assert!(
                    http_closed,
                    "HTTP server must already be shut down before dropping the audio sink"
                );
                let upnp_closed = TcpStream::connect_timeout(
                    &SocketAddr::from(([127, 0, 0, 1], self.upnp_port)),
                    Duration::from_millis(200),
                )
                .is_err();
                assert!(
                    upnp_closed,
                    "UPnP server must already be shut down before dropping the audio sink"
                );
                // Verify isolated temp `server.url` was already cleared before sink drops.
                let still_ours = std::fs::read_to_string(&self.url_file)
                    .ok()
                    .is_some_and(|s| s.trim() == self.http_url);
                assert!(
                    !still_ours,
                    "server.url must already be cleared before dropping the audio sink"
                );
                self.log.lock().unwrap().push("http_and_url_closed");
                self.log.lock().unwrap().push("sink_dropped");
                let (lock, cv) = &*self.gate;
                *lock.lock().unwrap() = true;
                cv.notify_all();
            }
        }

        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.current_epoch();
        // Start paused so the mock producer blocks cleanly at 0% CPU inside `wait_active()`
        // until teardown publishes `Shutdown` (zero busy-spin).
        let prod_ctrl = ProducerControl::new(ep, ProducerDirective::Paused);
        let controller = ShutdownController::new();
        controller.trigger(ShutdownSignal::CtrlC);

        let log_prod = Arc::clone(&log);
        let gate_prod = Arc::clone(&sink_dropped_gate);
        let prod_ctrl_bg = Arc::clone(&prod_ctrl);
        let producer = thread::spawn(move || {
            // Blocks cleanly in Condvar::wait until teardown publishes Shutdown.
            assert!(prod_ctrl_bg.wait_active().is_none());
            // Wait deterministically until MockSink::drop has fired (proves teardown drops sink
            // BEFORE joining the producer thread, with zero sleep race).
            let (lock, cv) = &*gate_prod;
            let mut dropped = lock.lock().unwrap();
            while !*dropped {
                let (g, timeout) = cv.wait_timeout(dropped, Duration::from_secs(2)).unwrap();
                dropped = g;
                assert!(!timeout.timed_out(), "sink was NOT dropped before producer join!");
            }
            log_prod.lock().unwrap().push("producer_exited");
        });

        let joined = teardown_player_resources_with_upnp(
            &buf,
            &prod_ctrl,
            ep,
            server,
            Some(upnp_srv),
            Some(&temp_url_file),
            &url,
            MockSink {
                log: Arc::clone(&log),
                gate: Arc::clone(&sink_dropped_gate),
                http_port: port,
                upnp_port,
                url_file: temp_url_file.clone(),
                http_url: url.clone(),
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
            vec!["http_and_url_closed", "sink_dropped", "producer_exited"],
            "teardown must close HTTP + server.url, then drop sink, then join producer"
        );
    }

    #[test]
    fn producer_loop_preserves_rew_tail_when_next_track_has_short_valid_prefix_then_corrupts_under_64k_ir() {
        // Review #4 Finding #2 (MEDIUM):
        // With a 65,536-tap REW IR (active_taps > 32768 -> UPOLS block_size = 65536), Track A
        // (44.1 kHz) hands off its REW reverb tail to Track B (44.1 kHz) whose first FLAC frame
        // (4096 frames -> ~32,768 upsampled < 65,536 -> 0 PCM emitted!) is valid, followed by
        // mid-stream corruption causing block 2 to error. Track A's full REW reverb tail MUST
        // still be flushed onto Track A before TrackEnd(A), and Track B MUST NOT open TrackStart!
        use flac_codec::encode::{FlacSampleWriter, Options};
        use std::fs::{self, File};
        use std::io::Write as _;

        let root = std::env::temp_dir().join(format!(
            "ap_cli_short_prefix_corrupt_64k_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let ir_dir = root.join("ir");
        let music_dir = root.join("music");
        fs::create_dir_all(&ir_dir).unwrap();
        fs::create_dir_all(&music_dir).unwrap();

        // 40,000-tap stereo 352.8 kHz IR (> 32,768 -> UPOLS block_size = 65,536!) with a strong
        // echo at tap 35,000 so Track A's REW reverb tail clearly extends ~35k samples past Track A.
        let ir_taps = 40_000_usize;
        let mut ir = vec![0.0_f32; ir_taps * 2];
        ir[0] = 0.5;
        ir[1] = 0.5;
        ir[35_000 * 2] = 0.35;
        ir[35_000 * 2 + 1] = -0.35;
        let data_bytes = (ir.len() * 4) as u32;
        {
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

        let write_flac = |path: &Path, frames: usize, amp: i32| {
            let mut w = FlacSampleWriter::create(
                path,
                Options::default(),
                44_100,
                16,
                2,
                Some((frames * 2) as u64),
            )
            .unwrap();
            let samples: Vec<i32> = (0..(frames * 2))
                .map(|i| if i % 2 == 0 { amp } else { -amp })
                .collect();
            w.write(&samples).unwrap();
            w.finalize().unwrap();
        };

        // 01_a.flac: valid 44.1 kHz track (10,000 frames = 80,000 upsampled > 65,536).
        let n_a = 10_000_usize;
        write_flac(&music_dir.join("01_a.flac"), n_a, 14_000);

        // 02_b_prefix_corrupt.flac: write 8192 frames (two 4096-frame FLAC blocks) then corrupt
        // the second half of the file so frame 1 (~4096 frames -> 32,768 upsampled < 65,536)
        // decodes cleanly and frame 2 errors!
        let b_path = music_dir.join("02_b_prefix_corrupt.flac");
        write_flac(&b_path, 8_192, 500);
        {
            let mut bytes = fs::read(&b_path).unwrap();
            let mid = (bytes.len() * 2) / 3;
            for b in &mut bytes[mid..] {
                *b ^= 0xA5;
            }
            fs::write(&b_path, &bytes).unwrap();
        }

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

        let mut started_paths: Vec<PathBuf> = Vec::new();
        let mut track_a_frames: Vec<[i32; 2]> = Vec::new();
        let mut saw_end_of_queue = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match buf.pop_for_epoch_timeout(ep, Duration::from_millis(50)) {
                Some(StreamEvent::TrackStart { path, .. }) => {
                    started_paths.push(path);
                    loop {
                        match buf.try_pop_track_pcm_block(ep) {
                            PopBlockResult::Block(b) => {
                                if started_paths.len() == 1 {
                                    track_a_frames.extend_from_slice(&b.frames);
                                }
                            }
                            PopBlockResult::Underrun => thread::sleep(Duration::from_millis(2)),
                            PopBlockResult::TrackEnd | PopBlockResult::StaleOrClosed => break,
                        }
                    }
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

        assert!(saw_end_of_queue, "expected EndOfQueue after corrupt Track B");
        assert_eq!(
            started_paths.len(),
            1,
            "corrupt pre-PCM Track B must NOT emit TrackStart: {started_paths:?}"
        );
        // Track A produced n_a * 8 = 80,000 main frames + ~35,000-tap echo tail -> MUST exceed
        // 80,000 + 30,000 frames and contain non-zero echo energy in the tail!
        assert!(
            track_a_frames.len() >= n_a * 8 + 30_000,
            "Track A REW tail was lost after Track B's short valid prefix errored! got {} frames",
            track_a_frames.len()
        );
        let echo_tail_energy: i64 = track_a_frames[(n_a * 8 + 10_000)..(n_a * 8 + 30_000)]
            .iter()
            .map(|f| i64::from(f[0]).abs())
            .sum();
        assert!(
            echo_tail_energy > 0,
            "Track A REW echo tail energy must be preserved across Track B post-handoff failure"
        );
    }

    #[test]
    fn convolver_only_handoff_without_rew_keeps_clean_carryover_flag_for_zero_ms_next() {
        // Review #4 Finding #14: with REW off (default), 44.1k -> 88.2k uses ConvolverOnlyHandoff
        // which drains all upsampler history onto A and attaches NOTHING to B. TrackStart(B) MUST
        // have `had_gapless_carryover == false` so mid-track Next on A reuses B without flushing!
        use flac_codec::encode::{FlacSampleWriter, Options};
        use std::fs;

        let root = std::env::temp_dir().join(format!(
            "ap_cli_conv_only_no_rew_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let write_flac = |path: &Path, rate: u32, frames: usize| {
            let mut w = FlacSampleWriter::create(path, Options::default(), rate, 16, 2, None)
                .unwrap();
            let samples = vec![4_000_i32; frames * 2];
            w.write(&samples).unwrap();
            w.finalize().unwrap();
        };
        write_flac(&root.join("01_44k.flac"), 44_100, 512);
        write_flac(&root.join("02_88k.flac"), 88_200, 512);

        let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
        let shared = SharedControl::new(root.clone(), "Mock DAC");
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep = buf.reset_and_flush();
        let ctrl = ProducerControl::new(
            ep,
            ProducerDirective::Active(PlayDirective::Sequential {
                root: root.clone(),
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

        let mut starts: Vec<(PathBuf, bool)> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && starts.len() < 2 {
            match buf.pop_for_epoch_timeout(ep, Duration::from_millis(50)) {
                Some(StreamEvent::TrackStart {
                    path,
                    had_gapless_carryover,
                    ..
                }) => {
                    starts.push((path, had_gapless_carryover));
                    loop {
                        match buf.try_pop_track_pcm_block(ep) {
                            PopBlockResult::Block(_) => {}
                            PopBlockResult::Underrun => thread::sleep(Duration::from_millis(2)),
                            PopBlockResult::TrackEnd | PopBlockResult::StaleOrClosed => break,
                        }
                    }
                }
                Some(StreamEvent::EndOfQueue(_)) => break,
                _ => {}
            }
        }

        buf.close();
        ctrl.set(ep + 1, ProducerDirective::Shutdown);
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&root);

        assert_eq!(starts.len(), 2);
        assert_eq!(
            starts[1].1, false,
            "44.1k->88.2k ConvolverOnlyHandoff with REW off must NOT set had_gapless_carryover=true on Track B"
        );
    }

    #[test]
    fn consumer_step_handles_prebuffer_requeue_and_resumes_interrupted_track_on_rejected_playfile() {
        // Review #4 Findings #15 & #20: exercise REAL `step_consumer_once` with a mock sink to
        // verify:
        // 1. Next arriving during TrackStart prebuffer after natural Track 1 EOF re-queues
        //    TrackStart(02) and plays 02 (no double-skip to 03).
        // 2. A rejected TOCTOU PlayFile interrupting an active track resumes playing that track's
        //    remaining PCM blocks instead of discarding them as orphans and jumping to Track 2!
        let root = std::env::temp_dir().join(format!(
            "ap_cli_consumer_step_test_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let f01 = root.join("01.flac");
        let f02 = root.join("02.flac");

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

        struct MockBlockSink {
            stop_flag: Arc<AtomicBool>,
            /// Optional command injected on first block pull to simulate mid-track interruption.
            inject_once: Option<(Arc<SharedControl>, ControlCommand)>,
            frames_consumed_per_call: Vec<usize>,
        }
        impl BlockStreamAudioSink for MockBlockSink {
            fn play_block_stream<F>(
                &mut self,
                sample_rate_hz: u32,
                wire_format: DacWireFormat,
                mut next_block: F,
            ) -> Result<PlaybackStats, OutputError>
            where
                F: FnMut() -> ap_output_win::wdm_ks::StreamBlock,
            {
                let mut frames = 0usize;
                loop {
                    if self.stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    match next_block() {
                        ap_output_win::wdm_ks::StreamBlock::Ready(b) => {
                            frames += b.frames.len();
                            if let Some((shared, cmd)) = self.inject_once.take() {
                                shared.submit_command(cmd);
                                break;
                            }
                        }
                        ap_output_win::wdm_ks::StreamBlock::Underrun => break,
                        ap_output_win::wdm_ks::StreamBlock::EndOfTrack => break,
                    }
                }
                self.frames_consumed_per_call.push(frames);
                Ok(PlaybackStats {
                    device_name: "Mock DAC".to_string(),
                    sample_rate_hz,
                    wire_format,
                    frames_played: frames,
                    wavert_buffer_bytes: 0,
                    notifications: 1,
                    mmcss_pro_audio: false,
                    underrun_halves: 0,
                })
            }
        }

        // Part 1 (Finding #15): Next arrives during TrackStart(02) prebuffer after natural Track 01 EOF.
        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let init = PlayDirective::Sequential {
            root: root.clone(),
            seq_gen: 1,
            start_after: None,
            folder_only: false,
            skip_count: 0,
        };
        let prod_ctrl = ProducerControl::new(ep0, ProducerDirective::Active(init.clone()));
        let shared = SharedControl::new(root.clone(), "Mock DAC");
        let stop_flag = Arc::new(AtomicBool::new(false));
        shared.attach_sink_stop_flag(Arc::clone(&stop_flag));
        let controller = ShutdownController::new();
        let mut state = EngineState::new(ep0, init, &buf, &prod_ctrl, &shared);
        state.record_anchor(f01.clone());
        state.just_skipped_track = false;

        // Push TrackStart(02) with only 64 frames (below 2 s prebuffer watermark, no TrackEnd yet)
        // so `step_consumer_once` enters the real prebuffer loop at lines 546-577.
        assert!(buf.push_for_epoch(
            ep0,
            StreamEvent::TrackStart {
                path: f02.clone(),
                upnp_uri: None,
        upnp_next_gen: None,
                mode_label: PlayModeLabel::Sequential,
                report: dummy_report.clone(),
                total_duration_s: Some(5.0),
                had_gapless_carryover: true,
            }
        ));
        assert!(buf.push_for_epoch(
            ep0,
            StreamEvent::PcmBlock(ap_core::buffer::InterleavedInt32Buffer {
                sample_rate_hz: 352_800,
                frames: vec![[1, -1]; 64],
            })
        ));
        let shared_bg = shared.clone();
        let injector = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            shared_bg.submit_command(ControlCommand::Next);
        });
        let mut mock_sink = MockBlockSink {
            stop_flag: Arc::clone(&stop_flag),
            inject_once: None,
            frames_consumed_per_call: Vec::new(),
        };
        assert_eq!(
            step_consumer_once(
                &mut state,
                &buf,
                &shared,
                &controller,
                &stop_flag,
                &mut mock_sink,
                || false,
            ),
            ConsumerStepResult::Continue
        );
        injector.join().unwrap();
        // TrackStart(02) must have been re-queued and retained (NOT double-skipped!).
        assert!(mock_sink.frames_consumed_per_call.is_empty());
        assert!(buf.push_for_epoch(ep0, StreamEvent::TrackEnd));
        assert_eq!(
            step_consumer_once(
                &mut state,
                &buf,
                &shared,
                &controller,
                &stop_flag,
                &mut mock_sink,
                || false,
            ),
            ConsumerStepResult::Continue
        );
        assert_eq!(mock_sink.frames_consumed_per_call, vec![64]);
        assert_eq!(state.last_started_track.as_ref(), Some(&f02));

        // Part 2 (Finding #20): Rejected TOCTOU PlayFile during active Track 01 resumes Track 01!
        let buf2 = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep2 = buf2.current_epoch();
        let init2 = PlayDirective::Sequential {
            root: root.clone(),
            seq_gen: 2,
            start_after: None,
            folder_only: false,
            skip_count: 0,
        };
        let prod_ctrl2 = ProducerControl::new(ep2, ProducerDirective::Active(init2.clone()));
        let shared2 = SharedControl::new(root.clone(), "Mock DAC");
        let stop_flag2 = Arc::new(AtomicBool::new(false));
        shared2.attach_sink_stop_flag(Arc::clone(&stop_flag2));
        let mut state2 = EngineState::new(ep2, init2, &buf2, &prod_ctrl2, &shared2);

        assert!(buf2.push_for_epoch(
            ep2,
            StreamEvent::TrackStart {
                path: f01.clone(),
                upnp_uri: None,
        upnp_next_gen: None,
                mode_label: PlayModeLabel::Sequential,
                report: dummy_report.clone(),
                total_duration_s: Some(1.0),
                had_gapless_carryover: false,
            }
        ));
        assert!(buf2.push_for_epoch(
            ep2,
            StreamEvent::PcmBlock(ap_core::buffer::InterleavedInt32Buffer {
                sample_rate_hz: 352_800,
                frames: vec![[10, -10]; 100],
            })
        ));
        assert!(buf2.push_for_epoch(
            ep2,
            StreamEvent::PcmBlock(ap_core::buffer::InterleavedInt32Buffer {
                sample_rate_hz: 352_800,
                frames: vec![[20, -20]; 200],
            })
        ));
        assert!(buf2.push_for_epoch(ep2, StreamEvent::TrackEnd));
        assert!(buf2.push_for_epoch(
            ep2,
            StreamEvent::TrackStart {
                path: f02.clone(),
                upnp_uri: None,
        upnp_next_gen: None,
                mode_label: PlayModeLabel::Sequential,
                report: dummy_report,
                total_duration_s: Some(1.0),
                had_gapless_carryover: false,
            }
        ));

        let mut sink2 = MockBlockSink {
            stop_flag: Arc::clone(&stop_flag2),
            // After consuming block 1 (100 frames), inject a deleted/nonexistent PlayFile!
            inject_once: Some((
                shared2.clone(),
                ControlCommand::PlayFile(root.join("deleted_toctou.flac")),
            )),
            frames_consumed_per_call: Vec::new(),
        };
        assert_eq!(
            step_consumer_once(
                &mut state2,
                &buf2,
                &shared2,
                &controller,
                &stop_flag2,
                &mut sink2,
                || false,
            ),
            ConsumerStepResult::Continue
        );
        // Must have played block 1 (100 frames), caught rejected PlayFile, and immediately
        // resumed block 2 (200 frames) of Track 01 without discarding it or jumping to Track 02!
        assert_eq!(
            sink2.frames_consumed_per_call,
            vec![100, 200],
            "rejected PlayFile must resume remaining 200 frames of Track 01 instead of discarding them"
        );
        assert_eq!(state2.last_started_track.as_ref(), Some(&f01));
        // Track 02 must still sit cleanly at the front of the buffer ready to play next!
        match buf2.pop_for_epoch_timeout(ep2, Duration::from_millis(20)) {
            Some(StreamEvent::TrackStart { path, .. }) => assert_eq!(path, f02),
            other => panic!("expected TrackStart(02.flac) still queued, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn simulated_foobar2000_upnp_session_plays_wav_and_gapless_next_flac_with_rate_following() {
        use std::io::Read;
        use std::net::{Ipv4Addr, TcpListener, TcpStream};
        use ap_upnp::TransportState;
        use flac_codec::encode::{FlacSampleWriter, Options};

        // 1. Build Track 1: 44.1 kHz / 16-bit streaming WAV (0xFFFFFFFF sentinel, chunked HTTP).
        let mut wav_pcm = Vec::new();
        for i in 0..4096i32 {
            let s = ((i * 11) % 10_000) as i16;
            wav_pcm.extend_from_slice(&s.to_le_bytes());
            wav_pcm.extend_from_slice(&(-s).to_le_bytes());
        }
        let mut wav_bytes = Vec::new();
        wav_bytes.extend_from_slice(b"RIFF");
        wav_bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        wav_bytes.extend_from_slice(b"WAVEfmt ");
        wav_bytes.extend_from_slice(&16u32.to_le_bytes());
        wav_bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav_bytes.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav_bytes.extend_from_slice(&44_100u32.to_le_bytes());
        wav_bytes.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        wav_bytes.extend_from_slice(&4u16.to_le_bytes());
        wav_bytes.extend_from_slice(&16u16.to_le_bytes());
        wav_bytes.extend_from_slice(b"data");
        wav_bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        wav_bytes.extend_from_slice(&wav_pcm);

        // 2. Build Track 2: 96 kHz / 24-bit native FLAC (exercises dynamic rate following 352.8k -> 384k!).
        let mut flac_samples = Vec::new();
        for i in 0..4096i32 {
            flac_samples.push(i * 128);
            flac_samples.push(-i * 128);
        }
        let mut flac_cur = std::io::Cursor::new(Vec::new());
        {
            let mut w = FlacSampleWriter::new(
                &mut flac_cur,
                Options::default(),
                96_000,
                24,
                2,
                Some(flac_samples.len() as u64),
            )
            .unwrap();
            w.write(&flac_samples).unwrap();
            w.finalize().unwrap();
        }
        let flac_bytes = flac_cur.into_inner();

        // 3. Spawn simulated foobar2000 HTTP audio server serving `/track1.wav` and `/track2.flac`.
        let media_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let media_port = media_listener.local_addr().unwrap().port();
        let media_srv = thread::spawn(move || {
            for _ in 0..2 {
                let (mut s, _) = media_listener.accept().unwrap();
                let mut req_buf = [0u8; 1024];
                let n = s.read(&mut req_buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&req_buf[..n]);
                if req.contains("/track1.wav") {
                    let hdr = "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                    s.write_all(hdr.as_bytes()).unwrap();
                    for ch in wav_bytes.chunks(1024) {
                        s.write_all(format!("{:x}\r\n", ch.len()).as_bytes()).unwrap();
                        s.write_all(ch).unwrap();
                        s.write_all(b"\r\n").unwrap();
                    }
                    s.write_all(b"0\r\n\r\n").unwrap();
                } else {
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        flac_bytes.len()
                    );
                    s.write_all(hdr.as_bytes()).unwrap();
                    s.write_all(&flac_bytes).unwrap();
                }
                let _ = s.flush();
            }
        });

        // 4. Start real UpnpRendererServer + real AudioPipeline + producer + mock WDM-KS sink.
        let root = std::env::temp_dir().join(format!(
            "ap_cli_upnp_e2e_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let shared = SharedControl::new(root.clone(), "Gungnir 2 Mock");
        let stop_flag = Arc::new(AtomicBool::new(false));
        shared.attach_sink_stop_flag(Arc::clone(&stop_flag));

        let mut upnp_srv = UpnpRendererServer::start(
            shared.clone(),
            UpnpRendererConfig {
                friendly_name: "Audiophile Player E2E".to_string(),
                port: None,
                bind_ip: Some(Ipv4Addr::LOCALHOST),
                enable_ssdp: false,
            },
        )
        .unwrap();
        let upnp_port = upnp_srv.port();
        let upnp_state = Arc::clone(upnp_srv.state());

        let soap = |action: &str, args: &str| {
            let body = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
                 <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                 <s:Body><u:{action} xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">{args}</u:{action}></s:Body>\
                 </s:Envelope>"
            );
            let req = format!(
                "POST /upnp/control/AVTransport HTTP/1.1\r\n\
                 Host: 127.0.0.1:{upnp_port}\r\n\
                 Content-Type: text/xml; charset=\"utf-8\"\r\n\
                 SOAPACTION: \"urn:schemas-upnp-org:service:AVTransport:1#{action}\"\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            let mut s = TcpStream::connect(("127.0.0.1", upnp_port)).unwrap();
            s.write_all(req.as_bytes()).unwrap();
            let mut resp = String::new();
            let _ = s.read_to_string(&mut resp);
            assert!(resp.starts_with("HTTP/1.1 200 OK"), "SOAP {action} failed: {resp}");
        };

        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let prod_ctrl = ProducerControl::new(ep0, ProducerDirective::Paused);
        let pipeline = AudioPipeline::new(UpsamplerPolicy::default()).unwrap();
        let p_ctrl = Arc::clone(&prod_ctrl);
        let p_buf = Arc::clone(&buf);
        let p_shared = Arc::clone(&shared);
        let p_upnp = Some(Arc::clone(&upnp_state));
        let producer = thread::spawn(move || {
            producer_loop_with_upnp(pipeline, p_ctrl, p_buf, p_shared, p_upnp, None);
        });

        let controller = ShutdownController::new();
        let mut engine_state = EngineState::with_upnp(
            ep0,
            PlayDirective::Random { root: root.clone() },
            &buf,
            &prod_ctrl,
            &shared,
            Some(Arc::clone(&upnp_state)),
        );
        engine_state.is_paused = true;

        // foobar2000 sends SetAVTransportURI(track1.wav), SetNextAVTransportURI(track2.flac), Play!
        soap(
            "SetAVTransportURI",
            &format!(
                "<InstanceID>0</InstanceID>\
                 <CurrentURI>http://127.0.0.1:{media_port}/track1.wav</CurrentURI>\
                 <CurrentURIMetaData></CurrentURIMetaData>"
            ),
        );
        soap(
            "SetNextAVTransportURI",
            &format!(
                "<InstanceID>0</InstanceID>\
                 <NextURI>http://127.0.0.1:{media_port}/track2.flac</NextURI>\
                 <NextURIMetaData></NextURIMetaData>"
            ),
        );
        soap("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>");

        struct RecordingRateSink {
            played_rates: Vec<(u32, usize)>,
        }
        impl BlockStreamAudioSink for RecordingRateSink {
            fn play_block_stream<F>(
                &mut self,
                sample_rate_hz: u32,
                _wire_format: DacWireFormat,
                mut next_block: F,
            ) -> Result<PlaybackStats, OutputError>
            where
                F: FnMut() -> ap_output_win::wdm_ks::StreamBlock,
            {
                let mut total_frames = 0usize;
                loop {
                    match next_block() {
                        ap_output_win::wdm_ks::StreamBlock::Ready(b) => {
                            total_frames += b.frames.len();
                        }
                        ap_output_win::wdm_ks::StreamBlock::Underrun => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        ap_output_win::wdm_ks::StreamBlock::EndOfTrack => break,
                    }
                }
                self.played_rates.push((sample_rate_hz, total_frames));
                Ok(PlaybackStats {
                    device_name: "Gungnir 2 Mock".to_string(),
                    sample_rate_hz,
                    wire_format: DacWireFormat::Int32,
                    frames_played: total_frames,
                    wavert_buffer_bytes: 0,
                    notifications: 0,
                    mmcss_pro_audio: false,
                    underrun_halves: 0,
                })
            }
        }

        let mut sink = RecordingRateSink {
            played_rates: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            step_consumer_once(
                &mut engine_state,
                &buf,
                &shared,
                &controller,
                &stop_flag,
                &mut sink,
                || producer.is_finished(),
            );
            if sink.played_rates.len() >= 2 && engine_state.is_paused {
                break;
            }
        }

        assert_eq!(
            sink.played_rates.len(),
            2,
            "both Track 1 (WAV 44.1k) and gapless NextURI Track 2 (FLAC 96k) must play"
        );
        assert_eq!(
            sink.played_rates[0].0, 352_800,
            "44.1 kHz WAV must dynamically upsample to 352.8 kHz"
        );
        assert_eq!(
            sink.played_rates[1].0, 384_000,
            "96 kHz FLAC must dynamically switch to 384 kHz (rate-following rule)"
        );
        assert!(sink.played_rates[0].1 >= 4096 * 8);
        assert!(sink.played_rates[1].1 >= 4096 * 4);
        assert!(
            engine_state.is_paused,
            "after UPnP queue completes, engine must pause and release the WDM-KS DAC pin"
        );
        assert_eq!(upnp_state.snapshot().transport_state, TransportState::Stopped);

        // Clean shutdown.
        buf.close();
        prod_ctrl.set(engine_state.active_epoch.wrapping_add(1), ProducerDirective::Shutdown);
        upnp_srv.shutdown();
        producer.join().unwrap();
        media_srv.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn late_set_next_after_end_of_queue_still_plays_and_rejected_playfile_does_not_preempt_upnp() {
        // Review #5 Findings #5, #6, #19:
        // 1. Rejected PlayFile during active UpnpStream must NOT preempt UPnP (state stays PLAYING, NextURI intact).
        // 2. Late SetNextAVTransportURI arriving after EndOfQueue is pushed causes consumer EndOfQueue
        //    to immediately promote & start the late-queued track instead of going STOPPED.
        use ap_upnp::TransportState;
        use std::net::Ipv4Addr;

        let root = std::env::temp_dir().join(format!(
            "ap_cli_upnp_late_next_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let shared = SharedControl::new(root.clone(), "Mock DAC");
        let stop_flag = Arc::new(AtomicBool::new(false));
        shared.attach_sink_stop_flag(Arc::clone(&stop_flag));

        let mut upnp_srv = UpnpRendererServer::start(
            shared.clone(),
            UpnpRendererConfig {
                friendly_name: "Late Next Test".to_string(),
                port: None,
                bind_ip: Some(Ipv4Addr::LOCALHOST),
                enable_ssdp: false,
            },
        )
        .unwrap();
        let upnp_state = Arc::clone(upnp_srv.state());

        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let prod_ctrl = ProducerControl::new(ep0, ProducerDirective::Paused);
        let controller = ShutdownController::new();

        upnp_state.set_av_transport_uri("http://127.0.0.1:59999/a.flac".into(), String::new());
        upnp_state.play().unwrap();
        let play_cmd = shared.take_pending_command().unwrap();

        let mut state = EngineState::with_upnp(
            ep0,
            PlayDirective::Random { root: root.clone() },
            &buf,
            &prod_ctrl,
            &shared,
            Some(Arc::clone(&upnp_state)),
        );
        assert_eq!(state.dispatch(play_cmd), DispatchOutcome::Applied);
        upnp_state.engine_mark_playing("http://127.0.0.1:59999/a.flac", Some(10.0));
        upnp_state.set_next_av_transport_uri("http://127.0.0.1:59999/b.flac".into(), String::new());

        // Finding #19: Rejected PlayFile during active UPnP must NOT set UPnP to STOPPED or clear NextURI!
        assert_eq!(
            state.dispatch(ControlCommand::PlayFile(root.join("nonexistent.flac"))),
            DispatchOutcome::Rejected
        );
        let snap_after_reject = upnp_state.snapshot();
        assert_eq!(snap_after_reject.transport_state, TransportState::Playing);
        assert_eq!(
            snap_after_reject.next.as_ref().map(|n| n.uri.as_str()),
            Some("http://127.0.0.1:59999/b.flac")
        );

        // Finding #6: Producer already pushed EndOfQueue for Track A while Track B sits in st.next.
        // Consumer EndOfQueue arm must promote Track B and start PlayDirective::UpnpStream(B)!
        assert!(buf.push_for_epoch(
            state.active_epoch,
            StreamEvent::EndOfQueue("Finished UPnP stream".into())
        ));
        struct PanicSink;
        impl BlockStreamAudioSink for PanicSink {
            fn play_block_stream<F>(
                &mut self,
                _rate: u32,
                _fmt: DacWireFormat,
                _next: F,
            ) -> Result<PlaybackStats, OutputError>
            where
                F: FnMut() -> ap_output_win::wdm_ks::StreamBlock,
            {
                unreachable!("EndOfQueue step should not open sink")
            }
        }
        let mut sink = PanicSink;
        assert_eq!(
            step_consumer_once(
                &mut state,
                &buf,
                &shared,
                &controller,
                &stop_flag,
                &mut sink,
                || false,
            ),
            ConsumerStepResult::Continue
        );
        assert!(
            !state.is_paused,
            "late SetNextAVTransportURI must prevent pause on EndOfQueue"
        );
        match &state.last_play {
            PlayDirective::UpnpStream { request } => {
                assert_eq!(request.uri, "http://127.0.0.1:59999/b.flac");
            }
            other => panic!("expected UpnpStream for late NextURI, got {other:?}"),
        }

        upnp_srv.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn review6_web_next_during_upnp_without_nexturi_rejects_and_resumes_stream_and_gen_prefetch() {
        use ap_upnp::{TransportState, UpnpRendererConfig, UpnpRendererServer};
        use std::net::Ipv4Addr;

        let root = std::env::temp_dir().join(format!(
            "ap_cli_r6_upnp_next_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&root);
        let shared = SharedControl::new(root.clone(), "Mock DAC");
        let stop_flag = Arc::new(AtomicBool::new(false));
        shared.attach_sink_stop_flag(Arc::clone(&stop_flag));

        let mut upnp_srv = UpnpRendererServer::start(
            shared.clone(),
            UpnpRendererConfig {
                friendly_name: "R6 UPnP Test".to_string(),
                port: None,
                bind_ip: Some(Ipv4Addr::LOCALHOST),
                enable_ssdp: false,
            },
        )
        .unwrap();
        let upnp_state = Arc::clone(upnp_srv.state());

        let buf = BoundedBlockStreamBuffer::new(DEFAULT_MAX_BUFFERED_FRAMES);
        let ep0 = buf.current_epoch();
        let prod_ctrl = ProducerControl::new(ep0, ProducerDirective::Paused);
        let controller = ShutdownController::new();

        upnp_state.set_av_transport_uri("http://127.0.0.1:59999/a.flac".into(), String::new());
        upnp_state.play().unwrap();
        let play_cmd = shared.take_pending_command().unwrap();

        let mut state = EngineState::with_upnp(
            ep0,
            PlayDirective::Random { root: root.clone() },
            &buf,
            &prod_ctrl,
            &shared,
            Some(Arc::clone(&upnp_state)),
        );
        assert_eq!(state.dispatch(play_cmd), DispatchOutcome::Applied);

        // Finding #7: Web/CLI Next during UPnP with NO NextURI queued MUST return Rejected
        // (and when submitted mid-track, consumer must resume playing remaining blocks!).
        assert_eq!(
            state.dispatch(ControlCommand::Next),
            DispatchOutcome::Rejected,
            "Next during UPnP without NextURI must return Rejected so active stream resumes"
        );

        let ep = state.active_epoch;
        let report = AudioPipeline::new(UpsamplerPolicy::default())
            .unwrap()
            .start_track(
                ap_core::format::StreamSpec {
                    sample_rate_hz: 44_100,
                    bits_per_sample: 16,
                    channels: 2,
                },
                &ap_core::headroom::TrackPeakMetadata::default(),
                true,
            )
            .unwrap()
            .report()
            .clone();
        assert!(buf.push_for_epoch(
            ep,
            StreamEvent::TrackStart {
                path: PathBuf::from("a.flac [UPnP FLAC]"),
                upnp_uri: Some("http://127.0.0.1:59999/a.flac".into()),
                upnp_next_gen: None,
                mode_label: PlayModeLabel::UpnpRenderer,
                report,
                total_duration_s: Some(10.0),
                had_gapless_carryover: false,
            }
        ));
        for _ in 0..4 {
            assert!(buf.push_for_epoch(
                ep,
                StreamEvent::PcmBlock(ap_core::buffer::InterleavedInt32Buffer {
                    sample_rate_hz: 44_100,
                    frames: vec![[100, -100]; 4096],
                })
            ));
        }
        assert!(buf.push_for_epoch(ep, StreamEvent::TrackEnd));

        struct InterruptOnceSink {
            shared: Arc<SharedControl>,
            stop_flag: Arc<AtomicBool>,
            interrupted: bool,
            frames_played: usize,
        }
        impl BlockStreamAudioSink for InterruptOnceSink {
            fn play_block_stream<F>(
                &mut self,
                rate: u32,
                _fmt: DacWireFormat,
                mut next: F,
            ) -> Result<PlaybackStats, OutputError>
            where
                F: FnMut() -> ap_output_win::wdm_ks::StreamBlock,
            {
                loop {
                    if self.stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    match next() {
                        ap_output_win::wdm_ks::StreamBlock::Ready(b) => {
                            self.frames_played += b.frames.len();
                            if !self.interrupted {
                                self.interrupted = true;
                                // Simulate Web UI Next click mid-UPnP-track with no NextURI!
                                self.shared.submit_command(ControlCommand::Next);
                                break;
                            }
                        }
                        ap_output_win::wdm_ks::StreamBlock::EndOfTrack => break,
                        ap_output_win::wdm_ks::StreamBlock::Underrun => break,
                    }
                }
                Ok(PlaybackStats {
                    device_name: "Mock DAC".to_string(),
                    sample_rate_hz: rate,
                    wire_format: DacWireFormat::Int32,
                    frames_played: self.frames_played,
                    wavert_buffer_bytes: 0,
                    notifications: 1,
                    mmcss_pro_audio: false,
                    underrun_halves: 0,
                })
            }
        }

        let mut sink = InterruptOnceSink {
            shared: shared.clone(),
            stop_flag: Arc::clone(&stop_flag),
            interrupted: false,
            frames_played: 0,
        };
        assert_eq!(
            step_consumer_once(
                &mut state,
                &buf,
                &shared,
                &controller,
                &stop_flag,
                &mut sink,
                || false,
            ),
            ConsumerStepResult::Continue
        );
        assert_eq!(
            sink.frames_played,
            4 * 4096,
            "consumer must resume after rejected Next and play all 4 blocks instead of discarding them"
        );
        assert_eq!(upnp_state.snapshot().transport_state, TransportState::Playing);

        // Finding #8: Replaced NextURI (B -> B2) after prefetching B promotes B accurately while
        // keeping B2 queued as NextURI, AND repeat-one (SetNext(B2) while playing B2) prefetches!
        upnp_state.set_next_av_transport_uri("http://127.0.0.1:59999/b.flac".into(), String::new());
        let pref_b = upnp_state
            .engine_peek_next_for_prefetch(None)
            .expect("must peek B");
        let gen_b = pref_b.prefetched_next_gen.expect("must carry generation token");
        assert_eq!(pref_b.uri, "http://127.0.0.1:59999/b.flac");

        // Control point replaces NextURI with B2 before B becomes audible.
        upnp_state.set_next_av_transport_uri("http://127.0.0.1:59999/b2.flac".into(), String::new());
        // B becomes audible on DAC -> must promote B to current AND keep B2 in next!
        upnp_state.engine_mark_playing_with_gen(&pref_b.uri, Some(20.0), Some(gen_b));
        let snap_b = upnp_state.snapshot();
        assert_eq!(
            snap_b.current.as_ref().map(|c| c.uri.as_str()),
            Some("http://127.0.0.1:59999/b.flac")
        );
        assert_eq!(
            snap_b.next.as_ref().map(|n| n.uri.as_str()),
            Some("http://127.0.0.1:59999/b2.flac")
        );

        // Producer now peeks B2 cleanly (new gen != gen_b), promotes B2, then queues repeat-one B2!
        let pref_b2 = upnp_state
            .engine_peek_next_for_prefetch(Some(gen_b))
            .expect("must peek B2");
        let gen_b2 = pref_b2.prefetched_next_gen.unwrap();
        assert_ne!(gen_b2, gen_b);
        upnp_state.engine_mark_playing_with_gen(&pref_b2.uri, Some(25.0), Some(gen_b2));
        upnp_state.set_next_av_transport_uri("http://127.0.0.1:59999/b2.flac".into(), String::new());
        let pref_repeat = upnp_state
            .engine_peek_next_for_prefetch(Some(gen_b2))
            .expect("repeat-one SetNext(B2) must prefetch gaplessly with fresh generation!");
        assert_eq!(pref_repeat.uri, "http://127.0.0.1:59999/b2.flac");
        assert_ne!(pref_repeat.prefetched_next_gen, Some(gen_b2));

        upnp_srv.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ultimate_panic_after_carry_take_finalizes_prev_tail_f8() {
        // Engine-level F8 recovery test: invoke `stream_track_ultimate` under the same
        // `catch_unwind` + `panic_backup_carry` -> `finalize_prev_track` recovery flow used by
        // `engine.rs:1671-1701`, with a panic injected right after `incoming_carryover.take()`.
        // Assert Track A's nonzero REW carryover tail PCM and `TrackEnd` are both emitted!
        use crate::ultimate::{
            TEST_PANIC_AFTER_CARRY_TAKE, UltimateRuntimeContext, stream_track_ultimate,
        };
        use ap_core::config::UltimateConfig;
        use ap_core::format::StreamSpec;
        use ap_core::headroom::TrackPeakMetadata;
        use ap_core::metadata::TrackMetadata;
        use ap_core::track_db::TrackDbStore;
        use ap_dsp::convolution::ConvolutionTail;
        use ap_dsp::pipeline::GaplessCarryover;
        use ap_dsp::selector::{FilterArchetype, NoiseShaperProfile, UpsamplerPolicy};
        use ap_source::flac::AudioBlockSource;

        struct Dummy31sSource {
            spec: StreamSpec,
            cursor: usize,
            total: usize,
        }
        impl AudioBlockSource for Dummy31sSource {
            fn spec(&self) -> StreamSpec {
                self.spec
            }
            fn peak_meta(&self) -> TrackPeakMetadata {
                TrackPeakMetadata::default()
            }
            fn is_streaming(&self) -> bool {
                false
            }
            fn total_frames(&self) -> Option<u64> {
                Some(self.total as u64)
            }
            fn track_meta(&self) -> TrackMetadata {
                TrackMetadata::default()
            }
            fn streaminfo_md5(&self) -> Option<[u8; 16]> {
                None
            }
            fn computed_md5(&self) -> Option<[u8; 16]> {
                None
            }
            fn read_next_block(
                &mut self,
                max_frames: usize,
            ) -> Result<Option<ap_core::buffer::StereoF64Buffer>, String> {
                if self.cursor >= self.total {
                    return Ok(None);
                }
                let n = max_frames.min(self.total - self.cursor);
                self.cursor += n;
                Ok(Some(ap_core::buffer::StereoF64Buffer {
                    sample_rate_hz: self.spec.sample_rate_hz,
                    left: vec![0.1; n],
                    right: vec![0.1; n],
                }))
            }
        }

        let root = std::env::temp_dir().join(format!("ap_eng_f8_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let track_b = root.join("track_b.flac");
        std::fs::write(&track_b, b"b").unwrap();

        let pipeline = AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy {
                filter_override: Some(FilterArchetype::Null),
                ..UpsamplerPolicy::default()
            },
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let ult = UltimateRuntimeContext {
            config: UltimateConfig {
                enabled: true,
                genre_rules: true,
                learn_on_play: false,
            },
            store: TrackDbStore::new(root.clone(), false),
        };
        let buf = BoundedBlockStreamBuffer::new(384_000 * 10);
        let ep = buf.current_epoch();

        let mut carried_tail = Some(GaplessCarryover {
            output_rate_hz: 44_100,
            output_bits: 24,
            effective_shaper: NoiseShaperProfile::TpdfDither,
            upsampler: None,
            deemphasis: None,
            convolver: Some(ConvolutionTail {
                sample_rate_hz: 44_100,
                left: vec![0.25; 512],
                right: vec![-0.25; 512],
            }),
            quantizer_state: None,
        });
        let mut panic_backup_carry: Option<GaplessCarryover> = None;
        let mut pending_track_end = true;

        let mut src = Dummy31sSource {
            spec: StreamSpec {
                sample_rate_hz: 44_100,
                bits_per_sample: 16,
                channels: 2,
            },
            cursor: 0,
            total: 44_100 * 31,
        };

        TEST_PANIC_AFTER_CARRY_TAKE.with(|c| c.set(true));
        let res = catch_unwind(AssertUnwindSafe(|| {
            stream_track_ultimate(
                &pipeline,
                &ult,
                &buf,
                ep,
                || false,
                &mut src,
                &track_b,
                Some(&track_b),
                None,
                None,
                PlayModeLabel::SingleFile,
                Some(31.0),
                &mut carried_tail,
                &mut panic_backup_carry,
                &mut pending_track_end,
                false,
                16_384,
            )
        }));
        TEST_PANIC_AFTER_CARRY_TAKE.with(|c| c.set(false));

        assert!(res.is_err(), "expected injected F8 panic");
        assert!(carried_tail.is_none());
        assert!(panic_backup_carry.is_some());

        // Exact engine.rs panic recovery path: restore backup carry and finalize Track A cleanly!
        if carried_tail.is_none() {
            carried_tail = panic_backup_carry.take();
        }
        finalize_prev_track(&pipeline, &buf, ep, &mut carried_tail, &mut pending_track_end);
        assert!(carried_tail.is_none());
        assert!(!pending_track_end);

        let mut saw_tail_pcm = false;
        let mut saw_track_end = false;
        while let Some(ev) = buf.pop_for_epoch_timeout(ep, Duration::from_millis(50)) {
            match ev {
                StreamEvent::PcmBlock(b) => {
                    assert_eq!(b.frames.len(), 512);
                    assert!(b.frames.iter().any(|f| f[0] != 0 || f[1] != 0));
                    saw_tail_pcm = true;
                }
                StreamEvent::TrackEnd => {
                    saw_track_end = true;
                    break;
                }
                other => panic!("unexpected event after F8 panic recovery: {other:?}"),
            }
        }
        assert!(saw_tail_pcm, "F8: Track A's carryover tail PCM must be flushed on panic");
        assert!(saw_track_end, "F8: Track A's TrackEnd must be emitted on panic");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stream_capture_end_to_end_upsamples_enforces_headroom_rejects_next_and_releases_dac() {
        use ap_source::stream_capture::{CaptureError, CaptureTransport};
        use ap_stream_protocol::{
            AP_STREAM_ABI_VERSION, ApDriverConfig, ApDriverStatus, ApPcmChunkHeader,
            ApStreamFormat,
        };

        struct MockCaptureTransport {
            chunks: std::collections::VecDeque<Vec<u8>>,
            generation: u32,
        }

        impl MockCaptureTransport {
            fn new(generation: u32, frames: usize) -> Self {
                // 24-in-32 integer PCM @ 44.1 kHz stereo, 1 kHz sine at 0.5 amplitude
                let mut pcm = Vec::with_capacity(frames * 8);
                for i in 0..frames {
                    let s = (2.0 * std::f64::consts::PI * 1000.0 * (i as f64) / 44_100.0).sin()
                        * 0.5;
                    let v = (s * 2_147_483_648.0).round() as i32;
                    pcm.extend_from_slice(&v.to_le_bytes());
                    pcm.extend_from_slice(&v.to_le_bytes());
                }
                let mut chunks = std::collections::VecDeque::new();
                for slice in pcm.chunks(4096 * 8) {
                    chunks.push_back(slice.to_vec());
                }
                Self { chunks, generation }
            }

            fn fmt() -> ApStreamFormat {
                ApStreamFormat {
                    sample_rate: 44_100,
                    channels: 2,
                    bits_per_sample: 32,
                    valid_bits_per_sample: 24,
                    block_align: 8,
                    is_float: 0,
                    packet_size_bytes: 8192,
                    packet_count: 2,
                    reserved: 0,
                }
            }

            fn caller(name: &str) -> [u8; 16] {
                let mut buf = [0u8; 16];
                let b = name.as_bytes();
                let n = b.len().min(15);
                buf[..n].copy_from_slice(&b[..n]);
                buf
            }
        }

        impl CaptureTransport for MockCaptureTransport {
            fn get_status(&mut self) -> Result<ApDriverStatus, CaptureError> {
                Ok(ApDriverStatus {
                    abi_version: AP_STREAM_ABI_VERSION,
                    stream_generation: self.generation,
                    stream_running: if self.chunks.is_empty() { 0 } else { 1 },
                    exclusive_verified: 1,
                    strict_reject_audiodg: 1,
                    format: Self::fmt(),
                    alloc_pid: 4242,
                    run_pid: 4242,
                    packet_pid: 4242,
                    alloc_proc: Self::caller("TIDALPlayer.ex"),
                    run_proc: Self::caller("TIDALPlayer.ex"),
                    packet_proc: Self::caller("TIDALPlayer.ex"),
                    create_proc: Self::caller("audiodg.exe"),
                    ..ApDriverStatus::default()
                })
            }

            fn set_config(&mut self, _cfg: ApDriverConfig) -> Result<(), CaptureError> {
                Ok(())
            }

            fn reset_ring(&mut self) -> Result<(), CaptureError> {
                Ok(())
            }

            fn read_pcm(
                &mut self,
                io_buf: &mut [u8],
            ) -> Result<(ApPcmChunkHeader, usize), CaptureError> {
                let hdr_sz = std::mem::size_of::<ApPcmChunkHeader>();
                if let Some(chunk) = self.chunks.pop_front() {
                    let n = chunk.len().min(io_buf.len() - hdr_sz);
                    io_buf[hdr_sz..hdr_sz + n].copy_from_slice(&chunk[..n]);
                    let hdr = ApPcmChunkHeader {
                        abi_version: AP_STREAM_ABI_VERSION,
                        stream_generation: self.generation,
                        format: Self::fmt(),
                        pcm_bytes: n as u32,
                        stream_running: 1,
                        exclusive_verified: 1,
                        ..ApPcmChunkHeader::default()
                    };
                    Ok((hdr, n))
                } else {
                    let hdr = ApPcmChunkHeader {
                        abi_version: AP_STREAM_ABI_VERSION,
                        stream_generation: self.generation,
                        format: Self::fmt(),
                        pcm_bytes: 0,
                        stream_running: 0,
                        exclusive_verified: 1,
                        ..ApPcmChunkHeader::default()
                    };
                    Ok((hdr, 0))
                }
            }
        }

        let transport =
            SharedCaptureTransport::new(Some(Box::new(MockCaptureTransport::new(7, 8_192))));
        let pipeline = AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy::default(),
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::new(384_000 * 10);
        let shared = SharedControl::new(PathBuf::from("."), "Mock DAC".to_string());
        let init_ep = buf.current_epoch();
        let req = StreamCaptureRequest {
            generation: 7,
            sample_rate_hz: 44_100,
            channels: 2,
            bits_per_sample: 32,
            valid_bits_per_sample: 24,
            caller_name: "TIDALPlayer.ex".to_string(),
            caller_pid: 4242,
        };
        let prod_ctrl = ProducerControl::new(init_ep, ProducerDirective::Paused);

        // Verify EngineState transitions cleanly on PlayStreamCapture and rejects Next while active,
        // and B5: Next while paused resumes the active capture generation!
        let mut eng_state = EngineState::with_sources(
            init_ep,
            PlayDirective::Random {
                root: PathBuf::from("."),
            },
            &buf,
            &prod_ctrl,
            &shared,
            None,
            Some(transport.clone()),
        );
        assert_eq!(
            eng_state.dispatch(ControlCommand::PlayStreamCapture(req.clone())),
            DispatchOutcome::Applied
        );
        assert_eq!(
            eng_state.dispatch(ControlCommand::Next),
            DispatchOutcome::Rejected,
            "Next must be rejected while live exclusive capture is actively playing"
        );
        // B1: a second PlayStreamCapture while already active must NOT flush the epoch!
        let ep_before = eng_state.active_epoch;
        let mut req_next = req.clone();
        req_next.generation = 8;
        assert_eq!(
            eng_state.dispatch(ControlCommand::PlayStreamCapture(req_next)),
            DispatchOutcome::Rejected,
            "B1: generation change while active must not flush or interrupt WDM-KS"
        );
        assert_eq!(eng_state.active_epoch, ep_before);

        // B5: Pause then Next resumes capture when stream is running!
        assert_eq!(
            eng_state.dispatch(ControlCommand::Pause),
            DispatchOutcome::Applied
        );
        assert!(eng_state.is_paused);
        assert_eq!(
            eng_state.dispatch(ControlCommand::Next),
            DispatchOutcome::Applied,
            "B5: Next while paused must resume active capture stream"
        );
        assert!(!eng_state.is_paused);
        let active_ep = eng_state.active_epoch;

        let buf_bg = Arc::clone(&buf);
        let ctrl_bg = Arc::clone(&prod_ctrl);
        let shared_bg = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            producer_loop_with_sources(
                pipeline,
                ctrl_bg,
                buf_bg,
                shared_bg,
                None,
                Some(transport),
                None,
            );
        });

        let mut saw_start = false;
        let mut total_out_frames = 0usize;
        let mut saw_end_of_queue = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(ev) = buf.pop_for_epoch_timeout(active_ep, Duration::from_millis(100)) {
                match ev {
                    StreamEvent::TrackStart {
                        mode_label,
                        report,
                        ..
                    } => {
                        assert_eq!(mode_label, PlayModeLabel::StreamCapture);
                        assert_eq!(report.output_rate_hz, 352_800);
                        assert!(
                            report.headroom.mode.gain_db() <= -2.99,
                            "live stream must enforce -3 dB safety headroom, got {} dB",
                            report.headroom.mode.gain_db()
                        );
                        saw_start = true;
                    }
                    StreamEvent::PcmBlock(b) => {
                        assert_eq!(b.sample_rate_hz, 352_800);
                        total_out_frames += b.frames.len();
                    }
                    StreamEvent::EndOfQueue(msg) => {
                        assert!(msg.contains("DAC released"));
                        saw_end_of_queue = true;
                        break;
                    }
                    StreamEvent::TrackEnd => {}
                    other => panic!("unexpected event in stream capture test: {other:?}"),
                }
            }
        }

        buf.close();
        prod_ctrl.set(active_ep.wrapping_add(1), ProducerDirective::Shutdown);
        handle.join().unwrap();

        assert!(saw_start, "expected TrackStart for PlayModeLabel::StreamCapture");
        assert!(
            total_out_frames >= 8_192 * 8,
            "expected >= 8x upsampled PCM frames, got {total_out_frames}"
        );
        assert!(
            saw_end_of_queue,
            "expected EndOfQueue releasing DAC when capture pauses/stops"
        );
    }

    #[test]
    fn capture_generation_change_preserves_previous_tail_and_follows_rate_in_same_epoch_b1_b2() {
        use ap_source::stream_capture::{CaptureError, CaptureTransport};
        use ap_stream_protocol::{
            AP_STREAM_ABI_VERSION, ApDriverConfig, ApDriverStatus, ApPcmChunkHeader,
            ApStreamFormat,
        };

        fn caller16(s: &str) -> [u8; 16] {
            let mut b = [0u8; 16];
            let n = s.len().min(15);
            b[..n].copy_from_slice(&s.as_bytes()[..n]);
            b
        }
        fn make_fmt(rate: u32) -> ApStreamFormat {
            ApStreamFormat {
                sample_rate: rate,
                channels: 2,
                bits_per_sample: 16,
                valid_bits_per_sample: 16,
                block_align: 4,
                is_float: 0,
                packet_size_bytes: 4096,
                packet_count: 2,
                reserved: 0,
            }
        }
        fn make_pcm16(frames: usize, val: i16) -> Vec<u8> {
            let mut out = Vec::with_capacity(frames * 4);
            for _ in 0..frames {
                out.extend_from_slice(&val.to_le_bytes());
                out.extend_from_slice(&(-val).to_le_bytes());
            }
            out
        }

        struct DualRateCaptureMock {
            cur_gen: u32,
            running: bool,
            chunks: std::collections::VecDeque<(ApPcmChunkHeader, Vec<u8>)>,
        }
        impl CaptureTransport for DualRateCaptureMock {
            fn get_status(&mut self) -> Result<ApDriverStatus, CaptureError> {
                let rate = if self.cur_gen == 7 { 44_100 } else { 48_000 };
                Ok(ApDriverStatus {
                    abi_version: AP_STREAM_ABI_VERSION,
                    stream_generation: self.cur_gen,
                    stream_running: if self.running { 1 } else { 0 },
                    exclusive_verified: 1,
                    strict_reject_audiodg: 1,
                    format: make_fmt(rate),
                    alloc_pid: 4242,
                    run_pid: 4242,
                    packet_pid: 4242,
                    alloc_proc: caller16("TIDALPlayer.ex"),
                    run_proc: caller16("TIDALPlayer.ex"),
                    packet_proc: caller16("TIDALPlayer.ex"),
                    ..ApDriverStatus::default()
                })
            }
            fn set_config(&mut self, _: ApDriverConfig) -> Result<(), CaptureError> { Ok(()) }
            fn reset_ring(&mut self) -> Result<(), CaptureError> { Ok(()) }
            fn read_pcm(&mut self, io_buf: &mut [u8]) -> Result<(ApPcmChunkHeader, usize), CaptureError> {
                let hdr_sz = std::mem::size_of::<ApPcmChunkHeader>();
                if let Some((hdr, data)) = self.chunks.pop_front() {
                    self.cur_gen = hdr.stream_generation;
                    let n = data.len();
                    io_buf[hdr_sz..hdr_sz + n].copy_from_slice(&data);
                    Ok((hdr, n))
                } else {
                    self.running = false;
                    let rate = if self.cur_gen == 7 { 44_100 } else { 48_000 };
                    let hdr = ApPcmChunkHeader {
                        abi_version: AP_STREAM_ABI_VERSION,
                        stream_generation: self.cur_gen,
                        stream_running: 0,
                        exclusive_verified: 1,
                        format: make_fmt(rate),
                        pcm_bytes: 0,
                        ..ApPcmChunkHeader::default()
                    };
                    Ok((hdr, 0))
                }
            }
        }

        let pcm7 = make_pcm16(4_096, 12_000);
        let pcm8 = make_pcm16(4_096, 10_000);
        let hdr7 = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 7,
            stream_running: 1,
            exclusive_verified: 1,
            format: make_fmt(44_100),
            pcm_bytes: pcm7.len() as u32,
            ..ApPcmChunkHeader::default()
        };
        let hdr8 = ApPcmChunkHeader {
            abi_version: AP_STREAM_ABI_VERSION,
            stream_generation: 8,
            stream_running: 1,
            exclusive_verified: 1,
            format: make_fmt(48_000),
            pcm_bytes: pcm8.len() as u32,
            ..ApPcmChunkHeader::default()
        };

        let mock = DualRateCaptureMock {
            cur_gen: 7,
            running: true,
            chunks: std::collections::VecDeque::from([(hdr7, pcm7), (hdr8, pcm8)]),
        };
        let transport = SharedCaptureTransport::new(Some(Box::new(mock)));
        let pipeline = AudioPipeline::with_rew_config_full_tail(
            UpsamplerPolicy::default(),
            ConvolutionConfig::disabled(),
            true,
        )
        .unwrap();
        let buf = BoundedBlockStreamBuffer::new(384_000 * 10);
        let shared = SharedControl::new(PathBuf::from("."), "Mock DAC".to_string());
        let ep = buf.current_epoch();
        let req = StreamCaptureRequest {
            generation: 7,
            sample_rate_hz: 44_100,
            channels: 2,
            bits_per_sample: 16,
            valid_bits_per_sample: 16,
            caller_name: "TIDALPlayer.ex".to_string(),
            caller_pid: 4242,
        };
        let prod_ctrl = ProducerControl::new(
            ep,
            ProducerDirective::Active(PlayDirective::StreamCapture { request: req }),
        );

        let buf_bg = Arc::clone(&buf);
        let ctrl_bg = Arc::clone(&prod_ctrl);
        let shared_bg = Arc::clone(&shared);
        let handle = thread::spawn(move || {
            producer_loop_with_sources(pipeline, ctrl_bg, buf_bg, shared_bg, None, Some(transport), None);
        });

        let mut start_rates = Vec::new();
        let mut frames_352 = 0usize;
        let mut frames_384 = 0usize;
        let mut track_ends = 0usize;
        let mut saw_eoq = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(ev) = buf.pop_for_epoch_timeout(ep, Duration::from_millis(100)) {
                match ev {
                    StreamEvent::TrackStart { report, .. } => {
                        start_rates.push(report.output_rate_hz);
                    }
                    StreamEvent::PcmBlock(b) => {
                        if b.sample_rate_hz == 352_800 {
                            frames_352 += b.frames.len();
                        } else if b.sample_rate_hz == 384_000 {
                            frames_384 += b.frames.len();
                        }
                    }
                    StreamEvent::TrackEnd => track_ends += 1,
                    StreamEvent::EndOfQueue(_) => {
                        saw_eoq = true;
                        break;
                    }
                    other => panic!("unexpected event: {other:?}"),
                }
            }
        }

        buf.close();
        prod_ctrl.set(ep.wrapping_add(1), ProducerDirective::Shutdown);
        handle.join().unwrap();

        assert_eq!(start_rates, vec![352_800, 384_000], "must follow 44.1 -> 48 kHz in ONE epoch");
        assert!(frames_352 >= 4_096 * 8, "gen 7 tail must not be truncated: {frames_352}");
        assert!(frames_384 >= 4_096 * 8, "gen 8 stashed opening must not be dropped: {frames_384}");
        assert_eq!(track_ends, 2, "clean TrackEnd between gen 7 and gen 8 and at final pause");
        assert!(saw_eoq);
    }
}
