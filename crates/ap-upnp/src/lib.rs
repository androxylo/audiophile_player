// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Pure-Rust UPnP AV 1.0 MediaRenderer (`urn:schemas-upnp-org:device:MediaRenderer:1`).
//!
//! Enables zero-mixer, rate-following lossless playback from **foobar2000 ≥ 2.25** (built-in
//! UPnP client output) and standard UPnP control points (BubbleUPnP, etc.) directly into the
//! `audiophile-player` pure-`f64` DSP pipeline and WDM-KS WaveRT DAC output.
//!
//! Architecture:
//! - [`ssdp`]: UDP multicast `239.255.255.250:1900` responder & announcer (`M-SEARCH`,
//!   `ssdp:alive`, `ssdp:byebye`) using `SO_REUSEADDR` and `IP_MULTICAST_LOOP`.
//! - [`xml`]: Device Description XML, SCPD XMLs, bounded SOAP/DIDL-Lite parser, deterministic UDN.
//! - [`state`]: Thread-safe AVTransport/RenderingControl state bridge shared with the audio engine.
//! - [`services`]: `AVTransport:1`, `ConnectionManager:1` (lossless-only sink MIME types), and
//!   `RenderingControl:1` (fixed volume 100, unmuted — zero digital volume attenuation).
//! - [`gena`]: `SUBSCRIBE` / `UNSUBSCRIBE` / async `NOTIFY` (`LastChange`) eventing with LAN-only
//!   callback SSRF protection.
//! - [`server`]: Dedicated bounded HTTP/1.1 server decoupled from `ap-control` and the RT DAC thread.

pub mod gena;
pub mod server;
pub mod services;
pub mod ssdp;
pub mod state;
pub mod xml;

pub use server::{UpnpRendererConfig, UpnpRendererServer};
pub use state::{TransportState, UpnpRendererState, UpnpTrackDescriptor};
