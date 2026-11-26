# Audiophile Player v0.1.0 — Forum Thread Starter & Social Pitch

## Suggested Thread Title

**[Free / Open Source] Audiophile Player v0.1.0 — Headless Windows WDM-KS player (pure f64 8fS up to 4M taps, REW convolution, Ultimate per-track filters, UPnP, opt-in Tidal Exclusive)**

---

## Forum Post (Audiophile Style / Head-Fi / Reddit)

Hi everyone,

I just tagged **v0.1.0** of **Audiophile Player** — a free, open-source (`GPL-3.0-or-later`), headless Windows 10/11 x64 player built in Rust for people who already think in terms of poly-sinc, apodizing, noise shapers, and room convolution, and want a set-and-forget engine dedicated purely to the signal.

There is no GUI, no volume knob, no EQ beyond your REW filter, and no Windows mixer anywhere in the path.

**GitHub:** https://github.com/androxylo/audiophile_player  
**v0.1.0 Release:** https://github.com/androxylo/audiophile_player/releases/tag/v0.1.0

### The Signal Path

```text
FLAC / UPnP / Tidal Exclusive (44.1–384 kHz)
  → Upsampling to 8fS (352.8 / 384 kHz, pure f64, up to 4M taps)
  → 50/15 µs de-emphasis (only when flagged)
  → REW stereo room convolution (up to 4M-tap IR at full 8fS)
  → [Last-stage pure-f64 downsampler — only if your DAC tops out below 8fS]
  → Single dithered, noise-shaped quantizer (16 / 18 / 20 / 24-bit)
  → WDM-KS WaveRT exclusive output
```

- **Pure `f64` end-to-end:** Everything between decode and the DAC stays in 64-bit float. Your DAC sees exactly one rounding step.
- **Native 8fS first, lower-rate DACs welcome:** Developed on a Schiit Gungnir 2 (Unison USB, NOS, 352.8/384 kHz) where there is zero downsampling. DACs limited to 44.1–192 kHz (16-, 24-, or 32-bit) are also supported automatically via a linear-phase last-stage downsampler right before the quantizer, with 4fS noise shapers at 176.4/192 kHz.
- **Filters & shapers:** Long Gaussian, steep poly-sinc, apodizing, extended-sharp, and minimum/intermediate phase filters up to 4M taps, paired with clean-room noise shapers (`tpdf`, `ns-gentle-2`, `ns5`, `lns15`, `pggb-optimal`).
- **Ultimate Mode (`--ultimate`):** 30-second look-ahead analyzes each recording (detecting fake hi-res, pre-ringing, and clipped/loud masters), selects an appropriate filter, and applies exact post-chain headroom — with an offline read-only library scanner (`scan`) and your own per-album/per-track overrides (`my-filters.yaml`).
- **Sources:** Local FLAC (gapless), lossless UPnP AV renderer (works with foobar2000, BubbleUPnP, etc.), and an optional standalone pure-Rust ACX 1.1 virtual render driver for zero-mixer **Tidal Desktop Exclusive Mode** (opt-in; requires Win 11 22H2+ with Secure Boot off / Test Mode on).
- **Control & Setup:** Minimal localhost web page for folder browsing and transport, a one-line PowerShell installer that compiles from source on your PC by default (so you don't have to trust an unsigned binary — though SHA-256 + GitHub provenance-attested prebuilt zips are also available via `-Binary`), and a built-in stdio MCP server if you want a free AI assistant to handle setup or control playback (*"turn on ultimate mode"*, *"use apodizing for this album"*).

### Honest Limits
- Windows 10/11 x64 only, WDM-KS exclusive only (no WASAPI yet, no macOS/Linux).
- No digital volume control (use your preamp/amp).
- Ultimate Mode's 4M-tap filters are CPU-hungry on modest PCs (an offline library scan makes subsequent playback start much faster with exact headroom from second zero).

### Looking for Feedback
I'd love to hear from folks who try it — especially:
1. **DAC compatibility reports** across different USB interfaces (native 352.8/384 kHz, 192 kHz, and NOS R-2R DACs).
2. **Listening impressions** comparing Normal vs. Ultimate mode, filter choices, and noise shapers against HQPlayer / PGGB.
3. **Setup friction** — whether the one-line source build, `-Binary`, or AI-assisted path worked smoothly on your machine.

Happy to answer any technical questions about the DSP, headroom policy, or WDM-KS path here in the thread!

---

## Short Social Blurb (X / Bluesky / Mastodon)

Just released **Audiophile Player v0.1.0** — a free, open-source (GPLv3), headless Windows WDM-KS player in Rust.

- Pure `f64` 8fS (352.8/384 kHz) pipeline up to 4M taps + REW convolution
- Single noise-shaped quantizer (16–24 bit), works on 44.1–384 kHz DACs
- Ultimate Mode: 30s look-ahead per-track filter & exact headroom
- Local FLAC, lossless UPnP, opt-in zero-mixer Tidal Exclusive driver
- No GUI, no volume knob, no Windows mixer — builds from source in 1 line or verified `-Binary`

https://github.com/androxylo/audiophile_player
