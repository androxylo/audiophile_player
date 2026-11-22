//! Audirvana-style Direct Windows Kernel Streaming (`WDM-KS`) WaveRT backend.
//!
//! Talks directly to `usbaudio2.sys` via `SetupDiGetClassDevs` -> `CreateFileW` ->
//! `KsCreatePin` (`ksuser.dll`) -> `KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION` ->
//! `KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT` -> `KSSTATE_RUN`, bypassing both
//! `audiodg.exe` and `AudioSes.dll` completely (see `docs/research/06-windows-kernel-streaming.md`).

/// Direct WDM-KS output sink handle.
#[derive(Debug)]
pub struct WdmKsSink {
    pub device_name: String,
}
