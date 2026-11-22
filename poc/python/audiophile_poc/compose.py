"""Original, copyright-free (CC0) algorithmic chamber-nocturne synthesizer.

Generates a lush, warm stereo 24-bit / 96 kHz acoustic-inspired musical fixture
(felt piano + bowed cello bass + crystalline celesta + stereo chamber reverb)
strictly in IEEE 754 float64 with zero external samples or copyrighted material.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import soundfile as sf

DEFAULT_SAMPLE_RATE = 96_000
INT24_MAX = (1 << 23) - 1


def _midi_to_hz(midi: float) -> float:
    return 440.0 * (2.0 ** ((midi - 69.0) / 12.0))


def _render_felt_piano_note(
    freq: float,
    duration_s: float,
    velocity: float,
    sample_rate: int,
    inharmonicity: float = 1.6e-4,
) -> np.ndarray:
    """Physically inspired felt-piano note: inharmonic partials with frequency-dependent decay."""
    n = int(round(duration_s * sample_rate))
    t = np.arange(n, dtype=np.float64) / float(sample_rate)
    out = np.zeros(n, dtype=np.float64)

    # Soft felt hammer attack (6 ms raised cosine)
    atk_n = max(1, int(round(0.006 * sample_rate)))
    atk = np.ones(n, dtype=np.float64)
    atk[:atk_n] = 0.5 * (1.0 - np.cos(np.pi * np.arange(atk_n, dtype=np.float64) / atk_n))

    nyquist = sample_rate * 0.45
    max_partials = min(14, int(nyquist // max(freq, 1.0)))

    for k in range(1, max_partials + 1):
        fk = k * freq * np.sqrt(1.0 + inharmonicity * (k * k))
        if fk >= nyquist:
            break
        # Warm felt spectrum: rapid roll-off on higher partials, slight velocity brightness
        amp = velocity / (k ** (1.55 - 0.25 * velocity))
        # Higher partials decay significantly faster (natural string damping)
        tau = (2.6 / (k ** 0.72)) * (260.0 / max(freq, 80.0)) ** 0.35
        env = np.exp(-t / max(tau, 0.08))
        # Subtle dual-string chorus detuning on lower partials
        phase_detune = 0.00035 * fk if k <= 4 else 0.0
        if phase_detune > 0.0:
            partial = 0.65 * np.sin(2.0 * np.pi * fk * t) + 0.35 * np.sin(
                2.0 * np.pi * (fk + phase_detune) * t + 0.4 * k
            )
        else:
            partial = np.sin(2.0 * np.pi * fk * t + 0.25 * k)
        out += amp * env * partial

    # Smooth release tail
    rel_n = min(n // 4, int(round(0.18 * sample_rate)))
    if rel_n > 0:
        rel = 0.5 * (1.0 + np.cos(np.pi * np.arange(rel_n, dtype=np.float64) / rel_n))
        out[-rel_n:] *= rel

    return out * atk


def _render_cello_pad(
    freq: float,
    duration_s: float,
    amplitude: float,
    sample_rate: int,
) -> np.ndarray:
    """Warm bowed cello/string voice with gentle expressive vibrato."""
    n = int(round(duration_s * sample_rate))
    t = np.arange(n, dtype=np.float64) / float(sample_rate)

    # Gradual vibrato bloom (delayed onset, 5.1 Hz, +/- 8 cents)
    vib_depth = 0.0045 * (1.0 - np.exp(-t / 0.65))
    inst_phase = 2.0 * np.pi * freq * (
        t - (vib_depth / (2.0 * np.pi * 5.1)) * np.cos(2.0 * np.pi * 5.1 * t)
    )

    out = np.zeros(n, dtype=np.float64)
    nyquist = sample_rate * 0.45
    # Warm cello harmonic weights (fundamental + mellow 2nd/3rd/4th/5th/6th)
    weights = (1.0, 0.52, 0.30, 0.14, 0.08, 0.04, 0.02)
    for k, w in enumerate(weights, start=1):
        if k * freq >= nyquist:
            break
        out += w * np.sin(k * inst_phase + 0.15 * k)

    # Slow bow swell in and out
    atk_n = min(n // 2, int(round(0.55 * sample_rate)))
    rel_n = min(n // 2, int(round(0.75 * sample_rate)))
    env = np.ones(n, dtype=np.float64)
    env[:atk_n] = 0.5 * (1.0 - np.cos(np.pi * np.arange(atk_n, dtype=np.float64) / atk_n))
    env[-rel_n:] = 0.5 * (1.0 + np.cos(np.pi * np.arange(rel_n, dtype=np.float64) / rel_n))
    return amplitude * out * env


def _apply_stereo_chamber_reverb(stereo: np.ndarray, sample_rate: int) -> np.ndarray:
    """Deterministic stereo early-reflection + smooth diffused chamber tail in float64."""
    n = stereo.shape[0]
    wet = np.zeros_like(stereo, dtype=np.float64)

    # Asymmetric early reflections (ms, gain, cross-feed) for realistic room width
    taps = (
        (17.3, 0.24, 0.10),
        (29.7, 0.20, 0.14),
        (43.1, 0.17, 0.18),
        (61.9, 0.14, 0.22),
        (89.3, 0.11, 0.26),
        (127.7, 0.085, 0.30),
        (173.1, 0.065, 0.34),
        (233.9, 0.048, 0.38),
        (311.3, 0.035, 0.42),
        (409.1, 0.024, 0.45),
    )
    for ms, gain, cross in taps:
        d = int(round(ms * sample_rate / 1000.0))
        if d >= n:
            continue
        # Slight opposite-ear offset for stereo spaciousness
        d_r = min(n - 1, d + int(round(2.7 * sample_rate / 1000.0)))
        src_l = stereo[:-d, 0]
        src_r = stereo[:-d_r, 1]
        wet[d:, 0] += gain * ((1.0 - cross) * src_l + cross * stereo[:-d, 1])
        wet[d_r:, 1] += gain * ((1.0 - cross) * src_r + cross * stereo[:-d_r, 0])

    # Gentle air absorption on the wet reverb tail (1-pole low-pass ~4.5 kHz)
    alpha = 1.0 - np.exp(-2.0 * np.pi * 4500.0 / float(sample_rate))
    # Fast vectorized IIR via scipy if available, else 2-tap warm smoothing
    try:
        from scipy.signal import lfilter

        b = np.array([alpha], dtype=np.float64)
        a = np.array([1.0, -(1.0 - alpha)], dtype=np.float64)
        wet[:, 0] = lfilter(b, a, wet[:, 0])
        wet[:, 1] = lfilter(b, a, wet[:, 1])
    except ImportError:
        pass

    return stereo + 0.42 * wet


def compose_nocturne_float64(
    sample_rate: int = DEFAULT_SAMPLE_RATE,
    target_peak_dbfs: float = -12.0,
) -> np.ndarray:
    """Compose a 12-second original chamber nocturne in D minor9 (float64 stereo).

    Progression (original algorithmic composition, CC0):
      Bar 1 (0.0–3.0 s):  Dm9     (D2 cello; D3, A3, E4, F4, A4 arpeggio)
      Bar 2 (3.0–6.0 s):  Bbmaj7#11 (Bb1 cello; Bb2, F3, A3, D4, E4)
      Bar 3 (6.0–9.0 s):  Gm9 → A7sus4 (G1→A1 cello; G2, D3, A3, Bb3, F4 → E4)
      Bar 4 (9.0–12.0 s): Dm9 resolution (D2 cello; D3, A3, E4, F4, A4, D5)
    """
    duration_s = 12.0
    total_frames = int(round(duration_s * sample_rate))
    mix = np.zeros((total_frames, 2), dtype=np.float64)

    def _add_voice(mono: np.ndarray, start_s: float, pan: float) -> None:
        # Equal-power stereo panning: pan in [-1.0 (L), +1.0 (R)]
        idx = int(round(start_s * sample_rate))
        end = min(total_frames, idx + mono.shape[0])
        if idx >= total_frames or end <= idx:
            return
        seg = mono[: end - idx]
        theta = (pan + 1.0) * (np.pi / 4.0)
        mix[idx:end, 0] += seg * np.cos(theta)
        mix[idx:end, 1] += seg * np.sin(theta)

    # 1. Warm bowed cello bass foundation (centered with subtle warmth)
    cello_notes = (
        (38, 0.05, 3.30, 0.26, -0.08),  # D2
        (34, 3.00, 3.30, 0.25, -0.05),  # Bb1
        (31, 6.00, 1.85, 0.24, -0.06),  # G1
        (33, 7.65, 1.75, 0.25, 0.04),   # A1
        (38, 9.05, 2.85, 0.27, 0.00),   # D2 final resolution
    )
    for midi, start_s, dur_s, amp, pan in cello_notes:
        voice = _render_cello_pad(_midi_to_hz(midi), dur_s, amp, sample_rate)
        _add_voice(voice, start_s, pan)

    # 2. Felt piano arpeggiated harmony & lyrical melody (natural stereo keyboard spread)
    # Tuples: (midi_note, start_s, dur_s, velocity)
    piano_events = (
        # Bar 1: Dm9
        (50, 0.10, 3.2, 0.58),  # D3
        (57, 0.45, 2.9, 0.52),  # A3
        (64, 0.85, 2.7, 0.62),  # E4
        (65, 1.35, 2.5, 0.68),  # F4 (melody)
        (69, 1.95, 2.3, 0.64),  # A4
        (64, 2.50, 1.9, 0.50),  # E4
        # Bar 2: Bbmaj7(#11)
        (46, 3.05, 3.1, 0.56),  # Bb2
        (53, 3.40, 2.8, 0.50),  # F3
        (57, 3.80, 2.6, 0.54),  # A3
        (62, 4.20, 2.5, 0.64),  # D4
        (64, 4.75, 2.3, 0.66),  # E4
        (62, 5.40, 1.9, 0.54),  # D4
        # Bar 3: Gm9 -> A7sus4
        (43, 6.05, 2.2, 0.56),  # G2
        (50, 6.38, 2.1, 0.50),  # D3
        (58, 6.72, 2.0, 0.56),  # Bb3
        (65, 7.10, 2.0, 0.66),  # F4
        (45, 7.65, 2.1, 0.56),  # A2
        (52, 7.95, 2.0, 0.52),  # E3
        (62, 8.28, 1.9, 0.60),  # D4 (sus4)
        (61, 8.65, 1.6, 0.56),  # C#4 (gentle leading tone)
        # Bar 4: Dm9 serene resolution
        (50, 9.05, 2.9, 0.56),  # D3
        (57, 9.28, 2.7, 0.52),  # A3
        (64, 9.52, 2.4, 0.58),  # E4
        (65, 9.78, 2.2, 0.60),  # F4
        (69, 10.08, 1.9, 0.55), # A4
        (74, 10.42, 1.55, 0.48),# D5 crystalline chime
    )
    for midi, start_s, dur_s, vel in piano_events:
        # Lower notes subtly left, upper notes subtly right (realistic piano perspective)
        pan = float(np.clip((midi - 60.0) / 28.0, -0.55, 0.55))
        voice = _render_felt_piano_note(_midi_to_hz(midi), dur_s, vel, sample_rate)
        _add_voice(voice, start_s, pan)

    # 3. Stereo chamber acoustics
    mix = _apply_stereo_chamber_reverb(mix, sample_rate)

    # 4. Smooth master fade-in (25 ms) and fade-out (600 ms) for guaranteed click-free edges
    in_n = int(round(0.025 * sample_rate))
    out_n = int(round(0.600 * sample_rate))
    mix[:in_n] *= (0.5 * (1.0 - np.cos(np.pi * np.arange(in_n) / in_n)))[:, np.newaxis]
    mix[-out_n:] *= (0.5 * (1.0 + np.cos(np.pi * np.arange(out_n) / out_n)))[:, np.newaxis]

    # 5. Normalize peak strictly to target_peak_dbfs (default -12 dBFS — safe & dynamic)
    peak = float(np.max(np.abs(mix)))
    if peak > 0.0:
        target_lin = 10.0 ** (target_peak_dbfs / 20.0)
        mix *= target_lin / peak

    return mix


def write_nocturne_flac(
    output_path: str | Path,
    sample_rate: int = DEFAULT_SAMPLE_RATE,
    target_peak_dbfs: float = -12.0,
) -> Path:
    """Render the original nocturne and write a 24-bit lossless FLAC with TPDF dither."""
    out = Path(output_path)
    out.parent.mkdir(parents=True, exist_ok=True)

    mix_f64 = compose_nocturne_float64(
        sample_rate=sample_rate,
        target_peak_dbfs=target_peak_dbfs,
    )

    # Deterministic 24-bit TPDF dither (1 LSB triangular) before integer quantization
    rng = np.random.default_rng(seed=20260401)
    dither = (rng.random(mix_f64.shape) - rng.random(mix_f64.shape)) / float(INT24_MAX)
    quantized = np.clip(mix_f64 + dither, -1.0, 1.0 - 1.0 / float(INT24_MAX))

    # Write true 24-bit PCM FLAC
    sf.write(str(out), quantized, sample_rate, subtype="PCM_24", format="FLAC")
    return out
