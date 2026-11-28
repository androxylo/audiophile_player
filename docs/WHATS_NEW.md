# ✨ What's New

[🏠 Home](../README.md) · [🚀 Quick Start](QUICK_START.md) · [🧭 Getting Started](GETTING_STARTED.md) · [💎 Ultimate Mode](ULTIMATE_MODE.md) · [🎛️ Sound Engine](SOUND_ENGINE.md) · [⌨️ CLI](CLI.md)

Release highlights for Audiophile Player. For upgrade instructions, see [Quick Start → Updating](QUICK_START.md#updating).

## v0.2.0 — WASAPI Exclusive Default, Timing Hardening & Pipeline Verification

| | |
|---|---|
| 🔌 **WASAPI Exclusive default (`--output wasapi`)** | Zero-dependency, event-driven WASAPI Exclusive (`AUDCLNT_SHAREMODE_EXCLUSIVE`) is now the default Windows output transport — bit-perfect with zero mixer/APO/SRC and automatic format negotiation (`Int32`/`Int24`/`Int16` + channel-mask fallback). Polling WASAPI Push (`--output wasapi-push`) and WDM-KS WaveRT (`--output wdm-ks`) remain available as fallbacks. |
| ⏱️ **WaveRT timing simulator, S2/S5/S6 fixes & observability** | Added a deterministic causal DMA simulator (`FakeWaveRtDevice`) covering coalesced/spurious wakes, partial underruns, 32-bit monotonic cursor wraparound, and ±600 ppm clock drift; fixed WDM-KS clock-rescue threshold, partial-half accounting, and 32-bit rollover handling; exposed timing counters & wake-interval histogram via `GET /api/v1/status` (`last_playback_stats`) and `--trace-wavert`. |
| 🧪 **End-to-end DSP verification suite (`ap-verify` / `ap-null`)** | Added the `ap-verify` crate and `ap-null` loopback null verifier (Layers A–D), L1–L12 structural invariants (chunk independence, gapless seams, linearity, anti-aliasing, determinism, engine queue/pause-resume), and automated push + nightly CI verification workflows. |
| 🎵 **FLAC zero-residual decode fix** | Added `SafeFlacSampleReader` to cleanly decode valid FLAC subframes whose first Rice partition has zero residual samples (working around an upstream `flac-codec 1.3.3` panic) and tightened Ultimate Mode DAC headroom margin + quantizer overs reporting. |
| 🎛️ **Room-FIR DSP building blocks (`ap-dsp`)** | Added stereo WAV writer, mono WAV loader, gain metrics, and generalized cepstrum helpers with full unit coverage. |

## v0.1.0 — Initial Public Release

| | |
|---|---|
| 🎚️ **Pure-f64 8fS DSP engine** | Locks upsampling (Gaussian, poly-sinc, apodizing, minimum/intermediate phase, up to 4M taps), 50/15 µs de-emphasis, and REW stereo room convolution (up to 4M-tap IRs) at 352.8 / 384 kHz in 64-bit float, with a single noise-shaped quantizer (`tpdf`, `ns-gentle-2`, `ns-sharp-5`, `ns-wide-9`, `ns-smooth-15`) and WDM-KS WaveRT exclusive output. |
| 🔽 **DACs below 352.8 / 384 kHz supported** | 192, 176.4, 96, 88.2, 48 and 44.1 kHz DACs (32-, 24- or 16-bit containers) work automatically via a last-stage pure-f64 downsampler ([how it works](SOUND_ENGINE.md#lower-rate-dacs-below-8fs)). 176.4 / 192 kHz outputs get proper 4fS noise shapers; `--max-rate` caps the output rate by hand. |
| 💎 **Ultimate Mode & Schema-2 Filter DB** | 30-second look-ahead recording analysis (fake hi-res, pre-ringing, clipped/loud masters), exact post-chain headroom, offline parallel `scan`, and user-owned per-album/track choices in `my-filters.yaml` ([details](ULTIMATE_MODE.md)). |
| 📡 **UPnP renderer & opt-in Tidal Exclusive** | Gapless lossless UPnP AV renderer plus an optional standalone pure-Rust ACX 1.1 virtual render driver (`audiophile-player-driver-v0.1.0-win-x64.zip`) for zero-mixer Tidal Desktop Exclusive Mode ([details](GETTING_STARTED.md#tidal-desktop-exclusive-mode-opt-in)). |
| 🤖 **Control it by asking (MCP)** | Built-in stdio MCP server (`audiophile-player mcp`) lets a free AI agent install, configure and run the player: *"turn on ultimate mode"*, *"use apodizing for this album"*, *"what's playing?"* ([details](QUICK_START.md#controlling-the-player-through-mcp)). |
| 🩺 **Self-diagnosing CLI & generated reference** | `doctor`, `devices`, `config show/set`, `status`, `stop`, `restart` (default: after current track), and `help` — all with `--json` and generated from the same table as [CLI Reference](CLI.md). |
| 🧰 **One-line installer & attested releases** | `bootstrap.ps1` builds from source by default (locked to `Cargo.lock`) with opt-in `-Binary` verified by `SHA256SUMS` and GitHub Build Provenance attestations, shipping app-only and opt-in driver zips separately ([details](QUICK_START.md)). |

---

[← Quick Start (Updating)](QUICK_START.md#updating) · [🏠 Home](../README.md)
