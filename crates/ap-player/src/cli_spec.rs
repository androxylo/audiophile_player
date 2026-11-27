// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Single-source-of-truth declarative CLI specification (`QUICK_START_AI_DESIGN.md` §4).
//!
//! Every user- and AI-facing view of the CLI is generated from [`COMMANDS`] and [`OPTIONS`]:
//! - `audiophile-player help` (grouped one-line summary)
//! - `audiophile-player help <option|command>` (detailed reference page)
//! - `audiophile-player help --json` (`schema: "ap.cli_spec/1"`, machine spec for agents & MCP)
//! - `audiophile-player help --markdown` (`docs/CLI.md`, enforced byte-for-byte by `cli_docs_in_sync`)
//! - Did-you-mean typo suggestions in `cli_args.rs`
//! - Config key bijection checks against `ap_core::config::ALL_KNOWN_CONFIG_KEYS`
//! - MCP `player_start` / `player_restart` option schemas

use ap_core::json::JsonValue;

pub const MANUAL_URL: &str = "https://github.com/androxylo/audiophile_player";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    Play,
    Scan,
    Filters,
    Help,
    Doctor,
    Devices,
    Config,
    Status,
    Stop,
    Restart,
    Mcp,
}

impl Cmd {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Play => "play",
            Self::Scan => "scan",
            Self::Filters => "filters",
            Self::Help => "help",
            Self::Doctor => "doctor",
            Self::Devices => "devices",
            Self::Config => "config",
            Self::Status => "status",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Mcp => "mcp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Output,
    Dsp,
    Ultimate,
    Library,
    Network,
    Capture,
    General,
}

impl Group {
    pub const ALL: [Self; 7] = [
        Self::Output,
        Self::Dsp,
        Self::Ultimate,
        Self::Library,
        Self::Network,
        Self::Capture,
        Self::General,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Output => "OUTPUT",
            Self::Dsp => "DSP",
            Self::Ultimate => "ULTIMATE",
            Self::Library => "LIBRARY",
            Self::Network => "NETWORK",
            Self::Capture => "CAPTURE",
            Self::General => "GENERAL",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Output => "Output & Hardware",
            Self::Dsp => "DSP, Filters & Room Correction",
            Self::Ultimate => "Ultimate Mode & Filter Database",
            Self::Library => "Library Scanning & Filter Overrides",
            Self::Network => "HTTP Control UI & UPnP Renderer",
            Self::Capture => "Exclusive Stream Capture",
            Self::General => "General, Diagnostics & Lifecycle",
        }
    }

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Output => "output",
            Self::Dsp => "dsp",
            Self::Ultimate => "ultimate",
            Self::Library => "library",
            Self::Network => "network",
            Self::Capture => "capture",
            Self::General => "general",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Always,
    NextTrack,
    NotApplicable,
}

impl Restart {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Always => "restart",
            Self::NextTrack => "next-track",
            Self::NotApplicable => "n/a",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Always => "needs restart",
            Self::NextTrack => "re-read on next track (no restart)",
            Self::NotApplicable => "command-only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Flag,
    Str(&'static str),
    Path(&'static str),
    Int {
        placeholder: &'static str,
        min: i64,
        max: i64,
    },
    Enum {
        placeholder: &'static str,
        values: &'static [&'static str],
    },
}

impl ValueKind {
    pub const fn placeholder(self) -> Option<&'static str> {
        match self {
            Self::Flag => None,
            Self::Str(p) | Self::Path(p) => Some(p),
            Self::Int { placeholder, .. } | Self::Enum { placeholder, .. } => Some(placeholder),
        }
    }

    pub const fn json_type(self) -> &'static str {
        match self {
            Self::Flag => "boolean",
            Self::Str(_) | Self::Path(_) | Self::Enum { .. } => "string",
            Self::Int { .. } => "integer",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OptSpec {
    pub name: &'static str,
    pub short: Option<char>,
    pub value: ValueKind,
    pub default: Option<&'static str>,
    pub config_key: Option<&'static str>,
    pub scope: &'static [Cmd],
    pub group: Group,
    pub restart: Restart,
    pub summary: &'static str,
    pub details: &'static str,
    pub examples: &'static [&'static str],
    pub since: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct CmdSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub details: &'static str,
    pub examples: &'static [&'static str],
    pub since: &'static str,
}

/// Config keys that live exclusively in `config.toml` (not directly mapped 1:1 to a single CLI flag).
pub const CONFIG_ONLY_KEYS: &[&str] = &[
    "convolution.enabled",
    "convolution.file_352800",
    "convolution.file_384000",
];

pub const FILTER_VALUES: &[&str] = &[
    "auto",
    "null",
    "simple-test",
    "gauss-long-lp",
    "gauss-hires-lp",
    "gauss-hires-mp",
    "gauss-hires-ip",
    "gauss-apod-4m",
    "sinc-steep-4m",
    "ext-sharp-lp",
    "minphase-long",
    "intermediate-long",
    "short-lp",
    "short-mp",
];

pub const SHAPER_VALUES: &[&str] = &[
    "auto",
    "bypass",
    "tpdf",
    "ns-gentle-2",
    "ns-sharp-5",
    "ns-smooth-15",
    "ns-wide-9",
];

pub const OPT_VALUES: &[&str] = &["normal", "high", "extreme"];
pub const MAX_RATE_VALUES: &[&str] = &[
    "auto", "44100", "48000", "88200", "96000", "176400", "192000", "352800", "384000",
];
pub const FILTER_MODE_VALUES: &[&str] = &["auto", "suggested", "mine"];
pub const WHEN_VALUES: &[&str] = &["after-track", "now"];

pub static COMMANDS: &[CmdSpec] = &[
    CmdSpec {
        name: "play",
        usage: "audiophile-player [OPTIONS]",
        summary: "Start headless WDM-KS 8fS playback + localhost HTTP UI + UPnP renderer (default)",
        details: "Default mode when invoked with no subcommand. Immediately opens the best 352.8/384 kHz USB DAC via direct Windows Kernel Streaming (WDM-KS WaveRT, zero Windows mixer), plays random FLAC files from the configured music folder, starts the localhost web control UI (auto port 8085..8199), and announces the built-in UPnP AV MediaRenderer.",
        examples: &[
            "audiophile-player",
            "audiophile-player --ultimate --filters mine",
            "audiophile-player --device \"Gungnir 2\" --music-dir D:\\Music",
        ],
        since: "0.1.0",
    },
    CmdSpec {
        name: "scan",
        usage: "audiophile-player scan [FOLDER|ROOT] [OPTIONS]",
        summary: "Offline parallel, incremental FLAC library scanner (Schema-2 filterdb)",
        details: "Strictly read-only on your music files. Analyses each FLAC track in parallel (sharded one album folder per worker), verifies STREAMINFO.md5, detects metadata-only retags and file moves, writes human-readable album YAML under %APPDATA%\\audiophile-player\\filterdb\\, and records any moved my-filters.yaml entries in relink-report.yaml. Omit FOLDER to scan all roots in [filters.roots] / library.yaml.",
        examples: &[
            "audiophile-player scan D:\\Music --post-chain --report",
            "audiophile-player scan main --jobs 4",
        ],
        since: "0.1.0",
    },
    CmdSpec {
        name: "filters",
        usage: "audiophile-player filters <show|diff|set|check|relink|migrate> [ARGS]",
        summary: "Inspect, edit, validate, relink, or migrate Schema-2 filter choices",
        details: "Manages your user-owned my-filters.yaml and inspects machine suggestions from filterdb/.\nSubcommands:\n  show <PATH>               Show active (mine) vs suggested filter, rule, and why\n  diff                      List all scanned tracks where mine differs from suggested\n  set <PATH> <FILTER>       Set a track or folder choice (preserves comments + .bak)\n  check                     Lint my-filters.yaml against library.yaml\n  relink [--apply]          Preview or apply moved-file rewrites from relink-report.yaml\n  migrate                   Non-destructively import legacy Schema-1 trackdb/ entries",
        examples: &[
            "audiophile-player filters show D:\\Music\\Jazz",
            "audiophile-player filters set D:\\Music\\Jazz gauss-long-lp --note \"warmer\"",
            "audiophile-player filters diff",
            "audiophile-player filters check",
            "audiophile-player filters relink --apply",
            "audiophile-player filters migrate",
        ],
        since: "0.1.0",
    },
    CmdSpec {
        name: "help",
        usage: "audiophile-player help [<OPTION|COMMAND>] [--json | --markdown]",
        summary: "Show grouped summary help, option/command detail, JSON spec, or Markdown docs",
        details: "Single source of truth generated directly from cli_spec.rs. Use `help <option>` for full details on any flag, `help --json` for machine-readable agent/MCP introspection, or `help --markdown` to regenerate docs/CLI.md.",
        examples: &[
            "audiophile-player help",
            "audiophile-player help --filters",
            "audiophile-player help scan",
            "audiophile-player help --json",
        ],
        since: "0.2.0",
    },
    CmdSpec {
        name: "doctor",
        usage: "audiophile-player doctor [--config <PATH>] [--json]",
        summary: "Check OS, toolchain, DAC 8fS support, config, music folder, ports, and driver status",
        details: "Runs non-destructive diagnostic checks and returns structured {ok, problem, fix} items. Reports opt-in ACX driver and Windows Test Mode state read-only without ever modifying system settings.",
        examples: &[
            "audiophile-player doctor",
            "audiophile-player doctor --json",
        ],
        since: "0.2.0",
    },
    CmdSpec {
        name: "devices",
        usage: "audiophile-player devices [--json]",
        summary: "List Windows WDM-KS audio render devices and probe 352.8 / 384 kHz 8fS support",
        details: "Enumerates present WDM-KS WaveRT audio render devices, probes 32-bit 352.8 kHz and 384 kHz support cleanly (reporting busy if another exclusive player holds the DAC), and marks which DAC auto-selection would choose.",
        examples: &[
            "audiophile-player devices",
            "audiophile-player devices --json",
        ],
        since: "0.2.0",
    },
    CmdSpec {
        name: "config",
        usage: "audiophile-player config <show|set|unset> [ARGS]",
        summary: "Inspect effective configuration or edit config.toml safely with .bak backup",
        details: "Subcommands:\n  show [--effective] [--json]  Show resolved config values and their source (default / file / cli)\n  set <KEY> <VALUE> [--json]   Validate and update a key in config.toml (preserves comments + .bak)\n  unset <KEY> [--json]         Remove a key from config.toml (preserves comments + .bak)",
        examples: &[
            "audiophile-player config show",
            "audiophile-player config show --json",
            "audiophile-player config set device \"Gungnir 2\"",
            "audiophile-player config set music_dir D:\\Music",
            "audiophile-player config unset player.filter",
        ],
        since: "0.2.0",
    },
    CmdSpec {
        name: "status",
        usage: "audiophile-player status [--json]",
        summary: "Query the running player instance via run.json and /api/v1/status",
        details: "Reads %LOCALAPPDATA%\\audiophile-player\\run.json and fetches live status (PID, HTTP URL, mode, active options, currently playing track, DAC state). Exits with code 5 when no instance is running.",
        examples: &[
            "audiophile-player status",
            "audiophile-player status --json",
        ],
        since: "0.2.0",
    },
    CmdSpec {
        name: "stop",
        usage: "audiophile-player stop [--when now|after-track] [--json]",
        summary: "Gracefully stop the running player instance and release the DAC",
        details: "Sends an authenticated shutdown request to /api/v1/shutdown (with named-event fallback) so the engine drains cleanly, sends UPnP byebye, releases the WDM-KS DAC pin, and exits. Use `--when after-track` to wait for the current song to finish first. Exits with code 5 if not running.",
        examples: &[
            "audiophile-player stop",
            "audiophile-player stop --when after-track",
            "audiophile-player stop --json",
        ],
        since: "0.2.0",
    },
    CmdSpec {
        name: "restart",
        usage: "audiophile-player restart [OPTIONS...] [--when after-track|now] [--json]",
        summary: "Gracefully restart the running player with updated options (default: after-track)",
        details: "Merges any newly supplied play options onto the running instance's recorded argv, requests graceful shutdown (defaulting to `--when after-track` so music is not cut mid-song), waits for the old process to release the DAC, spawns the new detached instance, waits until ready, and resumes active playback.",
        examples: &[
            "audiophile-player restart --ultimate --filters mine",
            "audiophile-player restart --when now --no-conv",
            "audiophile-player restart --json",
        ],
        since: "0.2.0",
    },
    CmdSpec {
        name: "mcp",
        usage: "audiophile-player mcp",
        summary: "Run the thin stdio MCP supervisor server for AI agents (never touches audio hardware)",
        details: "Exposes Audiophile Player's tools, resources (ap://cli/spec, ap://status, ap://docs/*), and prompts over JSON-RPC 2.0 stdio. Spawns and manages the detached player process via run.json and /api/v1/*, enforces strict safety limits (read-only music library, no shell/driver mutations), and logs every mutating call to mcp-audit.log.",
        examples: &["audiophile-player mcp"],
        since: "0.2.0",
    },
];

pub static OPTIONS: &[OptSpec] = &[
    // --- OUTPUT ---
    OptSpec {
        name: "--device",
        short: None,
        value: ValueKind::Str("NAME"),
        default: Some("auto (best USB DAC)"),
        config_key: Some("player.device"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Output,
        restart: Restart::Always,
        summary: "WDM-KS DAC name substring (default: auto-select best USB DAC)",
        details: "Case-insensitive substring matched against present WDM-KS WaveRT audio render devices. When omitted, prefers Schiit / Unison / USB Audio Class 2.0 DACs automatically.",
        examples: &["audiophile-player --device \"Gungnir 2\""],
        since: "0.1.0",
    },
    OptSpec {
        name: "--dac-bits",
        short: None,
        value: ValueKind::Int {
            placeholder: "N",
            min: 16,
            max: 24,
        },
        default: Some("24"),
        config_key: Some("quantizer.dac_bits"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Output,
        restart: Restart::Always,
        summary: "Quantizer DAC bit depth: 16 | 18 | 20 | 24",
        details: "Target quantization resolution (automatically capped to the DAC's wire bit depth so quantization still occurs strictly once). Allowed values are strictly 16, 18, 20, or 24. Leave at 24 unless matching a specific R-2R NOS DAC datasheet.",
        examples: &["audiophile-player --dac-bits 20"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--max-rate",
        short: None,
        value: ValueKind::Enum {
            placeholder: "HZ",
            values: MAX_RATE_VALUES,
        },
        default: Some("auto (highest DAC rate)"),
        config_key: Some("player.max_rate"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Output,
        restart: Restart::Always,
        summary: "Cap DAC output sample rate (Hz); main DSP stays locked at 8fS (352.8/384 kHz)",
        details: "By default (`auto`), Audiophile Player probes your DAC and picks its highest supported rate in each clock family (preferring native 352.8 / 384 kHz with zero downsampling). On DACs below 352.8/384 kHz — or when capped with `--max-rate` — the entire main DSP chain (upsampling, de-emphasis, REW room convolution) still runs at locked 8fS in pure f64, and a linear-phase Kaiser anti-aliasing downsampler runs strictly at the very last stage right before the single quantizer.",
        examples: &[
            "audiophile-player --max-rate 192000",
            "audiophile-player --max-rate 96000",
            "audiophile-player --max-rate auto",
        ],
        since: "0.2.0",
    },
    OptSpec {
        name: "--opt",
        short: None,
        value: ValueKind::Enum {
            placeholder: "LEVEL",
            values: OPT_VALUES,
        },
        default: Some("high (extreme in --ultimate)"),
        config_key: Some("player.opt"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Output,
        restart: Restart::Always,
        summary: "Windows kernel-streaming priority tier: normal | high | extreme",
        details: "Controls process priority, MMCSS Pro Audio registration, and timer resolution for the real-time WaveRT output thread. Ultimate Mode automatically promotes the default to `extreme` unless explicitly overridden.",
        examples: &["audiophile-player --opt extreme"],
        since: "0.1.0",
    },
    // --- DSP ---
    OptSpec {
        name: "--filter",
        short: None,
        value: ValueKind::Enum {
            placeholder: "NAME",
            values: FILTER_VALUES,
        },
        default: Some("auto"),
        config_key: Some("player.filter"),
        scope: &[Cmd::Play, Cmd::Scan, Cmd::Restart],
        group: Group::Dsp,
        restart: Restart::Always,
        summary: "Force upsampling filter globally (or 'auto')",
        details: "Overrides automatic per-track filter selection for every track. Use `auto` to select gauss-long-lp for 1x, gauss-hires-lp for hi-res, and null for native 8fS (or per-track analysis in Ultimate Mode). For per-album/track overrides without forcing globally, use `my-filters.yaml` and `--filters mine`.",
        examples: &[
            "audiophile-player --filter gauss-long-lp",
            "audiophile-player --filter auto",
        ],
        since: "0.1.0",
    },
    OptSpec {
        name: "--shaper",
        short: None,
        value: ValueKind::Enum {
            placeholder: "NAME",
            values: SHAPER_VALUES,
        },
        default: Some("auto"),
        config_key: Some("quantizer.shaper"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Dsp,
        restart: Restart::Always,
        summary: "Force 4fS/8fS noise shaper: auto | bypass | tpdf | ns-gentle-2 | ns-sharp-5 | ns-smooth-15 | ns-wide-9",
        details: "Selects the 4fS/8fS noise-shaping / dither profile applied during final f64 -> Int32 quantization. `auto` selects `ns-gentle-2` at 4fS/8fS, bit-perfect `bypass` for untouched native 32-bit 8fS, and `tpdf` at <= 2fS rates.",
        examples: &["audiophile-player --shaper ns-smooth-15"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--max-taps",
        short: None,
        value: ValueKind::Int {
            placeholder: "N",
            min: 1,
            max: 4_000_000,
        },
        default: Some("4000000"),
        config_key: Some("player.max_taps"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Dsp,
        restart: Restart::Always,
        summary: "Upsampling filter tap ceiling (1..=4000000)",
        details: "Hard upper bound on prototype FIR upsampler tap count. Lowering this cap causes auto-mode filters whose natural length exceeds the cap to fall back cleanly to bounded taps.",
        examples: &["audiophile-player --max-taps 2000000"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--conv-dir",
        short: None,
        value: ValueKind::Path("PATH"),
        default: None,
        config_key: Some("convolution.dir"),
        scope: &[Cmd::Play, Cmd::Scan, Cmd::Restart],
        group: Group::Dsp,
        restart: Restart::Always,
        summary: "REW stereo WAV folder (auto-picks newest 352.8 / 384 kHz WAV)",
        details: "Overrides [convolution] in config.toml to auto-select the most recently modified 2-channel .wav file in PATH matching the active 8fS output rate (352,800 or 384,000 Hz).",
        examples: &["audiophile-player --conv-dir \"C:\\REW\\Filters\""],
        since: "0.1.0",
    },
    OptSpec {
        name: "--conv-file",
        short: None,
        value: ValueKind::Path("PATH"),
        default: None,
        config_key: Some("convolution.file"),
        scope: &[Cmd::Play, Cmd::Scan, Cmd::Restart],
        group: Group::Dsp,
        restart: Restart::Always,
        summary: "Explicit 2-channel REW stereo WAV file",
        details: "Overrides [convolution] in config.toml to load one explicit 2-channel REW impulse response WAV file (active when its sample rate matches the 8fS output rate).",
        examples: &["audiophile-player --conv-file C:\\REW\\room-384k.wav"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--no-conv",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Play, Cmd::Scan, Cmd::Restart],
        group: Group::Dsp,
        restart: Restart::Always,
        summary: "Disable REW stereo room convolution (bypass)",
        details: "Temporarily bypasses REW stereo convolution even if enabled in config.toml. Ideal for quick A/B comparison.",
        examples: &["audiophile-player --no-conv"],
        since: "0.1.0",
    },
    // --- ULTIMATE ---
    OptSpec {
        name: "--ultimate",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: Some("ultimate.enabled"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::Always,
        summary: "Enable Ultimate Mode (30 s look-ahead, per-track analysis, exact-M)",
        details: "Enables 30-second bounded look-ahead analysis (detectors H/A/L/B + true peak), full-tail REW convolution, exact post-chain whole-track fixed gain G, VirtualLock RAM pinning, mega-tap local filters, and learn-on-play Schema-2 YAML caching.",
        examples: &["audiophile-player --ultimate"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--no-ultimate",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::Always,
        summary: "Force-disable Ultimate Mode even if enabled in config.toml",
        details: "Overrides `[ultimate] enabled = true` in config.toml to run in standard fast-start playback mode.",
        examples: &["audiophile-player --no-ultimate"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--no-genre-rules",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: Some("ultimate.genre_rules"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::Always,
        summary: "Disable Rule G genre-aware filter defaults in Ultimate Mode",
        details: "Turns off GENRE-tag filter defaults (Classical/Jazz/Rock) while keeping Rules A (apodizing), L (loud & clipped), H (fake hi-res), and P (de-emphasis) active.",
        examples: &["audiophile-player --ultimate --no-genre-rules"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--no-learn-on-play",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: Some("ultimate.learn_on_play"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::Always,
        summary: "Disable Schema-2 YAML learn-on-play caching at clean track EOF",
        details: "Prevents Ultimate Mode from writing verified whole-track analysis and post-chain peaks into filterdb/ at track completion.",
        examples: &["audiophile-player --ultimate --no-learn-on-play"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--filters",
        short: None,
        value: ValueKind::Enum {
            placeholder: "MODE",
            values: FILTER_MODE_VALUES,
        },
        default: Some("auto"),
        config_key: Some("filters.mode"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::Always,
        summary: "Filter choice mode: auto | suggested | mine",
        details: "Chooses which per-file filter decision wins in Ultimate Mode:\n  auto       mine if my-filters.yaml exists, else suggested\n  suggested  pure analyser suggestion; ignores my-filters.yaml and AP_* overrides (A/B mode)\n  mine       my-filters.yaml (track > folder) > AP_* FLAC tags > defaults > suggested\nNote: edits to my-filters.yaml itself are hot-reloaded on every track change without restart.",
        examples: &[
            "audiophile-player --ultimate --filters suggested",
            "audiophile-player --ultimate --filters mine",
        ],
        since: "0.1.0",
    },
    OptSpec {
        name: "--filterdb-dir",
        short: None,
        value: ValueKind::Path("DIR"),
        default: Some("%APPDATA%\\audiophile-player\\filterdb"),
        config_key: Some("filters.dir"),
        scope: &[Cmd::Play, Cmd::Scan, Cmd::Filters, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::Always,
        summary: "Override Schema-2 machine-managed filterdb directory",
        details: "Directory holding library.yaml, per-album analysis YAML files under analysis/, and relink-report.yaml.",
        examples: &["audiophile-player --filterdb-dir D:\\AudioCache\\filterdb"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--choices-file",
        short: None,
        value: ValueKind::Path("PATH"),
        default: Some("%APPDATA%\\audiophile-player\\my-filters.yaml"),
        config_key: Some("filters.choices_file"),
        scope: &[Cmd::Play, Cmd::Scan, Cmd::Filters, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::NextTrack,
        summary: "Override user-owned my-filters.yaml path",
        details: "Path to your personal filter overrides YAML file. Hot-reloaded automatically on every track change.",
        examples: &["audiophile-player --choices-file D:\\AudioCache\\my-filters.yaml"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--trackdb-dir",
        short: None,
        value: ValueKind::Path("PATH"),
        default: Some("%APPDATA%\\audiophile-player\\trackdb"),
        config_key: Some("trackdb.dir"),
        scope: &[Cmd::Play, Cmd::Scan, Cmd::Filters, Cmd::Restart],
        group: Group::Ultimate,
        restart: Restart::Always,
        summary: "Override legacy Schema-1 YAML trackdb folder",
        details: "Used for read-only Schema-1 fallback and `audiophile-player filters migrate`.",
        examples: &["audiophile-player filters migrate --trackdb-dir C:\\TrackDb"],
        since: "0.1.0",
    },
    // --- LIBRARY ---
    OptSpec {
        name: "--music-dir",
        short: None,
        value: ValueKind::Path("PATH"),
        default: Some("~/Music"),
        config_key: Some("player.music_dir"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Library,
        restart: Restart::Always,
        summary: "Root folder containing FLAC subfolders (default: ~/Music)",
        details: "Allowed root directory for random/sequential playback, HTTP folder browsing, and MCP play commands. The engine strictly confines file access inside this root.",
        examples: &["audiophile-player --music-dir D:\\Music"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--report",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Scan],
        group: Group::Library,
        restart: Restart::NotApplicable,
        summary: "Print one diagnostic line per scanned track",
        details: "Prints peak, LUFS loudness, real bit depth, detector verdicts, and suggested filter for every track processed by `scan`.",
        examples: &["audiophile-player scan D:\\Music --report"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--post-chain",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Scan],
        group: Group::Library,
        restart: Restart::NotApplicable,
        summary: "Also measure exact whole-track post-chain peak M during scan",
        details: "Renders each scanned track through its real upsampler, de-emphasis, and REW convolver at unity gain so Ultimate Mode gets exact cached headroom from the very first play.",
        examples: &["audiophile-player scan D:\\Music --post-chain"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--force",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Scan],
        group: Group::Library,
        restart: Restart::NotApplicable,
        summary: "Re-analyse tracks even if already current in filterdb",
        details: "Bypasses the incremental size+mtime fast-skip in `scan` and re-decodes every discovered FLAC file.",
        examples: &["audiophile-player scan D:\\Music --force"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--jobs",
        short: Some('j'),
        value: ValueKind::Int {
            placeholder: "N",
            min: 1,
            max: 32,
        },
        default: Some("auto (up to 4)"),
        config_key: None,
        scope: &[Cmd::Scan],
        group: Group::Library,
        restart: Restart::NotApplicable,
        summary: "Number of parallel folder workers for `scan` (1..=32)",
        details: "Sets worker concurrency for folder-sharded offline scanning. Each worker owns one album folder at a time.",
        examples: &["audiophile-player scan D:\\Music --jobs 6"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--note",
        short: None,
        value: ValueKind::Str("TEXT"),
        default: None,
        config_key: None,
        scope: &[Cmd::Filters],
        group: Group::Library,
        restart: Restart::NotApplicable,
        summary: "Optional comment note attached by `filters set`",
        details: "Stores a human-readable `note:` field alongside your filter override in my-filters.yaml.",
        examples: &["audiophile-player filters set D:\\Music\\Jazz gauss-long-lp --note \"smoother treble\""],
        since: "0.1.0",
    },
    OptSpec {
        name: "--apply",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Filters],
        group: Group::Library,
        restart: Restart::NotApplicable,
        summary: "Apply pending moves in `filters relink` (creates .bak backup)",
        details: "Without `--apply`, `filters relink` only previews pending path updates from filterdb/relink-report.yaml. Passing `--apply` rewrites my-filters.yaml in place while preserving comments and writing a .bak backup.",
        examples: &["audiophile-player filters relink --apply"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--sidecar-root",
        short: None,
        value: ValueKind::Path("DIR"),
        default: None,
        config_key: None,
        scope: &[Cmd::Filters],
        group: Group::Library,
        restart: Restart::NotApplicable,
        summary: "Also import legacy *.ap.yaml sidecars under DIR during `filters migrate`",
        details: "Walks DIR recursively to import any legacy `<track.flac>.ap.yaml` sidecar files into Schema-2 filterdb/ and my-filters.yaml without deleting the sidecars.",
        examples: &["audiophile-player filters migrate --sidecar-root D:\\Music"],
        since: "0.1.0",
    },
    // --- NETWORK ---
    OptSpec {
        name: "--port",
        short: None,
        value: ValueKind::Int {
            placeholder: "PORT",
            min: 1,
            max: 65535,
        },
        default: Some("auto (8085..8199)"),
        config_key: Some("player.port"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Network,
        restart: Restart::Always,
        summary: "Localhost HTTP control UI port (default: auto 8085..8199)",
        details: "Binds the localhost-only HTTP control UI and /api/v1/* JSON API on 127.0.0.1:PORT. When omitted, picks the first free port in 8085..=8199.",
        examples: &["audiophile-player --port 8090"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--upnp-name",
        short: None,
        value: ValueKind::Str("NAME"),
        default: Some("Audiophile Player"),
        config_key: Some("upnp.friendly_name"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Network,
        restart: Restart::Always,
        summary: "UPnP AV MediaRenderer display name on LAN",
        details: "Friendly name advertised over SSDP to foobar2000, BubbleUPnP, mconnect, and other UPnP control points.",
        examples: &["audiophile-player --upnp-name \"Living Room DAC\""],
        since: "0.1.0",
    },
    OptSpec {
        name: "--upnp-port",
        short: None,
        value: ValueKind::Int {
            placeholder: "PORT",
            min: 1,
            max: 65535,
        },
        default: Some("auto (49152..49250)"),
        config_key: Some("upnp.port"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Network,
        restart: Restart::Always,
        summary: "UPnP HTTP/SOAP listener port",
        details: "Explicit TCP port for UPnP description XML, SOAP AVTransport, and GENA eventing.",
        examples: &["audiophile-player --upnp-port 49170"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--upnp-bind",
        short: None,
        value: ValueKind::Str("IPV4"),
        default: Some("auto LAN IPv4"),
        config_key: Some("upnp.bind_ip"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Network,
        restart: Restart::Always,
        summary: "Bind and advertise UPnP renderer on a specific local IPv4",
        details: "Useful on multi-homed PCs (VPNs, Hyper-V, WSL virtual switches) to ensure SSDP and SOAP advertise your LAN interface.",
        examples: &["audiophile-player --upnp-bind 192.168.0.187"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--no-upnp",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: Some("upnp.enabled"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Network,
        restart: Restart::Always,
        summary: "Disable the built-in UPnP AV MediaRenderer",
        details: "Prevents starting the SSDP/SOAP UPnP renderer even when `[upnp] enabled = true` in config.toml.",
        examples: &["audiophile-player --no-upnp"],
        since: "0.1.0",
    },
    // --- CAPTURE ---
    OptSpec {
        name: "--no-capture",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: Some("capture.enabled"),
        scope: &[Cmd::Play, Cmd::Restart],
        group: Group::Capture,
        restart: Restart::Always,
        summary: "Disable opt-in exclusive capture watcher (\\\\.\\ApExclusiveAudio)",
        details: "Disables watching the opt-in ACX virtual render driver used for Tidal Desktop Exclusive Mode capture.",
        examples: &["audiophile-player --no-capture"],
        since: "0.1.0",
    },
    // --- GENERAL ---
    OptSpec {
        name: "--config",
        short: None,
        value: ValueKind::Path("PATH"),
        default: Some("auto-discover"),
        config_key: None,
        scope: &[
            Cmd::Play,
            Cmd::Scan,
            Cmd::Filters,
            Cmd::Doctor,
            Cmd::Config,
            Cmd::Restart,
        ],
        group: Group::General,
        restart: Restart::Always,
        summary: "Explicit TOML config file path (default: auto-discover)",
        details: "Overrides the default config discovery order (./audiophile-player.toml then %APPDATA%\\audiophile-player\\config.toml). Must exist and parse cleanly.",
        examples: &["audiophile-player --config C:\\Cfg\\player.toml"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--when",
        short: None,
        value: ValueKind::Enum {
            placeholder: "MODE",
            values: WHEN_VALUES,
        },
        default: Some("after-track (restart) / now (stop)"),
        config_key: None,
        scope: &[Cmd::Stop, Cmd::Restart],
        group: Group::General,
        restart: Restart::NotApplicable,
        summary: "Shutdown timing for `stop` / `restart`: after-track | now",
        details: "`after-track` lets the currently playing song finish before graceful teardown and restart (default for `restart`). `now` fades out and releases the DAC immediately (default for `stop`).",
        examples: &[
            "audiophile-player restart --when now --ultimate",
            "audiophile-player stop --when after-track",
        ],
        since: "0.2.0",
    },
    OptSpec {
        name: "--json",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[
            Cmd::Help,
            Cmd::Doctor,
            Cmd::Devices,
            Cmd::Config,
            Cmd::Status,
            Cmd::Stop,
            Cmd::Restart,
            Cmd::Play,
        ],
        group: Group::General,
        restart: Restart::NotApplicable,
        summary: "Emit one machine-readable JSON document to stdout",
        details: "Supported on `help`, `doctor`, `devices`, `config`, `status`, `stop`, `restart`, and `--version`. Every JSON document includes a versioned `schema` field (`ap.<kind>/1`). Diagnostic logs always go to stderr.",
        examples: &[
            "audiophile-player doctor --json",
            "audiophile-player --version --json",
        ],
        since: "0.2.0",
    },
    OptSpec {
        name: "--markdown",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Help],
        group: Group::General,
        restart: Restart::NotApplicable,
        summary: "Generate docs/CLI.md Markdown reference from cli_spec.rs",
        details: "Outputs the complete deterministic Markdown CLI reference checked byte-for-byte by the `cli_docs_in_sync` test.",
        examples: &["audiophile-player help --markdown"],
        since: "0.2.0",
    },
    OptSpec {
        name: "--effective",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Config],
        group: Group::General,
        restart: Restart::NotApplicable,
        summary: "Include default values in `config show` (not just file overrides)",
        details: "Shows every known configuration key with its resolved effective value and provenance (`default` or `file:<path>`).",
        examples: &["audiophile-player config show --effective"],
        since: "0.2.0",
    },
    OptSpec {
        name: "--yes",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Config, Cmd::Filters, Cmd::Restart, Cmd::Stop],
        group: Group::General,
        restart: Restart::NotApplicable,
        summary: "Confirm non-interactively without prompting on stdin",
        details: "Ensures scripts and AI agents never block waiting for interactive confirmation prompts.",
        examples: &["audiophile-player stop --yes"],
        since: "0.2.0",
    },
    OptSpec {
        name: "--idle",
        short: None,
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Play],
        group: Group::General,
        restart: Restart::Always,
        summary: "Start idle without opening audio hardware (signal test mode)",
        details: "Parks at 0% CPU until Ctrl+C / Ctrl+Break without opening a DAC or HTTP server.",
        examples: &["audiophile-player --idle"],
        since: "0.1.0",
    },
    OptSpec {
        name: "--version",
        short: Some('V'),
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[Cmd::Play],
        group: Group::General,
        restart: Restart::NotApplicable,
        summary: "Show version, copyright, and GPL-3.0-or-later notice (supports --json)",
        details: "Prints the standard FSF GNU GPLv3+ version and no-warranty banner. Combine with `--json` for `{schema, version, git_commit, build_date, install_kind, features}`.",
        examples: &[
            "audiophile-player --version",
            "audiophile-player --version --json",
        ],
        since: "0.1.0",
    },
    OptSpec {
        name: "--help",
        short: Some('h'),
        value: ValueKind::Flag,
        default: Some("false"),
        config_key: None,
        scope: &[
            Cmd::Play,
            Cmd::Scan,
            Cmd::Filters,
            Cmd::Help,
            Cmd::Doctor,
            Cmd::Devices,
            Cmd::Config,
            Cmd::Status,
            Cmd::Stop,
            Cmd::Restart,
            Cmd::Mcp,
        ],
        group: Group::General,
        restart: Restart::NotApplicable,
        summary: "Show summary help (or command-specific help)",
        details: "Prints the grouped CLI summary, or the detail page when passed after a subcommand (e.g. `audiophile-player scan --help`).",
        examples: &["audiophile-player --help", "audiophile-player scan --help"],
        since: "0.1.0",
    },
];

/// Return the mutually exclusive / negated CLI flags that should be stripped from a previous
/// argv when `flag` is passed on `restart` or via MCP (Review #9 Q7).
pub fn negated_options(flag: &str) -> &'static [&'static str] {
    match flag {
        "--ultimate" => &["--no-ultimate"],
        "--no-ultimate" => &["--ultimate"],
        "--no-conv" => &["--conv-dir", "--conv-file"],
        "--conv-dir" | "--conv-file" => &["--no-conv"],
        "--no-upnp" => &["--upnp-name", "--upnp-port", "--upnp-bind"],
        "--upnp-name" | "--upnp-port" | "--upnp-bind" => &["--no-upnp"],
        _ => &[],
    }
}

/// Look up an option by its canonical `config.toml` key (Review #9 Q15).
pub fn find_config_key(key: &str) -> Option<&'static OptSpec> {
    OPTIONS.iter().find(|opt| opt.config_key == Some(key))
}

/// Whether changing `key` in `config.toml` requires restarting the running player (Review #9 Q15).
pub fn config_key_requires_restart(key: &str) -> bool {
    find_config_key(key).is_none_or(|opt| opt.restart == Restart::Always)
}

/// Look up an option by `--name` or `-short` (with or without leading dashes).
pub fn find_option(query: &str) -> Option<&'static OptSpec> {
    let q = query.trim();
    let with_dashes = if q.starts_with('-') {
        q.to_string()
    } else if q.len() == 1 {
        format!("-{q}")
    } else {
        format!("--{q}")
    };
    OPTIONS.iter().find(|opt| {
        opt.name.eq_ignore_ascii_case(&with_dashes)
            || opt
                .short
                .is_some_and(|c| with_dashes.len() == 2 && with_dashes.ends_with(c))
    })
}

/// Look up a command by name.
pub fn find_command(query: &str) -> Option<&'static CmdSpec> {
    let q = query.trim();
    COMMANDS.iter().find(|c| c.name.eq_ignore_ascii_case(q))
}

/// Suggest the closest known option name for a typo using Levenshtein edit distance (<= 3).
pub fn suggest_nearest_option(unknown: &str) -> Option<&'static str> {
    let u = unknown.trim().to_ascii_lowercase();
    let mut best: Option<(usize, &'static str)> = None;
    for opt in OPTIONS {
        let d = levenshtein(&u, &opt.name.to_ascii_lowercase());
        if d <= 3 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, opt.name));
        }
    }
    best.map(|(_, name)| name)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    if av.is_empty() {
        return bv.len();
    }
    if bv.is_empty() {
        return av.len();
    }
    let mut prev: Vec<usize> = (0..=bv.len()).collect();
    let mut cur = vec![0usize; bv.len() + 1];
    for (i, &ca) in av.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in bv.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[bv.len()]
}

fn format_opt_flag(opt: &OptSpec) -> String {
    let mut s = String::new();
    if let Some(ch) = opt.short {
        s.push('-');
        s.push(ch);
        s.push_str(", ");
    }
    s.push_str(opt.name);
    if let Some(ph) = opt.value.placeholder() {
        s.push(' ');
        s.push_str(ph);
    }
    s
}

/// Render the concise grouped one-line summary (`audiophile-player help` / `--help`).
pub fn render_summary_help() -> String {
    let mut out = String::with_capacity(3072);
    out.push_str(&format!(
        "Audiophile Player v{} — headless 8fS WDM-KS upsampling player (GPL-3.0-or-later)\n\
         Manual: {MANUAL_URL}\n\n\
         USAGE  audiophile-player [OPTIONS]            play (default: ~/Music, WDM-KS + HTTP UI + UPnP)\n\
                audiophile-player <COMMAND> [ARGS]     ",
        env!("CARGO_PKG_VERSION")
    ));
    let cmd_names: Vec<&str> = COMMANDS
        .iter()
        .filter(|c| c.name != "play")
        .map(|c| c.name)
        .collect();
    out.push_str(&cmd_names.join(" | "));
    out.push_str("\n\n");

    for group in Group::ALL {
        let group_opts: Vec<&OptSpec> = OPTIONS.iter().filter(|o| o.group == group).collect();
        if group_opts.is_empty() {
            continue;
        }
        for (idx, opt) in group_opts.iter().enumerate() {
            let prefix = if idx == 0 {
                format!("{:<10}", group.label())
            } else {
                "          ".to_string()
            };
            let flag = format_opt_flag(opt);
            let restart_tag = if opt.restart == Restart::NotApplicable {
                String::new()
            } else {
                format!(" [{}]", opt.restart.tag())
            };
            out.push_str(&format!(
                "{prefix}  {flag:<26} {}{restart_tag}\n",
                opt.summary
            ));
        }
    }

    let exit_summary: String = crate::exit_codes::EXIT_CODE_SPECS
        .iter()
        .map(|s| format!("{}={}", s.code, s.summary_tag))
        .collect::<Vec<_>>()
        .join("  ");
    out.push_str(&format!(
        "\nDetails: audiophile-player help <option|command>  |  JSON: help --json  |  Docs: docs/CLI.md\n\
         Exit codes: {exit_summary}\n\
         Copyright (C) 2026 androxylo — License GPL-3.0-or-later (NO WARRANTY).\n"
    ));
    out
}

/// Render a detailed help page for one `--option` or `command` (`audiophile-player help <topic>`).
pub fn render_detail_help(topic: &str) -> Result<String, String> {
    if let Some(opt) = find_option(topic) {
        let mut out = String::new();
        let flag = format_opt_flag(opt);
        let cfg = opt.config_key.unwrap_or("(cli-only)");
        let def = opt.default.unwrap_or("none");
        out.push_str(&format!(
            "{flag}    config: {cfg}    default: {def}    {}\n",
            opt.restart.description()
        ));
        let scopes: Vec<&str> = opt.scope.iter().map(|s| s.as_str()).collect();
        out.push_str(&format!("  Scope: {}\n", scopes.join(", ")));
        if let ValueKind::Enum { values, .. } = opt.value {
            out.push_str(&format!("  Allowed values: {}\n", values.join(" | ")));
        } else if let ValueKind::Int { min, max, .. } = opt.value {
            out.push_str(&format!("  Range: {min}..={max}\n"));
        }
        out.push('\n');
        for line in opt.details.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        if !opt.examples.is_empty() {
            out.push_str("\n  Examples:\n");
            for ex in opt.examples {
                out.push_str(&format!("    {ex}\n"));
            }
        }
        out.push_str(&format!("\n  Since: v{}\n", opt.since));
        return Ok(out);
    }

    if let Some(cmd) = find_command(topic) {
        let mut out = String::new();
        out.push_str(&format!("{} — {}\n", cmd.name, cmd.summary));
        out.push_str(&format!("Usage: {}\n\n", cmd.usage));
        for line in cmd.details.lines() {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
        }
        let scoped_opts: Vec<&OptSpec> = OPTIONS
            .iter()
            .filter(|o| o.scope.iter().any(|s| s.as_str() == cmd.name))
            .collect();
        if !scoped_opts.is_empty() {
            out.push_str("\n  Options:\n");
            for opt in scoped_opts {
                let flag = format_opt_flag(opt);
                out.push_str(&format!("    {flag:<26} {}\n", opt.summary));
            }
        }
        if !cmd.examples.is_empty() {
            out.push_str("\n  Examples:\n");
            for ex in cmd.examples {
                out.push_str(&format!("    {ex}\n"));
            }
        }
        out.push_str(&format!("\n  Since: v{}\n", cmd.since));
        return Ok(out);
    }

    let hint = suggest_nearest_option(topic)
        .map(|s| format!("; did you mean '{s}'?"))
        .unwrap_or_default();
    Err(format!(
        "unknown help topic '{topic}'{hint} (run `audiophile-player help` to list commands and options)"
    ))
}

/// Append `enum` / `minimum` / `maximum` JSON schema constraints for `kind` (shared by `help --json`
/// and `mcp_play_options_schema`, Review #9 Q11).
fn append_value_kind_constraints(fields: &mut Vec<(&'static str, JsonValue)>, kind: ValueKind) {
    match kind {
        ValueKind::Enum { values, .. } => {
            fields.push((
                "enum",
                JsonValue::Array(values.iter().map(|&v| JsonValue::from(v)).collect()),
            ));
        }
        ValueKind::Int { min, max, .. } => {
            fields.push(("minimum", JsonValue::from(min)));
            fields.push(("maximum", JsonValue::from(max)));
        }
        _ => {}
    }
}

/// Render the full machine-readable CLI specification (`audiophile-player help --json`).
pub fn render_help_json() -> JsonValue {
    let cmds: Vec<JsonValue> = COMMANDS
        .iter()
        .map(|c| {
            JsonValue::obj(vec![
                ("name", JsonValue::from(c.name)),
                ("usage", JsonValue::from(c.usage)),
                ("summary", JsonValue::from(c.summary)),
                ("details", JsonValue::from(c.details)),
                (
                    "examples",
                    JsonValue::Array(c.examples.iter().map(|&e| JsonValue::from(e)).collect()),
                ),
                ("since", JsonValue::from(c.since)),
            ])
        })
        .collect();

    let opts: Vec<JsonValue> = OPTIONS
        .iter()
        .map(|o| {
            let mut entries = vec![
                ("name", JsonValue::from(o.name)),
                (
                    "short",
                    o.short
                        .map(|c| JsonValue::from(format!("-{c}")))
                        .unwrap_or(JsonValue::Null),
                ),
                ("type", JsonValue::from(o.value.json_type())),
                ("placeholder", JsonValue::opt_str(o.value.placeholder())),
                ("default", JsonValue::opt_str(o.default)),
                ("config_key", JsonValue::opt_str(o.config_key)),
                ("group", JsonValue::from(o.group.slug())),
                ("restart", JsonValue::from(o.restart.tag())),
                (
                    "scope",
                    JsonValue::Array(o.scope.iter().map(|s| JsonValue::from(s.as_str())).collect()),
                ),
                ("summary", JsonValue::from(o.summary)),
                ("details", JsonValue::from(o.details)),
                (
                    "examples",
                    JsonValue::Array(o.examples.iter().map(|&e| JsonValue::from(e)).collect()),
                ),
                ("since", JsonValue::from(o.since)),
            ];
            append_value_kind_constraints(&mut entries, o.value);
            JsonValue::obj(entries)
        })
        .collect();

    let exit_codes_obj = JsonValue::obj(
        crate::exit_codes::EXIT_CODE_SPECS
            .iter()
            .map(|s| (s.code_str, JsonValue::from(s.json_slug)))
            .collect(),
    );

    JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.cli_spec/1")),
        ("version", JsonValue::from(env!("CARGO_PKG_VERSION"))),
        ("exit_codes", exit_codes_obj),
        (
            "config_only_keys",
            JsonValue::Array(
                CONFIG_ONLY_KEYS
                    .iter()
                    .map(|&k| JsonValue::from(k))
                    .collect(),
            ),
        ),
        ("commands", JsonValue::Array(cmds)),
        ("options", JsonValue::Array(opts)),
    ])
}

/// Build a JSON Schema object for play/restart options directly from [`OPTIONS`] (used by MCP).
pub fn mcp_play_options_schema() -> JsonValue {
    let mut props = Vec::new();
    for opt in OPTIONS {
        if !opt.scope.contains(&Cmd::Play)
            || matches!(opt.name, "--help" | "--version" | "--json" | "--idle")
        {
            continue;
        }
        let prop_name = opt.name.trim_start_matches('-').replace('-', "_");
        let mut fields = vec![
            ("type", JsonValue::from(opt.value.json_type())),
            ("description", JsonValue::from(opt.summary)),
        ];
        append_value_kind_constraints(&mut fields, opt.value);
        props.push((prop_name, JsonValue::obj(fields)));
    }
    JsonValue::obj(vec![
        ("type", JsonValue::from("object")),
        ("additionalProperties", JsonValue::from(false)),
        ("properties", JsonValue::obj_owned(props)),
    ])
}

/// Generate the canonical Markdown CLI reference (`docs/CLI.md`).
/// Enforced byte-for-byte by the `cli_docs_in_sync` test!
pub fn render_help_markdown() -> String {
    let mut out = String::with_capacity(8192);
    out.push_str("<!-- GENERATED FILE: do not edit by hand! Regenerate with:\n");
    out.push_str("     cargo run --bin audiophile-player -- help --markdown > docs/CLI.md -->\n\n");
    out.push_str("# CLI & Configuration Reference\n\n");
    out.push_str("[🏠 Home](../README.md) · [🚀 Quick Start](QUICK_START.md) · [🧭 Getting Started](GETTING_STARTED.md) · [💎 Ultimate Mode](ULTIMATE_MODE.md) · [🎛️ Sound Engine](SOUND_ENGINE.md) · **⌨️ CLI**\n\n");
    out.push_str("> Generated directly from `crates/ap-cli/src/cli_spec.rs`. Machine-readable form: `audiophile-player help --json`.\n\n");

    out.push_str("## Commands\n\n");
    out.push_str("| Command | Usage | Summary |\n");
    out.push_str("|---|---|---|\n");
    for cmd in COMMANDS {
        let usage_esc = cmd.usage.replace('|', "\\|");
        let sum_esc = cmd.summary.replace('|', "\\|");
        out.push_str(&format!("| `{}` | `{}` | {} |\n", cmd.name, usage_esc, sum_esc));
    }

    out.push_str("\n## Options\n");
    for group in Group::ALL {
        let group_opts: Vec<&OptSpec> = OPTIONS.iter().filter(|o| o.group == group).collect();
        if group_opts.is_empty() {
            continue;
        }
        out.push_str(&format!("\n### {}\n\n", group.title()));
        out.push_str("| Option | Config key | Default | Scope | Restart | Summary |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for opt in group_opts {
            let flag = format_opt_flag(opt).replace('|', "\\|");
            let cfg = opt
                .config_key
                .map(|k| format!("`{k}`"))
                .unwrap_or_else(|| "—".to_string());
            let def = opt
                .default
                .map(|d| format!("`{}`", d.replace('|', "\\|")))
                .unwrap_or_else(|| "—".to_string());
            let scopes: Vec<&str> = opt.scope.iter().map(|s| s.as_str()).collect();
            let sum_esc = opt.summary.replace('|', "\\|");
            out.push_str(&format!(
                "| `{flag}` | {cfg} | {def} | {} | {} | {sum_esc} |\n",
                scopes.join(", "),
                opt.restart.tag()
            ));
        }
    }

    out.push_str("\n## Config-Only Keys (`config.toml`)\n\n");
    out.push_str("These keys exist in `config.toml` without a 1:1 standalone CLI flag:\n\n");
    for k in CONFIG_ONLY_KEYS {
        out.push_str(&format!("- `{k}`\n"));
    }

    out.push_str("\n## Exit Codes\n\n");
    out.push_str("| Code | Meaning |\n");
    out.push_str("|---|---|\n");
    for s in crate::exit_codes::EXIT_CODE_SPECS {
        out.push_str(&format!("| `{}` | {} |\n", s.code, s.markdown_meaning));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    use ap_core::config::ALL_KNOWN_CONFIG_KEYS;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    }

    /// Drift guard 1: `docs/CLI.md` must match `help --markdown` byte-for-byte.
    #[test]
    fn cli_docs_in_sync() {
        let cli_md_path = repo_root().join("docs").join("CLI.md");
        let on_disk = std::fs::read_to_string(&cli_md_path)
            .expect("docs/CLI.md must exist (regenerate with `audiophile-player help --markdown > docs/CLI.md`)")
            .replace("\r\n", "\n");
        let generated = render_help_markdown().replace("\r\n", "\n");
        assert_eq!(
            on_disk, generated,
            "docs/CLI.md is out of sync with cli_spec.rs! Regenerate with `cargo run --bin audiophile-player -- help --markdown > docs/CLI.md`"
        );
    }

    /// Drift guard 2: every option's examples must parse cleanly via `cli_args::parse_args`.
    #[test]
    fn every_option_parses() {
        for opt in OPTIONS {
            assert!(
                !opt.examples.is_empty(),
                "option {} must provide at least one example",
                opt.name
            );
            for ex in opt.examples {
                let argv = split_shell_example(ex);
                let parsed = crate::cli_args::parse_args(argv.iter().map(String::as_str));
                assert!(
                    parsed.is_ok(),
                    "example for {} failed to parse: `{ex}` -> {:?}",
                    opt.name,
                    parsed.err()
                );
            }
        }
        for cmd in COMMANDS {
            for ex in cmd.examples {
                let argv = split_shell_example(ex);
                let parsed = crate::cli_args::parse_args(argv.iter().map(String::as_str));
                assert!(
                    parsed.is_ok(),
                    "example for command {} failed to parse: `{ex}` -> {:?}",
                    cmd.name,
                    parsed.err()
                );
            }
        }
    }

    /// Drift guard 3: every `config_key` in `cli_spec.rs` exists in `PlayerConfig`, AND every
    /// field in `ALL_KNOWN_CONFIG_KEYS` is either mapped in `OPTIONS` or listed in `CONFIG_ONLY_KEYS`.
    #[test]
    fn config_keys_exist() {
        let known: HashSet<&str> = ALL_KNOWN_CONFIG_KEYS.iter().copied().collect();
        let mut covered: HashSet<&str> = HashSet::new();

        for opt in OPTIONS {
            if let Some(k) = opt.config_key {
                assert!(
                    known.contains(k),
                    "cli_spec.rs option {} references unknown config_key '{k}'",
                    opt.name
                );
                covered.insert(k);
            }
        }
        for &k in CONFIG_ONLY_KEYS {
            assert!(
                known.contains(k),
                "CONFIG_ONLY_KEYS references unknown key '{k}'"
            );
            covered.insert(k);
        }
        let missing: Vec<&str> = known.difference(&covered).copied().collect();
        assert!(
            missing.is_empty(),
            "PlayerConfig keys missing from cli_spec.rs (add config_key or CONFIG_ONLY_KEYS): {missing:?}"
        );
    }

    /// Drift guard 4: `docs/GETTING_STARTED.md` must link to `docs/CLI.md` instead of maintaining
    /// a hand-copied `--option` Markdown table that drifts from `cli_spec.rs`.
    #[test]
    fn getting_started_links_only() {
        let gs_path = repo_root().join("docs").join("GETTING_STARTED.md");
        let text = std::fs::read_to_string(&gs_path).unwrap();
        assert!(
            text.contains("CLI.md"),
            "docs/GETTING_STARTED.md must link to CLI.md"
        );
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            assert!(
                !trimmed.starts_with("| `--"),
                "docs/GETTING_STARTED.md line {} contains a duplicated CLI option table row (`{trimmed}`); link to docs/CLI.md instead",
                idx + 1
            );
        }
    }

    /// Drift guard 5 (Review #9 Q3): verify every `ValueKind::Enum` value set and `ValueKind::Int`
    /// range in `OPTIONS` is enforced identically by `cli_args::parse_args`.
    #[test]
    fn spec_enum_and_int_bounds_match_parser() {
        for opt in OPTIONS {
            let prefix: Vec<&str> = if opt.scope.contains(&Cmd::Play) {
                vec!["audiophile-player"]
            } else if opt.scope.contains(&Cmd::Scan) {
                vec!["audiophile-player", "scan"]
            } else if opt.scope.contains(&Cmd::Stop) {
                vec!["audiophile-player", "stop"]
            } else {
                continue;
            };
            match opt.value {
                ValueKind::Enum { values, .. } => {
                    for &v in values {
                        let mut argv = prefix.clone();
                        argv.push(opt.name);
                        argv.push(v);
                        assert!(
                            crate::cli_args::parse_args(argv).is_ok(),
                            "spec enum value `{v}` for {} failed to parse",
                            opt.name
                        );
                    }
                    let mut bad = prefix.clone();
                    bad.push(opt.name);
                    bad.push("__definitely_invalid_enum__");
                    assert!(
                        crate::cli_args::parse_args(bad).is_err(),
                        "invalid enum value for {} should be rejected",
                        opt.name
                    );
                }
                ValueKind::Int { min, max, .. } => {
                    for edge in [min, max] {
                        let s = edge.to_string();
                        let mut argv = prefix.clone();
                        argv.push(opt.name);
                        argv.push(&s);
                        assert!(
                            crate::cli_args::parse_args(argv).is_ok(),
                            "spec int bound {edge} for {} failed to parse",
                            opt.name
                        );
                    }
                    for oob in [min - 1, max + 1] {
                        let s = oob.to_string();
                        let mut argv = prefix.clone();
                        argv.push(opt.name);
                        argv.push(&s);
                        assert!(
                            crate::cli_args::parse_args(argv).is_err(),
                            "out-of-range int {oob} for {} should be rejected",
                            opt.name
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn split_shell_example(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        for ch in line.chars() {
            match ch {
                '"' => in_quotes = !in_quotes,
                c if c.is_whitespace() && !in_quotes => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }
}
