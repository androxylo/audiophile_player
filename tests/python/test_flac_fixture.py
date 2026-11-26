# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

"""TDD tests for the copyright-free 24/96 algorithmic FLAC fixture and RAM loader."""

from pathlib import Path

import numpy as np
import soundfile as sf

from audiophile_poc.playback import load_flac_full_memory
from audiophile_poc.synth import INT32_FULL_SCALE

FIXTURE_PATH = (
    Path(__file__).resolve().parents[1] / "fixtures" / "audio" / "original_nocturne_24_96.flac"
)


def test_original_nocturne_fixture_exists_and_is_24bit_96khz_stereo_flac() -> None:
    assert FIXTURE_PATH.exists(), f"Missing synthetic fixture: {FIXTURE_PATH}"
    info = sf.info(str(FIXTURE_PATH))
    assert info.format == "FLAC"
    assert info.subtype == "PCM_24"
    assert info.samplerate == 96000
    assert info.channels == 2
    assert 13.5 <= info.duration <= 14.5


def test_original_nocturne_loads_cleanly_in_ram_with_true_stereo_and_safe_headroom() -> None:
    track = load_flac_full_memory(FIXTURE_PATH)
    assert track.sample_rate == 96000
    assert track.channels == 2
    assert track.pcm_int32.dtype == np.int32
    assert track.pcm_int32.flags["C_CONTIGUOUS"]

    # Non-silent musical content and genuine stereo soundstage (L != R)
    peak_norm = float(np.max(np.abs(track.pcm_int32))) / float(INT32_FULL_SCALE)
    peak_dbfs = 20.0 * np.log10(peak_norm)
    assert -13.0 <= peak_dbfs <= -11.0, f"Unexpected peak level: {peak_dbfs:.2f} dBFS"
    assert not np.array_equal(track.pcm_int32[:, 0], track.pcm_int32[:, 1])

    # Click-free silent start and finish
    assert abs(int(track.pcm_int32[0, 0])) <= 1024
    assert abs(int(track.pcm_int32[-1, 0])) <= 1024
