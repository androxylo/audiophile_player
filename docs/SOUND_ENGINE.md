# 🎛️ Sound Engine

[← Home](../README.md) · [Ultimate Mode](ULTIMATE_MODE.md) · **Sound Engine** · [Getting Started](GETTING_STARTED.md)

This page assumes you already know the vocabulary: poly-sinc, apodizing, closed-form, NS5/LNS15, DAC bits. It covers what this player does differently, and why.

---

## Design choices at a glance

| Choice | Why |
|---|---|
| **8fS only** (352.8 kHz for the 44.1 kHz family, 384 kHz for the 48 kHz family) | Every filter, shaper and test is tuned for exactly two output rates, using integer ratios only (2×–8×) |
| **f64 from decoder to quantizer** | Rounding happens once, at the end, with dither |
| **≤ 4M taps** | No credible evidence of audible benefit beyond that, and it bounds memory |
| **No volume, no EQ, no effects** | Gain is used only to prevent clipping. Room correction is your REW filter, nothing else |
| **WDM-KS exclusive** | The Windows mixer is never in the path (and our opt-in Tidal virtual ACX driver blocks `audiodg.exe` in the kernel too). Pause **releases** the DAC |

---

## Upsampling filters

```mermaid
flowchart LR
    subgraph G["Gaussian family: low ringing, -300 dB stopband"]
        direction TB
        GL["gauss-long-lp<br/>1× default"]:::g
        GH["gauss-hires-lp<br/>hi-res default"]:::g
        MP["minphase-long<br/>no pre-ringing"]:::g
        IP["intermediate-long<br/>between"]:::g
        SH["short-lp / short-mp<br/>compact"]:::g
    end
    subgraph M["4M class: local files only"]
        direction TB
        AP["gauss-apod-4m<br/>apodizing, ≈0.5M taps @8×"]:::m
        SI["sinc-steep-4m<br/>long sinc, ≈1M taps @8×"]:::m
        EX["ext-sharp-lp<br/>extended passband, ≈0.5M taps @8×"]:::m
    end
    NU["null<br/>bit-exact for native 8fS"]:::n
    classDef g fill:#c3fae8,stroke:#0ca678,color:#000
    classDef m fill:#e5dbff,stroke:#7048e8,color:#000
    classDef n fill:#f1f3f5,stroke:#495057,color:#000
    style G fill:#f8fffc,stroke:#0ca678,color:#000
    style M fill:#faf8ff,stroke:#7048e8,color:#000
```

| Filter (`--filter`) | Family / character | Closest idea if you know HQPlayer | Typical use |
|---|---|---|---|
| `gauss-long-lp` | Linear-phase Gaussian. Flat to 20 kHz, −300 dB at source Nyquist | Gaussian / closed-form family | 1× default; jazz, acoustic, ambient |
| `gauss-hires-lp` | Same, flat to 0.42 × source rate | Hi-res Gaussian | 88.2–192 kHz default |
| `minphase-long` | Minimum-phase Gaussian (alias `gauss-hires-mp`) | `-mp` variants | Taste: all ringing after the transient |
| `intermediate-long` | Intermediate-phase Gaussian (alias `gauss-hires-ip`) | `-ip` / asymmetric variants | Taste: between linear and minimum |
| `gauss-apod-4m` | Ultra-steep **apodizing** Gaussian | Apodizing Gauss class | Masters with pre-ringing; loud & clipped masters |
| `sinc-steep-4m` | Very long Kaiser sinc | `sinc-L`-class long sinc | Classical; general 1× in Ultimate |
| `ext-sharp-lp` | Sharp, extended passband | Extended / "xtr"-style sharp | Rock, pop, electronic |
| `short-lp`, `short-mp` | Short Gaussian, −150 dB | Short filters | Low-power machines |
| `null` | Pass-through | — | Native 352.8 / 384 kHz sources |

The "closest idea" column is an analogy only. These are original designs, not copies of commercial filters.

### Which filter plays, in normal mode

```mermaid
flowchart LR
    S([Source rate]) --> A{"352.8 / 384?"}
    A -- yes --> N["null"]:::n
    A -- no --> B{"≤ 48 kHz?"}
    B -- yes --> L["gauss-long-lp"]:::g
    B -- "88.2–192" --> H["gauss-hires-lp"]:::g
    classDef n fill:#f1f3f5,stroke:#495057,color:#000
    classDef g fill:#c3fae8,stroke:#0ca678,color:#000
```

`--filter <name>` forces one filter for every track (native 8fS still passes through). For the **per-recording** choice, see [Ultimate Mode](ULTIMATE_MODE.md#how-the-filter-is-chosen).

---

## Noise shaping and DAC bits

There is one quantizer, right at the end. It always applies TPDF dither, plus noise shaping designed for **8fS only**. Coefficient tables designed for 44.1 or 48 kHz are never reused at 8fS.

```mermaid
xychart-beta
    title "In-band (0-20 kHz) noise vs flat TPDF, dB (384 kHz)"
    x-axis ["tpdf", "ns-gentle-2", "pggb-optimal", "lns15", "ns5"]
    y-axis "dB" -60 --> 0
    bar [0, -14.6, -40.6, -51.5, -58.7]
```

| Shaper (`--shaper`) | Order | In-band gain | Ultrasonic cost | Code activity* | When |
|---|---|---|---|---|---|
| `tpdf` | flat | 0 dB | none | 🟢 lowest | Outputs that aren't 8fS |
| `ns-gentle-2` | 2 | −14.6 dB | +6 dB peak | 🟢 low | **Default** at 8fS |
| `pggb-optimal` (`wide40-9`) | 9 | −40.6 dB, quiet to 40 kHz | +18 dB peak | 🟠 high | Listening option |
| `lns15` (`lns15-class`) | 15 | −51.5 dB | +15 dB peak | 🟠 high | Listening option |
| `ns5` (`ns5-class`) | 5 | −58.7 dB | +20 dB peak | 🔴 highest | Listening option |
| `bypass` | — | — | — | — | Automatic for native 32-bit 8fS with `null` (bit-perfect) |

\*Code activity is how far the output word jumps from sample to sample. On a NOS R-2R ladder, more activity means more major-carry glitches. That's why the default is the **gentlest shaper that already pushes noise below a high-end DAC's analog floor**, not the most aggressive one.

### DAC bits

```mermaid
flowchart LR
    Q["f64 signal"] --> D{"--dac-bits"}
    D -- "24 (default)" --> A["Shaped dither at 24 bit<br/>noise far below any analog floor"]:::a
    D -- "16 / 18 / 20" --> B["Shaped dither at your DAC's<br/>true resolution, so the DAC never<br/>truncates undithered internally"]:::b
    classDef a fill:#d0ebff,stroke:#1c7ed6,color:#000
    classDef b fill:#e5dbff,stroke:#7048e8,color:#000
```

Set `--dac-bits 16` only if you know your converter chips truncate to 16 bits (some R-2R designs do). At 16 bits, `ns-gentle-2` brings in-band noise from about −106 dBFS (flat TPDF) to about −121 dBFS. The output word is always 32-bit; only the dithered resolution changes.

---

## Headroom: never louder, just safe

```mermaid
flowchart TD
    P{"Do we know the peak?"} -->|"true-peak or ReplayGain tag,<br/>sanity-checked"| K["Known peak P"]
    P -->|"no tag / stream / bad tag"| U["Assume 0 dBFS"]:::warn
    K --> T{"P ≤ −3 dBFS?"}
    T -- yes --> Z["0 dB"]:::ok
    T -- no --> M["−3 dB"]:::warn
    U --> M
    Z --> C{"Would the REW filter's<br/>boost push it over 0 dBFS?"}
    M --> C
    C -- yes --> S["Scale the convolution down<br/>by exactly the excess"]:::warn
    C -- no --> O["Play"]:::ok
    S --> O
    classDef ok fill:#d3f9d8,stroke:#2f9e44,color:#000
    classDef warn fill:#fff3bf,stroke:#f08c00,color:#000
```

- Gain is **never > 0 dB**. There is no loudness matching and no volume control.
- Peak tags are checked for realism before they're trusted. Implausible values count as unknown.
- REW filters are **not** normalized at load, so their boosts can fill the room's dips. Only the excess that would clip is removed.
- If something still clips, the quantizer saturates cleanly and reports how many samples were affected (`[clip warn]`).
- Ultimate mode replaces this with an **exact measured gain**. See [Exact headroom](ULTIMATE_MODE.md#exact-headroom-one-gain-whole-track).

---

## REW room correction

```mermaid
flowchart LR
    F["📁 Filter folder<br/>(e.g. EqualizerAPO\\config)"] --> P{"Newest stereo .wav<br/>at the current 8fS rate"}
    P -->|352 800 Hz| A["44.1 kHz family"]:::a
    P -->|384 000 Hz| B["48 kHz family"]:::b
    A --> C["Convolution at 8fS<br/>up to 4,194,304 taps / channel"]
    B --> C
    classDef a fill:#d0ebff,stroke:#1c7ed6,color:#000
    classDef b fill:#ffe8cc,stroke:#e8590c,color:#000
```

- Export **two** stereo IRs from REW, one at 352,800 Hz and one at 384,000 Hz. Each source family then uses the matching filter.
- Point the player at a folder (it picks the newest matching file) or at explicit files. See [Getting Started → Room correction](GETTING_STARTED.md#room-correction-rew).
- Mono `.L` / `.R` helper files, WAVs at the wrong rate and truncated WAVs are skipped, and the reason is logged. The EqualizerAPO `config.txt` file itself is not read.
- Ultimate mode uses the full IR tail. Normal mode trims IR tails below −200 dB on IRs longer than 64k taps.

---

## Gapless

```mermaid
sequenceDiagram
    participant A as Track A
    participant B as Track B
    participant D as DAC
    A->>D: last samples
    Note over A,B: same rate family → filter history,<br/>REW tail and shaper state carry over
    A-->>B: tail of A added under the start of B
    B->>D: first samples (no click, no gap)
```

Gapless works for local random and sequential playback, and for UPnP controllers that send the next track ahead of time. When the rate family changes (44.1 → 48), Track A's tail is played out completely before Track B starts.

[← Ultimate Mode](ULTIMATE_MODE.md) · Next: [🚀 Getting Started →](GETTING_STARTED.md)
