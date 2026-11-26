// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Core domain types and strict invariants shared across all crates.
//!
//! Enforces:
//! - Pure IEEE 754 `f64` internal audio representation.
//! - Locked `352.8 kHz` / `384 kHz` (`8fS`) DSP target families.
//! - Strict `<= 0 dB` safety headroom policy (never boost volume; `-3 dB` when unknown).

pub mod buffer;
pub mod config;
pub mod filter_db;
pub mod format;
pub mod headroom;
pub mod json;
pub mod metadata;
pub mod track_db;
pub mod user_filters;
