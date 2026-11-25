# Research 03 — Filter, Noise-Shaping & Community Experience Knowledge Base

**Status:** Active Reference Knowledge Base (Synthesized from Primary Manuals, Community Listening Reports, Technical Guides, and DSP Literature)  
**Target Hardware & Pipeline:** **Schiit Gungnir 2** (Multiform balanced multibit, NOS mode, Unison 384 USB) at **`352.8 / 384 kHz` PCM (8fS)**, pure `f64` DSP, **`<= 4,000,000`-tap ceiling**, single output quantization stage, zero user volume control.  
**Companion Documents:**
- [00-landscape-and-sources.md](00-landscape-and-sources.md) — Player landscape and master source directory
- [02-audiophile-filter-survey.md](02-audiophile-filter-survey.md) — High-level filter & shaper algorithm survey
- [05-rew-convolution-and-delay.md](05-rew-convolution-and-delay.md) — REW stereo convolution and channel delay design
- [OPEN_QUESTIONS.md](OPEN_QUESTIONS.md) — Tracked technical decisions & open questions

---

## 1. Evidence Provenance & Confidence Grading

To prevent folklore from overriding engineering reality, every finding in this knowledge base is graded by source type. Per our repository copyright rule (`docs/research/sources/README.md`), only original engineering synthesis and structured citations are recorded here — no copyrighted manuals, articles, or forum threads are reproduced verbatim.

| Source Tag | Source | Type & Scope | Confidence / Weight |
| --- | --- | --- | --- |
| **[HQP-M5]** | Signalyst *HQPlayer Desktop User Manual* (v5.7.3, 60 pp.) | Primary engineering manual: complete filter table (§4.5), PCM noise shapers & DAC Bits (§4.3), Apodization counter & 20 kHz filter (§2.6, §2.8), `-3 dBFS` ISP headroom (§2.15), convolution engine (§6–7). | **Primary / Authoritative** (author's design intent & specs) |
| **[PGGB-V9]** | Remastero *Zaphod's Guide to PGGB* (V9) | Primary engineering manual: Precision Optimized Engine vs. legacy whole-track Nyquist-Shannon engine, `Optimal`/`Alternate`/`Minimal` filters, HF noise filtering (`2fS` vs `>= 4fS`), PCM noise shapers, R2R bit-depth settling limits, EQ & headroom guidance. | **Primary / Authoritative** (offline/live ultra-precision PCM/DSD engine) |
| **[GRIGG]** | Grigg Audio Solutions / Northern Audio — *Audio PC HQPlayer Filter Properties* | Technical synthesis summarizing HQPlayer filter properties, phase/ringing tradeoffs, compressed/loudness-war recommendations, and quoting Jussi Laako's 5-point technical critique of 1x NOS playback. | **High** (technical explainer; verify exact tap specs against `[HQP-M5]`) |
| **[ROON]** | Roon Labs Community — *"Which HQP Filter are you using? [2015–2023]"* | Practitioner discussion (including direct posts by `jussi_laako` and long-term users on PCM vs. DSD, NUC CPU scaling up to 1.5 MHz PCM, 1x vs. Nx filter pairings). | **Medium-High** (real-world listening & operational experience) |
| **[HEADFI]** | Head-Fi — *"HQPlayer Impressions and Settings Rolling Thread"* | Practitioner discussion (Holo May, Chord, Susvara/Abyss/Stealth setups; PCM `sinc-long` / `sinc-L` / `sinc-MGa` / `gauss-xla` impressions, FP64 compute vs. VRAM/RAM footprints, speaker vs. headphone sensitivities). | **Medium-High** (strong empirical hardware load & listening notes) |
| **[AS-CPU]** | Audiophile Style — *"Best CPU for HQPlayer"* (incl. `Miska` / Jussi Laako) | Hardware scaling & architecture discussion (multi-core thread splitting, DDR5 vs. DDR4 memory bandwidth, GPU vs. CPU offload limits for `ext3` / `sinc-L` / `gauss-long`, `1.536M / LNS15` PCM benchmark). | **High** (engineering & compute sizing) |
| **[ASR-AI]** | Audio Science Review — *"HQPlayer settings through AI"* | Mixed thread combining R-2R settling/tolerance formulas, Holo Spring 3 / Denafrips Ares II preset matrices, and foobar2000 + HQPlayer tandem workflow analysis. **Important caveat:** Several preset tables in this thread were LLM-synthesized (flagged by ASR moderators) and contain minor inaccuracies (e.g., citing `poly-sinc-ext2-xla` which does not exist in HQPlayer 5.7.3). Treated as secondary consensus only where corroborated by `[HQP-M5]`/`[GRIGG]`/`[HEADFI]`. | **Low-Medium** (corroborated items only) |
| **[DSP-LIT]** | Standard DSP texts & open-source references | Smith *DSP Guide* Ch. 16 (windowed sinc), MathWorks `dsp.FIRRateConverter` & flyingSand (polyphase decomposition), Julius O. Smith III & IEEE real-cepstrum minimum-phase FIR conversion, `r8brain-free-src`, `KFR`, `ReSinc`, `libsoxr`, `rubato`, `CamillaDSP`. | **Normative** (mathematical & implementation ground truth) |
| **[FSR]** | Project companion spec — *Dynamic Filter Selection from FLAC Metadata* | Per-track metadata & content-analysis decision architecture (`Objective` vs. `GenreAware` modes, rules R/P/H/A/L/E/G/M/B, CUESHEET 50/15 µs de-emphasis, optional 30 s pre-scan, Schema-1 YAML track DB). | **Project Design Reference** |

---

## 2. Core Reconstruction Physics & Why Software Oversampling Matters for NOS DACs

### 2.1 Why Unfiltered 1x NOS Playback Fails Technically (Jussi Laako's 5 Artifacts)
A common audiophile myth is that bypassing all digital filtering at 44.1 / 48 kHz ("1x NOS R2R") eliminates ringing. As Jussi Laako explains (`[GRIGG]` citing Audiophile Style):
- Every modern 44.1 / 48 kHz recording was already band-limited by a delta-sigma ADC decimation filter (and/or mastering SRC), so band-limiting ringing is **already baked into the source samples**.
- Playing 44.1 / 48 kHz raw through a NOS R2R ladder without proper oversampling introduces **five severe artifacts**:
  1. **Incomplete analog reconstruction:** Staircase zero-order-hold output heavily distorts the original band-limited waveform.
  2. **Transient amplitude & timing error:** Musical transients rarely land on exact sampling instants; without interpolation, peak height is attenuated or missed entirely.
  3. **Waveform-induced timing jitter:** Quantized staircase steps shift effective zero-crossings and transient edges.
  4. **Ultrasonic imaging & intermodulation (IMD):** Unattenuated spectral images extend into the MHz range and intermodulate back into the audible band inside the DAC analog stage and downstream amplifier.
  5. **Audible treble rolloff:** Zero-order-hold $\operatorname{sinc}(f / f_s)$ droop begins around 1 kHz and reaches roughly `-3.2 dB` by 20 kHz at 44.1 kHz.

**Design Implication for Gungnir 2:** Running Schiit Gungnir 2 in **NOS mode at 8fS (`352.8 / 384 kHz`)** eliminates all five 1x-NOS flaws (the ZOH droop at 20 kHz drops to `< 0.05 dB` and images are pushed above 330 kHz) while letting our pure-`f64`, multi-million-tap software engine act as the *sole* reconstruction filter without double-filtering (`[PGGB-V9]`, `[HQP-M5]`).

### 2.2 The Time–Frequency Uncertainty Tradeoff ($1/x$ Ringing vs. Steepness)
- Ringing duration scales directly with filter impulse length (`N` taps). For example, a 1M-tap linear-phase filter at `705.6 kHz` spans `~0.71 s` of pre-ringing and `~0.71 s` of post-ringing (`[GRIGG]`).
- Time localization and frequency selectivity obey an inverse ($1/x$) Heisenberg–Gabor uncertainty bound: steeper cutoff and deeper stopband attenuation require longer impulse responses.
- **Why Gaussian windows dominate audiophile consensus:** A Gaussian window uniquely minimizes the joint time–frequency uncertainty product, producing the shortest possible ringing envelope for a given transition width and stopband depth (`-300 dB` stopband with zero passband ripple, `[HQP-M5]`, `[GRIGG]`).
- **Why polynomial / "non-ringing" filters are rejected:** `polynomial-1` and `polynomial-2` (`[HQP-M5]` p. 31) eliminate apparent ringing only by rolling off the top octave early and leaking substantial ultrasonic imaging distortion — Jussi explicitly marks both *"Not recommended"*.

### 2.3 Phase Regimes (Linear vs. Intermediate vs. Minimum Phase)
| Phase Regime | Impulse Symmetry | Acoustic / Subjective Tradeoff | Typical Community / Manual Use Cases |
| --- | --- | --- | --- |
| **Linear Phase (`-lp`)** | Symmetric (equal pre- and post-ringing); constant group delay at all frequencies | Preserves exact inter-frequency phase alignment and spatial cues (**soundstage width/depth**); long steep `-lp` filters can cause audible pre-echo on sharp transients for sensitive listeners. | Classical, orchestral, concert-hall / live acoustic recordings, general default (`[HQP-M5]`, `[GRIGG]`). |
| **Minimum Phase (`-mp`)** | Causal (zero pre-ringing; all energy concentrated in post-ringing); frequency-dependent group delay | Eliminates unnatural pre-echo (post-ringing is largely psychoacoustically masked by the transient); emphasizes **transient snap / attack** at the cost of high-frequency phase shift. | Pop, rock, studio multi-track, electronic, percussive tracks, pre-ringing-sensitive listeners (`[HQP-M5]`, `[GRIGG]`). |
| **Intermediate / Asymmetric Phase (`-ip`, `asymFIR`)** | Small pre-echo, longer post-echo; milder phase shift than `-mp` | Middle ground preserving most spatial coherence while reducing pre-ringing ahead of sharp acoustic attacks. | Jazz, blues, acoustic percussion, HiRes material (`[HQP-M5]` pp. 26–30, `[GRIGG]`). |

### 2.4 Apodizing vs. Non-Apodizing & Half-Band Filters
- **What apodizing does:** An apodizing reconstruction filter reaches full stopband attenuation *at or slightly before* the source Nyquist frequency ($f_s/2$), replacing the impulse response of a flawed upstream ADC/mastering brickwall filter and cleaning up near-Nyquist aliasing (`[GRIGG]`, `[HQP-M5]` p. 8).
- **The Asymmetric Risk Rule (`[HQP-M5]` p. 8 & p. 25):**
  - HQPlayer's `Apod` counter counts ADC/mastering-origin errors per track and recommends switching to an apodizing filter whenever the count exceeds **10 per track**.
  - Key engineering rule: **there is no harm in using an apodizing filter on clean content that does not need one, whereas using a non-apodizing filter on flawed content causes audible degradation.**
- **Why Half-Band Filters (`poly-sinc-hb-*`, `poly-sinc-gauss-halfband*`) are specialist-only:** Half-band filters pass original sample points unchanged (`h[kL] = 0` for `k != 0`), so they *cannot* correct upstream ADC/mastering errors and leak slightly around Nyquist (`-6 dB` at $f_s/2$). Both `[HQP-M5]` (pp. 28, 31) and `[GRIGG]` warn that half-band filters are **unsuitable for ~95% of RedBook 44.1 kHz content** and should only be used on pristine HiRes / technically flawless masters.

---

## 3. Comprehensive Filter Taxonomy, Specs & Community Impressions

### 3.1 Gaussian-Windowed Polyphase Family (Consensus #1 All-Rounder)
| Reference Filter | Ratio | Apodizing | Focus & Genre (`[HQP-M5]`/`[GRIGG]`) | Community Impressions & Engineering Notes | `audiophile_player` Mapping |
| --- | --- | --- | --- | --- | --- |
| `poly-sinc-gauss-short` | Integer up | No | Transients; electronic, jazz, blues, pop, rock | Lowest CPU/load, very short ringing, slightly softer/less spatial depth (`[ASR-AI]`, `[ROON]`). | Covered by future `#8 short-*` family |
| `poly-sinc-gauss` | Any | Yes | Transients, timbre; any genre | Widely regarded as balanced, relaxed, natural reference starting point (`[ROON]`, `[ASR-AI]`). | Superseded by `gauss-long-lp` default |
| **`poly-sinc-gauss-long`** | Any | **Yes** | Transients, timbre, space; **any genre** | **Signalyst's official 1x default** (`[HQP-M5]` p. 17 & p. 22 screenshots, `[GRIGG]`, `[ROON]`). Flat to 20 kHz at 44.1 kHz, `-300 dB` at 22.05 kHz. Universally praised as organic, lifelike, zero fatigue. | **Implemented (Phase 1 default for 1x):** `gauss-long-lp` (5,063 taps @ 44.1→352.8 kHz, runs ~9.3x real-time on Ryzen 9 7900) |
| `poly-sinc-gauss-xl` / **`-xla`** | Any | No (`xl`) / **Yes (`xla`)** | Transients, timbre, space; classical, jazz, blues | Extra-long Gaussian; `gauss-xla` is a perennial Head-Fi/Roon favorite for holographic 3D space and acoustic/jazz realism (`[HEADFI]`, `[ASR-AI]`), heavier CPU than `-long` (`[ROON]`). | Mapped to planned `#3 gauss-apod-4m` / `gauss-xla` tier |
| **`poly-sinc-gauss-hires-lp` / `-ip` / `-mp`** | Any | **Yes** | Transients, timbre, space; **any genre (Nx HiRes)** | **Signalyst's official Nx HiRes default** (`-lp`, `[HQP-M5]` p. 17 & p. 22, `[ROON]`, `[GRIGG]`). Also recommended for lossy/MQA cleanup due to gentle roll-off above audible band and minimal ringing. `IgorSki` highlights both `-lp` and `-mp` as personal Nx favorites (`[ROON]`). | **Implemented (Phase 1 default for Nx):** `gauss-hires-lp`; planned `-ip` & `-mp` variants |
| `poly-sinc-gauss-halfband` / `-s` | Any | No | Transients, timbre, space; pristine masters only | Slightly leaky at Nyquist; strictly for technically clean recordings (`[HQP-M5]` p. 31). | Low priority (violates apodizing safety on 1x) |

### 3.2 Constant-Time & Mega-Tap Sinc Families (Flagship Resolution / Bass Authority)
| Reference Filter | Tap Scaling Formula (`[HQP-M5]` pp. 32–33) | Apodizing | Focus & Genre | Community Impressions & Compute Profile | `audiophile_player` Mapping |
| --- | --- | --- | --- | --- | --- |
| **`sinc-MGa`** / `sinc-MG` | **`65,536 × ratio`** (524,288 taps @ 8x 352.8/384k; 1,048,576 @ 16x) | **Yes (`MGa`)** / No (`MG`) | Transients, timbre, space; classical, jazz, blues | Constant-time mega-tap Gaussian (`poly-sinc-gauss-xla` / `-xl` family). Praised for effortless depth, black backgrounds, and zero fatigue (`[HEADFI]`, `[GRIGG]`). Note: fixed bug in older HQP versions where DSD512 ran 4M instead of 32M taps (`[GRIGG]`). | **Planned Flagship #3:** `gauss-apod-4m` (UPOLS FFT up to 4M taps) |
| **`sinc-Mx`** / `sinc-M` / `sinc-S` | `sinc-S`: `4096×r`; `sinc-M`: fixed `1,048,576`; **`sinc-Mx`**: `65,536×r` (constant time!) | **Yes** | Space, timbre; classical, jazz, blues (`sinc-S`: any) | `poly-sinc-ext3` variant. Crucial distinction (`[GRIGG]`): `sinc-M` has fixed taps so its steepness softens as ratio rises, whereas **`sinc-Mx` holds constant time duration** and identical steepness at all ratios! Praised for punch, attack, clarity, and live feel (`[HEADFI]`, `[ASR-AI]`). | **Planned Flagship:** `sinc-steep-4m` (constant-time steep windowed sinc) |
| **`sinc-L`** / `sinc-Ll` / `sinc-Lm` / `sinc-Ls` | `sinc-L`: **`131,070 × ratio`** (~1.05M @ 8x); `Ll`: `65,536×r`; `Lm`: `16,384×r`; `Ls`: `4,096×r` | **No** | Classical (`Lm`: jazz/blues; `Ls`: any) | Extreme steep roll-off with average attenuation. `sinc-Ll` praised for hypnotic solo/small classical strings (`[ROON]`). **`sinc-L` is a huge memory hog** (~11.6 GB GPU / 22 GB host RAM at DSD512, `[HEADFI]` `b0bb`), though much smaller at 8x PCM! | Covered by `sinc-steep-4m` (with apodizing option preferred) |
| **`sinc-long`** / `sinc-medium` / `sinc-short` | Adaptive tap count (medium attenuation; 2-stage 16x for SDM) | **No** | `long`: classical; `medium`: classical/jazz/blues; `short`: any | **Major HQPlayer 5 PCM standout (`[HEADFI]`):** Multiple Holo May/Chord users call `sinc-long` at 768 / 1411 kHz PCM their **new PCM go-to replacing `sinc-L`**, reporting "transient slam of PCM without congestion or harshness," realistic transients, and extreme instrument separation. Very heavy FP64 load at DSD (`~0.5 TFLOPS` at DSD64, `[HEADFI]` `b0bb`), **far lighter and ideal at 8x/16x PCM** (`~0.15 TFLOPS`, runs comfortably on CPU/mid-tier hardware). | Strong candidate for future steep-PCM contrast mode |

### 3.3 Extended-Passband Sharp Polyphase (`poly-sinc-ext*`)
| Reference Filter | Ratio | Apodizing | Focus & Genre | Community Impressions | `audiophile_player` Mapping |
| --- | --- | --- | --- | --- | --- |
| **`poly-sinc-ext2`** | Any | **Yes** | Timbre; **any genre** | Widely cited as a **"day-to-day go-to filter"** (`[ROON]` `IgorSki`, `[ASR-AI]`). Sharp cutoff reaching full attenuation right at Nyquist; clean, energetic, crisp microdetail for rock/metal/electronic. Slightly leaner tonal body than `gauss-long`. | Planned `#5 ext-sharp-lp` |
| **`poly-sinc-ext3`** | Any | **Yes** | Timbre; classical (and modern/electronic) | 8x longer, much steeper version of `ext2` (`[HQP-M5]` p. 29). Removes the slight digital edge some listeners report on `ext2`; popular with users who want extreme clarity + micro-dynamics (`[AS-CPU]` `pis99`). Higher CPU/RAM demand than `ext2` (`[ROON]`). | Planned `#5 ext-sharp-lp` (long/steep tier) |

### 3.4 Classic Polyphase, Short, Extreme (`xtr`), Minimum-Ringing & IIR Filters
| Filter Family | Apodizing | Strengths & Weaknesses (`[HQP-M5]` pp. 26–32, `[GRIGG]`, `[ROON]`) | `audiophile_player` Role |
| --- | --- | --- | --- |
| `poly-sinc-lp` / `-mp` / `-long-lp/ip/mp` | Yes | High quality all-rounders (`-lp` classical/space, `-ip` jazz/blues/electronic, `-mp` transients); note `poly-sinc-lp` has noticeably slow initialization time on low-power CPUs (`[ROON]`). | Superseded by Gaussian & `short-*` families |
| `poly-sinc-shrt-lp` / **`-shrt-mp`** | Yes | Very short pre/post-ringing (`-shrt-mp` has zero pre-ringing and shortest post-echo -> optimal transient snap). Recommended for **pop, rock, electronic, and loudness-war compressed 1x masters** (`[HQP-M5]` p. 27, `[GRIGG]`, `[ROON]` `IgorSki` 1x favorite). | Planned `#8 short-lp` / `short-mp` |
| `poly-sinc-xtr-*` (`-lp`, `-mp`, `-short-lp`, `-short-mp`) | `-short` only | Extreme cutoff & attenuation focusing on timbre; very CPU-heavy unless using `*-2s` two-stage variants (`[ROON]`); can sound forward/fatiguing on bright chains. | Low priority (covered by `ext` & `sinc-steep`) |
| `minringFIR-lp` / `-mp` | No | Special minimum-ringing FIR sitting between polynomial and `poly-sinc-short` (`[HQP-M5]` p. 32). | Optional future transient experiment |
| `IIR` / `IIR2` | Yes | Analog-like steep minimum-phase IIR (`IIR2` removes passband ripple); zero pre-echo, long masked post-echo; pop/rock/jazz/blues (`[HQP-M5]` p. 26). | Not planned for upsampling (IIR reserved for exact 50/15 µs de-emphasis) |
| `closed-form` / `-fast` / `-M` / `-16M` | No | $2^x$ only; Schiit Gungnir 2 already has Megacomboburrito closed-form DSP in hardware OS mode, so external software adds more value with Gaussian/apodizing/mega-tap sinc. | Low priority |
| `poly-sinc-mqa/mp3-lp/mp` | Yes | Early slow rolloff for lossy/MQA ultrasonic noise cleanup; violates our DR-1 `19 kHz` passband floor for general playback (`[FSR]`). | Rejected for general playback; warn on `MQAENCODER` tag |

### 3.5 PGGB V9 Filter & Engine Takeaways (`[PGGB-V9]`)
- **Validation of Finite-Length Precision-Matched Engine (our 4M-tap cap):** In PGGB V9, Remastero replaced its whole-track *Legacy Nyquist-Shannon* engine as the default with the **Precision Optimized Upsampling Engine**, whose memory footprint does **not** grow with track length and whose passband reconstruction accuracy matches the chosen floating-point precision (`64`, `107` double-double ~2.2× faster than 128-bit, `128`, or `256`-bit). Listeners report more natural timbre, clearer separation, and deeper space vs. the legacy engine.
- **Three Precision-Matched Filter Profiles:** `Optimal` (recommended default), `Alternate` (slightly wider passband before roll-off), and `Minimal` (least HF filtering — PGGB explicitly notes *"a wider bandwidth is not necessarily better"* because HF distortion can leak through).
- **Rate-Aware Source HF Noise Filtering:** PGGB explicitly distinguishes:
  - **`2fS` PCM (88.2 / 96 kHz):** May contain music up to ~40 kHz with less ultrasonic noise.
  - **`>= 4fS` PCM (176.4 / 192 / 352.8 kHz DXD):** Often carries heavy ADC ultrasonic noise above 40 kHz alongside ultrasonic musical content (`Aggressive` = smoother, `Minimal` = more open/lively, `Off` = risks harshness/fatigue on noisy HiRes).
  - This directly complements HQPlayer's optional **20 kHz filter** (`[HQP-M5]` p. 8 & p. 48) for cleaning up fake HiRes tracks that have a brickwall cliff around 20–24 kHz plus ultrasonic junk above it.

---

## 4. Content-Driven Filter Selection Playbook

Synthesizing `[HQP-M5]`, `[PGGB-V9]`, `[GRIGG]`, `[ROON]`, `[HEADFI]`, `[ASR-AI]`, and `[FSR]`, reliable selection separates **objective signal facts** from **subjective genre taste**:

### 4.1 High-Confidence Objective Rules (Always Active in Default `Objective` Mode)
1. **Separate 1x vs. Nx HiRes Defaults (`[HQP-M5]`, `[GRIGG]`, `[ROON]`):**
   - **1x Base Rates (`<= 48 kHz`):** **`gauss-long-lp`** (apodizing, flat to 20 kHz @ 44.1k, `-300 dB` at Nyquist) — prevents aliasing/ringing right next to the audible band.
   - **Nx HiRes (`88.2–192 kHz`):** **`gauss-hires-lp`** (or `-ip`/`-mp` for minimal/zero pre-ringing) — gentler impulse where Nyquist is already `>= 44.1 kHz`.
   - **Native 8fS (`352.8 / 384 kHz`):** **`null`** (1:1 zero-copy `f64` pass-through straight into REW convolution & quantization).
2. **Apodizing Trigger (`[HQP-M5]` p. 8 & p. 25, `[FSR]` §7.3):**
   - Whenever an upstream ADC/mastering filter error count exceeds threshold (`> 10` per whole track in HQPlayer; heuristic pre-ringing detector in our optional 30 s pre-scan / full-scan YAML DB), switch to an **apodizing** flagship filter (`gauss-apod-4m`) — even on HiRes files if fake-HiRes or ADC ringing is detected.
3. **Fake HiRes Cleanup (`[HQP-M5]` p. 8 & p. 48, `[FSR]` §7.2):**
   - If spectral analysis shows a `2x/4x` file has a steep brickwall cliff at `19.5–24.5 kHz`, treat its effective Nyquist as `f_eff = 22.05 / 24 kHz` and apply apodizing filtering targeting `f_eff` (preserving our DR-1 `>= 19 kHz` passband floor).
4. **Loudness-War / Heavily Compressed 1x Masters (`[GRIGG]`, `[FSR]` §2 Rule L):**
   - Over-compressed, clipped 1x masters (`REPLAYGAIN_TRACK_GAIN <= -8 dB` & peak `>= 0.98 FS`, or integrated loudness `>= -10 LUFS` with clipped runs) benefit from **shorter, transient-focused filters** (`short-lp` / `short-mp`) or transient-optimized Gaussian (`gauss-long-lp`) rather than mega-tap steep linear-phase filters whose long ringing exacerbates clipped transients.
5. **CD 50/15 µs Pre-Emphasis (`[FSR]` §6):**
   - Detected from FLAC `CUESHEET` track pre-emphasis bit or explicit `AP_DEEMPHASIS=1` tag: insert an exact first-order minimum-phase IIR shelf (`τ₁ = 50 µs`, `τ₂ = 15 µs`, `-10.46 dB` HF asymptote, never boosts) at 8fS after upsampling.

### 4.2 Genre & Listening-Environment Consensus (Opt-In `GenreAware` Mode Only)
Community sources show real patterns **and** real disagreements — which is why genre rules stay opt-in behind explicit user overrides (`CLI/UI > YAML user.filter > AP_FILTER > Objective rules`):

| Music / Recording Style | Consensus Direction | Notable Source Disagreements |
| --- | --- | --- |
| **Classical / Acoustic / Concert Hall** | Linear-phase, high spatial coherence (`gauss-apod-4m`, `gauss-xla`, `sinc-MGa`, `sinc-long` / `sinc-Ll`) (`[HQP-M5]`, `[GRIGG]`, `[ROON]`, `[HEADFI]`) | Pure steep `sinc-L`/`sinc-long` (maximum separation/slam) vs. Gaussian `sinc-MGa`/`gauss-xla` (more organic timbre, less clinical). |
| **Jazz / Blues / Real-Room Transients** | Intermediate/asymmetric phase (`-ip`, `asymFIR`) or long Gaussian (`gauss-long`, `gauss-xla`) (`[HQP-M5]`, `[GRIGG]`, `[ASR-AI]`) | Some listeners prefer `-mp` (`poly-sinc-mp`) for drum/cymbal attack; others prefer linear-phase Gaussian for horn timbre and room air. |
| **Rock / Pop / Studio Multi-Track** | Short / minimum-phase (`poly-sinc-shrt-mp`, `minphaseFIR`) OR sharp extended (`poly-sinc-ext2`, `sinc-Mx`) (`[HQP-M5]`, `[GRIGG]`, `[ASR-AI]`) | Strongest disagreement: `[GRIGG]`/`[HQP-M5]` lean minimum-phase short filters for drum punch; `[ASR-AI]`/`[ROON]` users frequently prefer `ext2` or `sinc-Mx` for clarity and slam. |
| **Electronic / Dense Modern Mixes** | Crisp layering & fast transients (`poly-sinc-ext2`/`ext3`, `poly-sinc-short-mp`/`gauss-short`) (`[HQP-M5]`, `[GRIGG]`, `[ASR-AI]`) | Linear-phase `ext2`/`ext3` for texture/layering vs. minimum-phase `-shrt-mp` for tight bass transients. |
| **Headphones vs. Loudspeakers** | Headphone listeners (`[HEADFI]` `Rayon`, `plumpudding2`) report higher sensitivity to **output sample rate** (soundstage expansion) and pre-ringing fatigue; speaker listeners often prioritize **bass authority, macro-scale, and harmonic density** (`sinc-long`, `sinc-Mx`, `gauss-long`). | System-dependent; no universal setting replaces auditioning on the user's own transducers. |

---

## 5. PCM Dither, Noise Shaping & Multibit / R2R "DAC Bits" Linearity

Because `audiophile_player` runs **100% in `f64`** and quantizes **exactly once** to integer PCM for the DAC, the final shaper and target bit-depth selection directly interact with the Schiit Gungnir 2's multibit conversion stage.

### 5.1 Verified PCM Dither & Noise-Shaper Rate Matrix (`[HQP-M5]` p. 18, `[PGGB-V9]`)
| Algorithm | Order / Type | Optimal Output Rate | Guidance & Warnings |
| --- | --- | --- | --- |
| **`TPDF`** | 0th-order triangular dither | Any rate (general-purpose default, especially 44.1/48 kHz; safe baseline at 8fS) | Zero ultrasonic noise bump; completely benign to NOS DACs, tweeters, and amplifiers. Current active baseline in `ap-dsp`. |
| **`Gauss1`** | 0th-order Gaussian dither | `<= 96 kHz` (or flat-noise purist baseline) | Flat frequency distribution; PGGB explicitly notes *Dither only* is **not recommended at `>= 8fS` (`352.8/384 kHz`)** where noise shaping outperforms flat dither. |
| **`RPDF`** | 0th-order rectangular dither | `>= 24-bit` output only | Lightweight, but `TPDF` eliminates noise-floor modulation cleanly at trivial CPU cost (`[HQP-M5]` p. 18). |
| **`shaped` / `NS4`** | Shaped dither / 4th-order NTF | `>= 88.2 / 96 kHz` | Gentle psychoacoustic shaping suitable from 2x upward. |
| **`NS1`** | 1st-order NTF | `176.4 / 192 kHz` | **Warning (`[HQP-M5]` p. 18):** *Not recommended with equipment sensitive to ultrasonic noise.* |
| **`NS9`** | 9th-order aggressive NTF | **4x (`176.4 / 192 kHz`)** | Specifically tuned for 4x 16-bit multibit DACs (e.g., TDA154x) — **not** optimal for 8x `352.8/384 kHz` where `NS5` or `LNS15` applies! |
| **`NS5`** | 5th-order high-rate NTF | **8x & 16x (`352.8 / 384 / 705.6 / 768 kHz`)** | **Primary multibit recommendation at 8fS (`[HQP-M5]` p. 18):** Fairly aggressive, designed specifically for 8x/16x multibit DACs (e.g., PCM1704). Not for `< 192 kHz`. |
| **`LNS15`** | 15th-order smooth linear NTF | **16x (`705.6 / 768 kHz`), also usable at 8x (`352.8 / 384 kHz`)** | Smooth slope; widely praised on R2R DACs (`Holo`, `Denafrips`) alongside 20-bit `DAC Bits` (`[HQP-M5]` pp. 18–19, `[AS-CPU]`, `[ASR-AI]`). Not for `< 352.8 kHz`. |
| **PGGB `NS: Optimal` / `Extended`** | High-order rate- & bit-adaptive PCM NTF | `>= 8fS` (`352.8 / 384 kHz+`) | `Optimal` keeps low-noise floor to `>= 40 kHz`; `Extended` widens the low-noise band with slightly higher audible floor (described as more open/airy on older/noisy masters, `[PGGB-V9]`). |

### 5.2 The Multibit / R2R "DAC Bits" Linearization Technique (`[HQP-M5]` pp. 18–19, `[PGGB-V9]`, `[ASR-AI]`)
- **Physical motivation:** Discrete R2R and multibit ladders have finite resistor-matching tolerance and switch settling times (`[PGGB-V9]`, `[ASR-AI]`): achieving true 24-bit static linearity requires `< 0.03 ppm` resistor matching, whereas real physical ladders achieve **~16–20 bits of monotonic linearity** (and faster settling at lower active bit counts at very high rates).
- **Signalyst & Remastero practice:** Quantize + noise-shape to the DAC's actual linear bit depth (e.g., **20 bits** for Holo Audio & Denafrips R2R DACs per `[HQP-M5]` p. 18; selectable 12–32 bits in `[PGGB-V9]`), zero-padded into the 24/32-bit USB container, paired with **`NS5` or `LNS15`** at `352.8 / 384 kHz+`.
- **Reported benefit (`[HQP-M5]` p. 19):** Corrects inherent R2R linearity errors, lowering low-level signal distortion and zero-crossing distortion.
- **Schiit Gungnir 2 safety caveat:** Gungnir 2 uses four 16-bit TI `DAC8812CRUZ` multiplying DACs in Schiit's balanced Multiform topology. Furthermore, `[HQP-M5]` p. 19 and `[PGGB-V9]` both warn that excessive ultrasonic noise from aggressive shapers can stress tweeters or cause intermodulation/hiss in Class-D or wideband amps. Therefore `audiophile_player` should keep **`TPDF` (24-bit)** as the unconditionally safe baseline and offer configurable **target quantization bit depth (`16 / 18 / 20 / 24-bit`) + 8fS noise shapers (`NS5`-class & `LNS15`-class)** for direct listening & measurement on Gungnir 2!

---

## 6. Headroom, Convolution EQ & Tandem Player Workflows

### 6.1 Inter-Sample Over (ISP) Headroom
- Both Signalyst (`[HQP-M5]` p. 9: *"maximum recommended volume level is -3 dBFS to avoid inter-sample overloads"*) and Remastero (`[PGGB-V9]`: `Auto` leaves gain at `0 dB` unless an ISP requires attenuation; `Fixed -3 dB`; `ReplayGain: Album Peak`) converge on the exact same physics.
- Our `HEADROOM_DESIGN.md` policy improves on static `-3 dB` attenuation by using **binary Stage-1 upsampler headroom** (`0 dB` when verified source peak `P <= -3 dBFS`, `-3 dB` otherwise) plus **excess-only Stage-2 convolution scaling** (never normalizing REW IR boosts at load) and a zero-clipping over-counter backstop.

### 6.2 Convolution EQ Best Practices (`[HQP-M5]` pp. 39–42, `[PGGB-V9]`)
- **Use native target-rate IRs (352.8 / 384 kHz):** Both HQPlayer (*"use extended frequency response convolution filters with 352.8 kHz sampling rate"*) and PGGB (*"import rate-matched .wav filters without modification when phase/timing must be retained"*) warn against naive IR resampling. Our `DirectoryRewConvolverFactory` enforces exact rate-matched WAV loading with `4,194,304`-tap support.
- **Linear-phase vs. minimum-phase EQ pre-ringing (`[HQP-M5]` p. 42):** HQPlayer warns that converting high-Q / high-dB parametric EQ to linear-phase introduces audible in-band pre-ringing — minimum-phase EQ is safer for steep room/headphone boosts, while linear-phase EQ works best for gentle corrections.
- **Overlap-add vs. overlap-save:** HQPlayer notes overlap-add consumes less CPU than naive overlap-save; our **Uniform Partitioned Overlap-Save (UPOLS)** in `ap-dsp` keeps `B = 65,536` (`fft_len = 131,072`) with sparse zero-partition skipping so a 4M-tap stereo IR + `gauss-long-lp` runs at **~5.9x real-time** on Ryzen 9 7900 in pure `f64` with bounded RAM.

### 6.3 Tandem Player + DSP Engine Architecture (`[ASR-AI]`, `[ROON]`, `[HEADFI]`)
- Across Roon, Head-Fi, and ASR, a consistent practical conclusion emerges: **library/UI players (foobar2000, Roon, JRiver) and extreme DSP engines (HQPlayer, PGGB, `audiophile_player`) solve different jobs**, and pairing a responsive front-end (`foobar2000` via UPnP) with a headless, bit-perfect DSP renderer delivers the best of both worlds without bloating the DSP engine with a GUI.

---

## 7. DSP Implementation References & Algorithmic Building Blocks (`[DSP-LIT]`)

Key technical references collected for implementing the remaining filter and shaper phases in clean-room, `GPL-3.0-or-later` Rust:

1. **Windowed-Sinc & Polyphase FIR Resampling:**
   - **Steven W. Smith — *The Scientist and Engineer's Guide to Digital Signal Processing*, Ch. 16:** Exact truncated windowed-sinc equations and window stopband/transition tradeoffs.
   - **MathWorks `dsp.FIRRateConverter` & flyingSand Audio Resampling Series:** Algebraic decomposition of $H(z)$ into $L$ polyphase branches avoiding zero-multiply work.
   - **Two-stage (`*-2s`) oversampling optimization (`[HQP-M5]` p. 34):** First stage performs steep `>= 8x` conversion; optional second stage converts with a gentler filter since aliases are already far above audio band. (For our `8fS` `352.8/384 kHz` target, single-stage polyphase + UPOLS FFT covers `2x / 4x / 8x` cleanly.)
2. **Linear-Phase to Minimum-Phase Conversion (`-mp` & `-ip` Filters):**
   - **Julius O. Smith III — *Introduction to Digital Filters* (Conversion to Minimum Phase):** Real-cepstrum FFT/IFFT method folding anticausal cepstral energy onto causal lags while preserving exact magnitude response $|H(e^{j\omega})|$.
   - **IEEE — *Minimum-Phase FIR Filter Design Using Real Cepstrum*:** High-order FFT-based minimum-phase synthesis avoiding numerically unstable polynomial root-finding on long kernels (`N > 1,000`).
3. **High-Rate Error-Feedback PCM Noise Shapers (`NS5` / `LNS15` Class):**
   - **Lipshitz, Wannamaker & Vanderkooy (JAES 1992) / Wannamaker (JAES 1992):** Error-feedback NTF stability bounds (Lee's rule / out-of-band gain limits) for designing smooth 5th–15th order NTFs directly at `352.8 / 384 kHz` rather than reusing 44.1 kHz coefficients.
4. **Open-Source Production Codebases to Study (License-Aware):**
   - **`r8brain-free-src` (MIT):** Linear-phase and minimum-phase polyphase/FFT resampling.
   - **`KFR` (GPL-2.0+ / commercial):** SIMD-accelerated FIR/polyphase resampling and windowing.
   - **`ReSinc`:** Compact real-time Kaiser-windowed sinc implementation.
   - **`libsoxr` (LGPL-2.1), `rubato` (MIT), `CamillaDSP` (GPL-3.0), `SSRC` (LGPL):** Reference phase modes, FFT resamplers, and Rust dither/shaper structures.

---

## 8. Actionable Mapping to `audiophile_player`

| Knowledge-Base Insight | Current Repo Status | Planned Next Step |
| --- | --- | --- |
| **1x `gauss-long-lp` + Nx `gauss-hires-lp` + 8fS `null` defaults** | **Implemented** (`ap-dsp::upsampler::PolyphaseFirUpsampler`, `DefaultFilterSelector`) | Keep as permanent rate defaults |
| **Zero-normalization REW IR + binary `{-3, 0} dB` headroom + over-counter** | **Implemented** (`ap-core::headroom`, `ap-dsp::convolution`, `ap-dsp::quantize`) | Complete Issue #7 Steps 2–3 (Schema-1 YAML `post_chain_peaks` + runtime backstop write-back + unity-gain scanner) |
| **Tandem `foobar2000` front-end via UPnP AV MediaRenderer** | **Implemented** (`ap-upnp`, `ap-source::http_stream`) | Polish gapless/companion flows in future UPnP/Tidal phases |
| **Mega-tap apodizing Gaussian (`gauss-apod-4m`) & steep contrast (`sinc-steep-4m`) + `ext-sharp-lp` + `short-lp/mp`** | Planned (`FilterArchetype` variants defined; UPOLS engine proven in `convolution.rs`) | Upsampling Phases 2–3 (FFT/UPOLS upsampler executor + remaining archetypes) |
| **Minimum-phase (`-mp`) & intermediate-phase (`-ip`) conversion** | Planned (`FilterPhase` enum defined) | Upsampling Phase 4 (FFT real-cepstrum minimum/intermediate phase) |
| **50/15 µs CUESHEET de-emphasis + `Objective` / `GenreAware` smart selector + 30 s pre-scan** | Planned (`FilterSelector` trait + `TrackSourceInfo::apodizing_needed` hook ready) | Upsampling Phases 5–7 (`[FSR]` implementation) |
| **8fS multibit noise shapers (`NS5`/`LNS15` class) + selectable `DAC Bits` (16/18/20/24-bit)** | Baseline `TPDF` (24-bit) active (`NoiseShaperProfile` enum ready) | Implement & measure 8fS error-feedback NTFs + configurable target bit depth on Gungnir 2 NOS |
