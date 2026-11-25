# Research 02 — Upsampling & Noise Shaping: Algorithm Sources & Audiophile Consensus

**Status:** In Progress (Initial Web & Forum Synthesis Complete; awaiting optional user snippets/PDFs for inaccessible sources in §4)  
**Scope:** Survey of upsampling/reconstruction filter families, dithering & noise-shaping algorithms, open-source/academic algorithm sources, and what audiophiles report on reviews and message boards—specifically focused on **PCM upsampling to NOS / Multibit / R2R DACs** (Schiit Gungnir 2 at 352.8 / 384 kHz) within our **4,000,000-tap cap** and **floating-point-only** pipeline.

---

## 1. Audiophile Consensus on PCM Upsampling Filters

Across **Audiophile Style**, **Roon Community**, **Head-Fi**, **Super Best Audio Friends (SBAF)**, **Audio Science Review (ASR)**, and **Archimago's Musings**, a remarkably small set of PCM filter families dominates praise for musicality and transparency.

### 1.1 The Most-Praised Filter Families

| Filter Archetype | Named References | Mathematical Essence | What Audiophiles Say (Reviews & Message Boards) | Relevance to 4M-Tap Cap & Gungnir 2 NOS |
| --- | --- | --- | --- | --- |
| **Gaussian-Windowed Polyphase Sinc (Apodizing / Optimal Time-Freq)** | HQPlayer `poly-sinc-gauss`, `-long`, `-xla`, `-hires-lp/mp/ip` | Sinc kernel windowed by a **Gaussian** (achieves the Heisenberg–Gabor lower bound for joint time-frequency localization). Available in linear (`-lp`), minimum (`-mp`), and intermediate (`-ip`) phase. | Widely voted the **#1 all-rounder**. Described as natural, organic, lifelike timbre, expansive 3D stage, non-fatiguing, zero harsh transient smear. Ideal for acoustic, jazz, vocals, classical; pairs naturally with R2R/multibit organic midrange. `-hires` variants preferred for 88.2/96+ kHz sources. | **Must-have core family.** Fits comfortably well under 4M taps; ideal default candidate for both 1x and HiRes inputs. |
| **Ultra-Long / Mega-Tap Apodizing Gaussian Sinc** | HQPlayer `sinc-MGa`, `sinc-MG`, **PGGB** (Gaussian/Kaiser mega-tap) | Massive constant-time windowed sinc (**HQPlayer 5.7.3 manual verified:** `sinc-MGa` / `sinc-MG` / `sinc-Mx` are `65,536 × ratio` taps = **524k taps at 8x `352.8/384 kHz`** and **1.05M taps at 16x**; PGGB runs multi-million-tap high-precision sinc). | Praised for **holographic layering, blacker backgrounds, effortless realism, and zero fatigue**—especially on orchestral/classical/complex acoustic jazz. Apodizing cleans up mastering-stage pre-ringing. PGGB users highlight grounded physical instrument placement and "OLED-like" quiet backgrounds. | **Flagship showcase filter!** Fits easily inside our **4M-tap ceiling** (we can even push constant-time Gaussian apodizing higher than HQPlayer's 524k taps at 8x if beneficial!). |
| **Ultra-Long Steep Brickwall Sinc** | HQPlayer `sinc-M` (1M taps), `sinc-Mx` (`65536×ratio`), `sinc-L` (`131070×ratio` = ~1.05M at 8x, ~2.1M at 16x); Chord WTA (1M-tap FPGA) | Very long linear-phase steep-cutoff windowed sinc with extreme stopband attenuation (>150–200 dB). | Celebrated by speaker listeners for **bass authority, punch, macro-dynamics, grand scale, and pinpoint spatial focus**. Some headphone listeners find pure steep sinc slightly more clinical or fatiguing than Gaussian variants due to longer ringing tails. | **Must-have flagship contrast** to Gaussian (steep brickwall transient/bass authority option within 1M–4M taps). |
| **Extended Sharp Linear/Apodizing Polyphase** | HQPlayer `poly-sinc-ext2`, `poly-sinc-ext3` | Sharp cutoff, high stopband attenuation, extended passband right up to Nyquist; `ext3` is significantly longer/smoother than `ext2`. | Praised as **engaging, fluid, clean, and natural** with slightly lighter tonal body than Gaussian; community favorite for **electronic, modern pop/rock, and soundtracks**. `ext3` removes the slight digital edge some hear on `ext2`. | **Strong candidate** (sharp-cutoff, non-Gaussian polyphase option). |
| **Minimum-Phase / Intermediate-Phase Short-Ringing Apodizing** | Meridian/Craven Apodizing, HQPlayer `poly-sinc-gauss-hires-mp/ip`, `poly-sinc-short-mp` | Zero pre-ringing (minimum phase) or asymmetric reduced pre-ringing (intermediate phase), trading linear phase for pre-echo elimination. | Preferred by listeners sensitive to linear-phase **pre-ringing** on sharp percussive transients and by HiRes (24/96) listeners who want gentler filtering where alias margins are already wide. | **Must-have phase option** (especially for 24/96 HiRes FLAC and pre-ringing-sensitive listeners). |
| **Extreme Steep Cutoff (`xtr`)** | HQPlayer `poly-sinc-xtr-lp/mp` | Ultra-steep transition band with extreme stopband attenuation. | Described as **intimate, forward, hyper-detailed, razor-sharp transients**—loved on some setups, fatiguing/glary on others. | Optional / lower priority (largely overlaps with steep `sinc-M/L` and `ext3`). |
| **Closed-Form / Polynomial / Spline** | HQPlayer `closed-form` / `closed-form-M`, Schiit **Megacomboburrito** (time-frequency optimized closed-form) | Analytical closed-form interpolation preserving original sample points rather than standard windowed FIR truncation. | **Polarizing on forums:** Schiit's Megacomboburrito is beloved on SBAF for transient snap and micro-dynamics (Gungnir 2 already has this built into its internal OS mode!). HQPlayer's `closed-form` gets mixed reviews (some love its clarity; others report drier/tighter bass). | **Low priority for external software** since Gungnir 2 already has Schiit's hardware closed-form filter in OS mode—our software adds maximum value via Gaussian, mega-tap sinc, and apodizing filters. |

### 1.2 Key Nuance: 1x Base Rates (44.1/48 kHz) vs. HiRes Sources (88.2/96/192 kHz)
Audiophile consensus (and Jussi Laako's own guidance) strongly favors **separate filter selection (or profiles) for 1x vs. Nx HiRes material**:
- **1x (44.1 / 48 kHz):** Transition band is very close to 20 kHz, and older masters frequently contain ADC brickwall pre-ringing → favors **apodizing** and steeper/longer filters (e.g., `sinc-MGa` / 4M-tap Gaussian apodizing, `poly-sinc-gauss-xla`, `sinc-M`, `poly-sinc-ext3`).
- **Nx HiRes (88.2 / 96 / 192 kHz, like the user's 24/96 FLACs):** Nyquist is already ≥ 44.1 kHz (far above human hearing), so gentler, shorter-impulse, low-ringing filters (e.g., Gaussian `-hires-lp` / `-hires-ip` / `-hires-mp`) avoid unnecessary time-domain ringing while keeping imaging ultrasonic and benign.

### 1.3 Preliminary Curated Filter Shortlist (To Be Finalized After Snippets & INV-03)
Instead of HQPlayer's 40+ filters, just **5–6 orthogonal, mathematically pure filters** cover nearly 100% of audiophile consensus:
1. **`gauss-apod-4m`** *(~4M taps, linear-phase apodizing Gaussian-windowed sinc — `sinc-MGa` / PGGB class)* — flagship holographic/classical/acoustic filter.
2. **`sinc-steep-4m`** *(1M–4M taps, steep linear-phase Kaiser/Nuttall-windowed sinc — `sinc-M`/`sinc-L`/WTA class)* — flagship bass authority & brickwall precision.
3. **`gauss-long-lp`** *(moderate/long linear-phase Gaussian polyphase — `poly-sinc-gauss-long/xla` class)* — balanced, organic all-rounder.
4. **`gauss-hires-ip` / `gauss-hires-mp`** *(gentle intermediate-phase & minimum-phase Gaussian — `poly-sinc-gauss-hires-ip/mp` class)* — zero/minimal pre-ringing for 24/96+ HiRes and transient purists.
5. **`ext-sharp-lp`** *(extended-passband sharp linear-phase polyphase — `poly-sinc-ext3` class)* — clean, fluid, punchy modern/electronic presentation.

---

## 2. Audiophile Consensus on Dither & Noise Shaping (PCM → Multibit / NOS DACs)

Because our pipeline runs **100% in floating point** and quantizes **strictly once** to integer PCM for the Schiit Gungnir 2, the final quantization stage is critical.

### 2.1 Base Dither Types
- **TPDF (Triangular Probability Density Function):** Industry gold standard (Lipshitz/Vanderkooy proof). Eliminates all signal-correlated harmonic/modulation distortion with flat white noise floor. Universally considered organic, natural, and safe at any sample rate.
- **Gauss1 (Gaussian PDF Dither):** Normal-distribution dither; theoretically lower signal correlation at slightly higher RMS noise variance than TPDF. Favored by some purists when high bit depth / high sample rate makes the tiny noise floor difference irrelevant.

### 2.2 Noise Shapers & Sample-Rate Pairing
Noise shapers feed back quantization error through an FIR/IIR Noise Transfer Function (NTF) to push quantization noise out of the audible band (20 Hz–20 kHz) into ultrasonic frequencies:

| Shaper Family | Order / Nature | Recommended Output Rate | What Audiophiles Say |
| --- | --- | --- | --- |
| **Flat Dither Only** (`TPDF` / `Gauss1`) | 0th-order (no shaping) | Any rate (safest at 44.1/48 kHz, and safe baseline at 352.8/384 kHz) | Zero ultrasonic energy bump; completely benign to any NOS DAC or amplifier. At 24-bit/32-bit, flat TPDF noise is already around `-141 dBFS` (below thermal noise of any real DAC). |
| **Classic Psychoacoustic (1x-rate origin)** (`Lipshitz`, `Shibata`, `modified-e-weighted`, `improved-e-weighted` in SoX/SSRC) | 5th–9th order FIR error feedback | Designed primarily for 44.1/48 kHz (16-bit mastering); coefficients must be redesigned for 352.8/384 kHz! | Proven effective for 16-bit CD mastering (`improved-e-weighted` / `Shibata` praised for low treble fatigue), **not** directly reusable at 384 kHz without recalculating NTF for 384 kHz Nyquist. |
| **Medium-Order High-Rate Shapers** (`NS4`, `NS5`, `NS9`) | 4th–9th order high-rate NTF *(verified from HQPlayer 5.7.3 manual p.18)* | **`NS4`**: ≥ 88.2 kHz (similar to `shaped`); **`NS5`**: **8x & 16x rates (`352.8/384/705.6/768 kHz`)**, *especially recommended for multibit DACs like PCM1704 at highest rates*; **`NS9`**: designed specifically for **4x rates (`176.4/192 kHz`)** (e.g., 16-bit TDA154x multibit DACs). | **Primary-source correction:** For `352.8/384 kHz` output to a multibit DAC, **`NS5`** (not `NS9`, which is tuned for 4x `176.4/192 kHz`) and **`LNS15`** are Signalyst's official recommendations alongside proper `DAC Bits` setting! |
| **High-Order Ultra-Rate Shapers** (`LNS15`, PGGB Adaptive *Optimal/Extended*) | 15th-order smooth linear NTF / high-order adaptive *(verified from PGGB V9 Guide)* | Designed for 16x (`705.6/768 kHz`), **also usable at 8x (`352.8/384 kHz`)**; PGGB explicitly recommends noise shaping (not dither-only) for `≥ 8fS` (`352.8/384 kHz`). | `LNS15` & PGGB `NS: Optimal` (keeps noise low to ≥ 40 kHz) and `NS: Extended` (flatter/wider low-noise band, described as more open/airy on older/noisy masters) deepen audible dynamic range—**but push substantial ultrasonic energy** above 50–100 kHz. Must be tested carefully on NOS DACs/amps for IMD or "glassiness." |

### 2.3 Crucial Discovery for Multibit / R2R DACs: The "DAC Bits" Linearity Technique
Highlighted repeatedly by Jussi Laako (`Miska`) on Audiophile Style and confirmed by R2R/multibit owners (Holo, Denafrips, Schiit):
- Physical multibit / R2R ladders have finite resistor/current-source linearity (true electrical linearity/ENOB is typically **16–20 bits**, even when accepting a 24-bit or 32-bit USB stream).
- **The Technique:** Quantize + noise-shape to a **configurable target bit depth** matching the DAC's true linear sweet spot (e.g., 16, 18, 20, or 24 bits, zero-padded into the 24/32-bit USB container) at **352.8 / 384 kHz** using `NS9` or `LNS15`.
- **Why audiophiles praise it:** Keeps the audio signal strictly inside the DAC ladder's most linear upper bits while shifting quantization error into the ultrasonic band—reportedly yielding cleaner micro-dynamics, blacker backgrounds, and wider soundstage.
- **Safety caveat for Gungnir 2 NOS:** Because Gungnir 2's TI `DAC8812CRUZ` is a 16-bit multiplying DAC (combined in balanced Multiform topology—exact effective bit depth to be verified in `INV-01`), **supporting selectable target quantization bit-depth (e.g., 16 / 18 / 20 / 24-bit) + selectable dither/shaper (`TPDF`, `Gauss1`, medium-order `NS`-class, high-order `LNS`-class)** could be a massive sonic differentiator on Gungnir 2!

---

## 3. Accessible Algorithm & Implementation Sources (Open-Source & Public)

All public/open-source resources verified so far (with license notes for our public `GPL-3.0-or-later` Rust codebase):

### 3.1 Upsampling Algorithms & Benchmarks
- **Julius O. Smith III — *Digital Audio Resampling Home Page* (Stanford CCRMA):** https://ccrma.stanford.edu/~jos/resample/ *(Open academic reference: bandlimited sinc interpolation, Kaiser windowing, polyphase structures).*
- **Infinite Wave SRC Comparisons:** https://src.infinitewave.ca/ *(Benchmark methodology: swept sine aliasing, impulse/ringing, passband ripple, phase plots).*
- **`libsoxr` (SoX Resampler — LGPL-2.1):** https://sourceforge.net/projects/soxr/ *(Reference high-precision polyphase & FFT resampler, linear/intermediate/minimum phase).*
- **`r8brain-free-src` (Voxengo — MIT):** https://github.com/avaneev/r8brain-free-src *(Clean C++ steep linear-phase & minimum-phase polyphase/FFT resampler).*
- **`rubato` (Rust — MIT):** https://github.com/HEnquist/rubato *(Pure Rust sinc & FFT resampler used in CamillaDSP).*
- **SSRC — Shibatch Sample Rate Converter (LGPL):** https://github.com/shibatch/SSRC *(Classic high-precision FFT resampler + Shibata noise-shaping dither).*
- **Archimago's Musings — PGGB Mega-Tap Analysis:** http://archimago.blogspot.com/2021/07/mega-taps-upsampling-remasteros-pan.html *(Objective sweep/impulse/noise measurements of PGGB).*
- **Sound On Sound — Apodizing / Craven / Meridian overview:** https://www.soundonsound.com/ *(Public technical summary of Peter Craven's apodizing work).*
- **Northern Audio / Grigg Audio HQPlayer Filter Guides:** Public summaries explaining `poly-sinc-gauss`, `sinc-M`, `sinc-L`, `sinc-MGa`, `poly-sinc-ext2/3`, `closed-form`.

### 3.2 Dither & Noise-Shaping Algorithms & Code
- **SoX (`chirlu/sox` — GPL/LGPL modules, reference C code in `src/dither.c`):** https://github.com/chirlu/sox *(Contains explicit error-feedback NTF coefficient arrays for `lipshitz`, `f_weighted`, `modified_e_weighted`, `improved_e_weighted`, `shibata`, `low_shibata`, `high_shibata`—note: for our `GPL-3.0-or-later` repo we still derive our own high-rate 352.8/384 kHz NTF coefficients mathematically and preserve attribution where any reference ideas are used).*
- **FFmpeg `libswresample` (`dither.c` / `dither_template.c` — LGPL-2.1+):** https://github.com/FFmpeg/FFmpeg *(Reference TPDF, triangular high-pass, Lipshitz, F-weighted, modified/improved-e-weighted, Shibata dither implementations).*
- **CamillaDSP (`dither.rs` — GPL-3.0, compatible with our `GPL-3.0-or-later` license with proper attribution):** https://github.com/HEnquist/camilladsp *(Rust reference for Flat/Highpass/F-weighted/Shibata/Lipshitz dither).*
- **Wikipedia / DSP textbooks on Error-Feedback Noise Shaping & Delta-Sigma:** Public derivations of `H(z)` / NTF stability criteria (Lee's rule / out-of-band gain limits).

---

## 4. Sources I Cannot Directly Access (Requested User Snippets / Browser PDFs)

Per our **Browser PDF Handoff Protocol**, the following high-value sources are paywalled, download-gated, image/graph-heavy, or buried inside huge forum threads that web search cannot read directly. Providing **targeted snippets** (or dropping PDFs into gitignored `docs/research/sources/`) for any of these will dramatically sharpen `INV-01`, `INV-02`, `INV-03`, and `INV-04`.

### Priority 1 — High-Impact Technical Manuals & Forum Posts (Most Useful Right Now)
1. ~~**HQPlayer 5 Desktop User Manual PDF**~~ — **RECEIVED & EXTRACTED** (v5.7.3, 60 pages, read locally from user's Downloads). Key primary-source insights integrated above:
   - Exact tap counts (`sinc-MGa`/`sinc-MG`/`sinc-Mx` = `65,536 × ratio`; `sinc-L` = `131,070 × ratio`).
   - Exact PCM noise-shaper rate targets (`NS5` for 8x/16x `352.8/384+ kHz` multibit; `NS9` for 4x `176.4/192 kHz` 16-bit multibit; `LNS15` for 16x and usable at 8x `352.8/384 kHz`; `NS1` explicitly warned against with ultrasonic-sensitive gear).
   - **Apodization auto-detection** (p.8 & p.25): detecting ADC/mastering filter errors per track—*"no harm using apodizing filter when not needed, harm using non-apodizing when needed."* Strong candidate for our on-the-fly switching!
   - **20 kHz filter** (p.8): optional ultrasonic cleanup for 2x+ fake-HiRes sources.
   - **Convolution guidance** (p.39–40): overlap-add recommended over overlap-save; *"For most optimal case for all kinds of source material, use extended frequency response convolution filters with 352.8 kHz sampling rate."*
2. ~~**Remastero PGGB User Guide (`PGGB - Zaphod's Guide` V9)**~~ — **RECEIVED & EXTRACTED** (read locally from user's `~/Downloads/pggb.txt`). Key primary-source insights:
   - **V9 Precision Optimized Engine (validates our 4M-tap cap):** PGGB V9 replaced its whole-track legacy Nyquist-Shannon engine as the default with a finite-memory engine whose passband reconstruction accuracy matches the chosen floating-point precision without growing RAM with track length.
   - **Internal Precision:** Supports 64-bit (`f64`), **107-bit (double-double, ~2.2× faster than 128-bit)**, 128-bit, and 256-bit internal math; separates internal precision from output DAC bit depth (12–32 bits, noting some R2R DACs prefer specific bit depths at very high rates).
   - **Filter Profiles:** Three precision-matched filter choices in V9 (`Optimal` default, `Alternate` slightly wider passband, `Minimal` least HF filtering), plus rate-aware HF source-noise filtering (`2fS` 88.2/96 kHz vs `≥ 4fS` 176.4/192/352.8 kHz which often contain ADC ultrasonic noise above 40 kHz).
   - **PCM Noise Shaping:** `NS: Optimal` (minimizes noise up to ≥ 40 kHz), `NS: Alternate`, `NS: Extended` (wider low-noise band, described as more open/airy on older/noisy masters), and `Dither only` (Gaussian dither — *not recommended by PGGB at `≥ 8fS` `352.8/384 kHz`*).
   - **Convolution EQ Best Practices:** Supports both **rate-matched untouched WAV sets** (preserving exact phase/timing) and **deriving target-rate filters from a single WAV** (linear-phase or minimum-phase) rather than naively resampling the IR file.
   - **Headroom / Gain:** Offers `Auto` (0 dB unless an ISP requires attenuation), `Fixed` (`-3 dB`), and `ReplayGain: Album Peak`—strongly echoing our ReplayGain/True Peak `-3 dB` to `0 dB` (never boosting) requirement.
3. **Audiophile Style — Jussi Laako (`Miska`) Posts on PCM Filters & Noise Shaping for NOS R2R/Multibit DACs** (`https://audiophilestyle.com/` → *HQPlayer* thread & *DAC Bits / LNS15 / NS9* discussions):
   - *What we need:* Snippets where Jussi discusses:
     - Best PCM filters for **NOS multibit DACs at 352.8/384 kHz** (`sinc-MGa` vs `poly-sinc-gauss-*` vs `sinc-Mx` vs `poly-sinc-ext2/3`).
     - Exact guidance on **`DAC Bits` + `NS9` vs `LNS15` vs `TPDF`/`Gauss1`** at 352.8/384 kHz (especially ultrasonic noise warnings for NOS multibit).
4. **Super Best Audio Friends (SBAF) & Head-Fi — Schiit Gungnir 2 Threads** (`https://www.superbestaudiofriends.org/` and `https://www.head-fi.org/`):
   - *What we need:* Any snippets/measurements of **Gungnir 2's true linearity/ENOB** (remembering it uses four 16-bit TI `DAC8812CRUZ` chips in Multiform topology), **NOS mode vs. internal Megacomboburrito OS filter**, and user impressions feeding Gungnir 2 NOS with HQPlayer or PGGB at 352.8/384 kHz.

### Priority 2 — Objective Measurement Graphs & Community Polls
5. **Archimago's Musings — PGGB & HQPlayer Filter Measurement Posts**:
   - `http://archimago.blogspot.com/2021/07/mega-taps-upsampling-remasteros-pan.html`
   - Any Archimago posts measuring HQPlayer PCM filters (`sinc-M`, `poly-sinc-gauss`, `LNS15`, `NS9`).
   - *What we need:* Key measurement takeaways or screenshots/PDFs of impulse responses, stopband attenuation, and noise-shaper ultrasonic noise plots.
6. **Roon Community — "Which HQPlayer Filter Are You Using?" / Favorite PCM Filters Threads** (`https://community.roonlabs.com/`):
   - *What we need:* Snippets of recent community polls or consensus summaries on favorite PCM filters (especially distinguishing 1x 44.1/48 kHz vs Nx 96+ kHz preferences).

### Priority 3 — Paywalled Foundational AES Papers (For Exact Mathematical Derivation in `INV-03` & `INV-04`)
*(Only needed if you have access or find public author-hosted preprints; we can also derive equivalent math from open DSP textbooks if unavailable.)*
7. **Robert A. Wannamaker (1992)** — *"Psychoacoustically Optimal Noise Shaping"*, *JAES*, Vol. 40, No. 7/8, pp. 611–620 (AES E-Library).
8. **Stanley P. Lipshitz, Robert A. Wannamaker & John Vanderkooy (1992)** — *"Quantization and Dither: A Theoretical Survey"*, *JAES*, Vol. 40, No. 5, pp. 355–375 (AES E-Library).
9. **Michael A. Gerzon & Peter G. Craven (1989)** — *"Optimal Noise Shaping and Dither of Digital Signals"*, *AES Preprint 2822*.
10. **Peter G. Craven (2004)** — *"Antialias Filters and System Transient Response at High Sample Rates"*, *JAES*, Vol. 52, No. 3, pp. 216–227 (AES E-Library — exact apodizing filter math).
11. **S. H. Nielsen & T. Lund (2000/2003)** — *"0 dBFS+ Levels in Digital Mastering"*, *AES Preprint 5116 / 5874* (exact worst-case inter-sample over math).
