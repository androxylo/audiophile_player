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
   - Instead, the agent will list the exact URL, thread title, or document name and ask the user to **grab the PDF from their browser** and place it in `docs/research/sources/` (or share it directly in chat) so the agent can read and incorporate it directly.
3. **Incremental Execution (Low-Token Cadence):**
   - Execute **one investigation (or half an investigation)** per session.
   - Save each completed investigation as `docs/research/0X-<topic>.md` with full source citations.
   - After each investigation, update [OPEN_QUESTIONS.md](OPEN_QUESTIONS.md) with resolved answers and any newly discovered questions.
4. **Hard Design Constraints Governing All Investigations:**
   - **Strict 4,000,000-tap (4M) cap** (bounds mathematical complexity and memory buffers; not a quality compromise).
   - **All internal calculations strictly in floating point**; integer conversion occurs **once** at final DAC output formatting.
   - **Curated filter set** covering all filters consistently praised in audiophile reviews and message boards as pleasant and audiophile-class (not HQPlayer's entire catalog).
   - **Fixed `-3 dB` safety attenuation**, no user volume control, stereo REW convolution, optional single-channel delay only, no other DSP.
   - **No real-time constraint**; **Windows-only**; **no GUI**; **Schiit Gungnir 2** reference DAC; **local FLAC + Tidal only**.

---

## 2. Ordered Plan of Investigations

| ID | Investigation | Output File | Status |
| --- | --- | --- | --- |
| **INV-00** | Landscape Survey, Existing Players & Master Source List | [00-landscape-and-sources.md](00-landscape-and-sources.md) | Done |
| **INV-01** | Schiit Gungnir 2 DAC Architecture, NOS Mode & Unison 384 USB | `01-gungnir2-hardware.md` | Pending |
| **INV-02** | Audiophile Filter Consensus Survey (Reviews & Message Boards) | `02-audiophile-filter-survey.md` | Pending |
| **INV-03** | Mathematical Filter Design, 4M-Tap Cap & Floating-Point Precision | `03-filter-math-and-precision.md` | Pending |
| **INV-04** | Final DAC Quantization, Dither/Noise Shaping & `-3 dB` ISP Headroom | `04-quantization-dither-headroom.md` | Pending |
| **INV-05** | REW Stereo Convolution, Processing Order & Channel Delay | `05-convolution-and-delay.md` | Pending |
| **INV-06** | Windows Kernel Streaming (WDM-KS), Bit-Perfect Output & System Hygiene | `06-windows-kernel-streaming.md` | Pending |
| **INV-07** | Tandem Architecture: Vanilla Tidal Lossless Capture & Local Companions | `07-tidal-and-companions.md` | Pending |
| **INV-08** | Software Architecture, Tech Stack & Objective/Subjective Verification Plan | `08-architecture-and-verification.md` | Pending |

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

### INV-04 — Final DAC Quantization, Dither/Noise Shaping & `-3 dB` ISP Headroom
- **Objective:** Perfect the single point where our floating-point pipeline converts to DAC integer PCM, while guaranteeing zero clipping under all real-world material.
- **What to investigate:**
  - Empirical and theoretical worst-case inter-sample over (ISP) peaks across mastered recordings + our chosen filters; confirm whether `-3 dB` fixed attenuation covers virtually all tracks and how to handle rare `> +3 dB` peaks or convolution boost peaks without adding user volume controls.
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

### INV-06 — Windows Kernel Streaming (WDM-KS), Bit-Perfect Output & System Hygiene
- **Objective:** Specify the exact Windows kernel-level output architecture to match/exceed Audirvana's Kernel Streaming with rock-solid stability on Gungnir 2.
- **What to investigate:**
  - Low-level implementation mechanics of **WDM-KS (Kernel Streaming)** on Windows 10/11 (`KS` pins, `WaveRT`, PortAudio `paWDMKS`, Audirvana KS, foobar2000 `foo_out_ks`) vs **WASAPI Exclusive** (Event vs Push) and **ASIO**.
  - Known WDM-KS quirks with Microsoft's `usbaudio2.sys` and Schiit Unison 384 USB (device enumeration, exclusive pin locking, clean stream teardown/reopen when switching between `352.8 kHz` and `384 kHz`, avoiding clicks/pops/hangs).
  - Full-memory (RAM) pre-buffering strategy (inspired by Album Player & Audirvana): pre-computing processed track buffers in RAM, locking pages (`VirtualLock`), pacing steady writes to the kernel audio buffer, MMCSS (`AvSetMmThreadCharacteristicsW` `"Pro Audio"`), thread affinity.
  - Objective vs. subjective evidence on Windows playback hygiene (CPU wakeups, USB polling burstiness, power plans, DPC latency) and how to verify 100% bit-perfect transfer to the DAC.

### INV-07 — Tandem Architecture: Vanilla Tidal Lossless Capture & Local Companions
- **Objective:** Design a zero-degradation bridge for vanilla Tidal Desktop and evaluate companion GUI options for local playback so Milestone 1 and Milestone 2 share one clean core.
- **What to investigate:**
  - Current behavior of the official **vanilla Tidal Windows desktop app** (HiRes FLAC 24/44.1–24/192 support, Exclusive Mode vs Shared Mode output, sample-rate switching behavior, DRM/output restrictions).
  - Bit-perfect capture methods on Windows without Windows mixer resampling:
    - Virtual audio drivers (**VB-Audio Hi-Fi Cable**, **VB-Audio Matrix**, **Eugene Muzychenko VAC**, **Virtual Audio Driver / WDM-WaveRT sample driver** from Windows Driver Kit).
    - Critical test question: Which virtual device preserves **dynamic sample-rate switching** from Tidal Exclusive Mode (44.1 → 96 → 192 kHz) bit-perfectly with zero Windows resampler involvement?
    - Alternative check: Are there official/sanctioned lossless Tidal integration paths (or companion bridges like HQPlayer/foobar2000 setups) worth noting?
  - Streaming-mode vs file-mode DSP architecture: How does a continuous live stream from Tidal (no known track end, live sample-rate changes) feed a high-latency 4M-tap overlap-add engine cleanly (accepting e.g. 5–15 s playback delay)?
  - Local companion options beyond Milestone 1's folder-shuffle CLI: Can `foobar2000` (or another lightweight UI player) cleanly hand off file paths / playlists / streams to our headless engine?

### INV-08 — Software Architecture, Tech Stack & Verification Plan
- **Objective:** Finalize language, dependencies, headless CLI/config specification, and rigorous verification/benchmarking suite before coding starts.
- **What to investigate:**
  - **Language & Ecosystem:** **Rust** vs **C++** (vs Rust core + small C/C++ Win32 KS shim):
    - Win32 WDM-KS (`ks.h`, `ksmedia.h`, `setupapi`) and WASAPI Exclusive bindings.
    - High-precision 64-bit FFT engines (`FFTW3`, `Intel IPP`, `PocketFFT`, `RustFFT`/`RealFFT`), SIMD (AVX2/AVX-512), and deterministic memory management.
    - Lossless FLAC decoding (`libFLAC` vs pure Rust `claxon`/`symphonia` — bit-exactness verification).
  - **Headless Engine Architecture:** Process/thread topology, configuration format (TOML/YAML/JSON) for filter selection, REW stereo WAV path, optional channel delay, and DAC device selection; Milestone 1 recursive `~/Music` random playback CLI design.
  - **Verification & Quality Assurance Plan:**
    - Automated objective DSP verification (sweep/chirp aliasing plots akin to Infinite Wave, impulse/step/phase response plots, passband ripple, stopband attenuation, THD+N, ISO stress tests, peak-clipping assertions).
    - Comparative null/delta testing (e.g., Paul Kane's **DeltaWave**) against HQPlayer / PGGB reference renders.
    - Bit-perfect Windows transport verification.
    - Structured listening evaluation protocol on the Schiit Gungnir 2.
