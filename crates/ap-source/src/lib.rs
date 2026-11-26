// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Audio sources: local FLAC files, HTTP/UPnP lossless streams, and Milestone 2 Tidal/capture interfaces.

pub mod flac;
pub mod http_stream;
pub mod library;
pub mod md5;
pub mod stream_capture;
pub mod tidal;
