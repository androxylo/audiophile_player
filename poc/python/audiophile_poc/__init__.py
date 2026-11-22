"""Zero-mixer Windows audio prototypes for audiophile_player."""

from .compose import compose_nocturne_float64, write_nocturne_flac
from .playback import LoadedTrack, load_flac_full_memory, play_flac_file
from .synth import generate_test_tone_int32
from .transport import (
    MixerPolicyViolationError,
    TransportMode,
    list_zero_mixer_devices,
    play_pcm_buffer,
    probe_device,
)

__all__ = [
    "LoadedTrack",
    "MixerPolicyViolationError",
    "TransportMode",
    "compose_nocturne_float64",
    "generate_test_tone_int32",
    "list_zero_mixer_devices",
    "load_flac_full_memory",
    "play_flac_file",
    "play_pcm_buffer",
    "probe_device",
    "write_nocturne_flac",
]
