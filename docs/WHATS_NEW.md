# ✨ What's New

[🏠 Home](../README.md) · [🚀 Quick Start](QUICK_START.md) · [🧭 Getting Started](GETTING_STARTED.md) · [💎 Ultimate Mode](ULTIMATE_MODE.md) · [🎛️ Sound Engine](SOUND_ENGINE.md) · [⌨️ CLI](CLI.md)

Recent notable additions and changes in Audiophile Player. For upgrade instructions, see [Quick Start → Updating](QUICK_START.md#updating).

| | |
|---|---|
| 🔽 **DACs below 352.8 / 384 kHz now work** | 192, 176.4, 96, 88.2, 48 and 44.1 kHz DACs, with 32-, 24- or 16-bit containers. The 8fS chain is untouched; a pure-f64 downsampler runs only at the very end ([how it works](SOUND_ENGINE.md#lower-rate-dacs-below-8fs)). 176.4 / 192 kHz outputs get proper 4fS noise shapers. `--max-rate` caps the output rate by hand. |
| 🤖 **Control it by asking** | A built-in MCP server (`audiophile-player mcp`) lets a free AI agent install, configure and run the player: *"turn on ultimate mode"*, *"use apodizing for this album"*, *"what's playing?"* ([details](QUICK_START.md#controlling-the-player-through-mcp)). |
| 🩺 **Self-diagnosing** | `doctor`, `devices`, `config show/set`, `status`, `stop` and `restart`, each with `--json`. Restarts wait for the current track to finish by default. |
| 📖 **Help that can't go out of date** | `help`, `help <option>` and `help --json` are generated from the same table as the parser, and so is [CLI Reference](CLI.md). |
| 🧰 **One-line installer** | `bootstrap.ps1` builds from source by default, with an optional verified prebuilt binary ([details](QUICK_START.md)). |

---

[← Quick Start (Updating)](QUICK_START.md#updating) · [🏠 Home](../README.md)
