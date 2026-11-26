//! Shared control state bridging the low-priority HTTP server thread and the audio engine.
//!
//! Pause and Stop are intentionally a single unified action ([`ControlCommand::Pause`]):
//! pausing stops playback cleanly and releases the WDM-KS driver pin immediately so
//! other exclusive players (such as Audirvana) can use the DAC while paused.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::log::ActionLog;
use crate::security::normalize_allowed_root;

/// Maximum number of queued relative commands (`Next`) before additional clicks are dropped.
const MAX_QUEUED_COMMANDS: usize = 16;

/// Metadata accompanying a UPnP `AVTransport::Play` / `SetAVTransportURI` / `Seek` stream request.
#[derive(Debug, Clone, PartialEq)]
pub struct UpnpPlayRequest {
    pub uri: String,
    pub display_name: String,
    pub protocol_info: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
    pub duration_s: Option<f64>,
    /// Optional start offset in seconds (used for `AVTransport::Seek` and Pause/Resume).
    pub seek_offset_s: Option<f64>,
    /// Monotonic generation token set ONLY when this request originated from a gapless
    /// `SetNextAVTransportURI` prefetch (`None` for explicit `Play`/`Seek`/`SetAVTransportURI`
    /// restarts of `CurrentURI`, Review #6 Findings #8 & #17).
    pub prefetched_next_gen: Option<u64>,
}

impl Eq for UpnpPlayRequest {}

/// Metadata accompanying a live exclusive-mode kernel capture stream (e.g. Tidal Desktop
/// playing into Audiophile Player's ACX 1.1 virtual render driver, zero Windows mixer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamCaptureRequest {
    pub generation: u32,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub valid_bits_per_sample: u16,
    pub caller_name: String,
    pub caller_pid: u32,
}

/// Unified control commands issued by the HTTP UI, UPnP MediaRenderer, or Exclusive Stream Capture
/// (no volume control).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlCommand {
    /// Play random FLAC files from the selected folder tree (`O(depth)` walk).
    PlayRandom(PathBuf),
    /// Play FLAC files sequentially from the selected folder tree (sorted lazy walk).
    PlaySequential(PathBuf),
    /// Play a specific selected FLAC file.
    PlayFile(PathBuf),
    /// Play a network lossless audio stream pushed by a UPnP control point (e.g. foobar2000).
    PlayUpnp(UpnpPlayRequest),
    /// Play a verified zero-mixer exclusive stream captured from the local ACX virtual endpoint
    /// (e.g. vanilla Tidal Desktop app in Exclusive Mode).
    PlayStreamCapture(StreamCaptureRequest),
    /// Skip immediately (with smooth fade-out) to the next track in the active playback mode.
    Next,
    /// Unified Pause/Stop: fade out cleanly and release the WDM-KS DAC pin immediately.
    Pause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayModeLabel {
    Random,
    Sequential,
    SingleFile,
    UpnpRenderer,
    StreamCapture,
    Paused,
}

impl PlayModeLabel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Random => "Play Random",
            Self::Sequential => "Play Sequential",
            Self::SingleFile => "Play File",
            Self::UpnpRenderer => "UPnP Renderer",
            Self::StreamCapture => "Exclusive Stream (Tidal / Live)",
            Self::Paused => "Paused (DAC released)",
        }
    }
}

/// Immutable status snapshot rendered by HTML and `/api/status`.
#[derive(Debug, Clone)]
pub struct ControlSnapshot {
    pub is_playing: bool,
    pub mode: PlayModeLabel,
    pub dac_name: String,
    pub active_folder: PathBuf,
    pub current_track: Option<String>,
    pub current_format: Option<String>,
    /// Optional Ultimate Mode showcase decision report summary (`U13`).
    pub current_decision: Option<String>,
    /// Current track maximum peak summary when known (metadata/cached whole-track peak and/or
    /// dynamically updated live DAC output max peak). `None` when paused or silent/unknown.
    pub current_peak: Option<String>,
    pub recent_actions: Vec<String>,
}

struct InnerState {
    is_playing: bool,
    mode: PlayModeLabel,
    dac_name: String,
    allowed_root: PathBuf,
    active_folder: PathBuf,
    current_track: Option<String>,
    current_format: Option<String>,
    current_decision: Option<String>,
    known_peak: Option<String>,
    pending_commands: VecDeque<ControlCommand>,
    log: ActionLog,
    sink_stop_flag: Option<Arc<AtomicBool>>,
    shutdown: bool,
}

/// Thread-safe control hub decoupled from real-time audio pumping.
pub struct SharedControl {
    inner: Mutex<InnerState>,
    cv: Condvar,
    /// Lock-free running maximum absolute 32-bit DAC sample observed for the currently audible
    /// track/stream (`0` = no non-zero audio observed yet; `2^31` = `0.00 dBFS`).
    live_output_peak_u32: Arc<AtomicU32>,
}

impl SharedControl {
    pub fn new(initial_folder: PathBuf, dac_name: impl Into<String>) -> Arc<Self> {
        let allowed_root = normalize_allowed_root(&initial_folder);
        let mut log = ActionLog::default();
        log.push(format!(
            "Initialized control interface (folder: {})",
            allowed_root.display()
        ));
        Arc::new(Self {
            inner: Mutex::new(InnerState {
                is_playing: false,
                mode: PlayModeLabel::Paused,
                dac_name: dac_name.into(),
                allowed_root: allowed_root.clone(),
                active_folder: allowed_root,
                current_track: None,
                current_format: None,
                current_decision: None,
                known_peak: None,
                pending_commands: VecDeque::new(),
                log,
                sink_stop_flag: None,
                shutdown: false,
            }),
            cv: Condvar::new(),
            live_output_peak_u32: Arc::new(AtomicU32::new(0)),
        })
    }

    /// Return a lock-free `Arc<AtomicU32>` handle for updating the active track's running maximum
    /// output sample amplitude (`fetch_max`) directly from the real-time WaveRT thread.
    pub fn live_output_peak_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.live_output_peak_u32)
    }

    /// Configured root folder that HTTP browsing and `/action/play-*` paths are confined to.
    pub fn allowed_root(&self) -> PathBuf {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.allowed_root.clone()
    }

    /// Attach the active audio sink's `stop_handle()` so Pause / mode switch interrupts immediately.
    pub fn attach_sink_stop_flag(&self, flag: Arc<AtomicBool>) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.sink_stop_flag = Some(flag);
    }

    /// Submit a command from the HTTP server. Interrupts any currently playing track cleanly
    /// (with fade-out & DAC release on Pause or mode change) and wakes a paused engine loop.
    pub fn submit_command(&self, cmd: ControlCommand) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match &cmd {
            ControlCommand::PlayRandom(dir) => {
                st.active_folder = dir.clone();
                st.log.push(format!("Command: Play Random -> {}", dir.display()));
            }
            ControlCommand::PlaySequential(dir) => {
                st.active_folder = dir.clone();
                st.log
                    .push(format!("Command: Play Sequential -> {}", dir.display()));
            }
            ControlCommand::PlayFile(file) => {
                if let Some(parent) = file.parent() {
                    st.active_folder = parent.to_path_buf();
                }
                st.log.push(format!("Command: Play File -> {}", file.display()));
            }
            ControlCommand::PlayUpnp(req) => {
                st.log.push(format!(
                    "Command: UPnP Play -> {} ({})",
                    req.display_name, req.uri
                ));
            }
            ControlCommand::PlayStreamCapture(req) => {
                st.log.push(format!(
                    "Command: Exclusive Stream -> {} (PID {}) {} Hz {}-in-{} bit PCM",
                    req.caller_name,
                    req.caller_pid,
                    req.sample_rate_hz,
                    req.valid_bits_per_sample,
                    req.bits_per_sample
                ));
            }
            ControlCommand::Next => {
                st.log.push("Command: Next track");
            }
            ControlCommand::Pause => {
                st.log
                    .push("Command: Pause (stopping playback & releasing DAC driver)");
            }
        }
        match cmd {
            // `Next` is a relative command: append it behind any pending `Play*` or earlier
            // `Next` so rapid clicks advance multiple tracks and never drop a just-clicked
            // `PlayFile` / `PlaySequential` / `PlayRandom`. If `Pause` is already queued,
            // keep `Next` after `Pause` so pausing then skipping behaves consistently.
            ControlCommand::Next => {
                if st.pending_commands.len() < MAX_QUEUED_COMMANDS {
                    st.pending_commands.push_back(ControlCommand::Next);
                }
            }
            // Absolute commands (`Play*`, `Pause`) supersede earlier unconsumed commands.
            other => {
                st.pending_commands.clear();
                st.pending_commands.push_back(other);
            }
        }
        if let Some(flag) = st.sink_stop_flag.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        self.cv.notify_all();
    }

    /// Take the next queued command in FIFO order without blocking.
    pub fn take_pending_command(&self) -> Option<ControlCommand> {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.pending_commands.pop_front()
    }

    /// Block cleanly at 0% CPU while paused until either a new command or process shutdown arrives.
    pub fn wait_for_command_or_shutdown(&self) -> Option<ControlCommand> {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if st.shutdown {
                return None;
            }
            if let Some(cmd) = st.pending_commands.pop_front() {
                return Some(cmd);
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Notify waiting engine threads that process shutdown was requested.
    pub fn request_shutdown(&self) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.shutdown = true;
        if let Some(flag) = st.sink_stop_flag.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
        self.cv.notify_all();
    }

    pub fn set_playing(&self, mode: PlayModeLabel, track: String, format: String) {
        self.set_playing_with_decision_and_peak(mode, track, format, None, None);
    }

    pub fn set_playing_with_decision(
        &self,
        mode: PlayModeLabel,
        track: String,
        format: String,
        decision: Option<String>,
    ) {
        self.set_playing_with_decision_and_peak(mode, track, format, decision, None);
    }

    pub fn set_playing_with_decision_and_peak(
        &self,
        mode: PlayModeLabel,
        track: String,
        format: String,
        decision: Option<String>,
        known_peak: Option<String>,
    ) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.live_output_peak_u32.store(0, Ordering::Relaxed);
        st.is_playing = true;
        st.mode = mode;
        st.log.push(format!("Playing: {track} ({format})"));
        st.current_track = Some(track);
        st.current_format = Some(format);
        st.current_decision = decision;
        st.known_peak = known_peak;
    }

    /// Lock-free, wait-free update of the current audible track's running maximum output sample
    /// amplitude (`0..=2_147_483_648`). Safe to call from the real-time WaveRT audio thread.
    pub fn observe_output_peak_u32(&self, block_max_abs: u32) {
        if block_max_abs > 0 {
            self.live_output_peak_u32
                .fetch_max(block_max_abs, Ordering::Relaxed);
        }
    }

    pub fn set_paused(&self, reason: impl AsRef<str>) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.live_output_peak_u32.store(0, Ordering::Relaxed);
        st.is_playing = false;
        st.mode = PlayModeLabel::Paused;
        st.current_track = None;
        st.current_format = None;
        st.current_decision = None;
        st.known_peak = None;
        st.log.push(reason.as_ref());
    }

    pub fn record_action(&self, msg: impl AsRef<str>) {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        st.log.push(msg.as_ref());
    }

    pub fn snapshot(&self) -> ControlSnapshot {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let live_peak_dbfs = if st.is_playing {
            let raw = self.live_output_peak_u32.load(Ordering::Relaxed);
            if raw > 0 {
                Some(20.0 * ((raw as f64) / 2_147_483_648.0).log10())
            } else {
                None
            }
        } else {
            None
        };
        let current_peak = match (st.known_peak.as_deref(), live_peak_dbfs) {
            (Some(known), Some(live)) => Some(format!("{known} · Live DAC: {live:+.2} dBFS")),
            (Some(known), None) => Some(known.to_string()),
            (None, Some(live)) => Some(format!("{live:+.2} dBFS (live DAC)")),
            (None, None) => None,
        };
        ControlSnapshot {
            is_playing: st.is_playing,
            mode: st.mode,
            dac_name: st.dac_name.clone(),
            active_folder: st.active_folder.clone(),
            current_track: st.current_track.clone(),
            current_format: st.current_format.clone(),
            current_decision: st.current_decision.clone(),
            current_peak,
            recent_actions: st.log.snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_next_clicks_queue_without_overwriting_play_commands() {
        let ctrl = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");

        // PlayFile followed immediately by two Next clicks preserves all three in order.
        ctrl.submit_command(ControlCommand::PlayFile(PathBuf::from(r"C:\Music\01.flac")));
        ctrl.submit_command(ControlCommand::Next);
        ctrl.submit_command(ControlCommand::Next);

        assert_eq!(
            ctrl.take_pending_command(),
            Some(ControlCommand::PlayFile(PathBuf::from(r"C:\Music\01.flac")))
        );
        assert_eq!(ctrl.take_pending_command(), Some(ControlCommand::Next));
        assert_eq!(ctrl.take_pending_command(), Some(ControlCommand::Next));
        assert_eq!(ctrl.take_pending_command(), None);
    }

    #[test]
    fn absolute_command_supersedes_pending_next_clicks() {
        let ctrl = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");
        ctrl.submit_command(ControlCommand::Next);
        ctrl.submit_command(ControlCommand::Next);
        ctrl.submit_command(ControlCommand::Pause);

        assert_eq!(ctrl.take_pending_command(), Some(ControlCommand::Pause));
        assert_eq!(ctrl.take_pending_command(), None);
    }

    #[test]
    fn next_queue_is_bounded() {
        let ctrl = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");
        for _ in 0..(MAX_QUEUED_COMMANDS + 10) {
            ctrl.submit_command(ControlCommand::Next);
        }
        let mut count = 0;
        while ctrl.take_pending_command().is_some() {
            count += 1;
        }
        assert_eq!(count, MAX_QUEUED_COMMANDS);
    }

    #[test]
    fn track_peak_combines_known_and_dynamic_live_peak_and_resets_cleanly() {
        let ctrl = SharedControl::new(PathBuf::from(r"C:\Music"), "Test DAC");
        assert_eq!(ctrl.snapshot().current_peak, None);

        // Live stream with unknown up-front peak: None until non-zero audio arrives, then monotonic max.
        ctrl.set_playing(
            PlayModeLabel::StreamCapture,
            "TIDALPlayer.ex".to_string(),
            "44.1 kHz -> 352.8 kHz".to_string(),
        );
        assert_eq!(ctrl.snapshot().current_peak, None);

        // ~-6.02 dBFS (0.5 FS)
        ctrl.observe_output_peak_u32(1 << 30);
        assert_eq!(
            ctrl.snapshot().current_peak.as_deref(),
            Some("-6.02 dBFS (live DAC)")
        );
        // Quieter block does not lower the track maximum peak
        ctrl.observe_output_peak_u32(1 << 28);
        assert_eq!(
            ctrl.snapshot().current_peak.as_deref(),
            Some("-6.02 dBFS (live DAC)")
        );
        // Louder block (-0.00 dBFS = 2^31) raises it dynamically
        ctrl.observe_output_peak_u32(1 << 31);
        assert_eq!(
            ctrl.snapshot().current_peak.as_deref(),
            Some("+0.00 dBFS (live DAC)")
        );

        // New track resets live peak and shows known peak immediately
        ctrl.set_playing_with_decision_and_peak(
            PlayModeLabel::Random,
            "song.flac".to_string(),
            "24/96 -> 384".to_string(),
            None,
            Some("-0.85 dBFS (whole-track)".to_string()),
        );
        assert_eq!(
            ctrl.snapshot().current_peak.as_deref(),
            Some("-0.85 dBFS (whole-track)")
        );
        ctrl.observe_output_peak_u32(1 << 30);
        assert_eq!(
            ctrl.snapshot().current_peak.as_deref(),
            Some("-0.85 dBFS (whole-track) · Live DAC: -6.02 dBFS")
        );

        ctrl.set_paused("Paused");
        assert_eq!(ctrl.snapshot().current_peak, None);
    }
}
