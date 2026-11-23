# Audiophile Player

An extreme, headless, zero-compromise audiophile playback engine for Windows, targeting sound quality at or above HQPlayer (without a desktop GUI) and exceeding Audirvana.

## Quick Start (Windows)

```powershell
# 1. Build optimized release binary
cargo build --release --bin audiophile-player

# 2. (Optional) Configure REW stereo convolution
#    Copy audiophile-player.example.toml -> %APPDATA%\audiophile-player\config.toml

# 3. Install Desktop & Start Menu "Audiophile Player" icon (starts player + opens Web UI)
powershell -ExecutionPolicy Bypass -File scripts\install-shortcuts.ps1

# Or run directly from terminal (zero args = random FLACs from ~/Music via WDM-KS + Web UI at :8085)
.\target\release\audiophile-player.exe
```

## Project Structure

```text
audiophile_player/
├── Cargo.toml                  # Rust production workspace root (architectural outline)
├── README.md
├── REQUIREMENTS.md             # Full project specification & constraints
├── crates/                     # Production Rust workspace (not compiled yet; ported from proven POCs)
│   ├── ap-core/                # Pure f64 buffers, 352.8/384 kHz targets, <= 0 dB headroom policy, config
│   ├── ap-source/              # Local FLAC RAM decoder, recursive shuffle, Milestone 2 Tidal trait
│   ├── ap-dsp/                 # Pure f64 4M-tap upsampling, core REW convolution, delay, 8fS quantization
│   ├── ap-output-win/          # Windows-only zero-mixer output (WDM-KS & WASAPI Exclusive ONLY)
│   ├── ap-control/             # Rudimentary local HTTP server (file/folder, Play/Stop, GitHub manual link)
│   └── ap-cli/                 # Headless binary wiring source -> DSP -> zero-mixer output + HTTP control
├── docs/
│   └── research/               # Phase 0 research documents & open questions
├── poc/                        # Self-contained prototypes (Python / C++) before Rust port
│   └── python/                 # Working zero-mixer WDM-KS/WASAPI Exclusive POC & CC0 synthesizer
└── tests/                      # Automated TDD test suite (zero copyrighted audio)
    ├── fixtures/audio/         # Original algorithmically synthesized CC0 FLAC fixtures
    └── python/
```

## Research & Specification Documents

- [REQUIREMENTS.md](REQUIREMENTS.md) — Full project specification, explicit compromises, `352.8 / 384 kHz` DSP pipeline constraints, rudimentary local HTTP control page, and milestones.
- [docs/research/INVESTIGATION_PLAN.md](docs/research/INVESTIGATION_PLAN.md) — Ordered Phase 0 investigation plan, resource-scanning scope, and browser-PDF handoff protocol.
- [docs/research/OPEN_QUESTIONS.md](docs/research/OPEN_QUESTIONS.md) — Tracked open technical, mathematical, hardware, and architectural questions.
- [docs/research/00-landscape-and-sources.md](docs/research/00-landscape-and-sources.md) — Landscape survey of existing audiophile players, best practices, and curated reference sources.
- [docs/research/02-audiophile-filter-survey.md](docs/research/02-audiophile-filter-survey.md) — Upsampling & noise-shaping algorithm sources, audiophile consensus, and list of inaccessible sources for user snippets.
- [docs/research/06-windows-kernel-streaming.md](docs/research/06-windows-kernel-streaming.md) — WASAPI Exclusive vs. Audirvana-style WDM-KS (`usbaudio2.sys` / WaveRT), bit-perfect verification, and minimal-effort zero-mixer POC design.
- [poc/README.md](poc/README.md) — Prototype setup, hardware probe, and test-tone/FLAC verification commands.

## License

This project is licensed under the **GNU Lesser General Public License v3.0 or later (`LGPL-3.0-or-later`)**.

See [LICENSE](LICENSE) (GNU LGPL v3.0) and [COPYING](COPYING) (GNU GPL v3.0, incorporated by reference) for full terms.
