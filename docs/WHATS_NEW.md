# ✨ What's New

[🏠 Home](../README.md) · [🚀 Quick Start](QUICK_START.md) · [🧭 Getting Started](GETTING_STARTED.md) · [💎 Ultimate Mode](ULTIMATE_MODE.md) · [🎛️ Sound Engine](SOUND_ENGINE.md) · [⌨️ CLI](CLI.md)

Release highlights for Audiophile Player. For upgrade instructions, see [Quick Start → Updating](QUICK_START.md#updating).

## v0.1.0 — Initial Public Release

| | |
|---|---|
| 🎚️ **Pure-f64 8fS DSP engine** | Locks upsampling (Gaussian, poly-sinc, apodizing, minimum/intermediate phase, up to 4M taps), 50/15 µs de-emphasis, and REW stereo room convolution (up to 4M-tap IRs) at 352.8 / 384 kHz in 64-bit float, with a single noise-shaped quantizer (`tpdf`, `ns-gentle-2`, `ns5`, `lns15`, `pggb-optimal`) and WDM-KS WaveRT exclusive output. |
| 🔽 **DACs below 352.8 / 384 kHz supported** | 192, 176.4, 96, 88.2, 48 and 44.1 kHz DACs (32-, 24- or 16-bit containers) work automatically via a last-stage pure-f64 downsampler ([how it works](SOUND_ENGINE.md#lower-rate-dacs-below-8fs)). 176.4 / 192 kHz outputs get proper 4fS noise shapers; `--max-rate` caps the output rate by hand. |
| 💎 **Ultimate Mode & Schema-2 Filter DB** | 30-second look-ahead recording analysis (fake hi-res, pre-ringing, clipped/loud masters), exact post-chain headroom, offline parallel `scan`, and user-owned per-album/track choices in `my-filters.yaml` ([details](ULTIMATE_MODE.md)). |
| 📡 **UPnP renderer & opt-in Tidal Exclusive** | Gapless lossless UPnP AV renderer plus an optional standalone pure-Rust ACX 1.1 virtual render driver (`audiophile-player-driver-v0.1.0-win-x64.zip`) for zero-mixer Tidal Desktop Exclusive Mode ([details](GETTING_STARTED.md#tidal-desktop-exclusive-mode-opt-in)). |
| 🤖 **Control it by asking (MCP)** | Built-in stdio MCP server (`audiophile-player mcp`) lets a free AI agent install, configure and run the player: *"turn on ultimate mode"*, *"use apodizing for this album"*, *"what's playing?"* ([details](QUICK_START.md#controlling-the-player-through-mcp)). |
| 🩺 **Self-diagnosing CLI & generated reference** | `doctor`, `devices`, `config show/set`, `status`, `stop`, `restart` (default: after current track), and `help` — all with `--json` and generated from the same table as [CLI Reference](CLI.md). |
| 🧰 **One-line installer & attested releases** | `bootstrap.ps1` builds from source by default (locked to `Cargo.lock`) with opt-in `-Binary` verified by `SHA256SUMS` and GitHub Build Provenance attestations, shipping app-only and opt-in driver zips separately ([details](QUICK_START.md)). |

---

[← Quick Start (Updating)](QUICK_START.md#updating) · [🏠 Home](../README.md)
