// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Independent loopback null verification suite for Audiophile Player (`PIPELINE_TESTS_REVIEW.md`).
//!
//! Shares **zero** DSP implementation code with `ap-dsp`: all reference convolutions, IEC 60908
//! de-emphasis, polyphase decimation, Welch PSD, and coherence checks are implemented from scratch
//! in pure IEEE 754 `f64` using complex `rustfft`.

pub mod metrics;
pub mod pick;
pub mod reference;
pub mod report;
pub mod runner;
