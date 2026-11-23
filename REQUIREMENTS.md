# Audiophile Player — Project Requirements

**Repository:** `git@github.com:androxylo/audiophile_player.git`  
**Status:** Phase 0 — Requirements & Preparation Research (no implementation code until research is complete)

---

## 1. Project Vision & Core Philosophy

Build an extreme, best-in-class headless audiophile playback engine for Windows with **zero compromises on sound quality** (as defined by audiophiles), achieved by deliberately sacrificing UI, convenience, real-time responsiveness, and broad feature scope.

### Quality Benchmarks
- **Target:** Match or exceed **HQPlayer** in sound quality (without HQPlayer's GUI).
- **Minimum Bar:** Clearly exceed **Audirvana** in sound quality.

### Development Approach
- **Research-First:** Complete thorough technical preparation and architectural research before writing a single line of production code.
- **Incremental, Low-Token Cadence:** Progress in small, self-contained steps over several months. Every research decision and design tradeoff will be documented in the repo so work can resume cleanly across sessions.
- **Test-Driven Development (TDD):** Development will be strictly test-driven, built in small steps with automated tests validating every mathematical, DSP, and transport requirement.
- **Language Strategy:**
  - **Final Production Language:** **Rust**.
  - **Prototyping, POCs & Test Harnesses:** Any language (e.g., Python/SciPy for DSP math & measurement plots, C/C++ for Win32 kernel-streaming POCs) may be used freely during experimentation and testing.
  - **Porting Rule:** Do **not** convert prototypes or POCs to Rust until they are proven and working.

### Licensing & Copyright Compliance
- **Public Open-Source Repository:** The repository is public and must strictly respect copyright laws.
- **License:** Open-source license (such as **LGPL**; exact license to be finalized later). All third-party library choices must be vetted for open-source license compatibility.
- **No Copyrighted or Restrictively Licensed Content in Repo:** The repository must **never** contain copyrighted music files **or any other restrictively copyrighted materials** (such as paywalled papers, copied articles, proprietary binaries/SDKs, proprietary filter coefficients, or restrictively licensed third-party code).
  - Real-world music tests run strictly against an external local folder (e.g., `~/Music`).
  - Automated tests in the repository may use programmatically generated test signals and/or AI-generated, non-copyrighted synthetic music samples.

---

## 2. Explicit Compromises (What We Deliberately Sacrifice)

To keep the signal path pure and engineering focused entirely on sound quality:

| Sacrificed / Excluded | Rationale |
| --- | --- |
| **No desktop GUI / rich UI** | Do not compete with existing players that already provide full user interfaces. Run headless, with only an **extremely rudimentary local HTTP control page** (pick a file/folder, Play/Stop, and link to the GitHub manual) for out-of-the-box testing, plus optional tandem operation with companion software. |
| **No real-time / low-latency requirement** | Playback latency (e.g., 10 seconds or whatever processing requires) is completely acceptable. Correct mathematical processing takes absolute priority over responsiveness. |
| **No volume control** | Avoid variable digital attenuation pipelines and user-facing gain stages (only fixed safety headroom is applied; see §5.2). |
| **No general DSP suite** | No parametric EQ UI, tone controls, crossfeed, compression, or effects. Only REW stereo convolution and minimal channel delay alignment. |
| **Windows-only** | No macOS or Linux portability overhead; optimize directly for Windows kernel-level audio transport. |
| **Single reference DAC** | Optimize and validate strictly against the **Schiit Gungnir 2** DAC (Unison 384, up to 32/384 kHz). |
| **`352.8 / 384 kHz` native optimization only** | All DSP algorithms, noise shapers, and tests are explicitly optimized for **`352.8 kHz` and `384 kHz`** (`8fS`). We cannot optimize all sample rates equally well; DACs lacking `352.8/384 kHz` support are slightly penalized and gently recommended to upgrade (e.g., even the accessible Schiit Bifrost 3 with Unison 384 supports up to 32/384 kHz). |
| **Only two audio sources** | Local FLAC files and Tidal only—nothing else (no Spotify, Qobuz, radio, podcasts, video, etc.). |

---

## 3. Target Environment & Hardware

- **Operating System:** Microsoft Windows only.
- **Reference / Test DAC:** **Schiit Gungnir 2** (sole test DAC available for development and listening/measurement validation).

---

## 4. Audio Sources & Ecosystem Integration

The player will handle **two and only two sources**, and work in tandem with existing software rather than replacing full-featured GUI players:

1. **Local Files**
   - Local **FLAC** audio library, commonly HiRes (e.g., 24-bit / 96 kHz — noted as 24/94 in initial brief — as well as standard 16/44.1 and other PCM rates).
   - Companion integration concept (to be researched): Can leverage or pair with an existing player (such as **foobar2000** or another better-fitting frontend/controller) for browsing/queueing if helpful.
2. **Tidal Streaming**
   - Support playback from a **Tidal** subscription.
   - Companion integration concept (to be researched): Work in tandem with the **vanilla Tidal desktop player** as an external streaming source (capturing/routing lossless PCM cleanly into our processing engine without OS mixer degradation).

---

## 5. Audio Processing Pipeline (Strict DSP Specification)

Every processing stage must prioritize mathematical purity:
- **Floating-Point-Only Processing:** **All internal calculations** across the entire DSP pipeline (decoding normalization, headroom scaling, upsampling, convolution, channel delay) must be executed strictly in **floating point only** (e.g., 64-bit IEEE 754 `float64` or higher as determined by research). **Only the final output** is converted/quantized into the DAC's native integer PCM format.
- **Strict 4-Million-Tap Cap:** Filter length is strictly capped at **4,000,000 taps (4M)**. This bounds mathematical complexity and memory buffers and is **not** a sound-quality compromise—there is no credible evidence that exceeding 4M taps yields any audible benefit.
- **Curated Audiophile Filter Set:** We will **not** replicate HQPlayer's massive catalog of filters. Instead, we will implement a curated set covering **all filters consistently praised in audiophile reviews and message boards as pleasant and audiophile-class**.
- **Dynamic On-the-Fly Algorithm Selection:** The engine will **switch upsampling and dithering/noise-shaping algorithms on the fly depending on the music source** (e.g., adapting automatically to input sample rate such as 1x `44.1/48 kHz` vs. Nx HiRes `88.2/96/192 kHz`, bit depth, local FLAC vs. Tidal stream, or other source/track attributes determined during research).

### 5.1 Locked `352.8 / 384 kHz` Floating-Point DSP Domain & HiRes Upsampling
- **Explicit `352.8 / 384 kHz` (`8fS`) Optimization (Decided):** All DSP algorithms (upsampling filters, REW stereo convolution, channel delay, and noise shapers) and **all automated tests** are explicitly designed and optimized for **`352.8 kHz`** (`44.1 / 88.2 / 176.4 kHz` family via exact 8x / 4x / 2x integer ratios) and **`384 kHz`** (`48 / 96 / 192 kHz` family via exact 8x / 4x / 2x integer ratios) in pure floating point.
- **No Compromise for Lower-Rate DACs:** Output runs natively at `352.8 / 384 kHz` with **zero downsampling** on Gungnir 2 (and other Unison 384 / 384 kHz-capable DACs such as Bifrost 3). Users with DACs that do not support `352.8 / 384 kHz` accept a slight penalty (e.g., end-stage downsampling if ever added) and are gently recommended to upgrade.
- Because there is **no real-time constraint**, computationally intensive algorithms (up to the strict **4,000,000-tap cap**, offline/whole-track or large-block processing, pure high-precision floating-point arithmetic) are viable and encouraged wherever they improve fidelity.

### 5.2 Safety Headroom (`-3 dB` to `0 dB`, Never Boosting)
- **No user volume control.**
- **Never increase volume (strictly ≤ 0 dB gain):** Digital gain boosting (`> 0 dB`) is strictly non-audiophile-compliant and forbidden.
- **FLAC ReplayGain & True Peak Aware Attenuation (`-3 dB` vs. `0 dB`):**
  - Read **ReplayGain** and **True Peak** metadata annotations in FLAC files when present.
  - For quiet / well-mastered tracks where the peak level does not require `-3 dB` safety attenuation, skip the `-3 dB` reduction (`0 dB` gain), giving low-volume recordings a natural leveling advantage without ever boosting.
  - **Unknown Peak Fail-Safe:** Whenever a track's maximum level is unknown (e.g., untagged FLAC or live Tidal stream), **always apply the full `-3 dB` safety attenuation**.
  - Exact switching threshold / curve between `-3 dB` and `0 dB` (and interaction with REW convolution peak gain) will be finalized in `INV-04`.

### 5.3 Stereo Convolution (REW) — Core First-Class Feature
- **Main Feature (Not a Sidecar):** Applying **stereo convolution filters** produced by **REW (Room EQ Wizard)** is a primary, first-class feature of the engine, deeply integrated into the locked `352.8 / 384 kHz` floating-point DSP pipeline.
- **Default Discovery Location & Selection Rule:** Convolution filters live in **`C:\Program Files\EqualizerAPO\config`** by default. The engine automatically picks the **most recent stereo (2-channel) `.wav` file by change/modification date** matching the active `8fS` target sample rate (`352,800 Hz` for the 44.1 kHz family; `384,000 Hz` for the 48 kHz family), ignoring mono L/R helper files.
- **Format & Tap Length:** Supports REW's native 32-bit IEEE float (and 64-bit float / 24-bit PCM) stereo WAV exports up to REW's `4M` (`2^22 = 4,194,304`-tap) power-of-two IR length. Exact processing order (sequential vs composite FIR kernel) will be finalized in `INV-05`.

### 5.4 Allowed Minor DSP Exception
- **No other DSP settings are permitted**, with one minor allowed exception:
  - **Per-channel time delay** (e.g., delaying one channel for time alignment in non-symmetric listening rooms).

---

## 6. Audio Output, Transport & Zero-Mixer Rule (Windows)

- **Audirvana-Style Kernel-Level Streaming:** Output directly via Windows Kernel Streaming (kernel mode as implemented in **Audirvana**—exact mechanism to be investigated and replicated).
- **Zero Windows Mixer Anywhere in the Pipeline:** The Windows audio mixer must **never** be in the signal path at any stage (neither on input capture/routing nor on DAC output).
- **Exclusive Mode Only:** Non-exclusive / Shared Mode will **not be supported at all, in any form**.
- **Virtual Audio Cables Disqualified (If Mixer-Involved):** Standard virtual audio cables are **non-compliant** because they involve the Windows audio mixer in one way or another. Any Tidal or companion integration mechanism must strictly guarantee zero Windows mixer involvement.

---

## 7. Release Milestones

### Milestone 0 — Preparation & Research (Current Phase)
Complete deep technical research before writing any production code.
- See [docs/research/INVESTIGATION_PLAN.md](docs/research/INVESTIGATION_PLAN.md) for the full ordered plan of investigations, resource-scanning scope (including audiophile message boards), and the browser-PDF handoff protocol for inaccessible resources.
- See [docs/research/OPEN_QUESTIONS.md](docs/research/OPEN_QUESTIONS.md) for all tracked open technical and architectural questions.

### Milestone 1 — Initial Fully Featured Local Release (MVP)
A headless player delivering the **complete uncompromising audio quality pipeline**, usable out of the box even before Tidal streaming is implemented:
- **CLI Input:** Accepts a root folder path containing subfolders (e.g., `~/Music`) or individual FLAC files.
- **Rudimentary Built-in HTTP Server:** Serves an ultra-minimal local web page so users can test and operate the player out of the box with little or no command-line knowledge:
  - Literally **pick a file or folder**, **Play**, and **Stop** — nothing more (no volume slider, no DSP UI, no heavy frontend, strictly isolated from the real-time audio output thread).
  - Includes a direct link to the project manual at the GitHub repository (`https://github.com/androxylo/audiophile_player`).
- **Playback Behavior:** Plays a selected FLAC file, or discovers supported FLAC files recursively in a selected folder and plays them **at random**.
- **Full Audio Pipeline:**
  - HiRes upsampling
  - Safety headroom (`-3 dB` to `0 dB` via FLAC ReplayGain / True Peak tags, never boosting `> 0 dB`, always `-3 dB` when max level is unknown; no user volume control)
  - Stereo REW convolution filter
  - Optional single-channel delay
  - Kernel-level exclusive audio output directly to Gungnir 2 (bypassing Windows mixer)
- **Latency:** Unconstrained (buffer/process as long as needed for maximum quality).

### Milestone 2 — Tidal & Tandem Companion Integration
- Integrate vanilla Tidal desktop player as an external streaming source through the identical audio processing and kernel-streaming pipeline.
- Optional tandem integration with a companion GUI player (e.g., foobar2000 or similar) for local library control.
