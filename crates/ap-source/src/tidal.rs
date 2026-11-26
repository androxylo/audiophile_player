// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Milestone 2 placeholder: strictly Windows-mixer-free Tidal lossless stream source.
//!
//! Architecture finalized in `docs/research/07-tidal-and-companions.md` (INV-07).
//! Standard mixer-involving virtual audio cables are strictly disqualified.

/// Source kind distinguishing local FLAC files from network UPnP/Tidal and live capture streams
/// (used for on-the-fly DSP selection and `-3 dB` unknown-peak headroom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    LocalFlac,
    UpnpUrl,
    TidalStream,
    StreamCapture,
}
