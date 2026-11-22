"""TDD unit tests enforcing the strict zero-Windows-mixer transport invariant."""

import pytest
import sounddevice as sd

from audiophile_poc.transport import (
    HOSTAPI_WASAPI,
    HOSTAPI_WDM_KS,
    MixerPolicyViolationError,
    TransportMode,
    build_stream_kwargs,
    validate_stream_kwargs,
)


def test_build_wdm_ks_kwargs_enforces_integer_no_dither_no_clip() -> None:
    kw = build_stream_kwargs(
        device_index=5,
        hostapi_name=HOSTAPI_WDM_KS,
        mode=TransportMode.WDM_KS,
        sample_rate=384000,
        dtype="int32",
    )
    assert kw["device"] == 5
    assert kw["samplerate"] == 384000
    assert kw["dtype"] == "int32"
    assert kw["dither_off"] is True
    assert kw["clip_off"] is True
    assert "extra_settings" not in kw


def test_build_wasapi_exclusive_kwargs_sets_exclusive_true() -> None:
    kw = build_stream_kwargs(
        device_index=3,
        hostapi_name=HOSTAPI_WASAPI,
        mode=TransportMode.WASAPI_EXCLUSIVE,
        sample_rate=352800,
        dtype="int32",
    )
    assert isinstance(kw["extra_settings"], sd.WasapiSettings)
    assert kw["dither_off"] is True
    assert kw["clip_off"] is True


def test_wasapi_shared_mode_is_strictly_rejected() -> None:
    shared_kwargs = {
        "device": 3,
        "samplerate": 48000,
        "channels": 2,
        "dtype": "int32",
        "dither_off": True,
        "clip_off": True,
        "extra_settings": sd.WasapiSettings(exclusive=False),
    }
    with pytest.raises(MixerPolicyViolationError, match="Shared Mode"):
        validate_stream_kwargs(shared_kwargs, hostapi_name=HOSTAPI_WASAPI)


def test_wasapi_missing_extra_settings_is_strictly_rejected() -> None:
    missing_kwargs = {
        "device": 3,
        "samplerate": 48000,
        "channels": 2,
        "dtype": "int32",
        "dither_off": True,
        "clip_off": True,
    }
    with pytest.raises(MixerPolicyViolationError, match="Shared Mode"):
        validate_stream_kwargs(missing_kwargs, hostapi_name=HOSTAPI_WASAPI)


@pytest.mark.parametrize("bad_api", ["MME", "Windows DirectSound", "Unknown API"])
def test_mixer_involved_hostapis_are_strictly_rejected(bad_api: str) -> None:
    kwargs = {
        "device": 1,
        "samplerate": 44100,
        "channels": 2,
        "dtype": "int32",
        "dither_off": True,
        "clip_off": True,
    }
    with pytest.raises(MixerPolicyViolationError):
        validate_stream_kwargs(kwargs, hostapi_name=bad_api)


def test_float32_wire_format_is_rejected() -> None:
    with pytest.raises(MixerPolicyViolationError, match="integer PCM"):
        build_stream_kwargs(
            device_index=5,
            hostapi_name=HOSTAPI_WDM_KS,
            mode=TransportMode.WDM_KS,
            sample_rate=44100,
            dtype="float32",
        )


def test_dither_on_or_clip_on_is_rejected() -> None:
    bad_dither = {
        "device": 5,
        "samplerate": 44100,
        "channels": 2,
        "dtype": "int32",
        "dither_off": False,
        "clip_off": True,
    }
    with pytest.raises(MixerPolicyViolationError, match="dither_off"):
        validate_stream_kwargs(bad_dither, hostapi_name=HOSTAPI_WDM_KS)

    bad_clip = {
        "device": 5,
        "samplerate": 44100,
        "channels": 2,
        "dtype": "int32",
        "dither_off": True,
        "clip_off": False,
    }
    with pytest.raises(MixerPolicyViolationError, match="clip_off"):
        validate_stream_kwargs(bad_clip, hostapi_name=HOSTAPI_WDM_KS)
