# Open Questions Tracker

Living list of unresolved technical, mathematical, hardware, and architectural questions to be answered during Phase 0 investigations. Each question maps to its owning investigation in [INVESTIGATION_PLAN.md](INVESTIGATION_PLAN.md).

---

## 1. Hardware & Schiit Gungnir 2 (`INV-01`)

- **OQ-01:** Does Schiit Gungnir 2 in **NOS mode** fed with external 352.8/384 kHz PCM objectively and subjectively outperform Gungnir 2's internal 8x Megacomboburrito/SHARC oversampling filter?
- **OQ-02:** What exact PCM sample rates and integer bit depths/containers (24-bit packed vs 32-bit padded, 352.8 kHz, 384 kHz, 176.4 kHz, 192 kHz) does Unison 384™ USB accept reliably on Windows 10/11?
- **OQ-03:** What is the measured electrical linearity / effective number of bits (ENOB) and noise floor of Gungnir 2's four TI `DAC8812CRUZ` chips, and does NOS mode exhibit ultrasonic imaging or sensitivity to high-order noise shaping?
- **OQ-04:** Does Schiit Unison 384 USB rely purely on Microsoft's inbox `usbaudio2.sys` driver, or is there a native ASIO/vendor driver? How cleanly does it handle Windows Kernel Streaming (WDM-KS) and sample-rate transitions (`352.8 ↔ 384 kHz`)?

## 2. Curated Audiophile Filters & 4M-Tap Mathematics (`INV-02`, `INV-03`)

- **OQ-05:** Which exact upsampling/reconstruction filters are most consistently praised across audiophile reviews and message boards (Audiophile Style, Head-Fi, SBAF, Roon, ASR, Archimago) for PCM upsampling—specifically into NOS multibit DACs?
- **OQ-06:** Should our engine use different default filters (or allow separate selection) for **1x base rates** (`44.1 / 48 kHz`, where steep brickwall/apodizing filtering is critical) versus **HiRes rates** (`88.2 / 96 / 192 kHz`, where gentler/shorter filters are often preferred)?
- **OQ-07:** What is the optimal target upsampling rate strategy for Gungnir 2—always maximum integer multiple (`44.1→352.8`, `48/96→384`), or is `176.4/192 kHz` ever preferable on `DAC8812CRUZ`?
- **OQ-08:** Under our strict **4,000,000-tap cap**, what exact tap counts per filter family (e.g., 4M-tap Gaussian-windowed sinc vs shorter linear/minimum/intermediate-phase polyphase filters) maximize sonic and objective performance without unnecessary compute?
- **OQ-09:** Is IEEE 754 `float64` (double precision) mathematically sufficient for 4M-tap FIR / FFT-based upsampling and convolution without measurable accumulation error, or is any extended precision (e.g., double-double / Kahan summation for filter coefficient generation) warranted?
- **OQ-10:** Should filter kernels be computed dynamically at startup from exact analytical equations in `float64` (with optional disk caching for 4M-tap kernels), and what startup/track-buffer latency and RAM budget should we expect?

## 3. Final Quantization, Dither & `-3 dB` Headroom (`INV-04`)

- **OQ-11:** When quantizing our final `float64` stream to Gungnir 2's native integer PCM (24-bit or 32-bit), what is the optimal dithering/noise-shaping method (TPDF dither vs low-order shaped dither vs high-order noise shaping vs simple 24/32-bit TPDF), specifically given a NOS multibit DAC?
- **OQ-12:** How do we guarantee zero digital clipping if a track contains rare inter-sample overs exceeding `+3 dB`, **or** if a user's REW convolution filter introduces positive boost peaks on top of `-3 dB` fixed headroom—while honoring the "no user volume control" and "always apply `-3 dB`" rules?
- **OQ-13:** Should the fixed `-3 dB` attenuation be applied at the very beginning of the `float64` pipeline (before upsampling and convolution) or verified against peak output before final quantization? (In pure linear `float64` math, linear gain commutes—except for potential denormals or final quantization scaling; confirm mathematically.)

## 4. REW Stereo Convolution & Channel Delay (`INV-05`)

- **OQ-14:** What is the mathematically purest **order of operations** for upsampling + REW stereo convolution:
  - Convolve at source rate then upsample?
  - Upsample audio then convolve at target rate (352.8/384 kHz)?
  - Combine the upsampling FIR and REW IR into a single mathematical convolution operation?
- **OQ-15:** What sample rate(s) will the user's REW stereo WAV convolution file be exported at? If the user supplies a single REW WAV (e.g., at 48 kHz or 96 kHz), how should our engine handle mixed-rate playback (`44.1`, `48`, `96`, `192 kHz` → `352.8 / 384 kHz`) with zero quality loss?
- **OQ-16:** Does REW export stereo IRs normalized to peak `0 dBFS` or unity passband gain (`0 dB` average)? How should our engine normalize/check REW IR gain automatically so stereo L/R balance is preserved and clipping is impossible?
- **OQ-17:** Is pure integer-sample delay at 352.8/384 kHz (~2.6 µs / ~0.9 mm acoustic resolution) completely sufficient for non-symmetric room channel delay, eliminating any need for fractional-delay filters? How should delay be specified in configuration (milliseconds, microseconds, millimeters, or samples)?

## 5. Audirvana Kernel Mode, Windows Kernel Streaming & Playback Hygiene (`INV-06`)

- **OQ-18:** **How exactly does Audirvana's Windows Kernel Streaming mode work** under the hood (WDM-KS pin creation, `WaveRT` vs IRP streaming, exclusive device locking, memory pre-loading, sample-rate switching), and can we replicate the exact same kernel-mode mechanism directly?
- **OQ-19:** On Windows 10/11 with Gungnir 2 Unison 384 USB, how reliably does Audirvana-style WDM-KS handle dynamic `352.8 ↔ 384 kHz` rate switches on Microsoft's `usbaudio2.sys`, and what low-level pin/buffer teardown handling is needed to avoid pops, clicks, or hangs? (Non-exclusive mode is strictly disallowed.)
- **OQ-20:** What is the cleanest, most reliable way to implement WDM-KS in our chosen language—direct Win32 Kernel Streaming API (`ks.h`/`ksmedia.h`), adapting PortAudio's `paWDMKS` / `foo_out_ks` architecture, or a dedicated thin C/C++ Win32 kernel-streaming module?
- **OQ-21:** Should Milestone 1 decode and process each entire track into RAM ("Full Memory" playback like Audirvana / Album Player) before streaming to the DAC, or process in large pre-buffered chunks (e.g., 10-second+ ring buffer) so the same engine handles both local files and continuous Tidal streams seamlessly?
- **OQ-22:** What is the best way to objectively prove 100% bit-perfect transfer to the USB endpoint on Windows and verify that the Windows audio mixer (`audiodg.exe`) is never involved at any point?

## 6. Mixer-Free Tidal Integration & Companion Software (`INV-07`)

- **OQ-23:** Given that **standard virtual audio cables are non-compliant** (they involve the Windows mixer in one way or another) and **non-exclusive mode is strictly forbidden**, is there *any* truly mixer-free way to capture lossless HiRes FLAC streams with dynamic sample-rate switching from the **vanilla Tidal Windows desktop app** (e.g., via a custom kernel-mode WaveRT exclusive endpoint bypassing `audiodg.exe`)?
- **OQ-24:** If tapping vanilla Tidal Desktop without any Windows mixer involvement proves impossible or requires prohibitive Windows 11 kernel-driver signing, what compliant mixer-free Tidal integration architectures exist (e.g., direct lossless stream retrieval / sanctioned APIs / UPnP / OpenHome / Tidal Connect-style renderer)?
- **OQ-25:** How should our engine handle track boundaries, silence detection, sample-rate transitions, and continuous overlap-add state when streaming live PCM from Tidal with multi-second latency (e.g., 4M-tap filter group delay)?
- **OQ-26:** For local library UX beyond Milestone 1's folder-shuffle mode, what is the cleanest tandem integration with an existing player (e.g., `foobar2000` custom plugin/IPC/playlist handoff) that sends file paths or pure PCM data directly to our engine without touching Windows audio routing?

## 7. System Architecture, Tech Stack & Verification (`INV-08`)

- **OQ-27:** Which programming language (**Rust** vs **C++** vs hybrid) gives the best combination of clean Win32 WDM-KS access, peak 64-bit FFT/SIMD performance, zero runtime GC pauses, and maintainability over a multi-month, low-token project?
- **OQ-28:** Should we build the core 64-bit FIR/FFT upsampling + convolution math directly (on top of a proven 64-bit FFT engine like `FFTW` or `RustFFT`) rather than wrapping a black-box resampler like `libsoxr`/`r8brain`, so we have 100% control over our curated audiophile filters and 4M-tap architecture?
- **OQ-29:** What is the exact configuration and CLI UX for Milestone 1 (e.g., command-line arguments + config file for `~/Music` folder, filter choice, REW WAV path, optional channel delay, DAC endpoint; handling Ctrl+C / skip / shuffle state without a GUI)?
- **OQ-30:** What objective measurement suite (sweep/aliasing plots, impulse/phase verification, ISO stress test, DeltaWave comparison vs HQPlayer/PGGB) and listening test protocol on Gungnir 2 will serve as our acceptance gate for "matches/exceeds HQPlayer and exceeds Audirvana"?
