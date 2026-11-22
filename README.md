# Audiophile Player

An extreme, headless, zero-compromise audiophile playback engine for Windows, targeting sound quality at or above HQPlayer (without a desktop GUI) and exceeding Audirvana.

## Project Structure

```text
audiophile_player/
├── README.md
├── REQUIREMENTS.md             # Full project specification & constraints
├── docs/
│   └── research/               # Phase 0 research documents & open questions
├── poc/                        # Self-contained prototypes (Python / C++) before Rust port
│   └── python/                 # Zero-mixer Windows transport & DSP prototypes
└── tests/                      # Automated TDD test suite (zero copyrighted audio)
    └── python/
```

## Research & Specification Documents

- [REQUIREMENTS.md](REQUIREMENTS.md) — Full project specification, explicit compromises, `352.8 / 384 kHz` DSP pipeline constraints, rudimentary local HTTP control page, and milestones.
- [docs/research/INVESTIGATION_PLAN.md](docs/research/INVESTIGATION_PLAN.md) — Ordered Phase 0 investigation plan, resource-scanning scope, and browser-PDF handoff protocol.
- [docs/research/OPEN_QUESTIONS.md](docs/research/OPEN_QUESTIONS.md) — Tracked open technical, mathematical, hardware, and architectural questions.
- [docs/research/00-landscape-and-sources.md](docs/research/00-landscape-and-sources.md) — Landscape survey of existing audiophile players, best practices, and curated reference sources.
- [docs/research/02-audiophile-filter-survey.md](docs/research/02-audiophile-filter-survey.md) — Upsampling & noise-shaping algorithm sources, audiophile consensus, and list of inaccessible sources for user snippets.
- [docs/research/06-windows-kernel-streaming.md](docs/research/06-windows-kernel-streaming.md) — WASAPI Exclusive vs. Audirvana-style WDM-KS (`usbaudio2.sys` / WaveRT), bit-perfect verification, and minimal-effort zero-mixer POC design.
- [poc/README.md](poc/README.md) — Prototype setup, hardware probe, and test-tone verification commands.
