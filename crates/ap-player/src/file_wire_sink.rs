// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Hardware-free [`FileWireSink`] implementing [`BlockStreamAudioSink`] (`PIPELINE_TESTS_REVIEW.md` L11).
//!
//! Writes the exact little-endian wire bytes produced by [`WaveRtFramePump::fill_slice`] (and
//! [`WaveRtFramePump::fill_fade_out_slice`] exclusively on stop) to disk while recording per-segment
//! sample rates, wire formats, byte offsets, frame counts, and monotonic timestamps.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use ap_core::buffer::InterleavedInt32Buffer;
use ap_core::format::DacWireFormat;
use ap_output_win::sink::{OutputError, PlaybackStats};
use ap_output_win::wdm_ks::{StreamBlock, WaveRtFramePump};

use crate::engine::BlockStreamAudioSink;

/// Metadata recorded for each contiguous `play_block_stream` hardware-pin session captured by
/// [`FileWireSink`].
#[derive(Debug, Clone)]
pub struct CapturedWireSegment {
    pub segment_index: usize,
    pub sample_rate_hz: u32,
    pub wire_format: DacWireFormat,
    pub byte_offset: u64,
    pub byte_len: u64,
    pub frames_written: usize,
    pub started_at_us: u128,
    pub ended_at_us: u128,
    /// True iff `stop_flag` interrupted this segment and triggered a linear stop fade-out.
    pub had_fade_out: bool,
}

/// Optional callback invoked after each block is encoded into [`FileWireSink`]:
/// receives `(segment_index, segment_frames_so_far, total_frames_so_far)`.
pub type WireBlockHook = Box<dyn FnMut(usize, usize, usize) + Send>;

/// File-backed [`BlockStreamAudioSink`] capturing exact WaveRT wire bytes + timestamps without
/// touching any audio hardware.
pub struct FileWireSink {
    path: PathBuf,
    file: File,
    stop_flag: Arc<AtomicBool>,
    origin: Instant,
    bytes_written: u64,
    total_frames_written: usize,
    segments: Vec<CapturedWireSegment>,
    block_hook: Option<WireBlockHook>,
    /// Stop fade-out length in frames (matches WaveRT short click-free ramp on stop).
    fade_out_frames: usize,
}

impl FileWireSink {
    pub fn create(path: PathBuf, stop_flag: Arc<AtomicBool>) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("failed to create FileWireSink '{}': {e}", path.display()))?;
        Ok(Self {
            path,
            file,
            stop_flag,
            origin: Instant::now(),
            bytes_written: 0,
            total_frames_written: 0,
            segments: Vec::new(),
            block_hook: None,
            fade_out_frames: 256,
        })
    }

    pub fn with_block_hook(mut self, hook: WireBlockHook) -> Self {
        self.block_hook = Some(hook);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn segments(&self) -> &[CapturedWireSegment] {
        &self.segments
    }

    /// Read back the exact wire bytes captured for `segment_index`.
    pub fn read_segment_bytes(&mut self, segment_index: usize) -> Result<Vec<u8>, String> {
        let seg = self
            .segments
            .get(segment_index)
            .ok_or_else(|| format!("segment {segment_index} out of range"))?;
        self.file
            .flush()
            .map_err(|e| format!("FileWireSink flush failed: {e}"))?;
        self.file
            .seek(SeekFrom::Start(seg.byte_offset))
            .map_err(|e| format!("FileWireSink seek failed: {e}"))?;
        let mut buf = vec![0u8; seg.byte_len as usize];
        self.file
            .read_exact(&mut buf)
            .map_err(|e| format!("FileWireSink read_exact failed: {e}"))?;
        self.file
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("FileWireSink seek end failed: {e}"))?;
        Ok(buf)
    }
}

impl BlockStreamAudioSink for FileWireSink {
    fn play_block_stream<F>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        next_block: F,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> StreamBlock,
    {
        self.play_block_stream_with_recycler(sample_rate_hz, wire_format, next_block, |_| {})
    }

    fn play_block_stream_with_recycler<F, R>(
        &mut self,
        sample_rate_hz: u32,
        wire_format: DacWireFormat,
        mut next_block: F,
        mut recycle_block: R,
    ) -> Result<PlaybackStats, OutputError>
    where
        F: FnMut() -> StreamBlock,
        R: FnMut(InterleavedInt32Buffer) + 'static,
    {
        let seg_idx = self.segments.len();
        let byte_offset = self.bytes_written;
        let started_at_us = self.origin.elapsed().as_micros();
        let bpf = wire_format.bytes_per_frame();
        let mut seg_frames = 0_usize;
        let mut seg_bytes = 0_u64;
        let mut had_fade_out = false;
        let mut underruns = 0_usize;
        let mut notifications = 0_usize;
        let mut last_frames: Vec<[i32; 2]> = Vec::new();

        loop {
            if self.stop_flag.load(Ordering::Acquire) {
                if !last_frames.is_empty() {
                    let n = last_frames.len().min(self.fade_out_frames);
                    let tail = &last_frames[last_frames.len() - n..];
                    let mut fade_bytes = vec![0u8; n * bpf];
                    let mut pump = WaveRtFramePump::new(tail, wire_format);
                    let written = pump.fill_fade_out_slice(&mut fade_bytes);
                    let used = written * bpf;
                    self.file
                        .write_all(&fade_bytes[..used])
                        .map_err(|e| OutputError::InvalidBuffer(e.to_string()))?;
                    seg_frames += written;
                    seg_bytes += used as u64;
                    self.bytes_written += used as u64;
                    self.total_frames_written += written;
                    had_fade_out = true;
                }
                break;
            }

            match next_block() {
                StreamBlock::Ready(pcm) => {
                    if !pcm.frames.is_empty() {
                        let mut bytes = vec![0u8; pcm.frames.len() * bpf];
                        let mut pump = WaveRtFramePump::new(&pcm.frames, wire_format);
                        let written = pump.fill_slice(&mut bytes);
                        let used = written * bpf;
                        self.file
                            .write_all(&bytes[..used])
                            .map_err(|e| OutputError::InvalidBuffer(e.to_string()))?;
                        seg_frames += written;
                        seg_bytes += used as u64;
                        self.bytes_written += used as u64;
                        self.total_frames_written += written;
                        notifications += 1;

                        let keep = pcm.frames.len().min(self.fade_out_frames);
                        last_frames.clear();
                        last_frames.extend_from_slice(&pcm.frames[pcm.frames.len() - keep..]);
                    }
                    recycle_block(pcm);
                    if let Some(ref mut hook) = self.block_hook {
                        hook(seg_idx, seg_frames, self.total_frames_written);
                    }
                }
                StreamBlock::Underrun => {
                    underruns += 1;
                    std::thread::yield_now();
                }
                StreamBlock::EndOfTrack => break,
            }
        }

        let ended_at_us = self.origin.elapsed().as_micros();
        self.segments.push(CapturedWireSegment {
            segment_index: seg_idx,
            sample_rate_hz,
            wire_format,
            byte_offset,
            byte_len: seg_bytes,
            frames_written: seg_frames,
            started_at_us,
            ended_at_us,
            had_fade_out,
        });

        Ok(PlaybackStats {
            device_name: format!("FileWireSink({})", self.path.display()),
            sample_rate_hz,
            wire_format,
            frames_played: seg_frames,
            wavert_buffer_bytes: (4096 * bpf) as u32,
            notifications,
            mmcss_pro_audio: false,
            underrun_halves: underruns,
        })
    }
}
