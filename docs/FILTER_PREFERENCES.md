# Community & Listener Filter Preferences

[← Back to Sound Engine](SOUND_ENGINE.md)

> This is a living reference notebook collecting community opinions, forum posts, and listening guides on PCM upsampling filters — kept **outside the core manual** and updated as useful perspectives appear.
>
> All entries reflect the subjective listening impressions of their respective authors. Mentions of commercial filter names (such as HQPlayer or PGGB) are for discussion and analogy only; Audiophile Player implements clean-room, open-source filters (`GPL-3.0-or-later`).

---

## Quick Taste Map → Audiophile Player (`--filter`)

| Listener goal / taste | Common HQPlayer reference | Closest Audiophile Player `--filter` | Notes |
|---|---|---|---|
| **All-rounder / best time–frequency balance (1×)** | `poly-sinc-gauss-long` / `-medium` / `-short` | `gauss-long-lp` (default 1×), `short-lp` | Balanced natural timbre & stage; zero harshness |
| **Hi-res (88.2–192 kHz) / gentle ringing** | `poly-sinc-gauss-hires-lp` / `-mp` / `-ip` | `gauss-hires-lp` (default Nx), `minphase-long` (`gauss-hires-mp`), `intermediate-long` (`gauss-hires-ip`) | Wide alias margin; `-mp`/`-ip` reduce or remove pre-ringing |
| **Natural timbre / extended sharp passband** | `poly-sinc-ext2` / `ext3` | `ext-sharp-lp` | Extended flat passband, crisp & engaging |
| **Old-school / classic analog recordings** | `poly-sinc-short-mp` / `-short-lp` | `short-mp`, `short-lp` | Compact impulse, less aggressive error correction |
| **Loudness-war / clipped / pre-ringing cleanup** | `poly-sinc-xtr-short-lp`, `poly-sinc-gauss-xla`, `sinc-MGa` | `gauss-apod-4m` | Apodizing cutoff cleans mastering-stage ringing (auto-picked by Ultimate Rules A & L) |
| **Concert hall / classical space & scale** | `poly-sinc-gauss-xla`, `sinc-L`, `sinc-M` | `sinc-steep-4m`, `gauss-apod-4m` | Very long linear-phase kernels emphasizing space & authority |
| **Pure sample-preserving / halfband** | `poly-sinc-gauss-halfband` / `-s`, `closed-form` | *(not implemented — `null` for native 8fS only)* | No error correction; only for pristine masters |

---

## Collected Community Posts & Guides

### 1. SBAF — "HQPlayer Filter Selection Made Easy" (`ohshitgorillas`, Aug 27, 2026)

- **Source:** [Super Best Audio Friends — *HQPlayer Filter Selection Made Easy*](https://superbestaudiofriends.org/index.php?threads/hqplayer-filter-selection-made-easy.15740/) by [ohshitgorillas](https://superbestaudiofriends.org/index.php?members/ohshitgorillas.830/) ([post #464524](https://superbestaudiofriends.org/index.php?posts/464524/))
- **Context:** Distilled from HQPlayer's 65+ PCM filters for HQPTuner's "Easy Mode" (reportedly with Jussi Laako's "grumpy approval") as approachable training wheels: pick a preset, pick an emphasis (`Space` ↔ `Transients`), and apply to 1× and/or Nx rates.

| Preset | Author's description | HQPlayer filter & emphasis axis | Closest Audiophile Player equivalent |
|---|---|---|---|
| **The Perfect Ten** *(start here)* | Does everything well, no strings attached — best-case solution to the time–frequency tension. `Transients` corrects errors less aggressively; if harsh, move toward `Space`. | `poly-sinc-gauss-long`<br>`Space \| Balanced \| Transients` → `-gauss-long` \| `-gauss-medium` \| `-gauss-short` | `gauss-long-lp` (default 1×) → `short-lp` |
| **The Perfect Ten: Hi-res Mode** | Version of The Perfect Ten for ≥ 88.2 kHz sources (also suitable for lossy / MQA). | `poly-sinc-gauss-hires-lp`<br>`Space \| Transients` → `-lp` \| `-mp` | `gauss-hires-lp` (default Nx) \| `minphase-long` (`gauss-hires-mp`) *(also `intermediate-long` in between)* |
| **Lifelike** | Focuses on natural, realistic timbre. `Transients` corrects errors less aggressively. | `poly-sinc-ext2-long`<br>`Space \| Balanced \| Transients` → `-ext2-long` \| `-ext2-medium` \| `-ext2-short` | `ext-sharp-lp` |
| **Lifelike: Hi-res Mode** | Hi-res version of Lifelike for ≥ 88.2 kHz (also lossy / MQA). | `poly-sinc-ext2-hires-lp`<br>`Space \| Transients` → `-ext2-hires-lp` \| `-ext2-hires-mp` | `ext-sharp-lp` / `minphase-long` |
| **Old School** | Classic-era analog recordings. Less aggressive error correction; if harsh, pick another preset. | `poly-sinc-short-mp`<br>`Space \| Transients` → `-short-lp` \| `-short-mp` | `short-lp` \| `short-mp` |
| **The Purist** | Passes original samples unchanged; higher resource use; no error correction — only for the highest technical quality sources. | `poly-sinc-gauss-halfband`<br>`Space \| Transients` → `-gauss-halfband` \| `-gauss-halfband-s` | *(No halfband filter in AP; `null` passes native 352.8 / 384 kHz bit-perfect)* |
| **Damage Control** | Aggressive clean-up for loudness-war and less-than-good masters. | `poly-sinc-xtr-short-lp`<br>`Space \| Transients` → `-lp` \| `-mp` | `gauss-apod-4m` *(automatically selected in Ultimate Mode by Rules A & L)* |
| **The Concert Hall** | Emphasizes recording space over transient snap; live classical with minimal transient content; heavy CPU. | `poly-sinc-gauss-xla` (`The Perfect Ten` vs `Lifelike` → `-gauss-xla` \| `-ext2-xla`; error correction `On \| Off` → `-xla` \| `-xl`) | `gauss-apod-4m` (apodizing) \| `sinc-steep-4m` (long linear-phase classical, Ultimate Rule G) |

**Key takeaway for Audiophile Player:**
- Our defaults (`gauss-long-lp` for 1×, `gauss-hires-lp` for Nx) align directly with **"The Perfect Ten"** starting point.
- Our **Ultimate Mode** automates the exact trade-offs highlighted above: triggering `gauss-apod-4m` ("Damage Control" / apodizing error correction) on pre-ringing or loudness-war masters, `sinc-steep-4m` ("Concert Hall" long linear-phase) on clean classical/1×, `ext-sharp-lp` on modern rock/pop/electronic, and gentle `gauss-hires-*` on true hi-res recordings.
- Potential future additions to consider if listeners request them: a `gauss-halfband` ("The Purist") archetype and a `gauss-medium` step between `gauss-long-lp` and `short-lp`.

---

### 2. Earlier Cross-Forum Survey (Audiophile Style, Roon, Head-Fi, SBAF, ASR, Archimago)

See [docs/research/02-audiophile-filter-survey.md](research/02-audiophile-filter-survey.md) for the broader Phase-0 consensus survey covering `poly-sinc-gauss-*`, `sinc-MGa`/`sinc-M`/`sinc-L`, `poly-sinc-ext2/3`, PGGB mega-tap Gaussian/Kaiser, and multibit/NOS `DAC Bits` + noise-shaper (`NS5` / `LNS15`) pairing.
