//! Windows-only strictly zero-mixer audio output backends.
//!
//! Hard architectural invariant:
//! - Only **Direct WDM-KS (Kernel Streaming)** and **Event-Driven WASAPI Exclusive**
//!   are implemented.
//! - Windows Shared Mode (`AUDCLNT_SHAREMODE_SHARED`) is intentionally absent and
//!   impossible to fall back to.

pub mod hygiene;
pub mod sink;
pub mod wasapi_exclusive;
pub mod wdm_ks;
