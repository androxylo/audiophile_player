# INV-04: Final Quantization, Dither & 8fS Noise Shapers

**Status:** Research complete for the noise-shaper / dither half of INV-04 (headroom half is resolved in `ap-core::headroom`, GitHub Issue #7). Hardware confirmation on Gungnir 2 is pending; see §7.
**Scope:** The single `f64 → int` quantizer at `352.8 / 384 kHz` feeding a **Schiit Gungnir 2** (four TI `DAC8812CRUZ`, NOS mode). Covers dither, error-feedback noise-shaper design, output bit depth ("DAC bits"), stability, ultrasonic noise budget, and a Rust implementation spec for `ap-dsp::quantize`.
**Companion documents:** [02-audiophile-filter-survey.md](02-audiophile-filter-survey.md) §2, [03-filter-knowledge-base.md](03-filter-knowledge-base.md) §5 (shaper rate matrix and DAC Bits technique from `[HQP-M5]` / `[PGGB-V9]`), [05-rew-convolution-and-delay.md](05-rew-convolution-and-delay.md) §1 (why the shaper is the last stage).
**Reproducible prototype:** [`poc/python/audiophile_poc/ns_design.py`](../../poc/python/audiophile_poc/ns_design.py) (numpy only; `python audiophile_poc/ns_design.py` from `poc/python`). All numbers in §3–§5 come from this script.

---

## 1. Research Verdict

1. **At 24-bit output, noise shaping has no objective benefit.** Flat TPDF at 24 bits and `384 kHz` leaves only **`-154 dBFS`** of noise in the `0–20 kHz` band. That is about 40 dB below the Gungnir 2's analog noise floor (S/N `> 112 dB`, Schiit spec). Any shaper only moves noise that is already far below the analog floor. **Flat TPDF / 24-bit stays the default.**
2. **Noise shaping matters only when the output is reduced to the DAC's real resolution ("DAC bits").** The `DAC8812` is a **16-bit** converter (INL/DNL `±1 LSB` max, TI product page). If Gungnir 2 drops input bits below 16 before the DAC8812s, those bits are lost undithered inside the DAC. Doing the reduction ourselves (dithered and shaped) turns that internal truncation into a no-op. Whether Gungnir 2 truncates, rounds or uses its four chips to extend resolution is **not published** and must be measured (§7, new OQ-32).
3. **Shaping is cheap at 8fS.** By the Gerzon–Craven theorem (§2.2), lowering in-band noise must be paid for by raising noise elsewhere. At `384 kHz`, 89.6 % of the spectrum is ultrasonic, so the price is small. A 2nd-order shaper gives `-14.6 dB` in-band for a peak ultrasonic gain of only `+6 dB`.
4. **Do not reuse 44.1/48 kHz coefficient tables.** SoX's tables stop at 48 kHz, and CamillaDSP's stop at 192 kHz (where the rate in the table name is only a label). No open-source table exists for 352.8/384 kHz. We design our own clean-room coefficients with a closed-form method (§3).
5. **The critical check: does ultrasonic shaping harm a NOS multibit DAC?** Not through ultrasonic power. The worst profile at 16 bits puts `-81 dBFS` RMS above 40 kHz, which is trivial for amplifiers and tweeters. The real risk is **code activity**: aggressive shapers swing the output by 3–6 LSB RMS per sample (versus 0.7 for TPDF). In an R-2R ladder running unfiltered (NOS), that means more major-carry glitches and more settling-related distortion. **Use the lowest-order shaper that pushes in-band noise below the analog floor.** For Gungnir 2 at 16 bits, that is the 2nd-order `ns-gentle-2`, not an NS5/LNS15-class shaper.
6. **The error-feedback loop is unconditionally stable with a multi-level quantizer.** The error stays within `±1.5 LSB` (rounding plus TPDF), so the feedback stays within `1.5·Σ|c_k|` LSB. Lee's rule (max `|NTF| ≲ 1.5`) applies to 1-bit and few-level ΔΣ modulators, not here. The one implementation rule: **keep clip error out of the fed-back error** (§6).

---

## 2. Theory (Clean-Room Basis)

### 2.1 Error-Feedback Quantizer with In-Loop TPDF
Per channel, with `x[n]` scaled to LSB units of the target bit depth `B`:

```text
v[n] = x[n] + Σ_{k=1..N} c_k · e[n-k]          (feedback of past total error)
q[n] = round(v[n] + d[n])                       (d = TPDF: u1 - u2, u ~ U[0,1), range ±1 LSB)
e[n] = q[n] - v[n]                              (total error = rounding + dither, |e| ≤ 1.5 LSB)
y[n] = clamp(q[n], -2^(B-1), 2^(B-1) - 1)       (clip AFTER computing e)
⇒  Y(z) = X(z) + NTF(z) · E(z),   NTF(z) = 1 + Σ c_k z^-k
```

- **TPDF inside the loop** makes the total error's first and second moments independent of the signal (Lipshitz / Wannamaker / Vanderkooy non-subtractive dither theory). The error is then white with variance `LSB²/4`, so the output noise spectrum is exactly `|NTF|² · LSB²/4`. Without dither, shaping only moves distortion around; it does not remove it.
- The dither is shaped along with the rounding error ("shaped dither"). That is intended.
- `N = 0` (`NTF = 1`) is exactly today's `TpdfDither` path in `ap-dsp::quantize`.

### 2.2 Gerzon–Craven Noise-Power Theorem
For a monic, minimum-phase `NTF`: `(1/π)∫₀^π ln|NTF(e^{jω})| dω = 0` (it is `> 0` for non-minimum-phase NTFs). The log-magnitude dB area removed from one band must be added back somewhere else. At 8fS the protected 0–20 kHz band is only `20/192 = 10.4 %` of `[0, π]`. A `-40 dB` in-band dip therefore needs only about `+4.6 dB` (log-average) across the ultrasonic region. All our designs meet the bound with equality: the prototype measures `mean ln|NTF| ≈ -6·10⁻⁵`, which is the grid integration error.
*(Standard result; the primary paper, Gerzon & Craven, AES Preprint 2822, 1989, was not re-read in this session. See §9.)*

### 2.3 Optimal Design = Linear Prediction (Closed Form, No Optimizer)
Minimizing the weighted noise `∫ W(ω)|NTF|² dω` over monic FIR `NTF` of order `N` gives the normal equations `R c = -r`. `R` is the Toeplitz autocorrelation of `W`, with `r_k = ∫ W(ω) cos(kω) dω`. The solution is the **prediction-error (whitening) filter of `W`**. It is found by Levinson–Durbin and is **guaranteed minimum phase**, so the Gerzon–Craven bound is met automatically. We use:

- `W = 1` on `[0, f_band]` and `W = ε` above it (optionally with a log-linear taper over `f_taper`).
- `ε` sets the trade-off. A bisection on `ε` meets a requested ceiling on **max |NTF|** (the ultrasonic peak gain).

The method is deterministic, about 20 lines of code in any language, needs no third-party coefficients, and has no licence concerns. It can run once at build time (frozen constants) or at startup.

---

## 3. Candidate Profiles (Designed by `ns_design.py`)

Coefficient sets designed for `384 kHz` (the `352.8 kHz` sets differ slightly; the script prints both). `NTF = 1 + Σ c_k z^-k`.

| Profile | Maps to enum | Design (`f_band`, order, max\|NTF\|) | `c_1 … c_N` @ 384 kHz |
| --- | --- | --- | --- |
| `tpdf` (flat) | `TpdfDither` | none | — |
| `ns-gentle-2` | *new* `NsGentle2` | 20 kHz, N=2, +6 dB | `-0.9717198203, 0.0235424947` |
| `ns5-class` | `Ns5Multibit` | 20 kHz, N=5, +20 dB | `-3.5311524507, 4.3759465168, -1.8110737471, -0.3752982917, 0.3428744549` |
| `lns15-class` | `Lns15Smooth` | 20 kHz (+40 kHz taper), N=15, +15 dB | see script output (15 taps, Σ\|c\| = 9.56) |
| `wide40-9` | `PggbOptimal` | 40 kHz, N=9, +18 dB | see script output (9 taps, Σ\|c\| = 11.21) |

> **Naming caution:** These are **clean-room analogs** of the published *intent* of HQPlayer `NS5` / `LNS15` and PGGB `NS: Optimal` (8fS multibit; smooth 15th-order; low noise up to ≥ 40 kHz), taken from `[HQP-M5]` / `[PGGB-V9]` in report 03. They are **not** bit-identical reproductions of proprietary shapers, and the docs and CLI must not claim otherwise.

### 3.1 Noise-Power Gain vs Flat TPDF (dB, average over band, `384 kHz`)

| Profile | 0–20 k | 20–40 k | 40–100 k | 100 k–Nyq | total | Σ\|c\| |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `tpdf` | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0.00 |
| `ns-gentle-2` | **-14.6** | -6.5 | +0.5 | +5.1 | +2.9 | 1.00 |
| `ns5-class` | **-58.7** | -22.3 | +6.8 | +18.6 | +15.6 | 10.44 |
| `lns15-class` | **-51.5** | -27.3 | +11.9 | +13.8 | +12.2 | 9.56 |
| `wide40-9` | **-40.6** | **-39.1** | +11.3 | +16.0 | +13.7 | 11.21 |

At `352.8 kHz` every in-band figure is 1–5 dB smaller, because the audio band is a larger fraction of Nyquist. Example: `ns5-class` gives `-55.9 dB`.

---

## 4. Absolute Noise Budget vs Gungnir 2 (dBFS re full-scale peak, `384 kHz`)

`in-band (0–20 kHz) / ultrasonic (40 kHz–Nyquist)`. Reference: Gungnir 2 S/N `> 112 dB` (Schiit spec). Taking it as relative to full-scale RMS, the analog floor is roughly **`-115 dBFS`** in-band (approximate; to be measured).

| Output bits | `tpdf` | `ns-gentle-2` | `ns5-class` | `lns15-class` | `wide40-9` |
| --- | --- | --- | --- | --- | --- |
| **16** | -106.2 / -97.3 | **-120.7** / -93.5 | -164.9 / -80.7 | -157.7 / -84.2 | -146.8 / -82.7 |
| **18** | **-118.2** / -109.4 | -132.8 / -105.5 | -176.9 / -92.8 | -169.7 / -96.2 | -158.8 / -94.7 |
| **20** | -130.2 / -121.4 | -144.8 / -117.6 | -189.0 / -104.8 | -181.7 / -108.3 | -170.9 / -106.8 |
| **24** | -154.3 / -145.5 | -168.9 / -141.7 | -213.1 / -128.9 | -205.8 / -132.3 | -195.0 / -130.8 |

**Reading the table:**
- **Bold** marks the cheapest profile that gets quantization noise below the analog floor. At 16 bits, flat TPDF (`-106`) is *above* the floor, and `ns-gentle-2` (`-121`) is enough. At ≥ 18 bits, flat TPDF is already below the floor.
- In-band figures below about `-130 dBFS` are physically meaningless for this DAC: the analog floor and the DAC8812's `±1 LSB` (16-bit) INL dominate. Choosing `ns5-class` over `ns-gentle-2` buys about 44 dB of theoretical in-band improvement that is inaudible and cannot be measured, in exchange for 4.6× the code activity (§5).
- **Ultrasonic power** is at most `-81 dBFS` RMS (16-bit `ns5-class`), about 0.009 % of full scale. That is negligible for any amplifier or tweeter, and far quieter than the ultrasonic noise of DSD64.

---

## 5. Critical Check: Ultrasonic Shaping into a NOS Multibit DAC

| Profile (16-bit, 384 kHz) | Error RMS | Sample-to-sample step RMS | Worst-case feedback bound | Simulated max feedback |
| --- | ---: | ---: | ---: | ---: |
| `tpdf` | 0.50 LSB | 0.71 LSB | 0 | 0 |
| `ns-gentle-2` | 0.70 LSB | 1.21 LSB | 1.5 LSB | 1.45 LSB |
| `lns15-class` | 2.03 LSB | 3.39 LSB | 14.3 LSB | 8.2 LSB |
| `wide40-9` | 2.41 LSB | 4.14 LSB | 16.8 LSB | 10.1 LSB |
| `ns5-class` | 3.01 LSB | 5.59 LSB | 15.7 LSB | 11.9 LSB |

- **Mechanism:** In NOS mode, every one of the 352,800/384,000 samples per second goes straight to the R-2R ladder with no digital filtering. A low-level signal plus shaped noise near zero crosses the mid-scale major carry (`0x7FFF ↔ 0x8000`) far more often under high-order shaping. Each crossing injects glitch energy, and the glitch is not linear in the code. The DAC8812 settles in `0.5 µs` (TI), well within the `2.6 µs` sample period, so settling itself is fine. Glitch energy (nV·s) is **unverified**: the datasheet PDF could not be parsed in this session (§9).
- **Balanced Multiform helps:** Gungnir 2 drives its DACs differentially, which cancels even-order glitch components to first order. It does not cancel code-dependent glitch area.
- **Folklore check:** The claim that shaping to DAC bits *corrects* R-2R linearity errors (`[HQP-M5]` p.19, report 03 §5.2) is only half right. TPDF decorrelates DNL errors from the signal (distortion turns into noise), and flat dither already does that. Shaping cannot remove static INL. The objective benefit of DAC bits is **avoiding undithered truncation inside the DAC**. The rest is a listening matter.
- **Conclusion:** Prefer `ns-gentle-2` for any reduced bit depth on Gungnir 2. Keep `ns5-class` / `lns15-class` / `wide40-9` as opt-in listening profiles, and use them only after the hardware probes in §7.

---

## 6. Rust Implementation Spec (`ap-dsp::quantize`)

1. **Structure:** Add per-channel `ShaperState { hist: [f64; 15], idx }` to `StreamingQuantizer`. Feedback is `Σ c_k·hist`. Update the history with `e = q - v`, computed **before** clamping. Clip error must never enter the loop: it is unbounded and would cause instability or limit cycles after an over.
2. **Coefficients:** Embed frozen `const` tables per `(profile, rate ∈ {352800, 384000})`. Add a unit test that recomputes them with the Rust port of §2.3 (Levinson plus `ε` bisection) and checks agreement to `1e-9`, so the design is auditable rather than magic numbers. Other rates (lower-rate DACs): fall back to `TpdfDither` with a `[shaper warn]`, or design at startup. Never reuse 8fS tables at other rates.
3. **Dither:** Keep the existing SplitMix64 TPDF, with independent draws per channel, so L/R noise stays uncorrelated. Order of operations: over-count first (Review 6 #18), then add feedback and dither, round, compute `e`, clamp.
4. **State lifetime:** State persists across blocks, track boundaries and gapless joins, because the shaper sits after convolution tail carry-over. Reset it on sample-rate change, stop or seek. A reset costs at most `N` samples of unshaped (still dithered) noise, which is inaudible.
5. **Output bits:** Add `output_bits ∈ {16, 18, 20, 24}` (config `[quantizer] dac_bits`, default `24`). Today's hard-coded `24` in `pipeline.rs` becomes this value. Samples stay left-justified in the `i32` container (`wrapping_shl(32 - B)` as today).
6. **Auto rule (`auto_select_shaper`):** `B = 24 → TpdfDither`; `B ≤ 20 → NsGentle2`; untouched native `≥ 32-bit → Bypass` (unchanged). NS5/LNS15/wide40 classes only by explicit override.
7. **TDD gates (port of the prototype checks):**
   - Measured band noise (Welch PSD of `y - x`) within `±0.5 dB` of `|NTF|²·LSB²/4` for every profile and rate (the prototype matches to within 0.1 dB).
   - Feedback magnitude `≤ 1.5·Σ|c_k|` LSB on a `-0.5 dBFS` sine and on a synthetic `+3 dB` over burst (no instability after clipping).
   - A `-100 dBFS` 997 Hz tone at 16 bits: no harmonic spur above the dithered noise floor (decorrelation check).
   - Digital silence in, bounded output out, no idle tones (no spectral line above `floor + 10 dB`).
   - `N = 0` profile is bit-identical to today's `TpdfDither` output for the same seed (regression).
   - Deterministic: same seed gives a byte-identical stream.

---

## 7. Hardware Probes Required (Gungnir 2, feeds INV-01)

Requires an ADC loopback into REW (any 24-bit interface).
1. **LSB passthrough probe (OQ-32):** Play a `384 kHz` test file holding a 1 kHz tone at about `-110 dBFS`, coded **only in bits 17–24** (bits 1–16 static). If the tone appears at the analog output, Gungnir 2 resolves below 16 bits and `dac_bits = 24` is right. If it is absent, the DAC truncates to 16 bits, and `dac_bits = 16` + `ns-gentle-2` is objectively better.
2. **Low-level linearity:** `-60 / -80 / -100 dBFS` tones at `tpdf/24`, `tpdf/16`, `ns-gentle-2/16` and `ns5-class/16`. Compare harmonic spurs and the noise floor (this exposes the glitch and code-activity effect from §5).
3. **Ultrasonic output:** Wideband capture (≥ 192 kHz ADC) of silence plus shaped noise for each profile. Confirms §4's ultrasonic figures, and shows NOS ZOH image levels and any analog reconstruction roll-off (not published by Schiit).

---

## 8. Open Questions Update

- **OQ-11 (Resolved, pending §7 confirmation):** Default **flat TPDF at 24-bit**. For reduced output depth, use clean-room `ns-gentle-2` (2nd-order, +6 dB ultrasonic ceiling). Higher-order 8fS profiles are opt-in listening options. Coefficients come from the Levinson closed form in §2.3.
- **OQ-03 (Partially answered):** `DAC8812` is 16-bit, `±1 LSB` INL/DNL, `0.5 µs` settling, `2 MSPS` (TI product page). Ultrasonic *power* from 8fS shaping is not a hazard (≤ `-81 dBFS`). Code-activity glitch effects remain to be measured (§7.2).
- **OQ-32 (New, INV-01):** Does Gungnir 2 (Unison 384 → SHARC → DAC8812 ×4, NOS mode) resolve input bits below 16, or truncate/round to 16 bits? This decides `dac_bits` (§7.1).

---

## 9. Sources

**Verified in this session**
- TI `DAC8812` product page: 16-bit, INL/DNL `±1 LSB`, `0.5 µs` settling, `2 MSPS`, 50 MHz SPI, 10 MHz reference bandwidth — https://www.ti.com/product/DAC8812 (datasheet SBAS349F: https://www.ti.com/lit/ds/symlink/dac8812.pdf)
- Schiit Gungnir product page: four `DAC8812C` in Multiform; SHARC 8x filter; NOS mode bypasses the filter; Unison 384 up to 32/384; THD+N `< 0.0008 %`, S/N `> 112 dB` — https://www.schiit.com/products/gungnir
- SoX `dither.c` (LGPL-2.1+): noise-shaping tables only for 8–48 kHz; falls back to TPDF otherwise — https://github.com/chirlu/sox/blob/master/src/dither.c
- CamillaDSP `filters/dither.rs` (GPL-3.0 / MPL-2.0): Shibata tables up to 192 kHz (sourced from SSRC); the rate in each name is a label and is not checked — https://github.com/HEnquist/camilladsp/blob/master/src/filters/dither.rs
- python-deltasigma (Schreier toolbox port, BSD-style licence) — https://github.com/ggventurini/python-deltasigma (consulted for licence only; no code used)
- Prior project primary sources: `[HQP-M5]` HQPlayer 5.7.3 manual p.18–19 and `[PGGB-V9]`, as extracted in report 03 §5.

**Standard theory, cited from DSP literature (primary text not re-read this session)**
- M. A. Gerzon & P. G. Craven, *Optimal Noise Shaping and Dither of Digital Signals*, AES Preprint 2822 (1989).
- S. P. Lipshitz, R. A. Wannamaker, J. Vanderkooy, *Quantization and Dither: A Theoretical Survey*, JAES 40(5), 1992.
- R. Schreier & G. C. Temes, *Understanding Delta-Sigma Data Converters* (Lee's rule; multibit stability).

**Could not access (Browser PDF Handoff: please drop into `docs/research/sources/` or paste snippets)**
1. `DAC8812` datasheet PDF text: **glitch impulse energy (nV·s)** and output noise density (downloaded but could not be parsed; no PDF tool available).
2. Gungnir 2 measurements: ASR review thread, Stereophile *Schiit Gungnir Multibit* measurements (`https://www.stereophile.com/content/schiit-audio-gungnir-multibit-dac`, HTTP 403), Archimago (403), SBAF Gungnir 2 thread. Wanted: low-level linearity, noise floor, ultrasonic output and NOS-mode behaviour.
3. Any Schiit statement (Jason Stoddard / Mike Moffat) on bit depth reaching the DAC8812s in Gungnir 2 (decides OQ-32).
4. Audiophile Style / ASR posts by Jussi Laako on `NS5` vs `LNS15` vs `TPDF` into NOS R-2R at 8fS, and on ultrasonic noise.
5. The three AES papers above, to confirm §2 against the primary text.
