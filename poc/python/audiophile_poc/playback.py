"""Full-memory RAM FLAC loader & zero-mixer playback helper."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np
import soundfile as sf

from .transport import OutputDeviceInfo, play_pcm_buffer


@dataclass(frozen=True)
class LoadedTrack:
    path: Path
    sample_rate: int
    channels: int
    subtype: str
    pcm_int32: np.ndarray  # Contiguous (frames, channels) MSB-aligned int32


def load_flac_full_memory(path: str | Path) -> LoadedTrack:
    """Decode an entire FLAC file into RAM as exact MSB-aligned int32 (zero float conversion)."""
    p = Path(path)
    info = sf.info(str(p))
    # libsndfile reads 16-bit and 24-bit FLAC losslessly into MSB-aligned int32 (pure bit-shift).
    pcm, sample_rate = sf.read(str(p), dtype="int32", always_2d=True)
    pcm = np.ascontiguousarray(pcm, dtype=np.int32)
    return LoadedTrack(
        path=p,
        sample_rate=int(sample_rate),
        channels=int(pcm.shape[1]),
        subtype=str(info.subtype),
        pcm_int32=pcm,
    )


def play_flac_file(
    path: str | Path,
    *,
    device: OutputDeviceInfo,
    latency: float | str = 0.1,
) -> dict[str, Any]:
    """Decode a FLAC completely into RAM and play strictly via WDM-KS or WASAPI Exclusive."""
    track = load_flac_full_memory(path)
    result = play_pcm_buffer(
        track.pcm_int32,
        sample_rate=track.sample_rate,
        device=device,
        latency=latency,
    )
    result["file"] = str(track.path)
    result["subtype"] = track.subtype
    result["channels"] = track.channels
    return result
