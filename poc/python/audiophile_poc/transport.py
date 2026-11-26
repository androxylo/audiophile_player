# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

"""Strictly zero-mixer Windows audio transport (WDM-KS and WASAPI Exclusive ONLY).

Policy enforced at runtime:
- ONLY 'Windows WDM-KS' (Direct Kernel Streaming) and 'Windows WASAPI' with
  exclusive=True are permitted.
- Shared Mode, MME, DirectSound, and floating-point wire formats are rejected
  with MixerPolicyViolationError. There is zero fallback to the Windows mixer.
- PortAudio dithering and clipping are explicitly disabled (dither_off=True,
  clip_off=True).
"""

from __future__ import annotations

import ctypes
import enum
import threading
from dataclasses import dataclass
from typing import Any, Iterable, Sequence

import numpy as np
import sounddevice as sd

ALLOWED_INTEGER_DTYPES = frozenset({"int16", "int32"})
HOSTAPI_WDM_KS = "Windows WDM-KS"
HOSTAPI_WASAPI = "Windows WASAPI"
DEFAULT_PROBE_RATES = (44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000)


class MixerPolicyViolationError(RuntimeError):
    """Raised whenever a caller attempts to use a Windows-mixer-involved mode."""


class TransportMode(str, enum.Enum):
    WDM_KS = "wdm-ks"
    WASAPI_EXCLUSIVE = "wasapi-exclusive"

    @property
    def hostapi_name(self) -> str:
        if self is TransportMode.WDM_KS:
            return HOSTAPI_WDM_KS
        if self is TransportMode.WASAPI_EXCLUSIVE:
            return HOSTAPI_WASAPI
        raise MixerPolicyViolationError(f"Unsupported transport mode: {self}")


@dataclass(frozen=True)
class OutputDeviceInfo:
    index: int
    name: str
    mode: TransportMode
    hostapi_name: str
    max_output_channels: int
    default_samplerate: float


@dataclass(frozen=True)
class RateProbeResult:
    sample_rate: int
    dtype: str
    check_ok: bool
    live_open_ok: bool
    error: str | None = None


def build_stream_kwargs(
    *,
    device_index: int,
    hostapi_name: str,
    mode: TransportMode,
    sample_rate: int,
    channels: int = 2,
    dtype: str = "int32",
    latency: float | str = 0.1,
) -> dict[str, Any]:
    """Build and strictly validate PortAudio OutputStream keyword arguments.

    Raises MixerPolicyViolationError if any parameter could involve the Windows
    mixer, sample-format dithering, clipping, or float wire output.
    """
    if mode is TransportMode.WDM_KS:
        if hostapi_name != HOSTAPI_WDM_KS:
            raise MixerPolicyViolationError(
                f"WDM-KS requires host API '{HOSTAPI_WDM_KS}', got '{hostapi_name}'"
            )
        extra_settings = None
    elif mode is TransportMode.WASAPI_EXCLUSIVE:
        if hostapi_name != HOSTAPI_WASAPI:
            raise MixerPolicyViolationError(
                f"WASAPI Exclusive requires host API '{HOSTAPI_WASAPI}', got '{hostapi_name}'"
            )
        extra_settings = sd.WasapiSettings(exclusive=True)
    else:
        raise MixerPolicyViolationError(f"Disallowed transport mode: {mode}")

    if dtype not in ALLOWED_INTEGER_DTYPES:
        raise MixerPolicyViolationError(
            f"Only integer PCM wire formats {sorted(ALLOWED_INTEGER_DTYPES)} are allowed, got '{dtype}'"
        )

    kwargs: dict[str, Any] = {
        "device": device_index,
        "samplerate": int(sample_rate),
        "channels": int(channels),
        "dtype": dtype,
        "latency": latency,
        "clip_off": True,
        "dither_off": True,
    }
    if extra_settings is not None:
        kwargs["extra_settings"] = extra_settings

    validate_stream_kwargs(kwargs, hostapi_name=hostapi_name)
    return kwargs


def validate_stream_kwargs(kwargs: dict[str, Any], *, hostapi_name: str) -> None:
    """Hard runtime invariant: guarantee Windows mixer can never be entered."""
    if hostapi_name == HOSTAPI_WDM_KS:
        pass  # WDM-KS opens the kernel streaming pin directly (no OS mixer).
    elif hostapi_name == HOSTAPI_WASAPI:
        extra = kwargs.get("extra_settings")
        # PortAudio's WasapiSettings stores the exclusive flag in paWasapiExclusive.
        if extra is None or not isinstance(extra, sd.WasapiSettings):
            raise MixerPolicyViolationError(
                "WASAPI requires sd.WasapiSettings(exclusive=True); Shared Mode is forbidden."
            )
        streaminfo = getattr(extra, "_streaminfo", None)
        flags = int(getattr(streaminfo, "flags", 0)) if streaminfo is not None else 0
        if not (flags & 1):
            # Verify paWinWasapiExclusive (0x1) bit is set on PaWasapiStreamInfo.
            raise MixerPolicyViolationError(
                "WASAPI Shared Mode is strictly forbidden (exclusive=True required)."
            )
    else:
        raise MixerPolicyViolationError(
            f"Host API '{hostapi_name}' is forbidden; only WDM-KS and WASAPI Exclusive are allowed."
        )

    if kwargs.get("dither_off") is not True:
        raise MixerPolicyViolationError("dither_off=True is mandatory to prevent PortAudio dithering.")
    if kwargs.get("clip_off") is not True:
        raise MixerPolicyViolationError("clip_off=True is mandatory to prevent PortAudio clipping.")
    if kwargs.get("dtype") not in ALLOWED_INTEGER_DTYPES:
        raise MixerPolicyViolationError(
            f"Floating-point or unsupported wire dtype '{kwargs.get('dtype')}' is forbidden."
        )


def list_zero_mixer_devices(mode: TransportMode | None = None) -> list[OutputDeviceInfo]:
    """List Windows output devices exposed under WDM-KS and/or WASAPI."""
    hostapis = sd.query_hostapis()
    devices = sd.query_devices()

    allowed_modes = (
        (mode,) if mode is not None else (TransportMode.WDM_KS, TransportMode.WASAPI_EXCLUSIVE)
    )
    allowed_names = {m.hostapi_name: m for m in allowed_modes}

    result: list[OutputDeviceInfo] = []
    for idx, dev in enumerate(devices):
        if dev["max_output_channels"] <= 0:
            continue
        api_name = hostapis[dev["hostapi"]]["name"]
        if api_name not in allowed_names:
            continue
        result.append(
            OutputDeviceInfo(
                index=idx,
                name=dev["name"],
                mode=allowed_names[api_name],
                hostapi_name=api_name,
                max_output_channels=int(dev["max_output_channels"]),
                default_samplerate=float(dev["default_samplerate"]),
            )
        )
    return result


def resolve_device(device_query: str | int, mode: TransportMode) -> OutputDeviceInfo:
    """Resolve a device by exact index or case-insensitive substring within the requested mode."""
    candidates = list_zero_mixer_devices(mode=mode)
    if isinstance(device_query, int) or (isinstance(device_query, str) and device_query.isdigit()):
        idx = int(device_query)
        for dev in candidates:
            if dev.index == idx:
                return dev
        raise ValueError(
            f"Device index {idx} is not an output device under '{mode.hostapi_name}'. "
            f"Run 'probe' to see valid indices."
        )

    needle = device_query.lower()
    matches = [d for d in candidates if needle in d.name.lower()]
    if not matches:
        raise ValueError(
            f"No output device matching '{device_query}' found under '{mode.hostapi_name}'."
        )
    if len(matches) > 1:
        names = ", ".join(f"[{m.index}] {m.name}" for m in matches)
        raise ValueError(f"Ambiguous device '{device_query}' under '{mode.hostapi_name}': {names}")
    return matches[0]


def probe_device(
    device: OutputDeviceInfo,
    rates: Sequence[int] = DEFAULT_PROBE_RATES,
    dtypes: Iterable[str] = ("int32", "int16"),
    live_open: bool = True,
) -> list[RateProbeResult]:
    """Probe exact rate & integer PCM container support under WDM-KS or WASAPI Exclusive."""
    results: list[RateProbeResult] = []
    for dtype in dtypes:
        for rate in rates:
            kwargs = build_stream_kwargs(
                device_index=device.index,
                hostapi_name=device.hostapi_name,
                mode=device.mode,
                sample_rate=rate,
                channels=2,
                dtype=dtype,
                latency="high",
            )
            check_kwargs = {
                k: v for k, v in kwargs.items() if k not in ("latency", "clip_off", "dither_off")
            }
            check_ok = False
            live_ok = False
            err_msg: str | None = None
            try:
                sd.check_output_settings(**check_kwargs)
                check_ok = True
            except Exception as exc:  # noqa: BLE001
                err_msg = str(exc)

            if live_open and check_ok:
                try:
                    # Open & close immediately with a silent callback to prove KsCreatePin /
                    # IAudioClient::Initialize truly succeeds on the hardware driver.
                    def _silent_cb(outdata: np.ndarray, frames: int, time_info: Any, status: Any) -> None:
                        outdata.fill(0)

                    with sd.OutputStream(callback=_silent_cb, **kwargs):
                        live_ok = True
                except Exception as exc:  # noqa: BLE001
                    err_msg = str(exc)

            results.append(
                RateProbeResult(
                    sample_rate=rate,
                    dtype=dtype,
                    check_ok=check_ok,
                    live_open_ok=live_ok,
                    error=err_msg,
                )
            )
    return results


def _enable_mmcss_pro_audio() -> ctypes.c_void_p | None:
    """Request Windows MMCSS 'Pro Audio' scheduling for the current thread (best-effort)."""
    try:
        avrt = ctypes.windll.avrt
        task_index = ctypes.c_ulong(0)
        avrt.AvSetMmThreadCharacteristicsW.argtypes = [ctypes.c_wchar_p, ctypes.POINTER(ctypes.c_ulong)]
        avrt.AvSetMmThreadCharacteristicsW.restype = ctypes.c_void_p
        handle = avrt.AvSetMmThreadCharacteristicsW("Pro Audio", ctypes.byref(task_index))
        return handle or None
    except Exception:  # noqa: BLE001
        return None


def _disable_mmcss(handle: ctypes.c_void_p | None) -> None:
    if not handle:
        return
    try:
        avrt = ctypes.windll.avrt
        avrt.AvRevertMmThreadCharacteristics.argtypes = [ctypes.c_void_p]
        avrt.AvRevertMmThreadCharacteristics.restype = ctypes.c_bool
        avrt.AvRevertMmThreadCharacteristics(handle)
    except Exception:  # noqa: BLE001
        pass


def play_pcm_buffer(
    pcm: np.ndarray,
    *,
    sample_rate: int,
    device: OutputDeviceInfo,
    latency: float | str = 0.1,
) -> dict[str, Any]:
    """Play a pre-loaded full-memory integer PCM buffer strictly via WDM-KS or WASAPI Exclusive.

    Uses PortAudio callback streaming (required by WDM-KS and reliable in WASAPI Exclusive).
    Returns stream diagnostics (negotiated latency, blocksize, underflow count).
    """
    if pcm.ndim != 2:
        raise ValueError(f"pcm buffer must be 2D (frames, channels), got shape {pcm.shape}")
    dtype = pcm.dtype.name
    if dtype not in ALLOWED_INTEGER_DTYPES:
        raise MixerPolicyViolationError(
            f"pcm dtype must be integer PCM {sorted(ALLOWED_INTEGER_DTYPES)}, got '{dtype}'"
        )

    pcm = np.ascontiguousarray(pcm)
    total_frames, channels = pcm.shape
    kwargs = build_stream_kwargs(
        device_index=device.index,
        hostapi_name=device.hostapi_name,
        mode=device.mode,
        sample_rate=sample_rate,
        channels=channels,
        dtype=dtype,
        latency=latency,
    )

    cursor = 0
    underflows = 0
    done = threading.Event()
    mmcss_registered = False

    def _callback(outdata: np.ndarray, frames: int, time_info: Any, status: sd.CallbackFlags) -> None:
        nonlocal cursor, underflows, mmcss_registered
        if not mmcss_registered:
            _enable_mmcss_pro_audio()
            mmcss_registered = True
        if status.output_underflow:
            underflows += 1

        remaining = total_frames - cursor
        if remaining <= 0:
            outdata.fill(0)
            raise sd.CallbackStop()

        n = min(frames, remaining)
        outdata[:n] = pcm[cursor : cursor + n]
        if n < frames:
            outdata[n:].fill(0)
            cursor = total_frames
            raise sd.CallbackStop()
        cursor += n

    stream_info: dict[str, Any] = {}
    with sd.OutputStream(callback=_callback, finished_callback=done.set, **kwargs) as stream:
        stream_info["actual_samplerate"] = stream.samplerate
        stream_info["actual_latency_s"] = stream.latency
        stream_info["actual_blocksize"] = stream.blocksize
        stream_info["actual_dtype"] = stream.dtype
        done.wait()

    stream_info["frames_played"] = cursor
    stream_info["underflows"] = underflows
    stream_info["mode"] = device.mode.value
    stream_info["hostapi"] = device.hostapi_name
    stream_info["device_name"] = device.name
    return stream_info
