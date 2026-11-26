# Audiophile Player

**A headless, no-compromise upsampling player for Windows: every track is rendered in 64-bit float to 8fS (352.8 / 384 kHz), with REW room correction and a single noise-shaped quantizer, sent bit-exact to your DAC over kernel streaming.**

![Output](https://img.shields.io/badge/output-352.8%20%2F%20384%20kHz-6f42c1)
![Math](https://img.shields.io/badge/DSP-64--bit%20float-1f6feb)
![Filters](https://img.shields.io/badge/filters-up%20to%204M%20taps-d73a49)
![Transport](https://img.shields.io/badge/transport-WDM--KS%20exclusive-2da44e)
![UPnP](https://img.shields.io/badge/UPnP-renderer-fb8500)
![Tidal](https://img.shields.io/badge/Tidal%20Desktop-exclusive%20ACX%20(opt--in)-00b4d8)
![License](https://img.shields.io/badge/license-GPL--3.0--or--later-555)

No GUI and no volume knob. There is no EQ beyond your REW filter, and no Windows mixer anywhere in the path. All the effort goes into the signal.

> [!TIP]
> **☕ Enjoying the music? Buy me a coffee!**
> Audiophile Player is 100% free and open source — built out of pure obsession with clean sound. If it brings your DAC to life, consider supporting continued development:
>
> [![Buy Me a Coffee — GitHub Sponsors](https://img.shields.io/badge/%E2%98%95_Buy_Me_a_Coffee-Sponsor_%40androxylo-ea4aaa?style=for-the-badge&logo=githubsponsors&logoColor=white)](https://github.com/sponsors/androxylo)
>
> 👉 **[https://github.com/sponsors/androxylo](https://github.com/sponsors/androxylo)**

---

## The signal path

```mermaid
flowchart LR
    SRC["🎵 FLAC file, UPnP,<br/>or Tidal Exclusive<br/>44.1 … 384 kHz"]:::src
    UP["⬆️ Upsampling<br/>to 8fS<br/>Gaussian / sinc / apodizing<br/>up to 4M taps"]:::dsp
    DE["De-emphasis<br/>50/15 µs<br/><i>only when flagged</i>"]:::opt
    REW["🏠 REW room<br/>convolution<br/>up to 4M-tap IR"]:::dsp
    Q["🎚️ One quantizer<br/>TPDF + noise shaping<br/>16 / 18 / 20 / 24 bit"]:::q
    DAC["🔊 Your DAC<br/>WDM-KS exclusive<br/>352.8 / 384 kHz"]:::dac

    SRC --> UP --> DE --> REW --> Q --> DAC

    classDef src fill:#fff3bf,stroke:#f08c00,color:#000
    classDef dsp fill:#d0ebff,stroke:#1c7ed6,color:#000
    classDef opt fill:#f1f3f5,stroke:#868e96,color:#000,stroke-dasharray: 4 3
    classDef q fill:#e5dbff,stroke:#7048e8,color:#000
    classDef dac fill:#d3f9d8,stroke:#2f9e44,color:#000
```

Everything between source and DAC runs in **f64**. The DAC sees exactly **one** rounding operation in the whole chain: the final dithered, noise-shaped quantization. **Upsampling always comes before room correction**, so your REW filter works at the full 8fS rate and covers the whole spectrum.

---

## Two ways to play

```mermaid
flowchart TB
    subgraph N["⚡ Normal mode (default)"]
        direction LR
        N1["Track arrives"] --> N2["Pick filter<br/>by sample rate"] --> N3["Play immediately"]
    end
    subgraph U["💎 Ultimate mode  (--ultimate)"]
        direction LR
        U1["Track arrives"] --> U2["Listen 30 s ahead<br/>+ read tags + track DB"] --> U3["Pick the best filter<br/>for <b>this</b> recording"] --> U4["One exact gain<br/>for the whole track"] --> U5["Remember it<br/>for next time"]
    end
    style N fill:#e7f5ff,stroke:#1c7ed6,color:#000
    style U fill:#f3f0ff,stroke:#7048e8,color:#000
```

| | ⚡ Normal | 💎 Ultimate |
|---|---|---|
| Filter choice | By sample rate: Gaussian long LP (1×) or hi-res LP | Per recording: apodizing, steep sinc, extended-sharp, min/intermediate phase… |
| Detects fake hi-res, pre-ringing (apodizing need), loud & clipped masters | — | ✅ |
| Uses FLAC tags (genre, original year, ReplayGain, pre-emphasis, your overrides) | Peak tags only | ✅ everything |
| Headroom | −3 dB or 0 dB from peak tags | One exact gain per track, measured after the full chain |
| REW impulse tail | Tail below −200 dB is trimmed on very long IRs | Full tail, never truncated |
| Start-up | Instant | First 30 s are rendered before the first sample plays |

👉 **[How Ultimate Mode thinks →](docs/ULTIMATE_MODE.md)**

---

## Where to go next

| | Page | What you'll find |
|---|---|---|
| 💎 | **[Ultimate Mode](docs/ULTIMATE_MODE.md)** | The 30-second look-ahead, the detectors, how a filter is chosen, exact headroom, learn-on-play, per-track overrides |
| 🎛️ | **[Sound Engine](docs/SOUND_ENGINE.md)** | Every filter and noise shaper explained for HQPlayer users, DAC bits, headroom policy, REW convolution, gapless |
| 🚀 | **[Getting Started](docs/GETTING_STARTED.md)** | Install (release or source), first play, web page, UPnP, Tidal Desktop exclusive driver, config file, command-line options |

---

## At a glance

```mermaid
mindmap
  root((Audiophile<br/>Player))
    Sources
      Local FLAC library
      UPnP / DLNA renderer
      Tidal Desktop (opt-in exclusive ACX driver)
    DSP
      f64 throughout
      8fS only
      ≤ 4M taps
      REW convolution
    Output
      WDM-KS kernel streaming
      Exclusive only
      Releases DAC on pause
    Control
      Tiny local web page
      foobar2000 / BubbleUPnP
      Command line
```

**Status and honest limits**
- 🟢 **Works today:** local FLAC (random or sequential, gapless), UPnP renderer (gapless), **Tidal Desktop Exclusive Mode** via our opt-in Rust ACX virtual driver (zero Windows mixer), normal and ultimate modes, REW convolution, offline library scan, local web page.
- 🟡 **Planned:** WASAPI exclusive output, per-channel delay.
- 🔴 **Not supported, by design:** Windows shared mode, volume control, DSP effects, macOS / Linux playback, DACs without 352.8 / 384 kHz input.
- Reference DAC: **Schiit Gungnir 2** (Unison USB, NOS). Any DAC that accepts 32-bit 352.8 / 384 kHz over a kernel-streaming driver should work.

## License

[GPL-3.0-or-later](LICENSE). The repository contains no copyrighted music and no proprietary filter coefficients. Shapers named after familiar ones (`ns5`, `lns15`, `pggb-optimal`) are clean-room designs that follow the published *intent* of those shapers. They are not copies.
