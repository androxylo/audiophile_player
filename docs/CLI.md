<!-- GENERATED FILE: do not edit by hand! Regenerate with:
     cargo run --bin audiophile-player -- help --markdown > docs/CLI.md -->

# CLI & Configuration Reference

[🏠 Home](../README.md) · [🚀 Quick Start](QUICK_START.md) · [🧭 Getting Started](GETTING_STARTED.md) · [💎 Ultimate Mode](ULTIMATE_MODE.md) · [🎛️ Sound Engine](SOUND_ENGINE.md) · **⌨️ CLI**

> Generated directly from `crates/ap-cli/src/cli_spec.rs`. Machine-readable form: `audiophile-player help --json`.

## Commands

| Command | Usage | Summary |
|---|---|---|
| `play` | `audiophile-player [OPTIONS]` | Start headless WDM-KS 8fS playback + localhost HTTP UI + UPnP renderer (default) |
| `scan` | `audiophile-player scan [FOLDER\|ROOT] [OPTIONS]` | Offline parallel, incremental FLAC library scanner (Schema-2 filterdb) |
| `filters` | `audiophile-player filters <show\|diff\|set\|check\|relink\|migrate> [ARGS]` | Inspect, edit, validate, relink, or migrate Schema-2 filter choices |
| `help` | `audiophile-player help [<OPTION\|COMMAND>] [--json \| --markdown]` | Show grouped summary help, option/command detail, JSON spec, or Markdown docs |
| `doctor` | `audiophile-player doctor [--config <PATH>] [--json]` | Check OS, toolchain, DAC 8fS support, config, music folder, ports, and driver status |
| `devices` | `audiophile-player devices [--json]` | List Windows WDM-KS audio render devices and probe 352.8 / 384 kHz 8fS support |
| `config` | `audiophile-player config <show\|set\|unset> [ARGS]` | Inspect effective configuration or edit config.toml safely with .bak backup |
| `status` | `audiophile-player status [--json]` | Query the running player instance via run.json and /api/v1/status |
| `stop` | `audiophile-player stop [--when now\|after-track] [--json]` | Gracefully stop the running player instance and release the DAC |
| `restart` | `audiophile-player restart [OPTIONS...] [--when after-track\|now] [--json]` | Gracefully restart the running player with updated options (default: after-track) |
| `mcp` | `audiophile-player mcp` | Run the thin stdio MCP supervisor server for AI agents (never touches audio hardware) |

## Options

### Output & Hardware

| Option | Config key | Default | Scope | Restart | Summary |
|---|---|---|---|---|---|
| `--device NAME` | `player.device` | `auto (best USB DAC)` | play, restart | restart | WDM-KS DAC name substring (default: auto-select best USB DAC) |
| `--dac-bits N` | `quantizer.dac_bits` | `24` | play, restart | restart | Quantizer DAC bit depth: 16 \| 18 \| 20 \| 24 |
| `--max-rate HZ` | `player.max_rate` | `auto (highest DAC rate)` | play, restart | restart | Cap DAC output sample rate (Hz); main DSP stays locked at 8fS (352.8/384 kHz) |
| `--opt LEVEL` | `player.opt` | `high (extreme in --ultimate)` | play, restart | restart | Windows kernel-streaming priority tier: normal \| high \| extreme |

### DSP, Filters & Room Correction

| Option | Config key | Default | Scope | Restart | Summary |
|---|---|---|---|---|---|
| `--filter NAME` | `player.filter` | `auto` | play, scan, restart | restart | Force upsampling filter globally (or 'auto') |
| `--shaper NAME` | `quantizer.shaper` | `auto` | play, restart | restart | Force 4fS/8fS noise shaper: auto \| bypass \| tpdf \| ns-gentle-2 \| ns-sharp-5 \| ns-smooth-15 \| ns-wide-9 |
| `--max-taps N` | `player.max_taps` | `4000000` | play, restart | restart | Upsampling filter tap ceiling (1..=4000000) |
| `--conv-dir PATH` | `convolution.dir` | — | play, scan, restart | restart | REW stereo WAV folder (auto-picks newest 352.8 / 384 kHz WAV) |
| `--conv-file PATH` | `convolution.file` | — | play, scan, restart | restart | Explicit 2-channel REW stereo WAV file |
| `--no-conv` | — | `false` | play, scan, restart | restart | Disable REW stereo room convolution (bypass) |

### Ultimate Mode & Filter Database

| Option | Config key | Default | Scope | Restart | Summary |
|---|---|---|---|---|---|
| `--ultimate` | `ultimate.enabled` | `false` | play, restart | restart | Enable Ultimate Mode (30 s look-ahead, per-track analysis, exact-M) |
| `--no-ultimate` | — | `false` | play, restart | restart | Force-disable Ultimate Mode even if enabled in config.toml |
| `--no-genre-rules` | `ultimate.genre_rules` | `false` | play, restart | restart | Disable Rule G genre-aware filter defaults in Ultimate Mode |
| `--no-learn-on-play` | `ultimate.learn_on_play` | `false` | play, restart | restart | Disable Schema-2 YAML learn-on-play caching at clean track EOF |
| `--filters MODE` | `filters.mode` | `auto` | play, restart | restart | Filter choice mode: auto \| suggested \| mine |
| `--filterdb-dir DIR` | `filters.dir` | `%APPDATA%\audiophile-player\filterdb` | play, scan, filters, restart | restart | Override Schema-2 machine-managed filterdb directory |
| `--choices-file PATH` | `filters.choices_file` | `%APPDATA%\audiophile-player\my-filters.yaml` | play, scan, filters, restart | next-track | Override user-owned my-filters.yaml path |
| `--trackdb-dir PATH` | `trackdb.dir` | `%APPDATA%\audiophile-player\trackdb` | play, scan, filters, restart | restart | Override legacy Schema-1 YAML trackdb folder |

### Library Scanning & Filter Overrides

| Option | Config key | Default | Scope | Restart | Summary |
|---|---|---|---|---|---|
| `--music-dir PATH` | `player.music_dir` | `~/Music` | play, restart | restart | Root folder containing FLAC subfolders (default: ~/Music) |
| `--report` | — | `false` | scan | n/a | Print one diagnostic line per scanned track |
| `--post-chain` | — | `false` | scan | n/a | Also measure exact whole-track post-chain peak M during scan |
| `--force` | — | `false` | scan | n/a | Re-analyse tracks even if already current in filterdb |
| `-j, --jobs N` | — | `auto (up to 4)` | scan | n/a | Number of parallel folder workers for `scan` (1..=32) |
| `--note TEXT` | — | — | filters | n/a | Optional comment note attached by `filters set` |
| `--apply` | — | `false` | filters | n/a | Apply pending moves in `filters relink` (creates .bak backup) |
| `--sidecar-root DIR` | — | — | filters | n/a | Also import legacy *.ap.yaml sidecars under DIR during `filters migrate` |

### HTTP Control UI & UPnP Renderer

| Option | Config key | Default | Scope | Restart | Summary |
|---|---|---|---|---|---|
| `--port PORT` | `player.port` | `auto (8085..8199)` | play, restart | restart | Localhost HTTP control UI port (default: auto 8085..8199) |
| `--upnp-name NAME` | `upnp.friendly_name` | `Audiophile Player` | play, restart | restart | UPnP AV MediaRenderer display name on LAN |
| `--upnp-port PORT` | `upnp.port` | `auto (49152..49250)` | play, restart | restart | UPnP HTTP/SOAP listener port |
| `--upnp-bind IPV4` | `upnp.bind_ip` | `auto LAN IPv4` | play, restart | restart | Bind and advertise UPnP renderer on a specific local IPv4 |
| `--no-upnp` | `upnp.enabled` | `false` | play, restart | restart | Disable the built-in UPnP AV MediaRenderer |

### Exclusive Stream Capture

| Option | Config key | Default | Scope | Restart | Summary |
|---|---|---|---|---|---|
| `--no-capture` | `capture.enabled` | `false` | play, restart | restart | Disable opt-in exclusive capture watcher (\\.\ApExclusiveAudio) |

### General, Diagnostics & Lifecycle

| Option | Config key | Default | Scope | Restart | Summary |
|---|---|---|---|---|---|
| `--config PATH` | — | `auto-discover` | play, scan, filters, doctor, config, restart | restart | Explicit TOML config file path (default: auto-discover) |
| `--when MODE` | — | `after-track (restart) / now (stop)` | stop, restart | n/a | Shutdown timing for `stop` / `restart`: after-track \| now |
| `--json` | — | `false` | help, doctor, devices, config, status, stop, restart, play | n/a | Emit one machine-readable JSON document to stdout |
| `--markdown` | — | `false` | help | n/a | Generate docs/CLI.md Markdown reference from cli_spec.rs |
| `--effective` | — | `false` | config | n/a | Include default values in `config show` (not just file overrides) |
| `--yes` | — | `false` | config, filters, restart, stop | n/a | Confirm non-interactively without prompting on stdin |
| `--idle` | — | `false` | play | restart | Start idle without opening audio hardware (signal test mode) |
| `-V, --version` | — | `false` | play | n/a | Show version, copyright, and GPL-3.0-or-later notice (supports --json) |
| `-h, --help` | — | `false` | play, scan, filters, help, doctor, devices, config, status, stop, restart, mcp | n/a | Show summary help (or command-specific help) |

## Config-Only Keys (`config.toml`)

These keys exist in `config.toml` without a 1:1 standalone CLI flag:

- `convolution.enabled`
- `convolution.file_352800`
- `convolution.file_384000`

## Exit Codes

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | General runtime error |
| `2` | CLI argument / usage error |
| `3` | Audio device / hardware error (missing DAC, unsupported 8fS, DAC busy) |
| `4` | Configuration file error (`config.toml` syntax or validation) |
| `5` | Process lifecycle state conflict (already running on start, or not running on stop/status) |
