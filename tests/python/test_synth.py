# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

"""TDD unit tests for copyright-free synthetic test signal generation."""

import numpy as np
import pytest

from audiophile_poc.synth import INT32_FULL_SCALE, generate_test_tone_int32


def test_generate_test_tone_int32_shape_dtype_and_level() -> None:
    pcm = generate_test_tone_int32(
        sample_rate=384000,
        duration_s=0.25,
        freq_hz=1000.0,
        level_dbfs=-24.0,
        fade_ms=20.0,
    )
    assert pcm.dtype == np.int32
    assert pcm.shape == (96000, 2)
    assert pcm.flags["C_CONTIGUOUS"]
    # L and R channels are identical for mono-centered test tone
    assert np.array_equal(pcm[:, 0], pcm[:, 1])

    # Starts and ends cleanly at zero (click-free raised cosine fade)
    assert pcm[0, 0] == 0
    assert abs(int(pcm[-1, 0])) <= 10

    # Peak matches -24 dBFS within 0.05 dB
    expected_peak = (10.0 ** (-24.0 / 20.0)) * INT32_FULL_SCALE
    actual_peak = float(np.max(np.abs(pcm)))
    assert abs(actual_peak - expected_peak) / expected_peak < 1e-3


def test_generate_test_tone_safety_cap_prevents_loud_output() -> None:
    with pytest.raises(ValueError, match="safety cap"):
        generate_test_tone_int32(sample_rate=44100, level_dbfs=0.0)


def test_generate_test_tone_rejects_above_nyquist() -> None:
    with pytest.raises(ValueError, match="Nyquist"):
        generate_test_tone_int32(sample_rate=44100, freq_hz=22050.0)


def test_generate_test_tone_is_deterministic() -> None:
    a = generate_test_tone_int32(sample_rate=352800, duration_s=0.1, freq_hz=440.0)
    b = generate_test_tone_int32(sample_rate=352800, duration_s=0.1, freq_hz=440.0)
    assert np.array_equal(a, b)
