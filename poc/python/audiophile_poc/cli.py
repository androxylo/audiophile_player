# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

"""Minimal CLI for probing Windows WDM-KS / WASAPI Exclusive and playing safe test tones."""

from __future__ import annotations

import argparse
import sys

from .compose import write_nocturne_flac
from .playback import play_flac_file
from .synth import generate_test_tone_int32
from .transport import (
    DEFAULT_PROBE_RATES,
    TransportMode,
    list_zero_mixer_devices,
    play_pcm_buffer,
    probe_device,
    resolve_device,
)


def cmd_probe(args: argparse.Namespace) -> int:
    mode = TransportMode(args.mode) if args.mode != "all" else None
    devices = list_zero_mixer_devices(mode=mode)
    if not devices:
        print("No Windows WDM-KS or WASAPI output devices found.")
        return 1

    print("=== Zero-Mixer Output Devices (WDM-KS & WASAPI Exclusive) ===")
    for dev in devices:
        print(
            f"  [{dev.index:>2}] {dev.mode.value:<16} | {dev.hostapi_name:<14} | "
            f"ch={dev.max_output_channels} | default={int(dev.default_samplerate)} Hz | {dev.name}"
        )

    if args.device is not None:
        target_modes = (
            [TransportMode(args.mode)]
            if args.mode != "all"
            else [TransportMode.WDM_KS, TransportMode.WASAPI_EXCLUSIVE]
        )
        for m in target_modes:
            try:
                dev = resolve_device(args.device, mode=m)
            except ValueError as exc:
                print(f"\n[{m.value}] {exc}")
                continue
            print(f"\n--- Probing [{dev.index}] {dev.name} ({dev.mode.value}) ---")
            results = probe_device(dev, rates=DEFAULT_PROBE_RATES, live_open=not args.no_live_open)
            for r in results:
                status = "OK (live open)" if r.live_open_ok else ("check-only OK" if r.check_ok else "UNSUPPORTED")
                detail = f"  [{r.error}]" if r.error and not r.live_open_ok else ""
                print(f"  {r.dtype:<5} @ {r.sample_rate:>6} Hz : {status}{detail}")
    return 0


def cmd_test_tone(args: argparse.Namespace) -> int:
    mode = TransportMode(args.mode)
    dev = resolve_device(args.device, mode=mode)
    print(
        f"Generating {args.freq} Hz sine ({args.level} dBFS, {args.duration}s) "
        f"@ {args.rate} Hz int32..."
    )
    pcm = generate_test_tone_int32(
        sample_rate=args.rate,
        duration_s=args.duration,
        freq_hz=args.freq,
        level_dbfs=args.level,
    )
    if args.dtype == "int16":
        pcm = (pcm >> 16).astype("int16")

    print(f"Opening [{dev.index}] '{dev.name}' via {dev.mode.value} ({dev.hostapi_name})...")
    info = play_pcm_buffer(pcm, sample_rate=args.rate, device=dev, latency=args.latency)
    print(
        f"Playback complete: {info['frames_played']} frames, "
        f"rate={info['actual_samplerate']} Hz, dtype={info['actual_dtype']}, "
        f"latency={info['actual_latency_s'] * 1000:.1f} ms, underflows={info['underflows']}"
    )
    return 0


def cmd_play_file(args: argparse.Namespace) -> int:
    mode = TransportMode(args.mode)
    dev = resolve_device(args.device, mode=mode)
    print(f"Loading '{args.file}' into RAM (full-memory int32)...")
    print(f"Opening [{dev.index}] '{dev.name}' via {dev.mode.value} ({dev.hostapi_name})...")
    info = play_flac_file(args.file, device=dev, latency=args.latency)
    print(
        f"Playback complete: {info['frames_played']} frames, "
        f"rate={info['actual_samplerate']} Hz ({info['subtype']}), "
        f"dtype={info['actual_dtype']}, latency={info['actual_latency_s'] * 1000:.1f} ms, "
        f"underflows={info['underflows']}"
    )
    return 0


def cmd_generate_fixture(args: argparse.Namespace) -> int:
    out = write_nocturne_flac(
        output_path=args.output,
        sample_rate=args.rate,
        target_peak_dbfs=args.level,
    )
    print(f"Generated copyright-free 24-bit/{args.rate} Hz FLAC fixture: {out}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="audiophile-poc",
        description="Zero-mixer Windows audio transport POC (WDM-KS & WASAPI Exclusive ONLY).",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_probe = sub.add_parser("probe", help="List and probe WDM-KS / WASAPI Exclusive devices.")
    p_probe.add_argument(
        "--mode",
        choices=["all", "wdm-ks", "wasapi-exclusive"],
        default="all",
        help="Transport mode filter (default: all).",
    )
    p_probe.add_argument(
        "--device",
        default=None,
        help="Device index or case-insensitive name substring to probe across sample rates.",
    )
    p_probe.add_argument(
        "--no-live-open",
        action="store_true",
        help="Only run PortAudio check_output_settings without opening hardware stream.",
    )
    p_probe.set_defaults(func=cmd_probe)

    p_tone = sub.add_parser("test-tone", help="Play a gentle, click-free synthetic test tone.")
    p_tone.add_argument(
        "--mode",
        choices=["wdm-ks", "wasapi-exclusive"],
        default="wdm-ks",
        help="Zero-mixer transport mode (default: wdm-ks).",
    )
    p_tone.add_argument("--device", required=True, help="Device index or name substring.")
    p_tone.add_argument("--rate", type=int, default=44100, help="Sample rate in Hz (default: 44100).")
    p_tone.add_argument(
        "--dtype",
        choices=["int32", "int16"],
        default="int32",
        help="Integer PCM wire format (default: int32).",
    )
    p_tone.add_argument("--freq", type=float, default=440.0, help="Tone frequency Hz (default: 440).")
    p_tone.add_argument(
        "--level",
        type=float,
        default=-24.0,
        help="Peak level in dBFS (default: -24.0, hard-capped <= -6.0).",
    )
    p_tone.add_argument("--duration", type=float, default=1.5, help="Duration seconds (default: 1.5).")
    p_tone.add_argument("--latency", type=float, default=0.1, help="Target latency seconds (default: 0.1).")
    p_tone.set_defaults(func=cmd_test_tone)

    p_play = sub.add_parser("play-file", help="Play a FLAC file from RAM via WDM-KS or WASAPI Exclusive.")
    p_play.add_argument("file", help="Path to FLAC file.")
    p_play.add_argument(
        "--mode",
        choices=["wdm-ks", "wasapi-exclusive"],
        default="wdm-ks",
        help="Zero-mixer transport mode (default: wdm-ks).",
    )
    p_play.add_argument("--device", required=True, help="Device index or name substring.")
    p_play.add_argument("--latency", type=float, default=0.1, help="Target latency seconds (default: 0.1).")
    p_play.set_defaults(func=cmd_play_file)

    p_gen = sub.add_parser(
        "generate-fixture",
        help="Generate the original copyright-free 24/96 chamber-nocturne FLAC fixture.",
    )
    p_gen.add_argument(
        "--output",
        default="tests/fixtures/audio/original_nocturne_24_96.flac",
        help="Output FLAC file path.",
    )
    p_gen.add_argument("--rate", type=int, default=96000, help="Sample rate Hz (default: 96000).")
    p_gen.add_argument("--level", type=float, default=-12.0, help="Peak level dBFS (default: -12.0).")
    p_gen.set_defaults(func=cmd_generate_fixture)

    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
