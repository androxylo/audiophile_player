# 💎 Ultimate Mode

[🏠 Home](../README.md) · [🚀 Quick Start](QUICK_START.md) · [🧭 Getting Started](GETTING_STARTED.md) · **💎 Ultimate Mode** · [🎛️ Sound Engine](SOUND_ENGINE.md) · [⌨️ CLI](CLI.md)

> **One idea:** before a track plays, *listen to it*. Ultimate mode takes the first 30 seconds of the recording, the FLAC tags, and anything it learned the last time it played the track. From those it chooses the filter, the de-emphasis and the gain that suit **this particular master**. Nothing in the chain is approximated to save CPU.

Turn it on for one session with `--ultimate`, or permanently with `audiophile-player config set ultimate.enabled true` (that's `enabled = true` under `[ultimate]` in the config file) followed by `audiophile-player restart`. With an AI agent connected over MCP, just say *"turn on ultimate mode"* ([how](QUICK_START.md#controlling-the-player-through-mcp)).

It works the same on every DAC. All analysis, filter choice and rendering happen in the 8fS chain; on a DAC below 352.8 / 384 kHz the [last-stage downsampler](SOUND_ENGINE.md#lower-rate-dacs-below-8fs) is applied afterwards.

---

## The three promises

```mermaid
flowchart LR
    A["🎯 <b>Adaptive</b><br/>filter chosen per recording,<br/>not per sample rate"]:::a
    B["🔒 <b>Lossless math</b><br/>no tail truncation, no f32,<br/>no shortcuts when the CPU is slow"]:::b
    C["📏 <b>Exact headroom</b><br/>one fixed gain per track,<br/>never boosts, never pumps"]:::c
    classDef a fill:#fff3bf,stroke:#f08c00,color:#000
    classDef b fill:#d0ebff,stroke:#1c7ed6,color:#000
    classDef c fill:#d3f9d8,stroke:#2f9e44,color:#000
```

"Slow is allowed, lossy is not." If your PC can't render a heavy filter fast enough, Ultimate mode switches to a **lighter filter that is still mathematically exact**. It never cuts corners on the filter it picked.

---

## What happens when you press Play

```mermaid
sequenceDiagram
    autonumber
    participant F as 🎵 Track
    participant L as 👂 Look-ahead (30 s)
    participant B as 🧠 Decision
    participant R as ⚙️ Render
    participant D as 🔊 DAC

    F->>L: First 30 s of audio + FLAC tags
    Note over L: Detectors analyse spectrum,<br/>transients, loudness, clipping
    L->>B: Evidence (and the filter database entry, if any)
    B->>B: Choose filter, phase, de-emphasis
    B->>R: Render the 30 s once, at unity gain
    R->>B: True post-chain peak of those 30 s
    B->>B: Fix ONE gain for the whole track
    R->>D: Play the rendered 30 s (decision report shown)
    loop Rest of the track
        F->>R: Stream to the end with the same filter and gain
        R->>D: Audio
    end
    R->>B: Whole-track exact peak → saved for next play
```

**Scanning is not buffering (streams vs. local files):**
- **Streams (UPnP and live exclusive capture):** buffer and look-ahead are strictly capped at **30 seconds** — a hard ceiling. Streams never run a whole-file peak scan and always keep the −3 dB fail-safe.
- **Local files:** detector look-ahead still runs on the first 30 seconds, while a separate parallel scan reader reads the full file quickly to compute an exact 8× true peak (`tp8x-32`, with a 2-second deadline after W2 finishes; if a slow NAS misses the deadline, playback falls back cleanly and the scan finishes in the background for the next play). Playback buffers only what the active filter needs (which can exceed 30 s for 4M-tap filters), never the full decoded file.

> ⏱️ **Start-up delay:** the first 30 seconds are fully rendered before the first sample plays. With the 4M-class filters on a modest PC, this takes noticeably longer than in normal mode. That cost buys the quality.

**Tracks shorter than 30 s** (interludes, hidden tracks) play with normal-mode filter defaults labelled `normal (<30 s)`, while local short tracks still use whole-track peak evidence (exact unity render `exact-M` or synchronous 8× `scan-tp`) so quiet short tracks never receive a blind −3 dB.

---

## The detectors: what it listens for

```mermaid
flowchart TB
    W["30 s of audio"]:::w
    W --> H["<b>H · Bandwidth</b><br/>Is this '96 kHz' file really<br/>a 44.1 kHz master in a big box?"]:::h
    W --> A["<b>A · Pre-ringing</b><br/>Does the master carry the<br/>ringing of a steep ADC/SRC filter?"]:::a
    W --> L["<b>L · Loud & clipped</b><br/>Brick-walled, flat-topped<br/>loudness-war master?"]:::l
    W --> X["<b>Ultrasonic noise</b><br/>DSD-style or ADC noise shelf<br/>above 35–40 kHz?"]:::x
    W --> B["<b>B · Real bits</b><br/>16 bits of content<br/>in a 24-bit file?"]:::b
    W --> P["<b>True peak</b><br/>4× oversampled peak,<br/>catches stale tags"]:::p

    classDef w fill:#f1f3f5,stroke:#495057,color:#000
    classDef h fill:#ffe3e3,stroke:#e03131,color:#000
    classDef a fill:#fff3bf,stroke:#f08c00,color:#000
    classDef l fill:#ffd8a8,stroke:#e8590c,color:#000
    classDef x fill:#e5dbff,stroke:#7048e8,color:#000
    classDef b fill:#d0ebff,stroke:#1c7ed6,color:#000
    classDef p fill:#d3f9d8,stroke:#2f9e44,color:#000
```

| Detector | What it actually looks for | What it changes |
|---|---|---|
| 🔴 **H, fake hi-res** | A spectral cliff at about 20 kHz with real music below it and nothing above 25 kHz. More sensitive when `ORIGINALSAMPLERATE` says the source was lower | The track is treated as 1× material: its real Nyquist is 22.05 / 24 kHz |
| 🟡 **A, apodizing need** | Pre-ringing events near Nyquist just before transients. More sensitive for 1982–1995 early-digital masters | Selects the **apodizing** filter, which replaces the master's pre-ringing with its own gentle roll-off |
| 🟠 **L, loud & clipped** | BS.1770 loudness ≥ −10 LUFS **and** repeated clipped runs | Also selects the apodizing filter, which is the gentlest on flat-topped transients |
| 🟣 **Ultrasonic noise** | A rising noise shelf above 35–40 kHz (DSD-derived or noisy ADC) | Reported, and an HF-noise-aware hi-res path is used |
| 🔵 **B, effective bits** | Lowest bit actually used across the samples | Report only (for example "16-bit content in 24-bit container") |
| 🟢 **True peak** | 4× oversampled peak of the window | Rejects ReplayGain peak tags that are provably wrong |

Lossy-origin suspicion (a cliff around 16 kHz) and MQA are only **reported**. They never change the sound on their own.

---

## Who gets the final word

Not every piece of evidence is equally trustworthy. Ultimate mode ranks it:

```mermaid
flowchart LR
    U["👤 <b>You</b><br/>my-filters.yaml<br/>or AP_* tags"]:::u --> C["💾 <b>Whole-track memory</b><br/>from a previous full play<br/>or offline scan"]:::c --> H["📄 <b>File header</b><br/>STREAMINFO, CUESHEET"]:::h --> W["👂 <b>30 s look-ahead</b>"]:::w --> T["🏷️ <b>Ordinary tags</b><br/>genre, year, ReplayGain"]:::t --> D["⚙️ <b>Defaults</b>"]:::d
    classDef u fill:#2f9e44,stroke:#2b8a3e,color:#fff
    classDef c fill:#51cf66,stroke:#2f9e44,color:#000
    classDef h fill:#94d82d,stroke:#66a80f,color:#000
    classDef w fill:#ffd43b,stroke:#f08c00,color:#000
    classDef t fill:#ffa94d,stroke:#e8590c,color:#000
    classDef d fill:#ced4da,stroke:#868e96,color:#000
```

Stronger evidence (left) beats weaker evidence (right). Tags are never trusted blindly: a ReplayGain peak that the music itself exceeds is discarded on the spot. A genre or year tag alone can never trigger the apodizing filter. It can only make the detector more sensitive.

---

## How the filter is chosen

```mermaid
flowchart TD
    S([Track]) --> N{Native<br/>352.8 / 384 kHz?}
    N -- yes --> NULL["<b>null</b><br/>bit-exact pass-through"]:::keep
    N -- no --> U{Your choice?<br/>my-filters.yaml / AP_FILTER}
    U -- yes --> UF["<b>Your filter</b>"]:::user
    U -- no --> A{"Rule A<br/>pre-ringing in master?"}
    A -- yes --> APOD["<b>gauss-apod-4m</b><br/>apodizing"]:::apod
    A -- no --> L{"Rule L<br/>loud & clipped?"}
    L -- yes --> APOD
    L -- no --> R1{"1× material?<br/>(≤48 kHz or fake hi-res)"}
    R1 -- no --> HR["<b>gauss-hires-lp</b><br/>clean hi-res"]:::hires
    R1 -- yes --> G{"Rule G<br/>genre known?"}
    G -- Classical --> SINC["<b>sinc-steep-4m</b>"]:::sinc
    G -- "Jazz / acoustic<br/>Ambient / OST" --> GL["<b>gauss-long-lp</b>"]:::gauss
    G -- "Rock / pop<br/>electronic" --> EXT["<b>ext-sharp-lp</b>"]:::ext
    G -- unknown --> SINC

    classDef keep fill:#f1f3f5,stroke:#495057,color:#000
    classDef user fill:#2f9e44,stroke:#2b8a3e,color:#fff
    classDef apod fill:#fff3bf,stroke:#f08c00,color:#000
    classDef hires fill:#d0ebff,stroke:#1c7ed6,color:#000
    classDef sinc fill:#e5dbff,stroke:#7048e8,color:#000
    classDef gauss fill:#c3fae8,stroke:#0ca678,color:#000
    classDef ext fill:#ffd8a8,stroke:#e8590c,color:#000
```

- "Your choice" means `--filter` on the command line, an entry in `my-filters.yaml`, or an `AP_FILTER` tag. The last two are used in `mine` mode (the default whenever `my-filters.yaml` exists); `--filters suggested` skips them for an A/B comparison. See [Your overrides](#your-overrides-my-filtersyaml).
- An `AP_PHASE` tag (or `phase:` in `my-filters.yaml`) with `minimum` / `intermediate` / `linear` switches to the **min-phase** or **intermediate-phase** member of the Gaussian family.
- The genre rules are on by default. Turn them off with `--no-genre-rules` or `genre_rules = false` in `[ultimate]`.
- On **streams** (UPnP and live exclusive Tidal capture), the 4M-class filters are replaced by their long Gaussian equivalents (`gauss-long-lp` / `gauss-hires-lp`). Live real-time capture starts with a short ~0.75 s cushion instead of waiting 30 s, while still keeping Ultimate mode's full-tail REW convolution and pure-`f64` 8fS path.

The filters themselves are described in [Sound Engine → Filters](SOUND_ENGINE.md#upsampling-filters).

---

## Exact headroom: one gain, whole track

For local files in Ultimate mode, the maximum peak is **always known** (except if decoding fails or a slow network share misses the 2 s scan deadline). Peak evidence is resolved in strict priority order:

| Tier | `peak_source` | Evidence | Meter / ISP allowance `A_tp` |
|---|---|---|---|
| **E1** | `exact-M` | Cached exact unity-gain post-chain peak `M` for this audio MD5 and DSP chain (`PostChainKey`). Survives `DETECTOR_VERSION` upgrades | `0.00 dB` (exact) |
| **E3** | `scan-tp` | Parallel whole-file 8× / 32-tap Kaiser true-peak scan (`tp8x-32`, ≤ 0.1 dB error across 0–0.45·fs, 2 s post-W2 deadline) | `0.10 dB` |
| **E4** | `cached-tp` | Cached whole-track true peak (`true_peak_dbtp`) for this audio MD5 when the DSP chain changed (`PostChainKey` miss) | `0.10 dB` (`tp8x-32`) / `0.70 dB` (`tp4x-12`) |
| **E5** | `tag-tp` / `tag-sp` | Validated FLAC true-peak or ReplayGain sample-peak tag | `0.70 dB` (`tag-tp`) / `3.00 dB` (`tag-sp`) |
| — | `unknown` | Fail-safe (streams, decode error, or missed 2 s deadline without fallback) | `3.00 dB` |

The gain step `G_up` is strictly binary (**0 dB** or **−3 dB**), and `S ≤ 0` handles only excess over the `-(m_q + 1.0 dB)` DAC ceiling:

```text
G_up  =  0 dB   if source TP <= -3.00 dBFS
      = -3 dB   otherwise  (including cached exact-M tracks with hot source peaks)

# Cached exact-M (E1):
S     = min( 0, -(max(M, M30) + G_up + m_q + 1.0 dB) )

# Known source peak without cached M (E3 / E4 / E5):
M_hat = max( M30, TP + A_tp + A_up(plan) + max(G_c, 0) )
S     = min( 0, -(M_hat + G_up + m_q + 1.0 dB) )

G     = G_up + S   (locked for the whole track; never boosts, never pumps)
```

- Quiet masters (`TP ≤ −3.00 dBFS`, no REW boost) play at **0.0 dB** on the very first play (and stay bit-perfect `Bypass` when the plan allows it).
- `A_up(plan)` bounds upsampler peak overshoot relative to ideal reconstruction (`0.00 dB` for `null`, `0.10 dB` for Gaussian LP, `0.40–0.45 dB` for 4M steep/apodizing LP, `1.75–1.80 dB` for intermediate/minimum phase).
- Peak validity (`true_peak_dbtp`, `sample_peak`, `post_chain_peaks`) is keyed strictly on audio identity (MD5), separate from `DETECTOR_VERSION`, so upgrades recompute detector rules without discarding cached peaks.
- If a ReplayGain tag disagrees with E3/E4 by more than `0.5 dB`, E3/E4 wins and a `stale ReplayGain tag` notice is logged.
- Running an **offline scan** (`scan --post-chain`) pre-measures exact `M` for your whole library so even the first play hits `exact-M`. See [Getting Started → Scan](GETTING_STARTED.md#scan-your-library-optional).
- Streams (UPnP and live exclusive Tidal capture) always keep −3 dB (`unknown`), because their peak and end are unknown ahead of time.

---

## Pre-emphasis

Early-1980s CDs with pre-emphasis get exact IEC **50/15 µs de-emphasis** in f64, applied before room correction. It is applied only when there is a **fact** to go on:
- the CUESHEET pre-emphasis flag,
- an `AP_DEEMPHASIS` tag,
- or a `deemphasis: true` entry in your `my-filters.yaml`.

A "sounds bright, 1982–1989" guess is only logged as a suggestion. It is never applied automatically.

---

## Your overrides (my-filters.yaml)

Ultimate mode cleanly separates **machine-managed analysis** from **your personal filter choices** (Schema 2):

- **Machine-managed analysis (`%APPDATA%\audiophile-player\filterdb\`):** one human-readable YAML file per album folder mirroring your library tree (e.g. `filterdb\analysis\main\Jazz\Kind of Blue.yaml`). Every track records `suggested`, a plain-English `why`, detector readings, and exact `post_chain_peaks`. Your music library stays 100% read-only.
- **Your choices (`%APPDATA%\audiophile-player\my-filters.yaml`):** a single file owned exclusively by you — never silently overwritten by the player or scanner, hot-reloaded on track change, with support for defaults, folder/artist rules, and per-track overrides:

```yaml
schema: 2

defaults:
  # optional fallback override

folders:
  "main:Jazz/Miles Davis - Kind of Blue":
    filter: gauss-long-lp
    note: "prefer gauss-long-lp on this pressing"

tracks:
  "main:Classical/Mahler/01_Adagio.flac":
    filter: sinc-steep-4m
    deemphasis: false
    note: "Japanese pressing, sounds better steep"
```

### Precedence & A/B mode (`--filters`)

- **Default (`--filters auto`):** uses `mine` mode whenever `my-filters.yaml` exists, falling back to `suggested`.
- **A/B comparison (`--filters suggested`):** ignores `my-filters.yaml` and `AP_*` overrides and plays pure machine suggestions without editing any files.
- **Precedence in `mine` mode:** CLI `--filter` → `my-filters.yaml` `tracks:` → deepest `folders:` → shallower `folders:` → FLAC Vorbis tags (`AP_FILTER`, `AP_PHASE`, `AP_DEEMPHASIS`, `AP_APODIZING`) → `my-filters.yaml` `defaults:` → machine `suggested` analysis.
- Explicit choices in `my-filters.yaml` also apply to short tracks (`< 30 s`), while automatic detectors remain strictly per-track.
- Whenever a user choice overrides the machine suggestion, both the console and the HTTP UI display them side by side:
  `playing: gauss-long-lp (mine: folder) | suggested: sinc-steep-4m (rule_G_classical)`.

You rarely need to edit the file by hand. The `filters` commands validate the filter name, keep your comments and save a `.bak` first:

```powershell
audiophile-player filters set "D:\Music\Jazz\Miles Davis - Kind of Blue" gauss-long-lp --note "prefer gauss-long-lp on this pressing"
audiophile-player filters set "D:\Music\Classical\Mahler\01_Adagio.flac" sinc-steep-4m
audiophile-player filters set "D:\Music" gauss-long-lp     # a default for the whole library root
audiophile-player filters show "D:\Music\Jazz\Miles Davis - Kind of Blue"   # mine vs suggested, and why
audiophile-player filters diff                               # every place you disagree with the machine
audiophile-player filters check                              # catch typos and dead paths
```

Changes apply from the **next track**, with no restart. With an AI agent, *"use apodizing for this album"* does the same through the `filters_set` tool. The full command list is in [Getting Started → Filter database](GETTING_STARTED.md#filter-database).

Upgrading from an older version? `audiophile-player filters migrate` imports your Schema-1 `trackdb\` entries and `user:` overrides into Schema 2 without deleting anything.

---

## Reading the decision report

Every track prints one line to the console and to the web page. The example below is illustrative:

```text
ultimate=[mode=[ultimate: live] rule=rule_A_apodizing fired=[A(apod:window(W30)),G(Classical)]
          M30=-5.12dBFS peak_source=scan-tp peak_scan_ms=142 A_tp=0.10dB A_up=0.45dB
          G=+0.000dB m_q=0.00027dB speed=3.4xRT]
```

| Field | Meaning |
|---|---|
| `mode` | 🟢 `cached` = exact memory · 🟡 `live` = first play · ⚪ `normal (<30 s)` |
| `rule` | Which rule picked the filter: `user_filter`, `user_phase_min` / `user_phase_ip`, `rule_A_apodizing`, `rule_L_loud_clipped`, `rule_G_classical` (etc.), `rule_H_fake_hires`, `rule_R_1x_clean`, `rule_R_hires_clean`, `throughput_fallback(…)`, `cli_override`, `short_track_bypass` |
| `fired` | Every rule that said "yes", and where its evidence came from (`user`, `cache(WT)`, `header`, `window(W30)`, `tag`) |
| `M` / `M30` | Whole-track / 30-second peak after the full chain |
| `peak_source` | Peak evidence tier used for headroom (`exact-M`, `scan-tp`, `cached-tp`, `tag-tp`, `tag-sp`, `unknown`) — also exposed in `status --json` and on the web UI |
| `peak_scan_ms` | Wall-clock duration of the parallel E3 whole-file peak scan in milliseconds (when E3 ran) |
| `A_tp` / `A_up` | Meter/ISP error allowance (`0.10–3.00 dB`) and upsampler overshoot bound (`0.00–1.80 dB`) |
| `G` | The fixed gain applied to the whole track |
| `speed` | Render speed in multiples of real time. Below the required speed, a lighter **exact** filter is used |

[← Getting Started](GETTING_STARTED.md) · [🏠 Home](../README.md) · Next: [🎛️ Sound Engine →](SOUND_ENGINE.md)
