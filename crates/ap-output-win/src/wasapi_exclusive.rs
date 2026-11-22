//! Event-driven WASAPI Exclusive (`AUDCLNT_SHAREMODE_EXCLUSIVE`) backend.
//!
//! Uses in-process `AudioSes.dll` to evict shared-mode clients (`audiodg.exe`), map the
//! `usbaudio2.sys` WaveRT cyclic buffer directly, perform the mandatory
//! `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` alignment dance, and stream integer PCM only.

/// Event-driven WASAPI Exclusive output sink handle.
#[derive(Debug)]
pub struct WasapiExclusiveSink {
    pub device_name: String,
}
