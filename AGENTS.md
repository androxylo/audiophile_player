# Audiophile Player — Agent Operating Guide

This file is the authoritative runbook for AI assistants helping a user install, configure, troubleshoot, or operate **Audiophile Player** on Windows.

## Hard Safety Rules (NEVER violate)

1. **Read-only music library:** NEVER write, modify, rename, tag, or delete files in the user's music folder.
2. **Never run kernel driver scripts unasked:** NEVER install `drivers/ap-exclusive-acx` (`install-driver.ps1`) or modify `bcdedit` / Secure Boot / BitLocker unless the user explicitly asks for Tidal Desktop Exclusive Mode and confirms the security trade-offs in `docs/GETTING_STARTED.md`.
3. **Never touch unrelated system state:** Only edit `%APPDATA%\audiophile-player\config.toml` (via `audiophile-player config set`) or `%APPDATA%\audiophile-player\my-filters.yaml` (via `audiophile-player filters set`).
4. **Confirm before killing playback:** Use `audiophile-player restart --when after-track` (the default) when applying new settings while music is playing; only pass `--when now` or `stop --when now` if the user asks to interrupt the current track immediately.
5. **UAC transparency:** You cannot click Windows UAC elevation prompts. Tell the user clearly before running any step (like `winget` installing VS Build Tools) that may pop a UAC prompt.

## First-Time Installation Flow (Windows 10/11 x64)

1. **Default — build from source** (transparent, verifiable):
   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\bootstrap.ps1 -NonInteractive
   ```
   Or if starting without a local clone:
   ```powershell
   irm https://github.com/androxylo/audiophile_player/releases/latest/download/bootstrap.ps1 | iex
   ```
2. **Opt-in fast path — verified pre-built binary** (no compiler toolchain needed):
   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\bootstrap.ps1 -Binary -NonInteractive
   ```
3. **Verify the environment immediately** using machine-readable JSON:
   ```powershell
   audiophile-player doctor --json
   audiophile-player devices --json
   ```
4. **Configure DAC, music folder, and optional REW room correction**:
   - Inspect discovered DACs from `devices --json`; prefer a DAC where `supports_8fs` is `true` (`rate_352800` and `rate_384000` both `"supported"`). DACs limited to `< 352.8 / 384 kHz` (`supports_any_rate: true`) are also supported automatically via last-stage pure-`f64` downsampling from the locked 8fS pipeline.
   - Persist defaults via:
     ```powershell
     audiophile-player config set device "<DAC substring>"
     audiophile-player config set music_dir "<path to FLAC library>"
     audiophile-player config set ultimate.enabled true
     ```
   - Never guess a DAC name or music path — verify with `devices --json` and ask the user if ambiguous.

## Controlling a Running Player (CLI & MCP)

- **Inspect full CLI contract dynamically:** `audiophile-player help --json` (schema `ap.cli_spec/1`) or `docs/CLI.md`.
- **Prefer the built-in MCP server** when your host supports MCP: `audiophile-player mcp` (stdio JSON-RPC 2.0, configured in `.agents/mcp_config.json` and `.vscode/mcp.json`).
- **Lifecycle CLI commands:**
  - `audiophile-player status --json` — check if running, current track, DAC, filter decision, effective options, and whether a restart is pending.
  - `audiophile-player stop [--when now|after-track]` — cleanly stop the running engine and release the DAC.
  - `audiophile-player restart [--when after-track|now] [-- <play flags>]` — stop cleanly (default: finish current track), merge new flags onto previous options, and start a single detached instance.
- **Per-track filter overrides:**
  - `audiophile-player filters show <path>` / `diff` / `set <path> <filter> [--note "..."]` / `check` / `relink`
- **Offline library scan (read-only):**
  - `audiophile-player scan [folder|root] --post-chain --report` (via MCP, use non-blocking `scan_start` + `scan_progress` + `scan_cancel`).

## Exit Codes

- `0`: success
- `1`: runtime / I/O error
- `2`: invalid CLI arguments (check `hint:` on stderr)
- `3`: audio device missing / busy / does not support 32-bit 352.8 & 384 kHz
- `4`: invalid configuration (`config.toml` or `my-filters.yaml`)
- `5`: instance state error (already running on play, or not running on `status`/`stop`)

## Key Documentation

- `docs/CLI.md` — complete auto-generated CLI & config reference (single source of truth)
- `docs/GETTING_STARTED.md` — installation, first play, UPnP, REW convolution, opt-in Tidal driver
- `docs/ULTIMATE_MODE.md` — 30 s look-ahead, detectors, exact headroom, Schema-2 Filter DB
- `docs/SOUND_ENGINE.md` — upsampling filters, noise shapers, DAC bit depth, WDM-KS pipeline
- `docs/FILTER_PREFERENCES.md` — community listening notes and filter preset comparisons
