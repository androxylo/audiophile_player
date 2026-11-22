# Research 00 — Existing Audiophile Players, Best Practices & Reference Sources

**Scope:** Broad landscape survey of existing audiophile players, DSP engines, Windows bit-perfect transport mechanisms, upsampling/convolution best practices, and authoritative primary sources to guide subsequent deep-dive research.

---

## 1. Existing Audiophile Players & Engines

Each existing tool represents a distinct design philosophy relevant to our headless, zero-compromise, latency-unconstrained Windows player:

| Software / Project | Core Philosophy | Key Strengths Relevant to Us | Key Limitations / Tradeoffs |
| --- | --- | --- | --- |
| **HQPlayer** (Signalyst) | State-of-the-art real-time mathematical DSP (upsampling, SDM/DSD modulation, convolution, noise shaping). | Benchmark filter suite (`sinc-M` 1M taps, `sinc-L`, `sinc-Mx`, `sinc-MG`, `poly-sinc-gauss-*`), 64-bit FFT convolution, high-order noise shapers (`NS9`, `LNS15`), NAA network decoupling, external input capture. | Closed-source, paid, includes GUI (Desktop) or complex Embedded setup; still bounded by real-time latency constraints. |
| **PGGB / PGGB-Live** (Remastero) | **Offline / non-real-time** ultra-high-precision remastering ("Pan Galactic Gargle Blaster"). | **Directly validates our non-real-time philosophy:** removes real-time constraints to run multi-million-tap windowed-sinc reconstruction filters at up to 256-bit internal precision, exact ISP handling, and adaptive noise shaping. | Closed-source, expensive; offline batch workflow requires massive disk storage unless using PGGB-Live. |
| **Audirvana** (Studio / Origin) | Integrated bit-perfect playback + RAM pre-loading + Windows Kernel Streaming. | Strong Windows **Kernel Streaming (KS)** implementation bypassing the OS mixer; whole-track RAM buffering; r8brain / SoX upsampling; Tidal/Qobuz integration. | Paid/subscription, closed-source, GUI-centric, limited filter depth compared to HQPlayer/PGGB. |
| **CamillaDSP** (Henrik Enquist) | Open-source, **headless Rust** DSP & convolution engine. | Cleanest open-source architectural reference for headless 64-bit float FFT convolution, resampling, per-channel delay, and Windows WASAPI/ASIO backends. | Real-time streaming design (not whole-track/mega-tap offline upsampling); no native WDM-KS backend out of the box. |
| **foobar2000** | Modular, ultra-lightweight native Windows player framework. | Free, rock-solid library/UI companion candidate; rich component ecosystem (`SoX` resampler, `foo_convolver`, legacy `foo_pggb_rt`, WASAPI/ASIO/KS output components). | Default DSP stack is basic unless heavily customized; Windows-only closed-source core. |
| **Album Player (APlayer)** (Igor Antonov) | Extreme Windows minimalism & Full Memory playback. | Free; loads entire decoded track into RAM before playback and idles disk I/O; direct/minimalist ASIO, WASAPI, and KS paths. | Spartan UI, closed-source, minimal DSP/upsampling sophistication. |
| **JRiver Media Center** | Heavyweight all-in-one media manager + full DSP studio. | Mature 64-bit audio engine, built-in convolution, WASAPI/ASIO/KS support, virtual WDM driver (can capture external apps like Tidal). | Bloated GUI/media suite, paid, not purist/minimalist. |
| **JPLAY** | Ultra-low-noise timing/transport purism (historically Windows kernel driver, now UPnP/iOS ecosystem). | Historically pioneered aggressive Windows OS-noise reduction, hibernation/minimal-thread playback, and Kernel Streaming tuning. | Paid subscription, closed-source, shifted primarily to UPnP/network renderer architecture. |
| **Diretta** | Network audio transport protocol (ASIO host → network target). | Smooths CPU/USB packet burstiness to minimize electrical/ground-plane noise at the DAC. | Not a player/DSP engine; requires dedicated network target hardware. |
| **Bug Head Emperor / XXHighEnd** | Extreme niche Windows purist players. | Cult experiments in memory rewriting, CPU cache alignment, minimal OS jitter, and custom upsampling. | Eccentric, closed-source, unstable UIs, heavy subjective folklore with limited objective verification. |

### Key Strategic Takeaway for Our Project
Our unique niche sits at the intersection of **PGGB** (unconstrained latency, ultra-long mega-tap filters, extreme precision), **HQPlayer** (Gaussian/sinc reconstruction filters + 64-bit convolution + noise shaping), **CamillaDSP** (clean, headless, open-source engineering), and **Audirvana / Album Player** (RAM-buffered Windows Kernel Streaming directly to the DAC).

---

## 2. Preliminary Hardware Context: Schiit Gungnir 2 DAC

Initial specifications confirm why our upsampling + NOS strategy is an unusually strong match for the **Schiit Gungnir 2**:
- **Architecture:** Schiit *Multiform™* balanced multibit architecture using four TI **DAC8812CRUZ** multiplying DACs.
- **Internal Filter vs. True NOS Mode:** Default mode runs Schiit's 8x oversampling time/frequency-optimized filter on an Analog Devices SHARC DSP. Crucially, Gungnir 2 offers a **true NOS (Non-Oversampling) mode** (selectable via IR remote or Forkbeard) that completely bypasses its internal digital filter.
- **USB Input:** Dual USB inputs including **Unison 384™ USB**, supporting up to **32-bit / 384 kHz PCM** (ideal targets: `352.8 kHz` for 44.1 kHz family, `384 kHz` for 48/96 kHz family).
- **Implication:** Feeding Gungnir 2 in **NOS mode** at **352.8 / 384 kHz** (integer multiples of source rates) lets our non-real-time engine serve as the *sole*, mathematically superior reconstruction filter without double-filtering. *(Will be verified in depth in Research Topic 1.)*

---

## 3. Audiophile DSP, Playback & Integration Best Practices

### 3.1 Upsampling & Reconstruction Filtering
- **Integer-Multiple Upsampling:** Prefer exact integer multiples (`44.1 → 176.4 / 352.8 kHz`, `48 / 96 → 192 / 384 kHz`) to simplify polyphase/sinc math and avoid non-integer fractional interpolation artifacts.
- **Exploiting Unconstrained Latency:** Real-time DACs use hundreds of taps; HQPlayer reaches ~1M taps (`sinc-M`); PGGB reaches multi-million taps offline. Without a real-time constraint, we can process entire tracks (or huge blocks) in **64-bit floating-point** (or higher) in RAM before playback begins.
- **Windowing & Time/Frequency Tradeoff:** Standard brickwall filters ring symmetrically (Gibbs phenomenon). Best-in-class designs evaluate **Kaiser**, **Gaussian** (optimal Heisenberg–Gabor time-frequency localization, as in HQPlayer `poly-sinc-gauss` and `sinc-MG`), and **apodizing** filters (which clean up pre-existing ADC ringing from mastering).
- **Precision & Output Quantization (Dither / Noise Shaping):** Perform all DSP in `float64` (minimum), and apply proper **TPDF dither** or **carefully bounded noise shaping** when quantizing to 24-bit/32-bit integer PCM for the DAC. Caution: overly aggressive high-order noise shaping pushes ultrasonic noise into NOS multibit DACs and downstream amps—must be evaluated specifically for Gungnir 2.

### 3.2 Inter-Sample Overs (ISOs) & `-3 dB` Safety Headroom
- Band-limited signals mastered at `0 dBFS` frequently reconstruct inter-sample peaks up to **`+3.01 dBFS`** (and occasionally slightly higher on pathological/loudness-war tracks).
- Software upsampling without headroom permanently clips these peaks before the DAC ever sees them.
- **Best Practice (Benchmark Media / Archimago / ASR consensus):** Always apply at least **`-3 dB`** digital attenuation *before* upsampling/interpolation (matching our core requirement).
- **Convolution Gain Interaction:** If a REW convolution filter contains positive gain boosts (e.g., +3 dB in bass) and is peak-normalized to 0 dBFS, total peak gain can exceed +3 dB. Best practice: analyze/normalize the combined IR peak gain (especially frequency-domain max magnitude) so `-3 dB` fixed headroom remains clip-proof, or verify peak headroom per track during whole-track pre-processing.

### 3.3 REW Stereo Convolution & Channel Delay
- **REW Export Format:** Export **Stereo, 32-bit Float WAV** impulse responses (`File → Export → Filters impulse response as WAV`), ideally using spatially averaged L/R measurements and restricting aggressive correction to below the Schroeder frequency (~250–300 Hz).
- **Sample-Rate Matching:** A convolution IR is tied to a specific sample rate. Either:
  1. Export REW IRs at the target upsampled rates (352.8 kHz / 384 kHz, or native rates), **or**
  2. Convolve at native rate vs. upsample the IR cleanly to the target rate (ordering between upsampling and convolution will be analyzed in Research Topic 3).
- **FFT Convolution Without Real-Time Constraints:** Real-time engines use complex partitioned convolution (uniform/non-uniform) to minimize latency. With unconstrained latency, **simple single-block / large-block overlap-add 64-bit FFT convolution** is simpler, mathematically exact, and zero-artifact.
- **Channel Delay:** At 352.8/384 kHz, a single sample is ~2.6 µs (~0.9 mm of acoustic distance), making pure integer-sample delay at the upsampled rate practically sub-millimeter accurate without extra fractional-delay filter coloration!

### 3.4 Windows Bit-Perfect Kernel-Level Transport
- **Three Exclusive Paths on Windows:**
  - **WDM-KS (Windows Kernel Streaming):** Talks directly to WDM audio miniport pins in kernel mode, completely bypassing the Windows audio engine/mixer without needing a vendor ASIO driver. Favored by Audirvana and purists; historically finicky on sample-rate switches if not carefully implemented (PortAudio `paWDMKS` is a key open-source reference).
  - **WASAPI Exclusive:** Microsoft's modern bit-perfect exclusive path (bypasses mixer/APOs, uses native Windows USB Audio Class 2.0 driver). Highest stability.
  - **ASIO:** Direct vendor driver path (only relevant if Schiit supplies a native ASIO driver for Unison USB—note Schiit Unison USB primarily uses native Windows UAC2 drivers!).
- **System Hygiene:** Disable all Windows audio enhancements, mute system sounds on the DAC endpoint, lock playback buffers in RAM (Full Memory playback), and use MMCSS / elevated thread priority for the output feeder thread to eliminate dropouts.

### 3.5 Tidal Lossless Capture & Tandem Companion Workflows
Because the official vanilla Tidal desktop app outputs through Windows audio APIs and lacks a direct DSP plugin hook:
- **Virtual Audio Loopback Bridge:** Route vanilla Tidal output (ideally via Exclusive/bit-transparent virtual driver) into a virtual cable/device (e.g., **VB-Audio Hi-Fi Cable**, **VB-Audio Matrix**, **Virtual Audio Cable (VAC)**, or a custom virtual WASAPI/WDM audio endpoint) → capture bit-perfect PCM in our engine → process → output via KS to Gungnir 2.
- **Key Pitfall to Solve in Research:** Ensuring dynamic sample-rate switching (e.g., 44.1 kHz → 96 kHz → 192 kHz tracks on Tidal) passes through the virtual bridge **without** Windows Shared Mode resampling.
- **Local Companion Option:** `foobar2000` (or CLI folder/playlist) can act as a library manager/controller, piping tracks or playlists into our headless engine.

---

## 4. Curated Master List of Authoritative Sources

### 4.1 Reference Players & Open-Source DSP Engines
- **HQPlayer (Signalyst):** https://signalyst.com/
- **PGGB / PGGB-Live (Remastero):** https://remastero.com/
- **Audirvana:** https://audirvana.com/
- **CamillaDSP (Rust headless DSP engine — GitHub):** https://github.com/HEnquist/camilladsp
- **foobar2000:** https://www.foobar2000.org/
- **Album Player (Igor Antonov):** https://www.albumplayer.ru/english.html (and `screamalsa`: https://github.com/igor63r/screamalsa)
- **JRiver Media Center:** https://www.jriver.com/
- **JPLAY:** https://jplay.app/
- **Diretta Audio Protocol:** https://www.diretta.link/
- **Hang Loose Convolver / Accurate Sound (Mitch Barnett):** https://accuratesound.ca/

### 4.2 Resampling Libraries, Benchmarks & DSP Foundations
- **Infinite Wave SRC Comparisons (gold-standard resampler measurement database):** https://src.infinitewave.ca/
- **Julius O. Smith III — Digital Audio Resampling Home Page (Stanford CCRMA):** https://ccrma.stanford.edu/~jos/resample/
- **Julius O. Smith III — Online DSP Textbooks (Fourier, Filters, Physical Audio):** https://ccrma.stanford.edu/~jos/
- **libsoxr (SoX Resampler Library):** https://sourceforge.net/projects/soxr/
- **r8brain-free-src (Voxengo open-source C++ SRC):** https://github.com/avaneev/r8brain-free-src
- **Rubato (Rust high-quality audio resampler used by CamillaDSP):** https://github.com/HEnquist/rubato
- **FFTW (Fastest Fourier Transform in the West):** https://www.fftw.org/
- **RustFFT / RealFFT (pure Rust 64-bit FFT):** https://github.com/ejmahler/RustFFT
- **The Scientist and Engineer's Guide to Digital Signal Processing (Steven W. Smith — free online textbook):** https://www.dspguide.com/

### 4.3 Inter-Sample Overs, Dither & Reconstruction Theory
- **Benchmark Media — *Intersample Overs in CD Recordings* (John Siau):** https://benchmarkmedia.com/blogs/application-notes/intersample-overs-in-cd-recordings
- **Benchmark Media — *Interpolator Overload Distortion*:** https://benchmarkmedia.com/blogs/application-notes/interpolator-overload-distortion
- **Benchmark Media — *Why Audio Goes to 11*:** https://benchmarkmedia.com/blogs/application-notes/why-audio-goes-to-11
- **Nielsen & Lund (TC Electronic, AES Paper) — *0 dBFS+ Levels in Digital Mastering*:** AES Convention Paper (foundational ISO paper cited by Benchmark & Archimago)
- **Lipshitz, Wannamaker & Vanderkooy (JAES) — *Quantization and Dither: A Theoretical Survey*:** Definitive mathematical reference on TPDF dither and psychoacoustic noise shaping
- **Rob Watts / Chord WTA Filter Technical Talks:** Search Head-Fi Chord threads & AES/CanJam presentations by Rob Watts on tap-length & transient timing theory

### 4.4 Room Correction & Convolution Tools / Guides
- **Room EQ Wizard (REW) Official Site & Docs:** https://www.roomeqwizard.com/
- **rePhase (Linear-phase FIR / phase correction generator):** https://rephase.org/
- **Acourate (Audiovero):** https://www.audiovero.de/en/acourate.php
- **Book — *Accurate Sound Reproduction Using DSP* (Mitch Barnett):** Practical bible on REW/Acourate/FIR convolution workflows
- **AV NIRVANA (Official REW Support Forum):** https://www.avnirvana.com/

### 4.5 Windows Audio Stack, Kernel Streaming & Virtual Routing
- **Microsoft Learn — Windows WDM Audio Driver Architecture (Kernel Streaming / WaveRT):** https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/audio-drivers
- **Microsoft Learn — WASAPI (Core Audio APIs):** https://learn.microsoft.com/en-us/windows/win32/coreaudio/wasapi
- **PortAudio (C library with `paWDMKS` Kernel Streaming & WASAPI Exclusive reference implementation):** http://www.portaudio.com/ and https://github.com/PortAudio/portaudio
- **FlexASIO (Universal ASIO-to-PortAudio/WDM-KS/WASAPI bridge):** https://github.com/dechamps/FlexASIO
- **VB-Audio Virtual Cable / Hi-Fi Cable & ASIO Bridge / Matrix:** https://vb-audio.com/Cable/
- **Virtual Audio Cable (Eugene Muzychenko):** https://vac.muzychenko.net/
- **wasapi-rs (Rust WASAPI bindings used by CamillaDSP):** https://github.com/HEnquist/wasapi-rs

### 4.6 Target Hardware (Schiit Gungnir 2)
- **Schiit Audio Official Site (Gungnir 2 product page & manual):** https://www.schiit.com/
- **Schiit Europe — Gungnir 2 Technical Specs:** https://www.schiit-europe.com/

### 4.7 High-Signal Audiophile Communities & Objective Measurement Blogs
- **Audiophile Style (formerly Computer Audiophile):** https://audiophilestyle.com/ — *Best community for HQPlayer (Jussi Laako posts directly), PGGB, Diretta, Convolution (Mitch Barnett), and Windows purist playback.*
- **Audio Science Review (ASR):** https://www.audiosciencereview.com/ — *Best source for objective DAC/filter/ISO measurements, CamillaDSP guides, and Windows audio stack testing.*
- **Archimago's Musings:** https://archimago.blogspot.com/ — *Independent objective measurements of HQPlayer filters, PGGB, DAC reconstruction filters, ISOs, and dither.*
- **Hydrogenaudio Forums & Wiki:** https://hydrogenaud.io/ — *Gold standard for blind ABX testing, FLAC, foobar2000 internals, resampler math, and perceptual transparency.*
- **Super Best Audio Friends (SBAF):** https://www.superbestaudiofriends.org/ — *Deep community expertise and measurements specifically on Schiit Multibit DACs (Yggdrasil, Gungnir, Bifrost, Unison USB).*
- **Head-Fi (Computer Audio & Schiit threads):** https://www.head-fi.org/ — *Active Schiit Gungnir 2 user impressions, Rob Watts WTA discussions, and PGGB/HQPlayer user setups.*
- **The Well-Tempered Computer:** https://www.thewelltemperedcomputer.com/ — *Clear introductory/intermediate reference on bit-perfect Windows playback, WASAPI/ASIO/KS, and convolution.*
