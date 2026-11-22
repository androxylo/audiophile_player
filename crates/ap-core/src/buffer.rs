//! Audio buffer types separating internal `f64` DSP from final integer DAC output.

/// Planar stereo IEEE 754 `f64` buffer used across the entire DSP pipeline.
/// All internal math (headroom, upsampling, REW convolution, delay) runs strictly in `f64`.
#[derive(Debug, Clone)]
pub struct StereoF64Buffer {
    pub sample_rate_hz: u32,
    pub left: Vec<f64>,
    pub right: Vec<f64>,
}

/// Final interleaved integer PCM buffer handed once to the zero-mixer Windows output sink.
#[derive(Debug, Clone)]
pub struct InterleavedInt32Buffer {
    pub sample_rate_hz: u32,
    pub frames: Vec<[i32; 2]>,
}
