# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

"""Copyright-free deterministic synthetic audio signals for TDD and hardware verification.

All synthesis runs strictly in IEEE 754 float64 and converts once to integer PCM
at the final step. Test-tone amplitude is strictly capped to protect external
audiophile amplifiers when running with no digital volume control.
"""

from __future__ import annotations

import numpy as np

MAX_SAFE_TEST_TONE_DBFS = -6.0
INT32_FULL_SCALE = (1 << 31) - 1  # 2_147_483_647


def generate_test_tone_int32(
    sample_rate: int,
    duration_s: float = 1.5,
    freq_hz: float = 440.0,
    level_dbfs: float = -24.0,
    fade_ms: float = 50.0,
    channels: int = 2,
) -> np.ndarray:
    """Generate a smooth, click-free stereo sine wave quantized once to int32.

    Args:
        sample_rate: Target PCM sample rate in Hz (e.g. 44100, 96000, 352800, 384000).
        duration_s: Total tone duration in seconds.
        freq_hz: Sine frequency in Hz (must be strictly below Nyquist).
        level_dbfs: Peak amplitude in dBFS (default -24 dBFS; capped at <= -6 dBFS).
        fade_ms: Raised-cosine fade-in and fade-out duration in milliseconds.
        channels: Number of channels (default 2 for stereo).

    Returns:
        Contiguous C-order int32 numpy array with shape (num_frames, channels).
    """
    if sample_rate <= 0:
        raise ValueError(f"sample_rate must be positive, got {sample_rate}")
    if duration_s <= 0:
        raise ValueError(f"duration_s must be positive, got {duration_s}")
    if not (0.0 < freq_hz < sample_rate / 2.0):
        raise ValueError(f"freq_hz ({freq_hz}) must lie in (0, Nyquist={sample_rate / 2.0})")
    if level_dbfs > MAX_SAFE_TEST_TONE_DBFS:
        raise ValueError(
            f"level_dbfs ({level_dbfs}) exceeds safety cap ({MAX_SAFE_TEST_TONE_DBFS} dBFS)"
        )
    if channels < 1:
        raise ValueError(f"channels must be >= 1, got {channels}")

    num_frames = int(round(sample_rate * duration_s))
    t = np.arange(num_frames, dtype=np.float64) / float(sample_rate)
    amplitude = 10.0 ** (level_dbfs / 20.0)
    wave = amplitude * np.sin(2.0 * np.pi * freq_hz * t)

    # Raised-cosine fade-in and fade-out to eliminate start/stop clicks
    fade_frames = min(int(round(sample_rate * fade_ms / 1000.0)), num_frames // 2)
    if fade_frames > 0:
        ramp = 0.5 * (1.0 - np.cos(np.pi * np.arange(fade_frames, dtype=np.float64) / fade_frames))
        wave[:fade_frames] *= ramp
        wave[-fade_frames:] *= ramp[::-1]

    # Single final float64 -> int32 quantization
    pcm_mono = np.rint(wave * float(INT32_FULL_SCALE)).astype(np.int32)
    pcm_multi = np.repeat(pcm_mono[:, np.newaxis], channels, axis=1)
    return np.ascontiguousarray(pcm_multi, dtype=np.int32)
