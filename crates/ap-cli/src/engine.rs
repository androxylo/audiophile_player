//! Full-memory headless playback pipeline coordinator.
//!
//! Pipeline stages (all internal DSP strictly in `f64` at `352.8 / 384 kHz`):
//! 1. Decode FLAC into RAM + parse ReplayGain/True Peak metadata (`ap-source`).
//! 2. Compute safe `<= 0 dB` headroom gain (`-3 dB` when unknown) (`ap-core`).
//! 3. Select upsampling filter & 8fS noise shaper on the fly (`ap-dsp::selector`).
//! 4. Upsample (<= 4M taps) -> Core REW stereo convolution -> Integer channel delay (`ap-dsp`).
//! 5. Quantize once to integer PCM (`ap-dsp::quantize`).
//! 6. Stream full-memory buffer strictly via WDM-KS or WASAPI Exclusive (`ap-output-win`).

use ap_core::config::EngineConfig;

/// Headless playback engine state container (outline only).
#[derive(Debug)]
pub struct PlaybackEngine {
    pub config: EngineConfig,
}
