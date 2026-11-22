# Investigation Plan (Phase 0 — Pre-Implementation Research)

**Rule:** No production code will be written until all required investigations below are completed, reviewed, and consolidated into a concrete technical specification.

---

## 1. Research Protocol & Resource Access Policy

1. **Broad, Multi-Source Coverage:** Every investigation must scan all accessible authoritative resources relevant to its topic:
   - Official hardware manuals, datasheets, and vendor whitepapers (e.g., Schiit, TI DAC8812, Microsoft Windows Audio SDK).
   - Academic & engineering literature (AES papers, Stanford CCRMA, DSP textbooks).
   - Objective measurement databases and blogs (Infinite Wave SRC, Audio Science Review, Archimago's Musings, DeltaWave/null-test analyses).
   - Open-source reference implementations (CamillaDSP, PortAudio WDM-KS, FlexASIO, `libsoxr`, `r8brain-free-src`, `rubato`).
   - **Audiophile reviews & message boards** (Audiophile Style / Computer Audiophile, Super Best Audio Friends, Head-Fi, Roon Community, Hydrogenaudio, AV NIRVANA, Audio Science Review forums, Reddit audiophile/DSP communities).
2. **Browser PDF Handoff Protocol (Inaccessible Resources):**
   - **Whenever the agent cannot access a required resource**—due to paywalls, login walls, anti-bot/Cloudflare blocks, JavaScript-only forum pages, rate limits, or unindexed PDFs—the agent **must not guess or skip it**.
   - Instead, the agent will list the exact URL, thread title, or document name and ask the user to **grab the PDF from their browser** and place it locally in `docs/research/sources/` (gitignored so copyrighted PDFs are never committed to the public repo) or share it directly in chat so the agent can read and incorporate it.
3. **Incremental Execution (Low-Token Cadence):**
   - Execute **one investigation (or half an investigation)** per session.
   - Save each completed investigation as `docs/research/0X-<topic>.md` with full source citations.
   - After each investigation, update [OPEN_QUESTIONS.md](OPEN_QUESTIONS.md) with resolved answers and any newly discovered questions.
4. **Hard Design Constraints Governing All Investigations:**
   - **Strict 4,000,000-tap (4M) cap** (bounds mathematical complexity and memory buffers; not a quality compromise).
   - **Locked `352.8 / 384 kHz` (`8fS`) DSP & test target:** All upsampling filters, REW convolution (first-class core feature), channel delay, noise shapers, and test suites are explicitly optimized for `352.8 kHz` (44.1x family) and `384 kHz` (48x family). Lower-rate DACs are not equally optimized.
   - **All internal calculations strictly in floating point**; integer conversion occurs **once** at final DAC output formatting.
   - **Curated filter set** covering all filters consistently praised in audiophile reviews and message boards as pleasant and audiophile-class (not HQPlayer's entire catalog), with **on-the-fly algorithm switching depending on the music source**.
   - **Safety headroom (`-3 dB` to `0 dB`, never boosting > 0 dB)** informed by FLAC ReplayGain & True Peak annotations (always `-3 dB` when max level is unknown), no user volume control, stereo REW convolution, optional single-channel delay only, no other DSP.
   - **No real-time constraint**; **Windows-only**; **no GUI**; **Schiit Gungnir 2** reference DAC; **local FLAC + Tidal only**.
   - **Zero Windows mixer anywhere in the pipeline** (input or output); **non-exclusive mode strictly forbidden in any form**; mixer-involving virtual audio cables are **non-compliant**; target **Audirvana-style Windows kernel-mode streaming**.
   - **Public repo / copyright compliance:** No copyrighted music or any other restrictively copyrighted materials committed (papers, articles, proprietary blobs/SDKs/code); open-source license (LGPL-like, TBD); **TDD** in small steps; **Rust** as final production language, with any language allowed for prototyping/POC/tests (port to Rust only once working).

---

## 2. Ordered Plan of Investigations

| ID | Investigation | Output File | Status |
| --- | --- | --- | --- |
| **INV-00** | Landscape Survey, Existing Players & Master Source List | [00-landscape-and-sources.md](00-landscape-and-sources.md) | Done |
| **INV-01** | Schiit Gungnir 2 DAC Architecture, NOS Mode & Unison 384 USB | `01-gungnir2-hardware.md` | Pending |
| **INV-02** | Audiophile Filter Consensus Survey (Reviews & Message Boards) | [02-audiophile-filter-survey.md](02-audiophile-filter-survey.md) | In Progress |
| **INV-03** | Mathematical Filter Design, 4M-Tap Cap & Floating-Point Precision | `03-filter-math-and-precision.md` | Pending |
| **INV-04** | Final DAC Quantization, Dither/Noise Shaping & `-3 dB` ISP Headroom | `04-quantization-dither-headroom.md` | Pending |
| **INV-05** | REW Stereo Convolution, Processing Order & Channel Delay | `05-convolution-and-delay.md` | Pending |
| **INV-06** | Audirvana Kernel Mode, Windows Kernel Streaming (WDM-KS) & Bit-Perfect Output | `06-windows-kernel-streaming.md` | Pending |
| **INV-07** | Tandem Architecture: Mixer-Free Tidal Capture & Local Companions | `07-tidal-and-companions.md` | Pending |
| **INV-08** | Rust Production Architecture, Prototyping Stack, Licensing & TDD Verification Plan | `08-architecture-and-verification.md` | Pending |

---

## 3. Detailed Scope per Investigation

### INV-01 — Schiit Gungnir 2 DAC Architecture, NOS Mode & Unison 384 USB
- **Objective:** Establish exact hardware specs, quirks, and optimal digital input format for the sole test DAC.
- **What to investigate:**
  - Official Gungnir 2 manual, specs, and Schiit technical notes on *Multiform™*, four TI **DAC8812CRUZ** chips, and Unison 384™ USB.
  - Real-world measurements and user/engineering reports (SBAF, Head-Fi, ASR, reviews) comparing **NOS mode vs. internal 8x Megacomboburrito/SHARC filter**.
  - Exact PCM sample rates & bit depths accepted by Unison 384 USB on Windows (e.g., 16/24/32-bit container format, native 352.8 kHz and 384 kHz behavior, 176.4/192 kHz comparison).
  - Effective linearity / noise floor of the DAC8812CRUZ implementation (what bit depth is actually resolved electrically) and sensitivity to ultrasonic noise in NOS mode.
  - Windows USB driver stack (`usbaudio2.sys` vs any Schiit driver), Kernel Streaming / WASAPI Exclusive compatibility, and relay-click/muting behavior on sample-rate switches.

### INV-02 — Audiophile Filter Consensus Survey (Reviews & Message Boards)
- **Objective:** Identify the exact curated set of upsampling/reconstruction filters audiophiles consistently praise as pleasant and audiophile-class, especially for PCM upsampling to NOS / R2R / multibit DACs.
- **What to investigate:**
  - Scan Audiophile Style (HQPlayer & PGGB mega-threads, Jussi Laako's posts), Roon Community (HQPlayer filter threads), Head-Fi, SBAF, ASR, Archimago's reviews, and Chord WTA discussions.
  - Categorize most-praised PCM filter archetypes and subjective/objective reputations:
    - Ultra-long sinc / Gaussian-windowed sinc (`sinc-M`, `sinc-L`, `sinc-Mx`, `sinc-MG` / `sinc-MGa`, PGGB windowed-sinc, Chord WTA-style).
    - Polyphase Gaussian & apodizing families (`poly-sinc-gauss`, `poly-sinc-gauss-long`, `poly-sinc-gauss-xla`, `poly-sinc-gauss-hires-*`, `poly-sinc-ext2/ext3`).
    - Closed-form / polynomial / short-transient & minimum/intermediate-phase options (`closed-form`, `poly-sinc-short-mp`, `poly-sinc-ip`, apodizing variants).
    - Separate best-practice recommendations for **1x base rates (44.1/48 kHz)** vs **HiRes sources (88.2/96/192 kHz)** (e.g., different filters for Redbook vs 24/96 FLAC).
  - Distill findings into a compact, well-justified shortlist of filters to design in INV-03.

### INV-03 — Mathematical Filter Design, 4M-Tap Cap & Floating-Point Precision
- **Objective:** Convert the shortlisted filters from INV-02 into rigorous mathematical algorithms bounded by the strict **4,000,000-tap cap** and **floating-point-only** internal math.
- **What to investigate:**
  - Exact mathematical formulations (window functions: Gaussian, Kaiser, Dolph-Chebyshev, Nuttall/Blackman-Harris; sinc/apodizing cutoffs; linear vs minimum vs intermediate phase construction via cepstrum/Hilbert transform).
  - Optimal integer-multiple upsampling ratios to Gungnir 2 (`44.1→352.8` 8x, `48→384` 8x, `88.2→352.8` 4x, `96→384` 4x, `176.4→352.8` 2x, `192→384` 2x) vs any non-standard input rates.
  - Floating-point precision analysis: Is IEEE 754 `float64` (53-bit mantissa, ~15–17 decimal digits) completely lossless up to 4M-tap accumulation, or do extended precision (`f80` / double-double / Kahan summation) or FFT-domain convolution prevent measurable rounding noise?
  - Computational and RAM budgeting: Direct polyphase FIR vs FFT overlap-add convolution for up to 4M taps; buffer sizes, startup latency (e.g., 5–15 seconds), and CPU/RAM footprint on Windows.
  - Pre-ringing, passband ripple, transition bandwidth, and stopband attenuation targets (benchmarked against Infinite Wave / HQPlayer / PGGB standards).

### INV-04 — Final DAC Quantization, Dither/Noise Shaping & `-3 dB` / `0 dB` ISP Headroom
- **Objective:** Perfect the single point where our floating-point pipeline converts to DAC integer PCM, while guaranteeing zero clipping under all real-world material and never boosting gain (`≤ 0 dB`).
- **What to investigate:**
  - Empirical and theoretical worst-case inter-sample over (ISP) peaks across mastered recordings + our chosen filters.
  - **FLAC ReplayGain & True Peak Metadata Rules:** Exact Vorbis comment tags used by tools like foobar2000/loudgain/r128gain (`REPLAYGAIN_TRACK_PEAK`, `REPLAYGAIN_ALBUM_PEAK`, `REPLAYGAIN_TRACK_GAIN`, `REPLAYGAIN_ALBUM_GAIN`, `R128_TRACK_GAIN`, etc.); define the exact decision rule for switching cleanly between **`-3 dB` and `0 dB`** (never `> 0 dB` boost; always `-3 dB` when max level is unknown, e.g., untagged FLAC or Tidal stream).
  - Interaction between the `-3 dB` / `0 dB` safety margin and REW convolution boost peaks (ensuring zero clipping without violating the no-boost / no-user-volume rules).
  - Conversion from `float64` to Gungnir 2's integer format (24-bit vs 32-bit PCM): scaling (`2^23` vs `2^23 - 1`, symmetric vs asymmetric), rounding modes.
  - Dither & noise shaping specifically for a **NOS multibit DAC (DAC8812CRUZ)** at **352.8/384 kHz**:
    - TPDF dither vs shaped dither (e.g., Lipshitz/Wannamaker, HQPlayer `NS1`/`NS4`/`NS5`/`NS9`/`LNS15`, PGGB adaptive noise shaping) vs straight 24-bit/32-bit rounding.
    - Critical check: Does high-order ultrasonic noise shaping harm a NOS multibit DAC or downstream analog electronics?

### INV-05 — REW Stereo Convolution, Processing Order & Channel Delay
- **Objective:** Design the mathematically purest way to apply REW stereo WAV impulse responses and single-channel room-alignment delay alongside HiRes upsampling.
- **What to investigate:**
  - Exact REW stereo WAV IR export formats (sample rates, 32-bit float vs 24-bit PCM, normalization modes, minimum-phase vs excess-phase IRs, typical lengths e.g., 65k–512k taps).
  - **Pipeline Ordering Tradeoffs:**
    1. Convolve at native source rate → Upsample;
    2. Upsample audio → Convolve with upsampled (or native-rate-exported) IR;
    3. **Combined single-stage FIR / frequency-domain multiplication** (convolving the upsampling kernel with the REW IR mathematically in `float64`!).
  - How to handle mixed-rate libraries (`44.1`, `48`, `88.2`, `96`, `192 kHz`) when the user provides a single stereo REW WAV file (or rate-matched set).
  - Measuring/handling REW IR gain so positive EQ boosts cannot exceed the `-3 dB` safety margin.
  - Single-channel delay implementation: Pure integer-sample delay at 352.8/384 kHz (~2.6 µs / ~0.9 mm resolution) vs fractional-sample allpass/sinc delay.

### INV-06 — Audirvana Kernel Mode, Windows Kernel Streaming (WDM-KS) & Bit-Perfect Output
- **Objective:** Understand exactly **how the kernel mode used in Audirvana works** so we can replicate the same mechanism, guaranteeing zero Windows mixer involvement and rock-solid stability on Gungnir 2.
- **What to investigate:**
  - **How Audirvana's Windows Kernel Streaming mode works in detail:** APIs/interfaces used (`ks.h`/`ksmedia.h`, WDM-KS pin creation, `WaveRT` vs standard KS IRP streaming, `SysAudio`/filter topology bypass), exclusive hardware acquisition, buffer management, memory/RAM pre-loading, sample-rate switching, and any published technical notes/posts from Damien Plisson / Audirvana forums / reverse-engineering analyses.
  - Open-source WDM-KS reference implementations (`PortAudio` `paWDMKS`, `foobar2000` `foo_out_ks`, `Album Player` KS) to compare against Audirvana's behavior.
  - Strict enforcement of **Exclusive-only / Zero Windows Mixer** policy (non-exclusive / Shared Mode explicitly unsupported in any form).
  - Known WDM-KS quirks with Microsoft's `usbaudio2.sys` and Schiit Unison 384 USB (device enumeration, exclusive pin locking, clean stream teardown/reopen when switching between `352.8 kHz` and `384 kHz`, avoiding clicks/pops/hangs).
  - Full-memory (RAM) pre-buffering strategy (inspired by Audirvana & Album Player): pre-computing processed track buffers in RAM, locking pages (`VirtualLock`), pacing steady writes to the kernel audio buffer, MMCSS (`AvSetMmThreadCharacteristicsW` `"Pro Audio"`), thread affinity.
  - Objective vs. subjective evidence on Windows playback hygiene (CPU wakeups, USB polling burstiness, power plans, DPC latency) and how to verify 100% bit-perfect transfer to the DAC (proving the Windows mixer is never touched).

### INV-07 — Tandem Architecture: Mixer-Free Tidal Capture & Local Companions
- **Objective:** Design a strictly **Windows-mixer-free, exclusive-only** architecture for working with Tidal and local companions so Milestone 1 and Milestone 2 share one clean core.
- **Hard Compliance Constraint:** Standard virtual audio cables are **non-compliant** because they involve the Windows audio mixer in one way or another. **Non-exclusive mode is strictly disallowed in any form.**
- **What to investigate:**
  - Exact audio output behavior of the official **vanilla Tidal Windows desktop app** in **Exclusive Mode** (WASAPI Exclusive / direct endpoint interaction, HiRes FLAC 24/44.1–24/192 dynamic sample-rate switching, whether Tidal supports ASIO or KS output, DRM/output constraints).
  - **Strictly mixer-free capture/integration paths** (verifying whether any mechanism genuinely keeps the Windows audio engine/mixer 100% out of the path):
    - Can a custom **kernel-mode / WaveRT virtual audio endpoint** (accepting Tidal's *WASAPI Exclusive* output directly at the kernel miniport level, completely bypassing `audiodg.exe` / Windows mixer, with dynamic sample-rate switching) be built cleanly, and what are Windows 11 driver-signing implications?
    - Can an **exclusive ASIO-only** or **direct kernel-pin** sink intercept Tidal without touching Windows Audio Engine? (Verify whether vanilla Tidal even supports ASIO.)
    - If vanilla Tidal Desktop cannot be tapped without involving the Windows mixer at any layer, investigate compliant mixer-free alternatives (e.g., official/sanctioned Tidal API / UPnP / OpenHome / Tidal Connect / Audirvana-like direct Tidal stream ingestion, or companion bridges that stream raw FLAC without Windows audio routing).
  - Streaming-mode vs file-mode DSP architecture: How does a continuous live stream from Tidal (no known track end, live sample-rate changes) feed a high-latency 4M-tap overlap-add engine cleanly (accepting e.g. 5–15 s playback delay)?
  - Local companion options beyond Milestone 1's folder-shuffle CLI: Can `foobar2000` (or another lightweight UI player) cleanly hand off file paths / playlists / lossless IPC streams to our headless engine without touching Windows audio routing at all?

### INV-08 — Rust Production Architecture, Prototyping Stack, Licensing & TDD Verification Plan
- **Objective:** Finalize the Rust production architecture, prototyping/POC toolchain, open-source license compatibility, and TDD verification/benchmarking suite before writing production code.
- **What to investigate:**
  - **Rust Production Crates & Bindings (with LGPL-compatible open-source licensing):**
    - Win32 WDM-KS (`windows-rs` / direct Win32 `ks.h`/`ksmedia.h` FFI vs thin C/Rust interop module).
    - High-precision 64-bit FFT engines (`RustFFT`/`RealFFT` MIT/Apache-2.0 vs `PocketFFT` BSD vs `FFTW3` GPL licensing caveat!), SIMD (AVX2/AVX-512), and deterministic memory management.
    - Lossless FLAC decoding (pure Rust `claxon`/`symphonia` vs `libFLAC` BSD — bit-exactness verification).
  - **Prototyping / POC & Test Toolchain:** Best rapid-prototyping stack (e.g., Python + NumPy/SciPy for DSP filter math & spectral verification, small C/C++ Win32 POCs for WDM-KS verification) before porting working designs to Rust.
  - **Headless Engine Architecture:** Process/thread topology, configuration format (TOML/YAML/JSON) for filter selection, REW stereo WAV path, optional channel delay, and DAC device selection; Milestone 1 recursive `~/Music` random playback CLI design.
  - **TDD & Quality Assurance Plan (Copyright-Free Test Assets):**
    - Synthetic / AI-generated non-copyrighted test fixtures (chirps/sweeps, impulses, multi-tone signals, ISP stress files, synthetic musical clips) — zero copyrighted audio in repo.
    - Automated objective DSP verification (sweep/chirp aliasing plots akin to Infinite Wave, impulse/step/phase response plots, passband ripple, stopband attenuation, THD+N, ISO stress tests, peak-clipping assertions).
    - Comparative null/delta testing (e.g., Paul Kane's **DeltaWave**) against HQPlayer / PGGB reference renders (using external `~/Music` or synthetic clips).
    - Bit-perfect Windows transport verification.
    - Structured listening evaluation protocol on the Schiit Gungnir 2 using external `~/Music`.
