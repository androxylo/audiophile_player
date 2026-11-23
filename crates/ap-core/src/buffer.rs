//! Audio buffer types separating internal `f64` DSP from final integer DAC output.

/// Default bounded source block size (frames per channel) fed into the streaming DSP pipeline.
/// Keeps peak RAM strictly bounded even for multi-hour 32-bit/384 kHz FLACs and live streams,
/// while remaining large enough for efficient FFT overlap-add processing.
pub const DEFAULT_SOURCE_BLOCK_FRAMES: usize = 65_536;

/// Planar stereo IEEE 754 `f64` block used across the entire streaming DSP pipeline.
/// All internal math (headroom, upsampling, REW convolution, delay) runs strictly in `f64`.
#[derive(Debug, Clone)]
pub struct StereoF64Buffer {
    pub sample_rate_hz: u32,
    pub left: Vec<f64>,
    pub right: Vec<f64>,
}

impl StereoF64Buffer {
    pub fn empty(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz,
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.left.len().min(self.right.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Final interleaved integer PCM block handed to the zero-mixer Windows output sink.
#[derive(Debug, Clone)]
pub struct InterleavedInt32Buffer {
    pub sample_rate_hz: u32,
    pub frames: Vec<[i32; 2]>,
}

impl InterleavedInt32Buffer {
    pub fn empty(sample_rate_hz: u32) -> Self {
        Self {
            sample_rate_hz,
            frames: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}
