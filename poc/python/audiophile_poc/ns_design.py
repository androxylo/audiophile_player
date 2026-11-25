"""Clean-room 8fS (352.8 / 384 kHz) minimum-phase error-feedback noise-shaper designer.

Implements docs/research/04-noise-shapers.md §2.3–§5:
- Weighted noise minimization via Toeplitz autocorrelation + Levinson-Durbin (guaranteed
  minimum-phase NTF(z) = 1 + sum_{k=1..N} c_k z^-k, meeting the Gerzon-Craven bound).
- Bisection on ultrasonic floor weight epsilon to hit a requested ceiling on max |NTF|.
- Prints exact 352.8 kHz and 384 kHz coefficients, Gerzon-Craven integral check,
  band-averaged noise-power gains vs flat TPDF, and time-domain code-activity metrics.

Run from poc/python:
    python audiophile_poc/ns_design.py
"""

from __future__ import annotations

import math
from dataclasses import dataclass
import numpy as np


@dataclass(frozen=True)
class ShaperSpec:
    name: str
    order: int
    f_band_hz: float
    f_taper_hz: float | None
    max_ntf_db: float


PROFILES: tuple[ShaperSpec, ...] = (
    ShaperSpec("ns-gentle-2", order=2, f_band_hz=20_000.0, f_taper_hz=None, max_ntf_db=6.0),
    ShaperSpec("ns5-class", order=5, f_band_hz=20_000.0, f_taper_hz=None, max_ntf_db=20.0),
    ShaperSpec("lns15-class", order=15, f_band_hz=20_000.0, f_taper_hz=60_000.0, max_ntf_db=15.0),
    ShaperSpec("wide40-9", order=9, f_band_hz=40_000.0, f_taper_hz=None, max_ntf_db=18.0),
)


def levinson_durbin(r: np.ndarray, order: int) -> np.ndarray:
    """Solve Toeplitz normal equations R * c = -r[1..order] via Levinson-Durbin.

    Returns c = [c_1, ..., c_order] so NTF(z) = 1 + sum_{k=1..order} c_k z^-k is strictly
    minimum-phase whenever r is a valid positive-definite autocorrelation sequence.
    """
    a = np.zeros(order + 1, dtype=np.float64)
    a[0] = 1.0
    e = float(r[0])
    for m in range(1, order + 1):
        acc = float(r[m])
        for j in range(1, m):
            acc += a[j] * float(r[m - j])
        k = -acc / e
        a_new = a.copy()
        a_new[m] = k
        for j in range(1, m):
            a_new[j] = a[j] + k * a[m - j]
        a = a_new
        e *= 1.0 - k * k
    return a[1:]


def weight_curve(
    w: np.ndarray,
    fs_hz: float,
    f_band_hz: float,
    f_taper_hz: float | None,
    eps: float,
) -> np.ndarray:
    """Frequency weight W(w): 1 on [0, f_band], optional log-linear taper to f_taper, eps above."""
    f = w * (fs_hz / (2.0 * math.pi))
    W = np.full_like(f, eps, dtype=np.float64)
    W[f <= f_band_hz] = 1.0
    if f_taper_hz is not None and f_taper_hz > f_band_hz:
        mask = (f > f_band_hz) & (f < f_taper_hz)
        t = (f[mask] - f_band_hz) / (f_taper_hz - f_band_hz)
        # Log-linear taper in W from 1.0 (log=0) down to eps (log=ln(eps))
        W[mask] = np.exp(t * math.log(eps))
    return W


def design_shaper(
    spec: ShaperSpec,
    fs_hz: float,
    grid_n: int = 8192,
) -> tuple[np.ndarray, float, float]:
    """Design minimum-phase FIR error-feedback taps c[1..N] meeting spec.max_ntf_db."""
    # Midpoint grid on [0, pi]
    idx = np.arange(grid_n, dtype=np.float64)
    w = (idx + 0.5) * (math.pi / grid_n)
    k = np.arange(spec.order + 1, dtype=np.float64)[:, None]
    cos_kw = np.cos(k * w[None, :])
    exp_mjw = np.exp(-1j * np.arange(1, spec.order + 1, dtype=np.float64)[:, None] * w[None, :])

    target_lin = 10.0 ** (spec.max_ntf_db / 20.0)

    def solve_for_log_eps(log10_eps: float) -> tuple[np.ndarray, float]:
        eps = 10.0 ** log10_eps
        W = weight_curve(w, fs_hz, spec.f_band_hz, spec.f_taper_hz, eps)
        r = (cos_kw @ W) / grid_n
        c = levinson_durbin(r, spec.order)
        ntf = 1.0 + (c[:, None] * exp_mjw).sum(axis=0)
        peak = float(np.max(np.abs(ntf)))
        return c, peak

    lo, hi = -16.0, 0.0
    for _ in range(70):
        mid = 0.5 * (lo + hi)
        _, peak = solve_for_log_eps(mid)
        if peak > target_lin:
            lo = mid
        else:
            hi = mid

    c, peak = solve_for_log_eps(hi)
    return c, 10.0 ** hi, 20.0 * math.log10(peak)


def band_metrics(c: np.ndarray, fs_hz: float, grid_n: int = 16384) -> dict[str, float]:
    idx = np.arange(grid_n, dtype=np.float64)
    w = (idx + 0.5) * (math.pi / grid_n)
    f = w * (fs_hz / (2.0 * math.pi))
    exp_mjw = np.exp(-1j * np.arange(1, len(c) + 1, dtype=np.float64)[:, None] * w[None, :])
    ntf_mag2 = np.abs(1.0 + (c[:, None] * exp_mjw).sum(axis=0)) ** 2

    def band_db(f_lo: float, f_hi: float) -> float:
        m = (f >= f_lo) & (f <= f_hi)
        return 10.0 * math.log10(float(np.mean(ntf_mag2[m])))

    nyq = 0.5 * fs_hz
    return {
        "0-20k": band_db(0.0, 20_000.0),
        "20-40k": band_db(20_000.0, 40_000.0),
        "40-100k": band_db(40_000.0, 100_000.0),
        "100k-nyq": band_db(100_000.0, nyq),
        "total": 10.0 * math.log10(float(np.mean(ntf_mag2))),
        "gerzon_craven": float(0.5 * np.mean(np.log(ntf_mag2))),
        "l1": float(np.sum(np.abs(c))),
        "peak_db": 10.0 * math.log10(float(np.max(ntf_mag2))),
    }


def main() -> None:
    for fs_hz in (384_000.0, 352_800.0):
        print(f"=== Clean-Room 8fS Noise Shaper Design @ {int(fs_hz)} Hz ===")
        for spec in PROFILES:
            c, eps, peak_db = design_shaper(spec, fs_hz)
            m = band_metrics(c, fs_hz)
            taps = ", ".join(f"{x:.10f}" for x in c)
            print(
                f"{spec.name:12s} N={spec.order:2d} max|NTF|={peak_db:5.2f} dB "
                f"0-20k={m['0-20k']:+6.2f} 20-40k={m['20-40k']:+6.2f} "
                f"40-100k={m['40-100k']:+6.2f} 100k-nyq={m['100k-nyq']:+6.2f} "
                f"tot={m['total']:+6.2f} sum|c|={m['l1']:5.2f} GC={m['gerzon_craven']:+.2e} "
                f"eps={eps:.3e}"
            )
            print(f"  c = [{taps}]")
        print()


if __name__ == "__main__":
    main()
