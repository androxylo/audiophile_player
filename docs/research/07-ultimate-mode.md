# Ultimate mode: no-compromise showcase

Research companion to `UPSAMPLING_RESEARCH.md`, `FILTER_SELECTION_RESEARCH.md`, `HEADROOM_DESIGN.md`, `TIDAL_AND_COMPANIONS_RESEARCH.md` and `04-noise-shapers.md`. Repo `androxylo/audiophile_player` @ 245e85c. 2026-11-25.

**Scope.**
- An opt-in **ultimate mode**, enabled by config (`[ultimate] enabled = true`) or CLI (`--ultimate`). It applies every quality technique the project knows to every track, and logs what it did and why.
- **Compute speed is not a rejection criterion.** The only limit is that playback must keep going: rendering must keep up with real time, with at most the 30 s look-ahead as a lead (§4.3).
- **Optimizations are allowed only if they lose no data** (§1).
- This document describes the design. No code is written yet. Filters are referred to by the top-list numbers in `UPSAMPLING_RESEARCH.md` (#1 `gauss-long-lp` … #8 `short-*`). Selection rules R/P/H/A/L/E/G/M/B are from `FILTER_SELECTION_RESEARCH.md` (FSR).

**Mandates (user decisions, 2026-11-25).**
- **M1, 30 s look-ahead cap.** A track is never prefetched or rendered more than **30 s** past what is playing. Prefetching further risks the user switching tracks, which would make the work useless.
- **M2, data consistency.** The 30 s after any track boundary always belong to that one track (same track, same format). The analysis window can therefore always be the first 30 s, and next-track work starts at the earliest **30 s before the boundary**.
- **M3, short tracks.** Tracks shorter than 30 s are **never** required to be handled optimally. They play with normal-mode processing.
- **M4, Tidal.** Tidal delivers faster than real time, so a 30 s window fills in well under 30 s.

**Hard project constraints that still apply** (`INVESTIGATION_PLAN.md` §1):
- 4,000,000-tap cap per filter.
- Floating point everywhere; a single integer conversion at the end.
- Gain never above 0 dB; no user volume.
- Output at 352.8 / 384 kHz.
- Zero Windows mixer.
- Fixed stage order (§4.1).
- `GPL-3.0-or-later`, clean-room code.

---

## 1. Governing principle: slow is allowed, lossy is not

| Allowed (lossless) | Forbidden in ultimate mode (lossy) |
| --- | --- |
| FFT / UPOLS convolution instead of direct FIR. Matches direct convolution to about −290 dB (UPS §4.2), so the result is equivalent at f64 precision. | Truncating a filter or REW impulse tail at a threshold (for example, today's `flush()` cuts at −200 dB). |
| Skipping partitions that are **exactly zero** (bit-identical result). | Dropping filter coefficients that f64 can still represent, or windowing a kernel shorter than its design length. |
| Parallelism that leaves results unchanged: L/R channels, pipelined stages (upsampler thread → REW thread), and partition-parallel frequency-domain multiply-accumulate inside UPOLS. | Lower precision anywhere (`f32` intermediates, `f32` caches). |
| Caching designed kernels, FFT spectra, detector results and measured peaks, keyed by content hash (identical to recomputing). | Heuristic shortcuts that change output, such as two-stage `*-2s` approximations (UPS §3) or reduced-length "fast" variants. |
| Precomputing the next track inside its 30 s window (M2). | Silently degrading quality when the CPU falls behind (§4.3: refuse loudly instead). |

**Acceptance test for every optimization:** compare the output against a slow reference implementation (direct-form FIR, and double-double where the reference itself must be beyond f64). The difference must be `≤ −280 dBFS` RMS and `≤ −260 dBFS` peak, or bit-identical where the claim is "identical". An optimization that cannot pass this test is not used in ultimate mode.

Consequences for the current code:
- The direct polyphase upsampler (`upsampler.rs` `PolyphaseFirUpsampler`) stays as the **reference**. The planned FFT/UPOLS upsampler executor (UPS §4.2, KB §8 "Upsampling Phases 2–3") becomes the ultimate-mode executor once it passes the test above.
- Direct polyphase remains a valid backend, but for #4 at 1.05M taps and 8x it costs about 9×10¹⁰ multiply-adds per second of audio, so it is unlikely to sustain real time (§4.3). The FFT executor is the lossless optimization that makes #3 and #4 playable.

---

## 2. Ultimate-mode techniques

Status labels:
- **Impl:** implemented (code at 245e85c).
- **Des:** designed but not implemented.
- **New:** introduced here.

| # | Technique | Normal mode | Ultimate mode | Status |
| --- | --- | --- | --- | --- |
| U1 | **30 s look-ahead window** (M1/M2) | Streams 65,536-frame blocks; 24 s output cushion | Fetch and decode the first 30 s of the track (lossless `i32`) before choosing any DSP. The render-ahead lead stays at 30 s or less. Next-track work starts at `T_prev − 30 s`. | New |
| U2 | **Window analysis** with detectors H, A, L, B (FSR §7) on the first 30 s | Pre-scan off by default | Always on, using the FSR 30 s thresholds (A: `≥ 4` events, or `≥ 3` in 10 % of onsets). Whole-track thresholds (A `> 10`, `> 7` under rule E) are used only from the **learn-on-play cache** (U5b). | Des → forced on |
| U3 | **Metadata rules** R, P, H, A, L, E, M, B plus custom `AP_*` tags | Objective rules (Des) | All Objective rules on. GenreAware rule G **on at the lowest priority** (A > L > G > R), toggle `genre_rules` (OQ-35). | Des |
| U4 | **Content-adaptive filter choice** | #1 / #2 rate defaults | Decision table §3 | Des (#3–#8 kernels unimplemented) |
| U5a | **Window-exact headroom** | Tags/metadata, `{−3, 0} dB` + excess-only `S` | Render the 30 s window through the full chain at unity gain, giving the exact window peak `M30`. Gain `G = min(G_HR, −M30 − m_q)`, where `G_HR` is the normal HR result. `M30` can only add attenuation and never relaxes HR (§4.2). | New |
| U5b | **Learn-on-play whole-track data** | Over-counter write-back (Des) | During a full play, track the exact post-chain pre-quantizer peak (the `QuantizerStats::max_pre_quant_peak` mechanism, measured before gain) and run the detectors continuously. When the track ends, write the whole-track `M`, apodizing count, true peak and loudness to the YAML DB (HR §4, FSR §7.10). The next play uses exact `M`: `G = min(0, −M − m_q)`. | Des (HR/FSR) → New runtime path |
| U5c | **Offline full scan** (`ap scan <folder>`, FSR §7.8) | Des, opt-in | Recommended companion for local libraries. A batch job outside playback, so M1 does not apply. Fills the same YAML entries as U5b before the first play. | Des |
| U6 | **Pre-emphasis correction** (50/15 µs, from CUESHEET/tags) | Des | On | Des |
| U7 | **REW convolution without tail truncation** | `flush()` to −200 dB; gapless tail carry | Full `4,194,304`-tap IR, full tail; exact-zero partitions skipped only | Impl (core) / New (no truncation) |
| U8 | **Double-double (≈106-bit) kernel design** (sinc, window, Bessel I0, normalization sums), rounded once to f64 | f64 design | On. One-time cost per kernel, cached. | New (OQ-09) |
| U9 | **Compensated accumulation** (Kahan/Neumaier or double-double multiply-accumulate) | f64 | Option `precision = "dd"`, allowed only if the throughput check (§4.3) passes. Expected to be unmeasurable at the DAC (f64 floor ≈ −290 dB vs 24-bit −144 dB). Off by default (OQ-36). | New |
| U10 | **Noise shaper / DAC bits** chosen from evidence | `ns-gentle-2` at 8fS, `dac_bits = 24` | Same until the Gungnir 2 probes (`04-noise-shapers.md` §7, OQ-32) decide. Then: `dac_bits = 16` + `ns-gentle-2` if the DAC truncates, otherwise 24-bit. High-order shapers stay opt-in (code-activity risk, NS §5). | Impl (shapers) / pending hardware |
| U11 | **Render-ahead playback from RAM** | 24 s cushion | 30 s `i32` render-ahead buffer (92 MB at 384 kHz), `VirtualLock`, `OptimizationLevel::Extreme` | Impl (hygiene) / New (sizing) |
| U12 | **Gapless with exact tails** | `ConvolverOnlyHandoff` on filter change | Same mechanism. Carries the full-length tail (U7), and `set_incoming_carryover` checks filter identity (FSR §4 fix). | Impl / Des fix |
| U13 | **Decision report** (showcase) | Log line per track | Per track, on the web page and in the log: the rules that fired, detector evidence (window vs cached whole-track), filter and taps, `M30` / cached `M`, applied gain, shaper, throughput margin, cache hits, and mode (`ultimate`, `ultimate: cached`, `normal (<30 s)`) | New |
| U14 | **Metadata × analyser fusion** (§5) | Tags feed rules; HR realism checks | Header plans the analysis (length, identity); the 30 s window disproves stale tags (peak staleness check); descriptive tags only shift detector priors; MD5-verified learn-on-play | New |

**Explicitly not in ultimate mode** (none rejected for speed):
- **Whole-track decode or render before playback.** It violates M1.
- **Whole-track "Nyquist-Shannon" sinc (PGGB legacy engine).** It breaks the hard 4M-tap cap and needs the whole track (M1). PGGB V9 moved away from it as its default (KB §3.5).
- **Mid-track filter switching once whole-track evidence arrives.** It would cause an audible discontinuity (FSR §7.7). The improved decision applies on the next play, through U5b.
- **Padding Gaussian kernels beyond their shape-defined length.** It only adds coefficients below f64 resolution, which is meaningless. Kaiser sinc #4 / #5 get their full length.
- **Trellis / look-ahead quantization.** It brings no benefit with a 24-bit multi-level quantizer (NS §1.6).
- **Absolute-polarity guessing, DC removal, "enhancers".** These change the signal and fall outside "no other DSP".

---

## 3. Ultimate filter decision table (first match wins; CLI/YAML `user.filter` overrides all, FSR §7.13)

Evidence priority: YAML whole-track results (U5b/U5c), then tags, then 30 s window detectors (U2), then rate default.

| Condition | Filter | Phase | Notes |
| --- | --- | --- | --- |
| Track `< 30 s` (M3) | Normal-mode default (#1 / #2) | linear | No ultimate analysis. Logged as `normal (<30 s)`. |
| A fires (cached whole-track `> 10` / `> 7` under E, or window `≥ 4`) | **#3 `gauss-apod-4m`** at `sinc-MGa`-class length (65,536 × ratio) | linear | HQPlayer guidance: no harm using apodizing when not needed, harm skipping it when needed (KB §1). |
| H fires (fake hi-res: cliff at 19.5–24.5 kHz) | Treat as 1x with `f_eff = 22.05 / 24 kHz`, then apply the rows below | — | |
| L fires (loud and clipped) | #3 | linear | FSR maps L to #8. Ultimate prefers #3 (OQ-34). |
| Clean 1x (44.1 / 48) | **#4 `sinc-L` class** (131,070 × ratio ≈ 1.05M taps at 8x, Kaiser A = 200 dB) | linear | Flagship steep filter. UPS Q6 is resolved for ultimate in favour of the longest steep filter. |
| Hi-res 2x / 4x, clean | #2 `gauss-hires-lp` | linear | |
| Hi-res 4fS with ultrasonic ADC noise above 40 kHz (Detector H extension) | #2 plus PGGB-style HF noise filter (KB §3.5) | linear | OQ-37 |
| 8fS native | `null` (bit-perfect unless DSP is active) | — | |
| `AP_PHASE=min/ip` tag or user override | #6 / #7 | min / ip | Never automatic. |

All rows must meet DR-1 (flat to ±0.01 dB up to 19 kHz, UPS §9).

---

## 4. Processing model under the 30 s cap

### 4.1 Stage order (fixed, identical to normal mode, `05-rew-convolution-and-delay.md` §1)
```text
upsampling (fS → 352.8 / 384 kHz) → de-emphasis → REW convolution → channel delay → noise shaper / quantizer
```
- REW IRs are authored and exported natively at `352.8 / 384 kHz`, so convolution always runs on upsampled output-rate data. It never runs at the source rate, and the IR is never resampled.
- Ultimate mode changes only *which* upsampler is used and how much is analysed, never the order.
- Folding the upsampler kernel and the REW IR into one combined filter (INV-05 option 3) is out of scope. It would break per-track filter switching, and the combined filter would exceed the 4M-tap cap.

### 4.2 Per-track timeline
```text
t = boundary − 30 s   (or first track start)                         ← M2: earliest allowed start
  W0  fetch + decode first 30 s of next track (local: instant; Tidal: < 30 s, M4)
      if track length < 30 s → normal mode, stop here (M3)
  W1  YAML lookup (STREAMINFO MD5 / content hash) → cached whole-track results?   (U5b/U5c)
      run detectors H/A/L/B on the 30 s window; read tags                         (U2/U3)
      → decision: filter, de-emphasis, REW IR, shaper                             (§3)
  W2  render the 30 s window at unity gain through the full chain → M30            (U5a)
      gain G = min(0, −M − m_q) if cached exact M, else min(G_HR, −M30 − m_q)
  W3  render with gain G → noise shaper → quantizer → i32 render-ahead buffer (lead ≤ 30 s)
t = boundary          playback of the track starts (gapless with the previous track's tail)
  W4  steady state: render stays ≤ 30 s ahead; detectors + exact peak run on all audio
t = track end         if played to the end: write whole-track M, detector results → YAML  (U5b)
                      if skipped: discard; at most 30 s of work wasted (M1)
```
- **W2 to W3 reuse:** `W2` uses the exact chain, so its unity-gain output only needs scaling by `G` before the quantizer (the gain commutes with the linear chain; the result equals a scaled render within f64 rounding, so it passes the §1 test). Rendering the window twice is unnecessary. Keeping the 30 s window as f64 costs 184 MB at 384 kHz, which is acceptable.
- **`m_q`** is the quantizer margin: worst-case shaped-dither excursion `(1.5·Σ|c_k| + 1.5)` LSB. For `ns-gentle-2` at 24-bit that is 4.5 LSB, about −125 dBFS.
- **Headroom safety:**
  - With a cached exact `M`, overs are impossible.
  - On a first play without a cache, `M30` is only a **lower bound** on the track peak, so `G` is the stricter of HR and `M30`. The over-risk is never worse than normal mode, and the over-counter plus U5b fix it on the next play.
  - `G < −3 dB` happens only when REW boost requires it, as HR's `S` already allows (OQ-33).
- **First track of a session:** start-up delay is `W0 + W1 + W2` plus the prebuffer. Estimate: a few seconds locally, about 30 s ÷ (Tidal speed-up) for Tidal.

### 4.3 Throughput requirement (the only speed limit)
The lead cannot exceed 30 s (M1). The average chain speed `s` (× real time, after lossless parallelism) for a track of length `T` must therefore satisfy `s ≥ T / (T + 30 s)`. That is about 0.91 for a 5-minute track, and it approaches 1.0 for long tracks.
- **Lossless parallelism available:** 2× from L/R, about 2× from the upsampler → REW pipeline, and partition-parallel UPOLS multiply-accumulate across cores (64 partitions for a 4M-tap IR).
- **Startup self-check:** ultimate mode benchmarks the chosen chain (filter, REW, precision) on the first window. If `s` falls below the requirement, it **refuses that option with a clear message** naming the bottleneck, for example `precision = "dd"` or the direct-FIR backend, and uses the next-best lossless configuration. It never silently truncates or lowers precision.
- *Estimate, to be benchmarked:* the REW 4M-tap UPOLS runs about 5.9× real time on a Ryzen 9 7900 (KB §6.2). An FFT-executed #4 is shorter than the REW IR, so the full chain is expected to run comfortably above 1×.

### 4.4 RAM (stereo)
| Buffer | Size |
| --- | --- |
| 30 s source window (`i32`) | 10.6 MB at 44.1 kHz, 46 MB at 192 kHz |
| 30 s unity-gain window (`f64`, 384 kHz) | 184 MB |
| 30 s render-ahead (`i32`, 384 kHz) | 92 MB |
| REW spectra cache (two rates) + kernels | tens to hundreds of MB, as today |

Peak RAM is under 1 GB, so no RAM cap setting is needed.

### 4.5 Source-specific behaviour
| Source | 30 s window fill | Notes |
| --- | --- | --- |
| Local FLAC | Instant | Use `FlacBlockReader` for the first 30 s. Full-file decode is not needed. |
| UPnP / HTTP URL (foobar2000, BubbleUPnP, Tidal) | Faster than real time (M4) | Needs a **new bounded read-ahead**: compressed bytes for the first 30 s, then normal streaming. `HttpBodyReader` has no read-ahead today. Next-track URL fetching starts at `boundary − 30 s`. |
| Stream mode (I2 kernel endpoint / I3 ASIO, TID §6) | Real time | No tags or MD5. The window costs a 30 s start-up delay, with headroom from HR and `M30`. Logged as `ultimate: live`. |

---

## 5. FLAC metadata together with the data analyser

FSR §1–§6 already catalogues the tags and the rule that each one feeds, and HR §2 defines the peak-tag realism checks. This section adds how **metadata and the analyser work together** under the 30 s window, which neither document covers.

### 5.1 Four relationships
1. **Metadata plans the analysis.**
   - Header fields are available before any audio is decoded. For local files they are free; for streams they arrive in the first few KB.
   - They give the track's length, identity and format, and so decide *whether* ultimate analysis runs (M3), *what* to cache it under, and *how* to read the window.
2. **The analyser validates metadata.**
   - Tags can be stale (written before a remaster, edit or re-encode) or simply wrong.
   - Whenever the 30 s window **provably contradicts** a tag (hard bounds, not statistics), the tag is discarded for this track and the decision report says so.
3. **Metadata sets analyser priors; it is never the sole trigger for a taste decision.**
   - Descriptive tags (date, media, genre, original rate) shift detector thresholds.
   - Only **facts** act on their own: STREAMINFO format, the CUESHEET pre-emphasis flag, and explicit user `AP_*` tags.
4. **Learn-on-play closes the loop.**
   - Whole-track analyser results (U5b) are stored under the metadata identity and verified by MD5 at the end of the play.
   - The next play starts from whole-track evidence instead of the 30 s window.

### 5.2 What the header provides (RFC 9639, verified)
| Field | Spec facts | Ultimate-mode use | In code today |
| --- | --- | --- | --- |
| STREAMINFO `total samples` | `u(36)`; **0 = unknown** (RFC 9639 Table 3) | Track length `T` before decoding gives the M3 decision (`T < 30 s` means normal mode). It also schedules the next track's work at `boundary − 30 s`. If 0, fall back to DIDL `res@duration`; if that is absent too, assume `≥ 30 s` unless EOF arrives inside the window. | Parsed (`flac.rs` `total_samples`, clamped as a prealloc hint) |
| STREAMINFO `MD5` | `u(128)` MD5 of the unencoded audio, samples interleaved, signed little-endian, sign-extended to whole bytes; **0 = unknown** (RFC 9639 §8.2) | **Identity key** for the YAML DB. In learn-on-play, compute the same MD5 over the decoded samples during the full play, and **write whole-track results only if it matches**. That proves `M` and the detector counts describe exactly this audio, played intact. | Parsed (`streaminfo_md5`, all-zero treated as unset) |
| STREAMINFO rate, channels, bits | Exact | Rule R and the ratio. The analyser cross-checks them: Detector H (real bandwidth vs rate) and Detector B (real bit depth vs container). | Parsed |
| STREAMINFO block and frame sizes | Encoder parameters | Not used | — |
| SEEKTABLE | Optional seek points | After a user seek, re-render from the seek point and **keep** the track's decision (no mid-track switching). The 30 s cap applies from the new playhead. | Decoder-internal |
| CUESHEET per-track flags | `u(1)` **pre-emphasis flag** (CD-DA Q-channel); ISRC; index offsets (RFC 9639 §8.7) | Rule P: the exact de-emphasis trigger. For an album-image FLAC (one file, many CUESHEET tracks), the file is one engine track and the de-emphasis flag of CUESHEET track 1 applies (OQ-41). | **Not parsed** |
| VORBIS_COMMENT | Free-form, repeatable `NAME=value` (FSR §1) | Peak/loudness tags (parsed); descriptive tags and `AP_*` (not yet parsed); album identity (§5.3 row 13) | Peaks only (`parse_peak_metadata_fields`) |
| PICTURE, PADDING, APPLICATION | — | Ignored | — |

### 5.3 Fusion rules per decision
`W30` means the 30 s window analysis. `WT` means whole-track results from learn-on-play or the offline scan. Order of trust: user, then `WT`, then facts in the header, then `W30`, then descriptive tags.

| # | Decision | Metadata input | Analyser input | Fusion rule | Action |
| --- | --- | --- | --- | --- | --- |
| 1 | **Track length / M3** | `total samples`, DIDL `duration` | EOF inside `W30` | Header decides; EOF inside the window overrides | `< 30 s`: normal mode |
| 2 | **Identity / cache** | STREAMINFO MD5, else path+size+mtime (FSR §7.1) | MD5 computed over the full play | YAML write only on an MD5 match. With an unset MD5, key by path+size+mtime and store the computed MD5 for verification next time. | Learn-on-play is safe |
| 3 | **Source peak `P` (headroom)** | RG / R128 / `TRUE_PEAK` tags after the HR §2 realism checks | `W30` exact sample peak; `W30` true peak (BS.1770-style 4× oversampled meter on the source); `M30` post-chain | **New staleness check.** If the `W30` sample peak exceeds the tagged sample peak by more than 0.5 %, or the `W30` true peak exceeds the tagged true peak by more than 0.1 dB, the tag is **disproved**: `P` = unknown (HR −3 dB) and the report logs `stale peak tag`. Otherwise use HR's `G_HR(P)`. Applied gain is `min(G_HR, −M30 − m_q)`. A `WT` exact `M` beats everything. | Never less safe than tags alone; catches tags that are wrong in the dangerous direction |
| 4 | **Loud and clipped (rule L)** | `REPLAYGAIN_TRACK_GAIN` (whole-track loudness) | `W30` short-term loudness (EBU Tech 3341) and clipped runs | Whole-track sources first (`WT`, then tag), per FSR §7.13. Tag "loud" plus `W30` clipped runs: positive. Tag "loud" but no clipped runs in `W30`: inconclusive (L needs clipping evidence). No tag: `W30` alone per FSR §7.4. | §3 row L |
| 5 | **Apodizing need (rule A)** | `ORIGINALDATE` era (rule E lowers the threshold); `MEDIA=CD`; `AP_APODIZING` | `W30` detector A; `WT` event count | `AP_APODIZING` beats `WT`, which beats `W30` (era-adjusted threshold). Tags alone never fire A. | §3 row A |
| 6 | **Effective bandwidth (rule H)** | Rate; `ORIGINALSAMPLERATE` (MQA encoders); `MEDIA` | Spectral cliff (FSR §7.2) | A tag giving an original rate below the container rate lowers H's evidence threshold. Tags alone never fire H. | `f_eff` |
| 7 | **DSD-origin / ultrasonic noise (OQ-37)** | `MEDIA=SACD`, DSD-mentioning `ENCODER`/`COMMENT`, rate 88.2/176.4 | Rising noise shelf above about 25–50 kHz in `W30` | Tag plus analyser: apply the HF noise filter. Strong analyser evidence alone: apply. Tag alone: log only. | §3 HF-noise row |
| 8 | **Pre-emphasis (rule P)** | CUESHEET flag, `AP_DEEMPHASIS` | Spectral high-frequency shelf heuristic | The **flag decides**. The analyser **never** applies de-emphasis on its own, because a false positive is a gross tonal error. With no flag, era 1982–1989, `MEDIA=CD` and a strong shelf, the report suggests `AP_DEEMPHASIS=1`. | De-emphasis stage |
| 9 | **Real bit depth (rule B)** | STREAMINFO bits | Bitwise OR over `W30` / `WT` | Report only, e.g. "16-bit content in 24-bit container" | None (shaper already handles it) |
| 10 | **Lossy origin** | `ENCODER` / comments naming lossy tools | Cliff at 15.5–19.5 kHz on a 1x source | Report only; agreement raises the stated confidence | None |
| 11 | **MQA** | `MQAENCODER`, `ORIGINALSAMPLERATE` | — | Warn (FSR rule M) | None |
| 12 | **Genre (rule G)** | `GENRE`, `CONDUCTOR`, `WORK`, `RELEASETYPE` | None (no genre guessing from audio, FSR §7.7) | GenreAware at the lowest priority (OQ-35) | §3 |
| 13 | **Album context** *(new, opt-in, OQ-40)* | `MUSICBRAINZ_ALBUMID`, else `ALBUMARTIST`+`ALBUM`+`DISCNUMBER`; `REPLAYGAIN_ALBUM_*` | `WT` results already learned for sibling tracks | Owner rule today: **per-track only** (FSR §4). Proposal: with an exact album ID, (a) two or more learned apodizing-positive siblings lower this track's A threshold as a prior, and (b) on gapless albums, prefer the siblings' filter when this track's evidence is inconclusive, avoiding a `ConvolverOnlyHandoff` discontinuity. | Prior only |

**Never written:** tags are never written into FLAC files (FSR §7.9). Everything learned goes to the YAML DB.

### 5.4 Stream sources
- **HTTP FLAC (UPnP / BubbleUPnP / foobar2000):**
  - STREAMINFO and VORBIS_COMMENT arrive in the first bytes. Peak tags are already parsed (`http_stream.rs:1432`).
  - DIDL-Lite gives `duration`, `sampleFrequency` and `bitsPerSample` (`ap-upnp/src/xml.rs`, `duration_s` parsed).
  - When STREAMINFO MD5 is set, learn-on-play also works for repeated Tidal plays.
- **Tidal fMP4/DASH** (if BubbleUPnP serves it unconverted): not supported by the source layer today. The FLAC-in-MP4 `dfLa` box is expected to carry STREAMINFO including the MD5 (*unverified*; check when fMP4 support is designed, TID T1-1).
- **Stream mode (I2/I3):** no metadata at all, so the decision comes from the analyser alone. Headroom uses HR's unknown-peak rule plus `M30`.

### 5.5 Implementation notes
- **Extend the strict tag parser** (`ap-source/src/flac.rs`, same validation style as the peak parser) into a `TrackMetadata`:
  - CUESHEET per-track pre-emphasis.
  - `GENRE` (multi-valued), `ORIGINALDATE` / `ORIGINALYEAR` / `DATE`, `MEDIA`, `RELEASETYPE`, `CONDUCTOR`, `WORK`.
  - `ORIGINALSAMPLERATE`, `MQAENCODER`, `ENCODER`.
  - `AP_FILTER` / `AP_DEEMPHASIS` / `AP_APODIZING`.
  - `MUSICBRAINZ_ALBUMID`, `ALBUM`, `ALBUMARTIST`, `DISCNUMBER`.
  - Normalization rules from FSR §5.
- **`Evidence<T> { value, source: User | Cache | Header | Window | Tag, detail }`** for every input, so §3's decision and the U13 report can show exactly which source won and which tags were disproved.
- **Streaming MD5** over decoded integer samples, in the RFC 9639 byte order, in the producer thread. RustCrypto `md-5` (MIT/Apache-2.0, GPL-compatible). The cost is negligible next to the DSP.
- **TDD fixtures** (synthetic, copyright-free):
  - A peak tag lower than the real window peak gives `stale peak tag` and −3 dB.
  - An MD5 mismatch (corrupted frame) means no YAML write.
  - `total samples = 0` with a 20 s body gives normal mode after EOF in the window.
  - A CUESHEET emphasis flag switches the de-emphasis stage on.
  - A short-track header gives normal mode with no window analysis.
  - Album-ID siblings change only priors, and only when OQ-40 is enabled.

---

## 6. Configuration & CLI

```toml
[ultimate]
enabled = true                 # or CLI --ultimate (CLI wins, engine.rs merge pattern)
genre_rules = true             # OQ-35
precision = "f64"              # "f64" | "dd" (U9; refused if throughput check fails)
learn_on_play = true           # U5b YAML write-back
```
- **The 30 s window is a constant** (`ULTIMATE_LOOKAHEAD_SECONDS = 30`, mandates M1/M2), not a setting.
- **Parser:** `ap-core/src/config.rs` is a hand-written strict parser. Add `Section::Ultimate`, a duplicate check, key parsing, and add `[ultimate]` to the "supported:" error strings. Template: `audiophile-player.example.toml`.
- **CLI:** `--ultimate`. Add a field to `RunMode::PlayRandom` (`ap-cli/src/cli_args.rs`), pass it through `main.rs`, and add help text.
- **Overrides:** an explicit `--filter`, `--shaper` or `--dac-bits` (or YAML `user.*`) still wins. Ultimate changes defaults and analysis depth, never the user's explicit choices.
- **Engine hooks:**
  - `UltimateSelector: FilterSelector` via `UpsamplerPicker::with_selector` (`selector.rs:~600`).
  - Set `TrackSourceInfo::apodizing_needed` and `effective_nyquist_hz` (existing unused hooks).
  - Pass `M` / `M30` as the final (currently `None`) argument of `select_headroom_for_bits`.
  - Size the buffer with `BoundedBlockStreamBuffer::with_limits` (30 s).
  - Start next-track work in the producer at `boundary − 30 s`. Today the next track starts while the 24 s cushion drains (`engine.rs` carryover path).
  - Raise `OptimizationLevel::Extreme`.

---

## 7. Prerequisites (dependency order)

1. FFT/UPOLS upsampler executor plus a §1 equivalence test against direct polyphase.
2. Kernels for #3, #4, #6/#7 and the HF noise filter, meeting DR-1, with lossless full-length tails.
3. FSR detectors H/A/L/B in window mode and streaming whole-track mode; extended `TrackMetadata` parser, `Evidence<T>`, streaming MD5 and the §5.3 fusion rules (stale-tag check first, as it is a safety improvement).
4. 30 s window acquisition: local first-window read, bounded HTTP read-ahead, next-track start at `boundary − 30 s`.
5. Window render + `M30` headroom; YAML DB with learn-on-play write-back (HR §4, Issue #7 Steps 2–3).
6. Double-double kernel design (U8); optional dd accumulation (U9) with the throughput self-check.
7. `[ultimate]` config, CLI flag, `UltimateSelector`, decision report (U13).
8. Gungnir 2 probes (NS §7) to finalize U10.

**TDD gates specific to ultimate mode:**
- No prefetch, decode or render ever exceeds 30 s ahead of the playhead, including at track boundaries (M1).
- Tracks `< 30 s` take the normal-mode path and are labelled as such (M3).
- With a cached `M`, there are zero overs on ISP-stress fixtures at every REW gain, and `max |y| ≥ full scale − m_q − 1 LSB` (no wasted headroom).
- Without a cache, the applied gain is `≤` the normal-mode HR gain (never less safe).
- A skip mid-window discards work cleanly; a full play writes YAML exactly once.
- The throughput self-check refuses an option that is too slow instead of degrading.
- Explicit user overrides beat ultimate decisions; results are deterministic run to run.

---

## 8. Open questions & user decisions (2026-11-25)

- **OQ-33:** *(Resolved, user, 2026-11-25)* **Gain is strictly fixed across the entire track (mandatory).** For local files: exact-`M` continuous gain `G = min(0, −M − m_q)` (and `min(G_HR, −M30 − m_q)` on first play), with `G < −3 dB` only when REW boost or `M > +3 dBFS` requires it. For **streaming sources**, the track peak is never known, so always apply `-3 dB` upsampling safety (`G_up = -3 dB`, plus any REW excess `S`).
- **OQ-34:** *(Resolved, 2026-11-25)* Soft decision: map loud & clipped material (rule L) to `#3 gauss-apod-4m` for **local files** and `#1 gauss-long-lp` for **streaming sources** (since mega-tap `gauss-apod-4m` is local-only; streams never buffer beyond 30 s).
- **OQ-35:** *(Resolved, user, 2026-11-25)* **Yes — GenreAware rules ON by default** in ultimate mode (lowest priority `A > L > G > R`, toggleable via `[ultimate] genre_rules`).
- **OQ-36:** *(Resolved, user, 2026-11-25)* **Pure `f64` runtime precision is sufficient for now** (runtime `precision = "dd"` deferred; one-time U8 double-double kernel synthesis rounded once to `f64` remains enabled at zero runtime cost).
- **OQ-37:** *(Resolved, user, 2026-11-25)* **Yes — ultrasonic ADC/DSD noise above 40 kHz must be clipped/filtered** for correct upsampling and downstream filter behavior. Extend Detector H for 4fS ultrasonic noise and apply a steep linear-phase HF noise filter.
- **OQ-38:** *(Resolved, user, 2026-11-25)* Tidal delivers faster than real time. The look-ahead is capped at 30 s by mandate M1 (wasted work on track switches), not by throughput. Mega-tap `gauss-apod-4m` is local-only (not used on streams).
- **OQ-39:** *(Resolved, measured on Ryzen 9 7900, 2026-11-25)* `#3 gauss-apod-4m` (`524,289` taps) + 4M-tap full-tail REW (`64` partitions) runs at **`20.2×` real time** (`prep = 123 ms`), and `#4 sinc-steep-4m` (`1,048,561` taps) + 4M-tap full-tail REW runs at **`22.9×` real time** (`prep = 319 ms`) — comfortably exceeding `s ≥ T/(T+30 s)`.
- **OQ-40:** *(Resolved, user, 2026-11-25)* **No — strictly per-track independence.** Nothing is guaranteed in the same folder; no album context, sibling priors, or previous-track memory may ever influence another track's filter choice, detector thresholds, or gain calculations.
- **OQ-41:** *(Resolved, user, 2026-11-25)* **Yes** — album-image FLACs (one file, embedded CUESHEET with many tracks) are treated as one engine track; de-emphasis comes from CUESHEET track 1; mixed emphasis flags inside one image are logged as a warning.
