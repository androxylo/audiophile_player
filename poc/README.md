# Prototypes & Proof-of-Concepts (`poc/`)

Self-contained prototypes built to validate hardware transport, DSP math, and verification harnesses before porting proven designs to **Rust**.

> **Zero-Mixer Rule:** Every POC in this directory enforces strict exclusive / kernel-streaming output. Windows Shared Mode / OS mixer fallback is forbidden even in the earliest prototype.

## Subprojects

- [`poc/python/`](python/) — Python POC package (`audiophile_poc`):
  - Strictly mixer-free Windows output (**WDM-KS / Kernel Streaming** and **WASAPI Exclusive** only).
  - Hardware capability probe (`--probe`) for Schiit Gungnir 2 / `usbaudio2.sys` (`44.1 → 384 kHz`).
  - Copyright-free deterministic synthetic test tone generator (safe `-24 dBFS` with smooth cosine fade-in/fade-out).

## Quick Setup & Run (Windows PowerShell)

```powershell
# 1. Create isolated local virtual environment (gitignored)
python -m venv .venv
.\.venv\Scripts\pip install -e .\poc\python[dev]

# 2. Run TDD test suite
.\.venv\Scripts\pytest tests/python

# 3. Probe WDM-KS and WASAPI Exclusive output devices & supported sample rates
.\.venv\Scripts\python -m audiophile_poc probe

# 4. Play a gentle -24 dBFS 440 Hz test tone via Kernel Streaming (WDM-KS)
.\.venv\Scripts\python -m audiophile_poc test-tone --mode wdm-ks --device <ID> --rate 44100
```
