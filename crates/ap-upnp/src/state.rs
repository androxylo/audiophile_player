// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Shared UPnP MediaRenderer transport state synchronized between `ap-upnp` SOAP/GENA and
//! the `ap-cli` audio engine.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ap_control::state::{ControlCommand, SharedControl, UpnpPlayRequest};

use crate::gena::{GenaEventHub, GenaService};
use crate::xml::{format_upnp_duration, parse_didl_lite, xml_escape};

/// Standard UPnP `AVTransport:1` `TransportState` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    NoMediaPresent,
    Stopped,
    Transitioning,
    Playing,
    PausedPlayback,
}

impl TransportState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoMediaPresent => "NO_MEDIA_PRESENT",
            Self::Stopped => "STOPPED",
            Self::Transitioning => "TRANSITIONING",
            Self::Playing => "PLAYING",
            Self::PausedPlayback => "PAUSED_PLAYBACK",
        }
    }
}

/// Complete description of a UPnP track queued via `SetAVTransportURI` or `SetNextAVTransportURI`.
#[derive(Debug, Clone, PartialEq)]
pub struct UpnpTrackDescriptor {
    pub uri: String,
    pub raw_metadata: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub protocol_info: Option<String>,
    pub duration_s: Option<f64>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
}

impl UpnpTrackDescriptor {
    pub fn from_uri_and_metadata(uri: String, raw_metadata: String) -> Self {
        let didl = parse_didl_lite(&raw_metadata);
        Self {
            uri,
            raw_metadata,
            title: didl.title,
            artist: didl.artist,
            album: didl.album,
            protocol_info: didl.protocol_info,
            duration_s: didl.duration_s,
            sample_rate_hz: didl.sample_rate_hz,
            channels: didl.channels,
            bits_per_sample: didl.bits_per_sample,
        }
    }

    pub fn display_name(&self) -> String {
        match (&self.artist, &self.title) {
            (Some(a), Some(t)) => format!("{a} - {t}"),
            (None, Some(t)) => t.clone(),
            _ => self.uri.clone(),
        }
    }

    pub fn to_play_request(&self) -> UpnpPlayRequest {
        self.to_play_request_at(None)
    }

    pub fn to_play_request_at(&self, seek_offset_s: Option<f64>) -> UpnpPlayRequest {
        UpnpPlayRequest {
            uri: self.uri.clone(),
            display_name: self.display_name(),
            protocol_info: self.protocol_info.clone(),
            sample_rate_hz: self.sample_rate_hz,
            channels: self.channels,
            bits_per_sample: self.bits_per_sample,
            duration_s: self.duration_s,
            seek_offset_s,
            prefetched_next_gen: None,
        }
    }

    pub fn to_play_request_prefetched(&self, next_gen: u64) -> UpnpPlayRequest {
        UpnpPlayRequest {
            uri: self.uri.clone(),
            display_name: self.display_name(),
            protocol_info: self.protocol_info.clone(),
            sample_rate_hz: self.sample_rate_hz,
            channels: self.channels,
            bits_per_sample: self.bits_per_sample,
            duration_s: self.duration_s,
            seek_offset_s: None,
            prefetched_next_gen: Some(next_gen),
        }
    }
}

/// Immutable snapshot of AVTransport state for SOAP getters (`GetTransportInfo`, `GetPositionInfo`,
/// `GetMediaInfo`) and GENA `LastChange` rendering.
#[derive(Debug, Clone)]
pub struct AvTransportSnapshot {
    pub transport_state: TransportState,
    pub transport_status: String,
    pub current: Option<UpnpTrackDescriptor>,
    pub next: Option<UpnpTrackDescriptor>,
    pub elapsed_s: f64,
    pub duration_s: Option<f64>,
}

struct InnerUpnpState {
    transport_state: TransportState,
    transport_status: String,
    current: Option<UpnpTrackDescriptor>,
    /// Queued `NextURI` paired with its monotonic `SetNextAVTransportURI` generation token.
    next: Option<(u64, UpnpTrackDescriptor)>,
    next_gen_seq: u64,
    /// Bounded cache of recently prefetched `(gen, descriptor)` pairs so replacing `NextURI`
    /// (`B -> B2`) after the producer already prefetched `B` still promotes `B` accurately when
    /// `B` becomes audible while keeping `B2` queued in `next` (Review #6 Finding #8).
    prefetched_nexts: VecDeque<(u64, UpnpTrackDescriptor)>,
    playing_started_at: Option<Instant>,
    accumulated_elapsed_s: f64,
    active_duration_s: Option<f64>,
    volume_notice_logged: bool,
}

/// Thread-safe state hub bridging UPnP SOAP actions, GENA eventing, and the playback engine.
pub struct UpnpRendererState {
    inner: Mutex<InnerUpnpState>,
    control: Arc<SharedControl>,
    gena: Arc<GenaEventHub>,
    udn: String,
    friendly_name: String,
}

impl UpnpRendererState {
    pub fn new(
        control: Arc<SharedControl>,
        gena: Arc<GenaEventHub>,
        udn: String,
        friendly_name: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(InnerUpnpState {
                transport_state: TransportState::NoMediaPresent,
                transport_status: "OK".to_string(),
                current: None,
                next: None,
                next_gen_seq: 0,
                prefetched_nexts: VecDeque::new(),
                playing_started_at: None,
                accumulated_elapsed_s: 0.0,
                active_duration_s: None,
                volume_notice_logged: false,
            }),
            control,
            gena,
            udn,
            friendly_name,
        })
    }

    pub fn udn(&self) -> &str {
        &self.udn
    }

    pub fn friendly_name(&self) -> &str {
        &self.friendly_name
    }

    pub fn dac_name(&self) -> String {
        self.control.snapshot().dac_name
    }

    pub fn gena(&self) -> &Arc<GenaEventHub> {
        &self.gena
    }

    /// Snapshot current AVTransport state including live elapsed `RelTime`.
    pub fn snapshot(&self) -> AvTransportSnapshot {
        let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut elapsed_s = st.accumulated_elapsed_s;
        if st.transport_state == TransportState::Playing {
            if let Some(started) = st.playing_started_at {
                elapsed_s += started.elapsed().as_secs_f64();
            }
        }
        let duration_s = st
            .active_duration_s
            .or_else(|| st.current.as_ref().and_then(|c| c.duration_s));
        if let Some(dur) = duration_s {
            if dur > 0.0 && elapsed_s > dur {
                elapsed_s = dur;
            }
        }
        AvTransportSnapshot {
            transport_state: st.transport_state,
            transport_status: st.transport_status.clone(),
            current: st.current.clone(),
            next: st.next.as_ref().map(|(_, d)| d.clone()),
            elapsed_s,
            duration_s,
        }
    }

    /// Handle UPnP `AVTransport::SetAVTransportURI`.
    pub fn set_av_transport_uri(&self, uri: String, metadata: String) {
        let trimmed_uri = uri.trim().to_string();
        let mut play_now: Option<UpnpPlayRequest> = None;
        let mut stop_now = false;

        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.transport_status = "OK".to_string();
            st.playing_started_at = None;
            st.accumulated_elapsed_s = 0.0;
            st.active_duration_s = None;
            st.next = None;
            st.prefetched_nexts.clear();

            if trimmed_uri.is_empty() {
                st.current = None;
                let was_active = matches!(
                    st.transport_state,
                    TransportState::Playing | TransportState::Transitioning
                );
                st.transport_state = TransportState::NoMediaPresent;
                if was_active {
                    stop_now = true;
                }
            } else {
                let desc = UpnpTrackDescriptor::from_uri_and_metadata(trimmed_uri, metadata);
                let was_playing = matches!(
                    st.transport_state,
                    TransportState::Playing | TransportState::Transitioning
                );
                if was_playing {
                    // UPnP AVTransport 1.0: SetAVTransportURI while PLAYING transitions directly
                    // to playing the new URI (and any redundant Play right after is idempotent).
                    st.transport_state = TransportState::Transitioning;
                    play_now = Some(desc.to_play_request());
                } else {
                    st.transport_state = TransportState::Stopped;
                }
                st.current = Some(desc);
            }
        }

        if let Some(req) = play_now {
            self.control.submit_command(ControlCommand::PlayUpnp(req));
        } else if stop_now {
            self.control.submit_command(ControlCommand::Pause);
        }
        self.emit_avt_last_change();
    }

    /// Handle UPnP `AVTransport::SetNextAVTransportURI` (gapless next-track queue).
    pub fn set_next_av_transport_uri(&self, uri: String, metadata: String) {
        let trimmed = uri.trim().to_string();
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if trimmed.is_empty() {
                st.next = None;
            } else {
                st.next_gen_seq = st.next_gen_seq.wrapping_add(1).max(1);
                let next_gen = st.next_gen_seq;
                st.next = Some((
                    next_gen,
                    UpnpTrackDescriptor::from_uri_and_metadata(trimmed, metadata),
                ));
            }
        }
        self.emit_avt_last_change();
    }

    /// Handle UPnP `AVTransport::Play`.
    ///
    /// Resumes from `accumulated_elapsed_s` when called from `PAUSED_PLAYBACK` (Review #5 #7)
    /// instead of restarting from byte 0.
    pub fn play(&self) -> Result<(), (u16, &'static str)> {
        let req_to_start: Option<UpnpPlayRequest>;
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let (req, resume_offset) = {
                let Some(cur) = st.current.as_ref() else {
                    return Err((701, "Transition not available"));
                };
                if matches!(
                    st.transport_state,
                    TransportState::Playing | TransportState::Transitioning
                ) {
                    // Already playing or transitioning to CurrentURI: idempotent 200 OK.
                    return Ok(());
                }
                let offset = if st.accumulated_elapsed_s > 0.05 {
                    Some(st.accumulated_elapsed_s)
                } else {
                    None
                };
                (cur.to_play_request_at(offset), offset)
            };
            st.transport_state = TransportState::Transitioning;
            st.transport_status = "OK".to_string();
            st.playing_started_at = None;
            if resume_offset.is_none() {
                st.accumulated_elapsed_s = 0.0;
            }
            req_to_start = Some(req);
        }
        if let Some(req) = req_to_start {
            self.control.submit_command(ControlCommand::PlayUpnp(req));
        }
        self.emit_avt_last_change();
        Ok(())
    }

    /// Handle UPnP `AVTransport::Seek` (`Unit = REL_TIME`, Review #5 Finding #8).
    pub fn seek(&self, unit: &str, target: &str) -> Result<(), (u16, &'static str)> {
        if !unit.trim().eq_ignore_ascii_case("REL_TIME") {
            return Err((710, "Seek mode not supported"));
        }
        let Some(target_s) = crate::xml::parse_upnp_duration(target) else {
            return Err((711, "Illegal seek target"));
        };
        let play_now: Option<UpnpPlayRequest>;
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cur) = st.current.as_ref() else {
                return Err((701, "Transition not available"));
            };
            let clamped_s = if let Some(dur) = st.active_duration_s.or(cur.duration_s) {
                if dur > 0.0 {
                    target_s.min(dur)
                } else {
                    target_s
                }
            } else {
                target_s
            };
            let was_active = matches!(
                st.transport_state,
                TransportState::Playing | TransportState::Transitioning
            );
            let req = cur.to_play_request_at(if clamped_s > 0.0 {
                Some(clamped_s)
            } else {
                None
            });
            st.accumulated_elapsed_s = clamped_s;
            st.playing_started_at = None;
            if was_active {
                st.transport_state = TransportState::Transitioning;
                st.transport_status = "OK".to_string();
                play_now = Some(req);
            } else {
                play_now = None;
            }
        }
        if let Some(req) = play_now {
            self.control.submit_command(ControlCommand::PlayUpnp(req));
        }
        self.emit_avt_last_change();
        Ok(())
    }

    /// Handle UPnP `AVTransport::Stop`. Releases the WDM-KS DAC pin immediately.
    pub fn stop(&self) {
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.next = None;
            st.prefetched_nexts.clear();
            st.playing_started_at = None;
            st.accumulated_elapsed_s = 0.0;
            st.active_duration_s = None;
            st.transport_state = if st.current.is_some() {
                TransportState::Stopped
            } else {
                TransportState::NoMediaPresent
            };
        }
        self.control.submit_command(ControlCommand::Pause);
        self.emit_avt_last_change();
    }

    /// Handle UPnP `AVTransport::Pause`. Releases the WDM-KS DAC pin immediately.
    pub fn pause(&self) {
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if st.transport_state == TransportState::Playing {
                if let Some(started) = st.playing_started_at.take() {
                    st.accumulated_elapsed_s += started.elapsed().as_secs_f64();
                }
            }
            st.transport_state = if st.current.is_some() {
                TransportState::PausedPlayback
            } else {
                TransportState::NoMediaPresent
            };
        }
        self.control.submit_command(ControlCommand::Pause);
        self.emit_avt_last_change();
    }

    /// Handle UPnP `AVTransport::Next` (advances immediately to queued `NextURI` if present).
    pub fn next_track(&self) -> Result<(), (u16, &'static str)> {
        let promoted = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some((_, next)) = st.next.take() else {
                return Err((701, "Transition not available"));
            };
            st.prefetched_nexts.clear();
            let req = next.to_play_request();
            st.current = Some(next);
            st.transport_state = TransportState::Transitioning;
            st.transport_status = "OK".to_string();
            st.playing_started_at = None;
            st.accumulated_elapsed_s = 0.0;
            st.active_duration_s = None;
            req
        };
        self.control
            .submit_command(ControlCommand::PlayUpnp(promoted));
        self.emit_avt_last_change();
        Ok(())
    }

    /// Log once when a control point calls `SetVolume`/`SetMute` (ignored for bit-perfect output).
    pub fn note_volume_control_ignored(&self) {
        let mut should_log = false;
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !st.volume_notice_logged {
                st.volume_notice_logged = true;
                should_log = true;
            }
        }
        if should_log {
            self.control.record_action(
                "UPnP RenderingControl: volume/mute kept fixed at 100% (no digital volume attenuation)",
            );
        }
    }

    // -----------------------------------------------------------------------
    // Engine-side lifecycle callbacks (called by `ap-cli/src/engine.rs`)
    // -----------------------------------------------------------------------

    /// Called by the engine consumer when a UPnP stream actually begins playing on the WDM-KS DAC.
    ///
    /// Compatibility wrapper for explicit starts/restarts of `CurrentURI` (`prefetched_next_gen =
    /// None`); never consumes `st.next` (Review #6 Finding #17).
    pub fn engine_mark_playing(&self, uri: &str, decoded_duration_s: Option<f64>) {
        self.engine_mark_playing_with_gen(uri, decoded_duration_s, None);
    }

    /// Generation-aware promotion when a UPnP track becomes audible on the WDM-KS DAC
    /// (Review #5 Finding #5 + Review #6 Findings #8 & #17):
    /// - `prefetched_next_gen = Some(gen)`: promotes the prefetched track into `current` and resets
    ///   `RelTime = 0`, matching either active `st.next` (consuming it) or the cached prefetched
    ///   descriptor if `SetNextAVTransportURI` was replaced (`B -> B2`) after `B` was already
    ///   prefetched (leaving `B2` intact in `st.next`).
    /// - `prefetched_next_gen = None`: explicit `Play`/`Seek`/`SetAVTransportURI` start/restart of
    ///   `current`; NEVER consumes `st.next` (even when `NextURI == CurrentURI` on repeat-one) and
    ///   preserves `accumulated_elapsed_s`!
    pub fn engine_mark_playing_with_gen(
        &self,
        uri: &str,
        decoded_duration_s: Option<f64>,
        prefetched_next_gen: Option<u64>,
    ) {
        let mut changed = false;
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(next_gen) = prefetched_next_gen {
                let promoted_desc = if st.next.as_ref().is_some_and(|(g, _)| *g == next_gen) {
                    st.prefetched_nexts.retain(|(g, _)| *g != next_gen);
                    st.next.take().map(|(_, d)| d)
                } else if let Some(pos) = st
                    .prefetched_nexts
                    .iter()
                    .position(|(g, _)| *g == next_gen)
                {
                    st.prefetched_nexts.remove(pos).map(|(_, d)| d)
                } else {
                    None
                };

                if let Some(desc) = promoted_desc {
                    st.current = Some(desc);
                    st.accumulated_elapsed_s = 0.0;
                    st.active_duration_s = decoded_duration_s;
                    st.transport_state = TransportState::Playing;
                    st.transport_status = "OK".to_string();
                    st.playing_started_at = Some(Instant::now());
                    changed = true;
                } else if st.current.as_ref().is_some_and(|c| c.uri == uri) {
                    if st.transport_state != TransportState::Playing {
                        changed = true;
                    }
                    st.transport_state = TransportState::Playing;
                    st.transport_status = "OK".to_string();
                    st.playing_started_at = Some(Instant::now());
                    if let Some(dur) = decoded_duration_s {
                        if st.active_duration_s != Some(dur) {
                            st.active_duration_s = Some(dur);
                            changed = true;
                        }
                    }
                }
            } else if st.current.as_ref().is_some_and(|c| c.uri == uri) {
                if st.transport_state != TransportState::Playing {
                    changed = true;
                }
                st.transport_state = TransportState::Playing;
                st.transport_status = "OK".to_string();
                st.playing_started_at = Some(Instant::now());
                if let Some(dur) = decoded_duration_s {
                    if st.active_duration_s != Some(dur) {
                        st.active_duration_s = Some(dur);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.emit_avt_last_change();
        }
    }

    /// Mark whatever UPnP track is currently active as `PLAYING` on the WDM-KS DAC.
    pub fn engine_mark_playing_current(&self, decoded_duration_s: Option<f64>) {
        let uri = {
            let st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.current.as_ref().map(|c| c.uri.clone())
        };
        if let Some(u) = uri {
            self.engine_mark_playing_with_gen(&u, decoded_duration_s, None);
        }
    }

    /// Peek `NextURI` for producer-side gapless prefetch WITHOUT prematurely promoting
    /// `CurrentURI` or resetting `RelTime` (Review #5 #5 + Review #6 #8).
    ///
    /// Keyed by monotonic `SetNextAVTransportURI` generation token (`already_prefetched_gen`)
    /// rather than URI string so repeat-one (`NextURI == CurrentURI`) prefetches gaplessly and
    /// replacing `NextURI` (`B -> B2`) after prefetching `B` preserves `B`'s descriptor for
    /// accurate metadata/RelTime promotion when `B` becomes audible.
    pub fn engine_peek_next_for_prefetch(
        &self,
        already_prefetched_gen: Option<u64>,
    ) -> Option<UpnpPlayRequest> {
        let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (next_gen, next) = st.next.as_ref()?.clone();
        if already_prefetched_gen == Some(next_gen) {
            return None;
        }
        if !st.prefetched_nexts.iter().any(|(g, _)| *g == next_gen) {
            if st.prefetched_nexts.len() >= 8 {
                st.prefetched_nexts.pop_front();
            }
            st.prefetched_nexts.push_back((next_gen, next.clone()));
        }
        Some(next.to_play_request_prefetched(next_gen))
    }

    /// Called by the DSP producer or consumer when advancing immediately to queued `NextURI`:
    /// atomically promotes `NextURI` -> `CurrentURI`.
    pub fn engine_take_next_for_gapless(&self) -> Option<UpnpPlayRequest> {
        let promoted = {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let (next_gen, next) = st.next.take()?;
            st.prefetched_nexts.retain(|(g, _)| *g != next_gen);
            let req = next.to_play_request();
            st.current = Some(next);
            st.playing_started_at = Some(Instant::now());
            st.accumulated_elapsed_s = 0.0;
            st.active_duration_s = None;
            req
        };
        self.emit_avt_last_change();
        Some(promoted)
    }

    /// Called by the engine when a UPnP stream finishes naturally (EOF with no `NextURI`).
    pub fn engine_mark_finished(&self) {
        let mut changed = false;
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.prefetched_nexts.clear();
            if matches!(
                st.transport_state,
                TransportState::Playing | TransportState::Transitioning
            ) {
                st.transport_state = if st.current.is_some() {
                    TransportState::Stopped
                } else {
                    TransportState::NoMediaPresent
                };
                st.playing_started_at = None;
                st.accumulated_elapsed_s = 0.0;
                changed = true;
            }
        }
        if changed {
            self.emit_avt_last_change();
        }
    }

    /// Called by the engine when opening/decoding a UPnP stream fails.
    pub fn engine_mark_error(&self, _reason: &str) {
        let mut changed = false;
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            st.transport_status = "ERROR_OCCURRED".to_string();
            if st.transport_state != TransportState::Stopped {
                st.transport_state = if st.current.is_some() {
                    TransportState::Stopped
                } else {
                    TransportState::NoMediaPresent
                };
                changed = true;
            }
            st.playing_started_at = None;
            st.accumulated_elapsed_s = 0.0;
        }
        if changed {
            self.emit_avt_last_change();
        }
    }

    /// Called by the engine when local Web UI commands pre-empt active UPnP playback.
    pub fn engine_mark_preempted(&self) {
        let mut changed = false;
        {
            let mut st = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if matches!(
                st.transport_state,
                TransportState::Playing | TransportState::Transitioning
            ) {
                st.transport_state = if st.current.is_some() {
                    TransportState::Stopped
                } else {
                    TransportState::NoMediaPresent
                };
                st.next = None;
                st.prefetched_nexts.clear();
                st.playing_started_at = None;
                st.accumulated_elapsed_s = 0.0;
                changed = true;
            }
        }
        if changed {
            self.emit_avt_last_change();
        }
    }

    /// Render the GENA `e:propertyset` XML containing `LastChange` for `AVTransport:1`.
    pub fn render_avt_event_xml(&self) -> String {
        let snap = self.snapshot();
        let cur_uri = snap.current.as_ref().map(|c| c.uri.as_str()).unwrap_or("");
        let cur_meta = snap
            .current
            .as_ref()
            .map(|c| c.raw_metadata.as_str())
            .unwrap_or("");
        let next_uri = snap.next.as_ref().map(|n| n.uri.as_str()).unwrap_or("");
        let next_meta = snap
            .next
            .as_ref()
            .map(|n| n.raw_metadata.as_str())
            .unwrap_or("");
        let dur_str = snap
            .duration_s
            .map(format_upnp_duration)
            .unwrap_or_else(|| "00:00:00".to_string());
        let nr_tracks = if snap.current.is_some() { 1 } else { 0 };
        let actions = if snap.current.is_some() {
            "Play,Stop,Pause,Seek"
        } else {
            "Stop"
        };

        let inner_event = format!(
            "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/AVT/\">\
             <InstanceID val=\"0\">\
             <TransportState val=\"{}\"/>\
             <TransportStatus val=\"{}\"/>\
             <TransportPlaySpeed val=\"1\"/>\
             <NumberOfTracks val=\"{nr_tracks}\"/>\
             <CurrentTrack val=\"{nr_tracks}\"/>\
             <CurrentTrackDuration val=\"{dur_str}\"/>\
             <CurrentMediaDuration val=\"{dur_str}\"/>\
             <AVTransportURI val=\"{}\"/>\
             <AVTransportURIMetaData val=\"{}\"/>\
             <CurrentTrackURI val=\"{}\"/>\
             <CurrentTrackMetaData val=\"{}\"/>\
             <NextAVTransportURI val=\"{}\"/>\
             <NextAVTransportURIMetaData val=\"{}\"/>\
             <CurrentTransportActions val=\"{actions}\"/>\
             </InstanceID></Event>",
            snap.transport_state.as_str(),
            xml_escape(&snap.transport_status),
            xml_escape(cur_uri),
            xml_escape(cur_meta),
            xml_escape(cur_uri),
            xml_escape(cur_meta),
            xml_escape(next_uri),
            xml_escape(next_meta),
        );

        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
             <e:property><LastChange>{}</LastChange></e:property>\
             </e:propertyset>\n",
            xml_escape(&inner_event)
        )
    }

    /// Render the GENA `e:propertyset` XML for `RenderingControl:1` (fixed 100 / unmuted).
    pub fn render_rcs_event_xml(&self) -> String {
        let inner_event = "<Event xmlns=\"urn:schemas-upnp-org:metadata-1-0/RCS/\">\
             <InstanceID val=\"0\">\
             <Mute channel=\"Master\" val=\"0\"/>\
             <Volume channel=\"Master\" val=\"100\"/>\
             <VolumeDB channel=\"Master\" val=\"0\"/>\
             <PresetNameList val=\"FactoryDefaults\"/>\
             </InstanceID></Event>";
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
             <e:property><LastChange>{}</LastChange></e:property>\
             </e:propertyset>\n",
            xml_escape(inner_event)
        )
    }

    fn emit_avt_last_change(&self) {
        let xml = self.render_avt_event_xml();
        self.gena.notify_service(GenaService::AvTransport, xml);
    }
}
