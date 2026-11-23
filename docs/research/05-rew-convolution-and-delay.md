# INV-05: Core REW Stereo Convolution & Pipeline Ordering

## 1. Research Verdict: Convolution MUST Be Strictly **Before** the Noise Shaper

Across reference audiophile DSP engines (**HQPlayer**, **PGGB**, **Roon DSP**, **CamillaDSP**) and digital quantization theory (Lipshitz, Wannamaker & Vanderkooy), **stereo FIR convolution must always execute in high-precision floating point (`f64`) strictly BEFORE dithering, noise shaping, and final integer quantization**.

### Why Noise Shaping / Quantization MUST Be the Absolute Final Stage
1. **Noise shaping is inseparable from final integer word-length reduction:**
   - Noise shapers (`NS5`, `LNS15`, TPDF dither, etc.) shape the *quantization error* generated when collapsing continuous `f64` samples (`53`-bit mantissa, `~319 dB` dynamic range) down to a fixed-point integer grid (`24`-bit / `32`-bit / DAC-effective bits) right before the DAC (`Schiit Gungnir 2` `DAC8812CRUZ`).
   - Placing convolution *after* noise shaping turns the quantized integer stream back into continuous floating-point numbers via millions of multiply-accumulate (MAC) operations, **forcing a second quantization pass** that destroys the first shaper's error decorrelation.
2. **Convolution warps the Noise Transfer Function (NTF):**
   - An `8fS` noise shaper carefully pushes quantization noise out of the audible band (`0–20 kHz`) into ultrasonic frequencies (`50–192 kHz`).
   - Any FIR room filter applied *after* noise shaping multiplies that shaped ultrasonic noise spectrum by $|H_{\text{REW}}(e^{j\omega})|$, altering the NTF, increasing out-of-band energy where the filter boosts, and potentially causing numerical overload on the ultrasonic noise floor.
3. **Zero internal truncation in `f64` + exact headroom safety:**
   - Executing all linear time-invariant (LTI) stages in IEEE 754 `f64` before the single final quantizer keeps internal rounding noise at `~-319 dB`—orders of magnitude below both 24-bit PCM (`-144 dB`) and any physical DAC.
   - Because `select_headroom(..., rew_max_boost_db)` accounts for the exact FFT peak magnitude of the loaded REW IR (`max_boost_db`), linear headroom scaling (`-3 dB` or `0 dB`), upsampling, convolution, and channel delay all remain mathematically clip-free prior to the single final quantizer.

### Locked Pipeline Order (`ap-dsp`)
```text
Source f64 blocks (<= 65,536 frames)
  -> 1. Headroom scaling (-3 dB / 0 dB hardcoded, f64)
  -> 2. Dynamic per-track upsampling (fS -> 352.8 / 384 kHz, f64; or NullUpsampler)
  -> 3. Core REW Stereo Convolution (352.8 / 384 kHz, pure f64 UPOLS FFT)
  -> 4. Per-Channel Integer Delay (352.8 / 384 kHz, f64)
  -> 5. Single Final Dither + Noise Shaper + Quantizer (f64 -> i32)
```

---

## 2. Empirical Findings on User's REW Filters (`C:\Program Files\EqualizerAPO\config`)

- **Auto-Discovery Rule:** Scan `C:\Program Files\EqualizerAPO\config` (header-only fast probe, `< 1 ms`), filter strictly to **2-channel (`channels == 2`) `.wav`** files whose sample rate matches the active `8fS` domain (`352,800 Hz` or `384,000 Hz`), and select the **most recent by modification date** (ignoring mono `.L`/`.R` helper WAVs).
- **Verified Filter Formats:**
  - `Oct26rs225-384k.wav`: `WAVE_FORMAT_IEEE_FLOAT` (`32`-bit float), `2 ch`, `384,000 Hz`, `4,194,304` taps (`2^22`), `max_boost_db = 0.000 dB`, energy `> -200 dB` ends at `433,771` samples (`1.13 s`).
  - `Oct26rs225-384k-352k.wav`: `WAVE_FORMAT_IEEE_FLOAT` (`32`-bit float), `2 ch`, `352,800 Hz`, `4,194,304` taps (`2^22`), `max_boost_db = 0.000 dB`, energy `> -200 dB` ends at `398,833` samples (`1.13 s`).
  - WAV loader also supports `64`-bit IEEE float, `16/24/32`-bit PCM, and `WAVE_FORMAT_EXTENSIBLE` (`0xFFFE`).

---

## 3. Uniform Partitioned Overlap-Save (UPOLS) in Pure `f64`

- **FFT Engine:** `realfft` + `rustfft` (`MIT OR Apache-2.0`, LGPL-3.0 compatible, runtime x86_64 AVX2/FMA SIMD).
- **Partitioning:** Dynamic block size `B = active_taps.next_power_of_two().clamp(1024, 65_536)` with real FFT size `N = 2 * B` (`131,072` for 4M-tap IRs → `64` partitions max, `170.7 ms` block quantum at `384 kHz`, ~5% of one CPU core at 384 kHz stereo!).
- **Exactness:**
  - Every non-zero partition up to the last non-zero tap is convolved verbatim in `f64` (zero audible or ultrasonic truncation during playback); exact-zero partitions are skipped without changing a single bit.
  - Arbitrary streaming chunk sizes match one-shot linear convolution bit-for-bit (`< 1e-13` vs. naive $O(L \cdot M)$ time-domain convolution).
  - End-of-track `flush()` flushes the full impulse tail down to `-200 dB` (`1e-10`, `~1.13 s` on `Oct26rs225-384k.wav`, avoiding `8+ s` of sub-`-250 dB` denormal silence between songs while preserving exact mathematical `active_taps - 1` tail for all short/unit-test IRs).
- **Caching & Hot Reload:** `RewConvolverFactory` caches `Arc<PreparedStereoIr>` per sample rate (`352,800` / `384,000 Hz`) keyed by `(path, modified_time, file_len)` so switching tracks reuses pre-transformed spectra in microseconds and automatically picks up newly saved REW filters on the next track.
