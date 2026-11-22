//! Abstract zero-mixer Windows output sink trait shared by WDM-KS and WASAPI Exclusive.

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;

/// Output sink guaranteed to bypass `audiodg.exe` and the Windows audio mixer.
pub trait ZeroMixerAudioSink {
    /// Stream a pre-computed full-memory integer PCM buffer to the locked hardware DAC.
    fn play_full_memory(
        &mut self,
        pcm: &InterleavedInt32Buffer,
        wire_format: DacWireFormat,
    ) -> Result<(), String>;

    /// Stop playback immediately and release the exclusive hardware pin/endpoint cleanly.
    fn stop(&mut self) -> Result<(), String>;
}
