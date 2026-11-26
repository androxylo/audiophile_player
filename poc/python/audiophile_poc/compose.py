# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 androxylo

"""Original, copyright-free (CC0) physical-modeling acoustic chamber duet.

Synthesizes a warm, intimate 14-second nylon-string acoustic guitar & upright
double-bass nocturne strictly in IEEE 754 float64 using:
  - Vectorized extended Karplus-Strong waveguide strings with fractional-delay
    tuning, finger-flesh excitation, pluck-position comb filtering, and
    nylon/gut frequency-dependent string damping.
  - Analytical wooden soundboard body-resonance modeling (Helmholtz A0, main
    top-plate T1, and upper-bout wood modes) applied in float64 frequency domain.
  - Humanized expressive fingerstyle phrasing (rolled chords, subtle rubato,
    natural bass pluck pitch-bloom).
  - True dense stereo recital-room FFT convolution reverb (asymmetric early
    reflections + uncorrelated frequency-damped diffuse stereo tail).

Zero external samples, zero copyrighted music, 100% reproducible CC0 DSP.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import soundfile as sf

DEFAULT_SAMPLE_RATE = 96_000
INT24_MAX = (1 << 23) - 1


def _midi_to_hz(midi: float) -> float:
    return 440.0 * (2.0 ** ((midi - 69.0) / 12.0))


def _fft_convolve_1d(x: np.ndarray, h: np.ndarray) -> np.ndarray:
    """Exact linear convolution in float64 via rfft (returns len(x) samples)."""
    n_out = x.shape[0]
    n_fft = 1 << int(np.ceil(np.log2(n_out + h.shape[0] - 1)))
    X = np.fft.rfft(x, n=n_fft)
    H = np.fft.rfft(h, n=n_fft)
    y = np.fft.irfft(X * H, n=n_fft)
    return y[:n_out]


def _apply_body_resonator(
    signal: np.ndarray,
    sample_rate: int,
    modes: tuple[tuple[float, float, float], ...],
    lowpass_hz: float,
    highpass_hz: float = 35.0,
) -> np.ndarray:
    """Apply physical wooden-body modal resonances & natural acoustic roll-off in float64."""
    n = signal.shape[0]
    n_fft = 1 << int(np.ceil(np.log2(n + 8192)))
    freqs = np.fft.rfftfreq(n_fft, d=1.0 / float(sample_rate))
    X = np.fft.rfft(signal, n=n_fft)

    # Start with smooth direct bridge radiation + modal wood peaks (parallel 2nd-order resonators)
    H = np.full(freqs.shape, 0.55 + 0.0j, dtype=np.complex128)
    for f0, q, gain in modes:
        # Normalized continuous-time resonant peak transfer function
        w_ratio = freqs / f0
        denom = (1.0 - w_ratio * w_ratio) + 1j * (w_ratio / q)
        H += gain / denom

    # Smooth 2nd-order Butterworth-like highpass (sub-rumble) & natural wood lowpass
    hp_ratio = freqs / highpass_hz
    H_hp = (hp_ratio**2) / np.sqrt(1.0 + hp_ratio**4)
    lp_ratio = freqs / lowpass_hz
    H_lp = 1.0 / np.sqrt(1.0 + lp_ratio**4)

    Y = X * H * H_hp * H_lp
    return np.fft.irfft(Y, n=n_fft)[:n]


def _make_pluck_excitation(
    period_len: int,
    sample_rate: int,
    velocity: float,
    pluck_pos: float,
    brightness_hz: float,
    rng: np.random.Generator,
) -> np.ndarray:
    """Generate a warm, finger-flesh plucked string initial displacement waveform."""
    # White noise -> smooth pink/warm spectrum (real fingertip softens highs)
    raw = rng.standard_normal(period_len * 2)
    freqs = np.fft.rfftfreq(raw.shape[0], d=1.0 / float(sample_rate))
    spec = np.fft.rfft(raw)

    # Finger flesh low-pass + gentle 1/f^0.65 spectral tilt
    cutoff = brightness_hz * (0.65 + 0.75 * velocity)
    lp = 1.0 / np.sqrt(1.0 + (freqs / cutoff) ** 4)
    tilt = 1.0 / np.maximum(freqs / 180.0, 1.0) ** 0.55

    # Pluck-position comb filter: sin(pi * f / f0 * pluck_pos)
    # Zeroes harmonics whose node sits at the pluck point (classic guitar warmth!)
    delay_s = max(1, int(round(period_len * pluck_pos))) / float(sample_rate)
    comb = 1.0 - np.exp(-2j * np.pi * freqs * delay_s)

    exc = np.fft.irfft(spec * lp * tilt * comb, n=raw.shape[0])[:period_len]
    # Remove DC offset and normalize energy smoothly
    exc -= np.mean(exc)
    rms = float(np.sqrt(np.mean(exc * exc))) + 1e-12
    exc /= rms
    # Slight smooth triangular window so the initial transient is fleshy, never clicky
    win = np.sin(np.pi * np.arange(period_len, dtype=np.float64) / period_len) ** 0.35
    return exc * win * velocity


def _render_waveguide_string(
    freq_hz: float,
    duration_s: float,
    velocity: float,
    sample_rate: int,
    rng: np.random.Generator,
    *,
    decay_factor: float = 0.9968,
    damping: float = 0.28,
    pluck_pos: float = 0.21,
    brightness_hz: float = 2800.0,
    attack_ms: float = 4.5,
) -> np.ndarray:
    """Vectorized period-by-period Karplus-Strong waveguide with exact fractional pitch."""
    total_n = int(round(duration_s * sample_rate))
    exact_period = float(sample_rate) / freq_hz
    # Subtract the ~damping*0.5 phase delay introduced by the 1-pole loss filter
    eff_period = max(4.0, exact_period - damping)
    n_int = int(np.floor(eff_period))
    frac = eff_period - n_int

    buf = _make_pluck_excitation(
        n_int, sample_rate, velocity, pluck_pos, brightness_hz, rng
    )
    # Subtle warm fundamental reinforcement for natural round nylon-string body
    t_init = np.arange(n_int, dtype=np.float64) / float(n_int)
    buf = 0.72 * buf + 0.28 * velocity * np.sin(2.0 * np.pi * t_init)

    out = np.zeros(total_n, dtype=np.float64)
    pos = 0
    prev_loss = 0.0
    prev_frac = 0.0

    # Per-period decay scaled cleanly to frequency so low & high strings ring naturally
    g = decay_factor ** (220.0 / max(freq_hz, 60.0)) ** 0.22
    g = min(0.9992, max(0.975, g))
    s = float(np.clip(damping, 0.05, 0.48))

    while pos < total_n:
        chunk = min(n_int, total_n - pos)
        out[pos : pos + chunk] = buf[:chunk]
        pos += chunk

        # 1. Frequency-dependent nylon/gut string loss filter (1-pole moving average)
        shifted = np.empty_like(buf)
        shifted[0] = prev_loss
        shifted[1:] = buf[:-1]
        prev_loss = float(buf[-1])
        filtered = g * ((1.0 - s) * buf + s * shifted)

        # 2. Exact fractional-delay linear interpolation for spot-on concert tuning
        if frac > 1e-6:
            shifted_f = np.empty_like(filtered)
            shifted_f[0] = prev_frac
            shifted_f[1:] = filtered[:-1]
            prev_frac = float(filtered[-1])
            buf = (1.0 - frac) * filtered + frac * shifted_f
        else:
            buf = filtered

    # Soft finger-release attack ramp (removes digital impulse sharpness)
    atk_n = min(total_n // 4, max(1, int(round(attack_ms * sample_rate / 1000.0))))
    atk = 0.5 * (1.0 - np.cos(np.pi * np.arange(atk_n, dtype=np.float64) / atk_n))
    out[:atk_n] *= atk

    # Smooth finger-mute release at end of note
    rel_n = min(total_n // 3, int(round(0.16 * sample_rate)))
    if rel_n > 0:
        rel = 0.5 * (1.0 + np.cos(np.pi * np.arange(rel_n, dtype=np.float64) / rel_n))
        out[-rel_n:] *= rel

    return out


def _render_upright_bass_note(
    freq_hz: float,
    duration_s: float,
    velocity: float,
    sample_rate: int,
    rng: np.random.Generator,
) -> np.ndarray:
    """Organic plucked acoustic double-bass pizzicato (gut waveguide + wood thump)."""
    # Deep, heavily damped gut-string waveguide
    string = _render_waveguide_string(
        freq_hz=freq_hz,
        duration_s=duration_s,
        velocity=velocity,
        sample_rate=sample_rate,
        rng=rng,
        decay_factor=0.9925,
        damping=0.44,
        pluck_pos=0.31,
        brightness_hz=520.0,
        attack_ms=11.0,
    )
    # Subtle warm upright fingerboard/fundamental bloom
    n = string.shape[0]
    t = np.arange(n, dtype=np.float64) / float(sample_rate)
    # Realistic slight upward pitch bloom (+12 cents settling over 70 ms)
    pitch_env = 1.0 + 0.007 * np.exp(-t / 0.07)
    phase = 2.0 * np.pi * freq_hz * np.cumsum(pitch_env) / float(sample_rate)
    fund = (
        0.55 * np.sin(phase) * np.exp(-t / 1.45)
        + 0.22 * np.sin(2.0 * phase + 0.2) * np.exp(-t / 0.65)
    )
    atk_n = min(n // 4, int(round(0.014 * sample_rate)))
    fund[:atk_n] *= 0.5 * (1.0 - np.cos(np.pi * np.arange(atk_n) / atk_n))
    rel_n = min(n // 3, int(round(0.20 * sample_rate)))
    fund[-rel_n:] *= 0.5 * (1.0 + np.cos(np.pi * np.arange(rel_n) / rel_n))

    return 0.65 * string + 0.35 * velocity * fund


def _build_stereo_recital_room_ir(
    sample_rate: int,
    rt60_s: float = 1.55,
    seed: int = 20260402,
) -> np.ndarray:
    """Synthesize a dense, silky wooden recital-room stereo IR (zero metallic ringing)."""
    rng = np.random.default_rng(seed)
    n = int(round((rt60_s + 0.35) * sample_rate))
    t = np.arange(n, dtype=np.float64) / float(sample_rate)
    ir = np.zeros((n, 2), dtype=np.float64)

    # 1. Early wooden-room reflections (asymmetric L/R for realistic stereo soundstage)
    early_l = ((11.2, 0.28), (19.6, -0.22), (28.4, 0.18), (41.3, -0.14), (54.8, 0.11))
    early_r = ((13.1, 0.26), (22.4, -0.21), (31.9, 0.17), (45.2, -0.13), (59.1, 0.10))
    for ms, g in early_l:
        ir[int(round(ms * sample_rate / 1000.0)), 0] += g
    for ms, g in early_r:
        ir[int(round(ms * sample_rate / 1000.0)), 1] += g

    # 2. Multi-band uncorrelated stereo diffuse tail (highs decay faster than lows = warm air)
    n_fft = 1 << int(np.ceil(np.log2(n)))
    freqs = np.fft.rfftfreq(n_fft, d=1.0 / float(sample_rate))

    # Smooth bloom from 18 ms to 70 ms so direct sound stays crisp and intimate
    bloom = np.clip((t - 0.018) / 0.055, 0.0, 1.0)
    bloom = 0.5 * (1.0 - np.cos(np.pi * bloom))

    bands = (
        # (low_hz, high_hz, rt60_band_s, weight)
        (60.0, 450.0, rt60_s * 1.10, 0.55),
        (450.0, 2200.0, rt60_s * 0.85, 0.45),
        (2200.0, 7500.0, rt60_s * 0.48, 0.25),
    )
    for ch in range(2):
        tail = np.zeros(n, dtype=np.float64)
        for f_lo, f_hi, band_rt60, weight in bands:
            noise = rng.standard_normal(n)
            env = np.exp(-6.907755 * t / band_rt60) * bloom
            spec = np.fft.rfft(noise * env, n=n_fft)
            # Smooth zero-phase bandpass mask
            hp = (freqs / f_lo) ** 2 / np.sqrt(1.0 + (freqs / f_lo) ** 4)
            lp = 1.0 / np.sqrt(1.0 + (freqs / f_hi) ** 4)
            band_sig = np.fft.irfft(spec * hp * lp, n=n_fft)[:n]
            tail += weight * band_sig
        ir[:, ch] += 0.065 * tail

    # Fade out the very end smoothly
    end_n = int(round(0.10 * sample_rate))
    ir[-end_n:] *= (0.5 * (1.0 + np.cos(np.pi * np.arange(end_n) / end_n)))[:, np.newaxis]
    return ir


def compose_nocturne_float64(
    sample_rate: int = DEFAULT_SAMPLE_RATE,
    target_peak_dbfs: float = -12.0,
) -> np.ndarray:
    """Compose a 14-second original nylon-guitar & upright-bass chamber nocturne (float64 stereo)."""
    rng = np.random.default_rng(seed=20260401)
    duration_s = 14.0
    total_frames = int(round(duration_s * sample_rate))

    guitar_bus = np.zeros((total_frames, 2), dtype=np.float64)
    bass_bus = np.zeros((total_frames, 2), dtype=np.float64)

    def _add_panned(bus: np.ndarray, mono: np.ndarray, start_s: float, pan: float) -> None:
        idx = int(round(start_s * sample_rate))
        end = min(total_frames, idx + mono.shape[0])
        if idx >= total_frames or end <= idx:
            return
        seg = mono[: end - idx]
        theta = (pan + 1.0) * (np.pi / 4.0)
        bus[idx:end, 0] += seg * np.cos(theta)
        bus[idx:end, 1] += seg * np.sin(theta)

    # 1. Nylon-string acoustic guitar fingerstyle performance (humanized timing & rolled voicings)
    # Original D-minor chamber progression: Dm9 -> Gm7 -> C9 -> Fmaj7 -> Bbmaj7 -> A7sus4/A7 -> Dm9
    # Tuples: (midi, start_s, dur_s, velocity, pluck_pos)
    guitar_events = (
        # Bar 1: Dm9 (warm rolled bass + lyrical top)
        (50, 0.14, 3.1, 0.72, 0.26),  # D3
        (57, 0.175, 2.9, 0.56, 0.22), # A3 (rolled +35 ms)
        (64, 0.21, 2.8, 0.62, 0.19),  # E4
        (65, 0.88, 2.3, 0.78, 0.18),  # F4 (singing melody)
        (69, 1.54, 2.0, 0.66, 0.20),  # A4
        (64, 2.18, 1.7, 0.54, 0.24),  # E4
        (62, 2.66, 1.5, 0.58, 0.22),  # D4
        # Bar 2: Gm7 -> C9
        (43, 3.16, 2.2, 0.70, 0.28),  # G2
        (53, 3.195, 2.0, 0.52, 0.23), # F3
        (58, 3.23, 1.9, 0.58, 0.20),  # Bb3
        (62, 3.84, 1.7, 0.72, 0.18),  # D4
        (65, 4.46, 1.5, 0.66, 0.19),  # F4
        (48, 5.02, 2.1, 0.68, 0.26),  # C3
        (55, 5.055, 1.9, 0.52, 0.22), # G3
        (58, 5.09, 1.8, 0.54, 0.21),  # Bb3
        (64, 5.66, 1.7, 0.74, 0.18),  # E4
        (62, 6.26, 1.5, 0.60, 0.23),  # D4
        # Bar 3: Fmaj7 -> Bbmaj7 (warm expressive swell)
        (41, 6.78, 2.3, 0.70, 0.27),  # F2
        (52, 6.82, 2.1, 0.54, 0.21),  # E3
        (57, 6.86, 2.0, 0.60, 0.20),  # A3
        (60, 7.42, 1.8, 0.68, 0.19),  # C4
        (64, 7.98, 1.6, 0.62, 0.22),  # E4
        (46, 8.50, 2.2, 0.70, 0.27),  # Bb2
        (53, 8.54, 2.0, 0.52, 0.23),  # F3
        (57, 8.58, 1.9, 0.58, 0.20),  # A3
        (62, 9.14, 1.7, 0.72, 0.18),  # D4
        # Bar 4: A7sus4 -> A7 -> Dm9 serene rubato cadence
        (45, 9.72, 1.9, 0.68, 0.26),  # A2
        (52, 9.755, 1.8, 0.52, 0.22), # E3
        (55, 9.79, 1.7, 0.54, 0.21),  # G3
        (62, 9.83, 1.4, 0.66, 0.19),  # D4 (sus4)
        (61, 10.48, 1.4, 0.64, 0.21), # C#4 (expressive resolution)
        # Final arpeggiated Dm9 chord (gentle ritardando)
        (50, 11.12, 2.8, 0.70, 0.27), # D3
        (57, 11.20, 2.7, 0.56, 0.24), # A3
        (60, 11.29, 2.6, 0.52, 0.22), # C4
        (64, 11.39, 2.5, 0.62, 0.20), # E4
        (65, 11.50, 2.4, 0.64, 0.19), # F4
        (69, 11.63, 2.3, 0.54, 0.22), # A4
    )
    for midi, start_s, dur_s, vel, ppos in guitar_events:
        # Subtle micro-humanization (+/- 4 ms) & natural close-mic stereo guitar perspective
        jitter_s = float(rng.uniform(-0.004, 0.004))
        note = _render_waveguide_string(
            freq_hz=_midi_to_hz(midi),
            duration_s=dur_s,
            velocity=vel,
            sample_rate=sample_rate,
            rng=rng,
            decay_factor=0.9968,
            damping=0.27,
            pluck_pos=ppos,
            brightness_hz=2600.0,
            attack_ms=5.0,
        )
        # Guitar slightly left-of-center, treble strings subtly angled toward right
        pan = float(np.clip(-0.18 + (midi - 55.0) * 0.015, -0.38, 0.15))
        _add_panned(guitar_bus, note, max(0.0, start_s + jitter_s), pan)

    # Apply physical cedar-top classical guitar wooden body resonance to L & R
    guitar_modes = (
        (104.0, 6.5, 0.55),  # Helmholtz air cavity (A0)
        (206.0, 8.5, 0.70),  # Main soundboard monopole (T1)
        (418.0, 6.0, 0.40),  # Upper bout wood dipole
        (690.0, 5.5, 0.24),  # Warm midrange bridge wood
    )
    for ch in range(2):
        guitar_bus[:, ch] = _apply_body_resonator(
            guitar_bus[:, ch],
            sample_rate,
            modes=guitar_modes,
            lowpass_hz=5200.0,
            highpass_hz=68.0,
        )

    # 2. Upright double-bass acoustic pizzicato (positioned slightly right-of-center for duet stage)
    bass_events = (
        (38, 0.15, 2.9, 0.85),  # D2
        (45, 1.92, 1.3, 0.55),  # A2 gentle pickup
        (31, 3.17, 1.9, 0.82),  # G1
        (36, 5.03, 1.8, 0.80),  # C2
        (41, 6.79, 1.8, 0.82),  # F2
        (34, 8.51, 1.4, 0.80),  # Bb1
        (33, 9.73, 1.4, 0.82),  # A1
        (38, 11.13, 2.8, 0.88), # D2 final warm pedal
    )
    for midi, start_s, dur_s, vel in bass_events:
        note = _render_upright_bass_note(
            freq_hz=_midi_to_hz(midi),
            duration_s=dur_s,
            velocity=vel,
            sample_rate=sample_rate,
            rng=rng,
        )
        _add_panned(bass_bus, note, start_s, pan=0.22)

    # Upright bass carved spruce/maple body resonance
    bass_modes = (
        (64.0, 5.5, 0.65),
        (128.0, 7.0, 0.55),
        (248.0, 5.5, 0.30),
    )
    for ch in range(2):
        bass_bus[:, ch] = _apply_body_resonator(
            bass_bus[:, ch],
            sample_rate,
            modes=bass_modes,
            lowpass_hz=1100.0,
            highpass_hz=32.0,
        )

    # 3. Balance acoustic duet and place inside true FFT-convolved wooden recital room
    dry = 0.78 * guitar_bus + 0.52 * bass_bus
    ir = _build_stereo_recital_room_ir(sample_rate, rt60_s=1.55)

    # True stereo room convolution (direct + gentle cross-room bleed)
    wet_l = _fft_convolve_1d(dry[:, 0], ir[:, 0]) + 0.35 * _fft_convolve_1d(dry[:, 1], ir[:, 1])
    wet_r = _fft_convolve_1d(dry[:, 1], ir[:, 1]) + 0.35 * _fft_convolve_1d(dry[:, 0], ir[:, 0])
    mix = dry + 0.42 * np.column_stack((wet_l, wet_r))

    # 4. Smooth master fade-in (20 ms) and fade-out (900 ms) for click-free edges
    in_n = int(round(0.020 * sample_rate))
    out_n = int(round(0.900 * sample_rate))
    mix[:in_n] *= (0.5 * (1.0 - np.cos(np.pi * np.arange(in_n) / in_n)))[:, np.newaxis]
    mix[-out_n:] *= (0.5 * (1.0 + np.cos(np.pi * np.arange(out_n) / out_n)))[:, np.newaxis]

    # 5. Normalize peak strictly to target_peak_dbfs (default -12 dBFS)
    peak = float(np.max(np.abs(mix)))
    if peak > 0.0:
        mix *= (10.0 ** (target_peak_dbfs / 20.0)) / peak

    return mix


def write_nocturne_flac(
    output_path: str | Path,
    sample_rate: int = DEFAULT_SAMPLE_RATE,
    target_peak_dbfs: float = -12.0,
) -> Path:
    """Render the original physical-modeling nocturne and write a 24-bit lossless FLAC."""
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

    sf.write(str(out), quantized, sample_rate, subtype="PCM_24", format="FLAC")
    return out
