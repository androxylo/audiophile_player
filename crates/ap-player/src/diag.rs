// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Diagnostics, DAC enumeration, config editor, and version JSON commands
//! (`QUICK_START_AI_DESIGN.md` §§2.4, 4.3, 4.4):
//! - `audiophile-player devices [--json]`
//! - `audiophile-player doctor [--config <PATH>] [--json]`
//! - `audiophile-player config <show|set|unset> ...`
//! - `audiophile-player --version --json`

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use ap_control::server::{DEFAULT_PORT_END, DEFAULT_PORT_START};
use ap_core::config::{
    ALL_KNOWN_CONFIG_KEYS, LOCAL_CONFIG_FILENAME, OutputTransport, PlayerConfig,
    canonicalize_config_key, format_toml_value_for_key, unset_toml_key_preserving_comments,
    upsert_toml_key_preserving_comments, write_config_with_backup,
};
use ap_core::format::DacWireFormat;
use ap_core::json::JsonValue;
use ap_output_win::sink::OutputError;
use ap_output_win::wasapi_exclusive::{
    WasapiRenderDevice, enumerate_wasapi_render_devices, probe_wasapi_device_capabilities,
    probe_wasapi_device_format, select_wasapi_device,
};
use ap_output_win::wdm_ks::{
    KsRenderDevice, enumerate_wdm_ks_render_devices, probe_ks_device_capabilities,
    probe_ks_device_format, select_wdm_ks_device,
};
use ap_source::library::{default_user_music_dir, discover_flac_files};

use crate::cli_args::ConfigCommand;
use crate::exit_codes::CliError;

/// Render `--version --json` (`schema: "ap.version/1"`).
pub fn version_json() -> JsonValue {
    let install_kind = detect_install_kind();
    JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.version/1")),
        ("version", JsonValue::from(env!("CARGO_PKG_VERSION"))),
        ("git_commit", JsonValue::from(env!("AP_GIT_COMMIT"))),
        ("build_date", JsonValue::from(env!("AP_BUILD_DATE"))),
        ("install_kind", JsonValue::from(install_kind)),
        ("license", JsonValue::from("GPL-3.0-or-later")),
        (
            "features",
            JsonValue::Array(vec![
                JsonValue::from("wdm-ks-wavert"),
                JsonValue::from("wasapi-exclusive"),
                JsonValue::from("8fs-f64-dsp"),
                JsonValue::from("rew-upols-convolution"),
                JsonValue::from("upnp-av-renderer"),
                JsonValue::from("ultimate-mode"),
                JsonValue::from("filterdb-schema-2"),
                JsonValue::from("stdio-mcp"),
            ]),
        ),
    ])
}

fn detect_install_kind() -> &'static str {
    if let Ok(exe) = std::env::current_exe() {
        // Review #9 Q14: check `install-receipt.yaml` FIRST (handling both quoted `'binary'`
        // written by bootstrap.ps1 and unquoted values) in both exe's parent and grandparent
        // (for `<root>\bin\vX.Y.Z\audiophile-player.exe` layouts).
        for dir in [
            exe.parent(),
            exe.parent().and_then(Path::parent),
            exe.parent().and_then(Path::parent).and_then(Path::parent),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(text) = fs::read_to_string(dir.join("install-receipt.yaml")) {
                for line in text.lines() {
                    if let Some(rest) = line.trim().strip_prefix("install_kind:") {
                        let val = rest.trim().trim_matches('\'').trim_matches('"');
                        if val.eq_ignore_ascii_case("binary") {
                            return "binary";
                        }
                        if val.eq_ignore_ascii_case("source") {
                            return "source";
                        }
                    }
                }
            }
        }
        let lower = exe.to_string_lossy().to_ascii_lowercase();
        if lower.contains("\\target\\release\\") || lower.contains("\\target\\debug\\") {
            return "source";
        }
        if lower.contains("\\bin\\v") || lower.contains("/bin/v") {
            return "binary";
        }
    }
    "source"
}

#[derive(Debug, Clone)]
pub struct ProbedDeviceInfo {
    pub friendly_name: String,
    pub device_path: String,
    pub render_pin_id: u32,
    pub supports_looped_wavert: bool,
    pub rate_352800: String,
    pub rate_384000: String,
    pub supports_8fs: bool,
    pub max_rate_44k: Option<u32>,
    pub wire_format_44k: Option<&'static str>,
    pub max_rate_48k: Option<u32>,
    pub wire_format_48k: Option<&'static str>,
    pub supports_any_rate: bool,
    pub downsampling_needed: bool,
    pub auto_selected: bool,
}

fn probe_rate_status(dev: &KsRenderDevice, rate_hz: u32) -> String {
    match probe_ks_device_format(dev, rate_hz, DacWireFormat::Int32) {
        Ok(()) => "supported".to_string(),
        Err(OutputError::DeviceBusy { .. }) => "busy".to_string(),
        Err(OutputError::UnsupportedFormat { .. }) => "unsupported".to_string(),
        Err(err) => format!("error: {err}"),
    }
}

pub fn probe_all_render_devices() -> Result<Vec<ProbedDeviceInfo>, String> {
    let devices = enumerate_wdm_ks_render_devices().map_err(|e| e.to_string())?;
    let auto_pick_path = select_wdm_ks_device(&devices, None)
        .ok()
        .map(|d| d.device_path.clone());

    let mut out = Vec::with_capacity(devices.len());
    for dev in devices {
        let r352 = probe_rate_status(&dev, 352_800);
        let r384 = probe_rate_status(&dev, 384_000);
        // If device is busy (e.g. player itself is holding it), treat 8fS as busy rather than unsupported.
        let busy_8fs = r352 == "busy" && r384 == "busy";
        let supports_8fs = (r352 == "supported" && r384 == "supported") || busy_8fs;

        let (max_rate_44k, wire_format_44k, max_rate_48k, wire_format_48k) = if supports_8fs {
            (
                Some(352_800),
                Some(DacWireFormat::Int32.label()),
                Some(384_000),
                Some(DacWireFormat::Int32.label()),
            )
        } else {
            match probe_ks_device_capabilities(&dev, None) {
                Ok(caps) => (
                    caps.target_for_352800.map(|t| t.sample_rate_hz),
                    caps.target_for_352800.map(|t| t.wire_format.label()),
                    caps.target_for_384000.map(|t| t.sample_rate_hz),
                    caps.target_for_384000.map(|t| t.wire_format.label()),
                ),
                Err(OutputError::DeviceBusy { .. }) => (None, None, None, None),
                Err(_) => (None, None, None, None),
            }
        };

        let supports_any_rate = supports_8fs || max_rate_44k.is_some() || max_rate_48k.is_some();
        let downsampling_needed = supports_any_rate && !supports_8fs;
        let auto_selected = auto_pick_path
            .as_deref()
            .is_some_and(|p| p == dev.device_path);
        out.push(ProbedDeviceInfo {
            friendly_name: dev.friendly_name,
            device_path: dev.device_path,
            render_pin_id: dev.render_pin_id,
            supports_looped_wavert: dev.supports_looped_wavert,
            rate_352800: r352,
            rate_384000: r384,
            supports_8fs,
            max_rate_44k,
            wire_format_44k,
            max_rate_48k,
            wire_format_48k,
            supports_any_rate,
            downsampling_needed,
            auto_selected,
        });
    }
    Ok(out)
}

fn probe_wasapi_rate_status(dev: &WasapiRenderDevice, rate_hz: u32) -> String {
    match probe_wasapi_device_format(dev, rate_hz, DacWireFormat::Int32) {
        Ok(()) => "supported".to_string(),
        Err(OutputError::DeviceBusy { .. }) => "busy".to_string(),
        Err(OutputError::UnsupportedFormat { .. }) => "unsupported".to_string(),
        Err(err) => format!("error: {err}"),
    }
}

pub fn probe_all_wasapi_render_devices() -> Result<Vec<ProbedDeviceInfo>, String> {
    let devices = enumerate_wasapi_render_devices().map_err(|e| e.to_string())?;
    let auto_pick_id = select_wasapi_device(&devices, None)
        .ok()
        .map(|d| d.endpoint_id.clone());

    let mut out = Vec::with_capacity(devices.len());
    for dev in devices {
        let r352 = probe_wasapi_rate_status(&dev, 352_800);
        let r384 = probe_wasapi_rate_status(&dev, 384_000);
        let busy_8fs = r352 == "busy" && r384 == "busy";
        let supports_8fs = (r352 == "supported" && r384 == "supported") || busy_8fs;

        let (max_rate_44k, wire_format_44k, max_rate_48k, wire_format_48k) = if supports_8fs {
            (
                Some(352_800),
                Some(DacWireFormat::Int32.label()),
                Some(384_000),
                Some(DacWireFormat::Int32.label()),
            )
        } else {
            match probe_wasapi_device_capabilities(&dev, None) {
                Ok(caps) => (
                    caps.target_for_352800.map(|t| t.sample_rate_hz),
                    caps.target_for_352800.map(|t| t.wire_format.label()),
                    caps.target_for_384000.map(|t| t.sample_rate_hz),
                    caps.target_for_384000.map(|t| t.wire_format.label()),
                ),
                Err(_) => (None, None, None, None),
            }
        };

        let supports_any_rate = supports_8fs || max_rate_44k.is_some() || max_rate_48k.is_some();
        let downsampling_needed = supports_any_rate && !supports_8fs;
        let auto_selected = auto_pick_id
            .as_deref()
            .is_some_and(|p| p == dev.endpoint_id);
        out.push(ProbedDeviceInfo {
            friendly_name: dev.friendly_name,
            device_path: dev.endpoint_id,
            render_pin_id: 0,
            supports_looped_wavert: true,
            rate_352800: r352,
            rate_384000: r384,
            supports_8fs,
            max_rate_44k,
            wire_format_44k,
            max_rate_48k,
            wire_format_48k,
            supports_any_rate,
            downsampling_needed,
            auto_selected,
        });
    }
    Ok(out)
}

/// Build `devices --json` payload (`schema: "ap.devices/1"`) for the default backend (WASAPI Exclusive).
pub fn devices_report_json() -> Result<JsonValue, CliError> {
    devices_report_json_for(OutputTransport::default())
}

/// Build `devices --json` payload (`schema: "ap.devices/1"`) for a specific zero-mixer backend.
pub fn devices_report_json_for(transport: OutputTransport) -> Result<JsonValue, CliError> {
    let probed = match transport {
        OutputTransport::WdmKs => probe_all_render_devices().map_err(CliError::device)?,
        OutputTransport::WasapiExclusive | OutputTransport::WasapiPush => {
            probe_all_wasapi_render_devices().map_err(CliError::device)?
        }
    };
    let arr: Vec<JsonValue> = probed
        .iter()
        .map(|d| {
            JsonValue::obj(vec![
                ("friendly_name", JsonValue::from(d.friendly_name.clone())),
                ("device_path", JsonValue::from(d.device_path.clone())),
                ("render_pin_id", JsonValue::from(d.render_pin_id)),
                ("supports_looped_wavert", JsonValue::from(d.supports_looped_wavert)),
                ("rate_352800", JsonValue::from(d.rate_352800.clone())),
                ("rate_384000", JsonValue::from(d.rate_384000.clone())),
                ("supports_8fs", JsonValue::from(d.supports_8fs)),
                (
                    "max_rate_44k",
                    d.max_rate_44k.map(JsonValue::from).unwrap_or(JsonValue::Null),
                ),
                ("wire_format_44k", JsonValue::opt_str(d.wire_format_44k)),
                (
                    "max_rate_48k",
                    d.max_rate_48k.map(JsonValue::from).unwrap_or(JsonValue::Null),
                ),
                ("wire_format_48k", JsonValue::opt_str(d.wire_format_48k)),
                ("supports_any_rate", JsonValue::from(d.supports_any_rate)),
                ("downsampling_needed", JsonValue::from(d.downsampling_needed)),
                ("auto_selected", JsonValue::from(d.auto_selected)),
            ])
        })
        .collect();
    Ok(JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.devices/1")),
        ("backend", JsonValue::from(transport.as_str())),
        ("count", JsonValue::from(probed.len())),
        ("devices", JsonValue::Array(arr)),
    ]))
}

pub fn run_devices_cli(
    output_transport: Option<OutputTransport>,
    as_json: bool,
) -> Result<(), CliError> {
    let transport = match output_transport {
        Some(t) => t,
        None => PlayerConfig::discover_and_load(None)
            .ok()
            .and_then(|c| c.config.player.output)
            .and_then(|s| OutputTransport::parse(&s).ok())
            .unwrap_or_default(),
    };
    let json = devices_report_json_for(transport)?;
    if as_json {
        println!("{}", json.to_pretty_string());
        return Ok(());
    }
    let Some(devs) = json.get("devices").and_then(JsonValue::as_array) else {
        return Ok(());
    };
    let label = transport.display_label();
    if devs.is_empty() {
        println!("No {label} audio render devices found.");
        return Ok(());
    }
    println!("{label} Audio Render Devices ({}):", devs.len());
    for d in devs {
        let name = d
            .get("friendly_name")
            .and_then(JsonValue::as_str)
            .unwrap_or("?");
        let r352 = d.get("rate_352800").and_then(JsonValue::as_str).unwrap_or("?");
        let r384 = d.get("rate_384000").and_then(JsonValue::as_str).unwrap_or("?");
        let auto = d
            .get("auto_selected")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let ds_needed = d
            .get("downsampling_needed")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let mark = if auto { " [AUTO-SELECTED]" } else { "" };
        println!("  - {name}{mark}");
        println!("      352.8 kHz Int32: {r352}  |  384 kHz Int32: {r384}");
        if ds_needed {
            let m44 = d
                .get("max_rate_44k")
                .and_then(JsonValue::as_u64)
                .map(|r| format!("{r} Hz"))
                .unwrap_or_else(|| "none".to_string());
            let w44 = d.get("wire_format_44k").and_then(JsonValue::as_str).unwrap_or("-");
            let m48 = d
                .get("max_rate_48k")
                .and_then(JsonValue::as_u64)
                .map(|r| format!("{r} Hz"))
                .unwrap_or_else(|| "none".to_string());
            let w48 = d.get("wire_format_48k").and_then(JsonValue::as_str).unwrap_or("-");
            println!(
                "      Last-stage f64 downsample ceiling: 44.1k family -> {m44} ({w44})  |  48k family -> {m48} ({w48})"
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct DoctorCheck {
    id: &'static str,
    ok: bool,
    severity: &'static str, // "error" | "warn" | "info"
    detail: String,
    problem: Option<String>,
    fix: Option<String>,
}

impl DoctorCheck {
    fn pass(id: &'static str, detail: impl Into<String>) -> Self {
        Self {
            id,
            ok: true,
            severity: "info",
            detail: detail.into(),
            problem: None,
            fix: None,
        }
    }

    fn fail(
        id: &'static str,
        severity: &'static str,
        detail: impl Into<String>,
        problem: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        Self {
            id,
            ok: false,
            severity,
            detail: detail.into(),
            problem: Some(problem.into()),
            fix: Some(fix.into()),
        }
    }

    fn to_json(&self) -> JsonValue {
        JsonValue::obj(vec![
            ("id", JsonValue::from(self.id)),
            ("ok", JsonValue::from(self.ok)),
            ("severity", JsonValue::from(self.severity)),
            ("detail", JsonValue::from(self.detail.clone())),
            ("problem", JsonValue::opt_str(self.problem.as_deref())),
            ("fix", JsonValue::opt_str(self.fix.as_deref())),
        ])
    }
}

/// Build `doctor --json` report (`schema: "ap.doctor/1"`).
pub fn doctor_report_json(config_path: Option<&Path>) -> (JsonValue, bool) {
    let mut checks: Vec<DoctorCheck> = Vec::new();

    // 1. OS & architecture
    let os_ok = cfg!(target_os = "windows") && cfg!(target_arch = "x86_64");
    if os_ok {
        checks.push(DoctorCheck::pass(
            "os",
            format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
        ));
    } else {
        checks.push(DoctorCheck::fail(
            "os",
            "error",
            format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
            "Audiophile Player requires Windows 10/11 64-bit (x86_64)",
            "Run on a 64-bit Windows 10 or Windows 11 PC",
        ));
    }

    // 2. Toolchain checks (only when running inside a source checkout with Cargo.toml)
    let in_source_tree = Path::new("Cargo.toml").is_file() && Path::new("crates").is_dir();
    if in_source_tree {
        for (id, bin, args, fix) in [
            ("toolchain.git", "git", &["--version"][..], "Install Git via `winget configure -f .config\\configuration.winget`"),
            ("toolchain.cargo", "cargo", &["--version"][..], "Install Rust via `winget configure -f .config\\configuration.winget` and ensure %USERPROFILE%\\.cargo\\bin is in PATH"),
            ("toolchain.rustc", "rustc", &["--version"][..], "Run `rustup default stable-x86_64-pc-windows-msvc`"),
        ] {
            match Command::new(bin).args(args).output() {
                Ok(out) if out.status.success() => {
                    let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    checks.push(DoctorCheck::pass(id, ver));
                }
                _ => {
                    checks.push(DoctorCheck::fail(
                        id,
                        "warn",
                        format!("{bin} not found in PATH"),
                        format!("Required source-build tool '{bin}' is missing from PATH"),
                        fix,
                    ));
                }
            }
        }
    }

    // 3. Configuration file parsing & convolution path validation
    let loaded_cfg = PlayerConfig::discover_and_load(config_path);
    let active_cfg = match &loaded_cfg {
        Ok(loaded) => {
            let src = loaded
                .source_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "built-in defaults (no config.toml yet)".to_string());
            match loaded.config.convolution.validate_paths() {
                Ok(()) => checks.push(DoctorCheck::pass("config", format!("valid ({src})"))),
                Err(err) => checks.push(DoctorCheck::fail(
                    "config",
                    "error",
                    src,
                    err,
                    "Fix [convolution] paths in config.toml or run `audiophile-player config set convolution.enabled false`",
                )),
            }
            Some(loaded.config.clone())
        }
        Err(err) => {
            checks.push(DoctorCheck::fail(
                "config",
                "error",
                "config.toml failed to parse",
                err.clone(),
                "Fix the syntax error in config.toml or run `audiophile-player config show`",
            ));
            None
        }
    };

    // 4. Music folder readable + FLAC presence
    let music_dir = active_cfg
        .as_ref()
        .and_then(|c| c.player.music_dir.clone())
        .or_else(default_user_music_dir);
    match music_dir {
        Some(ref dir) if dir.is_dir() => match discover_flac_files(dir) {
            Ok(files) if !files.is_empty() => {
                checks.push(DoctorCheck::pass(
                    "music_dir",
                    format!("{} ({} FLAC file(s))", dir.display(), files.len()),
                ));
            }
            Ok(_) => {
                checks.push(DoctorCheck::fail(
                    "music_dir",
                    "warn",
                    format!("{} (0 FLAC files found)", dir.display()),
                    "Music directory exists but contains no .flac files",
                    "Set your FLAC folder with `audiophile-player config set music_dir \"D:\\Music\"`",
                ));
            }
            Err(err) => {
                checks.push(DoctorCheck::fail(
                    "music_dir",
                    "error",
                    dir.display().to_string(),
                    format!("Could not read music folder: {err}"),
                    "Check folder permissions or run `audiophile-player config set music_dir \"<PATH>\"`",
                ));
            }
        },
        Some(ref dir) => {
            checks.push(DoctorCheck::fail(
                "music_dir",
                "error",
                dir.display().to_string(),
                format!("Music folder '{}' does not exist", dir.display()),
                "Set an existing folder with `audiophile-player config set music_dir \"<PATH>\"`",
            ));
        }
        None => {
            checks.push(DoctorCheck::fail(
                "music_dir",
                "error",
                "unset",
                "Could not resolve default ~/Music folder",
                "Run `audiophile-player config set music_dir \"<PATH>\"`",
            ));
        }
    }

    // 5. DAC enumeration & capability check (native 8fS preferred; < 8fS supported via last-stage f64 downsampling)
    match probe_all_render_devices() {
        Ok(devs) if devs.is_empty() => {
            checks.push(DoctorCheck::fail(
                "dac",
                "error",
                "0 WDM-KS render devices found",
                "No USB/WDM-KS audio DAC detected",
                "Connect a WDM-KS USB DAC (352.8/384 kHz native preferred; 44.1..192 kHz supported via last-stage f64 downsampling) and run `audiophile-player devices`",
            ));
        }
        Ok(devs) => {
            let cap_any: Vec<&ProbedDeviceInfo> =
                devs.iter().filter(|d| d.supports_any_rate).collect();
            if let Some(chosen) = devs
                .iter()
                .find(|d| d.auto_selected)
                .or_else(|| cap_any.first().copied())
            {
                if chosen.supports_8fs {
                    let note = if chosen.rate_352800 == "busy" {
                        " (currently open/busy by an exclusive player)"
                    } else {
                        " (352.8 & 384 kHz Int32 supported)"
                    };
                    checks.push(DoctorCheck::pass(
                        "dac",
                        format!("'{}'{note}", chosen.friendly_name),
                    ));
                } else if chosen.supports_any_rate {
                    let r44 = chosen
                        .max_rate_44k
                        .map(|r| format!("{r} Hz"))
                        .unwrap_or_else(|| "none".to_string());
                    let r48 = chosen
                        .max_rate_48k
                        .map(|r| format!("{r} Hz"))
                        .unwrap_or_else(|| "none".to_string());
                    let wf = chosen
                        .wire_format_48k
                        .or(chosen.wire_format_44k)
                        .unwrap_or("Int32");
                    checks.push(DoctorCheck::pass(
                        "dac",
                        format!(
                            "'{}' (max 44.1k={r44}, 48k={r48}, {wf}; last-stage f64 downsampling from locked 8fS)",
                            chosen.friendly_name
                        ),
                    ));
                } else {
                    checks.push(DoctorCheck::fail(
                        "dac",
                        "error",
                        format!(
                            "'{}' (352.8k={}, 384k={}, no usable WDM-KS PCM rate found)",
                            chosen.friendly_name, chosen.rate_352800, chosen.rate_384000
                        ),
                        "Auto-selected DAC did not accept any standard WDM-KS PCM rate (44.1..=384 kHz)",
                        "Run `audiophile-player devices` and select a working DAC with `audiophile-player config set device \"<NAME>\"`",
                    ));
                }
            } else {
                checks.push(DoctorCheck::fail(
                    "dac",
                    "error",
                    format!("{} device(s), none usable", devs.len()),
                    "No WDM-KS DAC accepting standard PCM rates was found",
                    "Run `audiophile-player devices` to inspect device capabilities",
                ));
            }
        }
        Err(err) => {
            checks.push(DoctorCheck::fail(
                "dac",
                "error",
                "WDM-KS enumeration error",
                err,
                "Check Windows audio drivers and run `audiophile-player devices`",
            ));
        }
    }

    // 6. Localhost HTTP control port availability (8085..=8199)
    let free_port = (DEFAULT_PORT_START..=DEFAULT_PORT_END)
        .find(|&p| TcpListener::bind(("127.0.0.1", p)).is_ok());
    match free_port {
        Some(p) => checks.push(DoctorCheck::pass(
            "http_port",
            format!("localhost port {p} available (range {DEFAULT_PORT_START}..={DEFAULT_PORT_END})"),
        )),
        None => checks.push(DoctorCheck::fail(
            "http_port",
            "error",
            format!("all ports {DEFAULT_PORT_START}..={DEFAULT_PORT_END} busy"),
            "No free localhost TCP port in default range",
            "Pass `--port <PORT>` or set `[player] port = <PORT>` in config.toml",
        )),
    }

    // 7. Opt-in ACX driver status (strictly read-only diagnostic!)
    let acx_present = is_capture_driver_present();
    checks.push(DoctorCheck::pass(
        "capture_driver",
        if acx_present {
            "opt-in ACX virtual driver (\\\\.\\ApExclusiveAudio) detected (ready for Tidal Exclusive)"
        } else {
            "opt-in ACX virtual driver not installed (not required for local FLAC / UPnP)"
        },
    ));

    let has_errors = checks.iter().any(|c| !c.ok && c.severity == "error");
    let report = JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.doctor/1")),
        ("ok", JsonValue::from(!has_errors)),
        ("version", JsonValue::from(env!("CARGO_PKG_VERSION"))),
        ("install_kind", JsonValue::from(detect_install_kind())),
        (
            "checks",
            JsonValue::Array(checks.iter().map(DoctorCheck::to_json).collect()),
        ),
    ]);
    (report, !has_errors)
}

#[cfg(windows)]
fn is_capture_driver_present() -> bool {
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_SHARING_VIOLATION: u32 = 32;
    let invalid_handle: *mut c_void = (-1isize) as *mut c_void;

    unsafe extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: *mut c_void,
        ) -> *mut c_void;
        fn CloseHandle(h_object: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    let wide: Vec<u16> = OsStr::new(r"\\.\ApExclusiveAudio")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated UTF-16 and desired_access=0 probes device existence only.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle != invalid_handle && !handle.is_null() {
        // SAFETY: `handle` is a valid Win32 handle returned by `CreateFileW`.
        unsafe {
            CloseHandle(handle);
        }
        true
    } else {
        let err = unsafe { GetLastError() };
        err == ERROR_ACCESS_DENIED || err == ERROR_SHARING_VIOLATION
    }
}

#[cfg(not(windows))]
fn is_capture_driver_present() -> bool {
    false
}

pub fn run_doctor_cli(config_path: Option<&Path>, as_json: bool) -> Result<(), CliError> {
    let (report, all_ok) = doctor_report_json(config_path);
    if as_json {
        println!("{}", report.to_pretty_string());
    } else if let Some(checks) = report.get("checks").and_then(JsonValue::as_array) {
        println!(
            "Audiophile Player Doctor v{} ({}):",
            env!("CARGO_PKG_VERSION"),
            detect_install_kind()
        );
        for c in checks {
            let id = c.get("id").and_then(JsonValue::as_str).unwrap_or("?");
            let ok = c.get("ok").and_then(JsonValue::as_bool).unwrap_or(false);
            let sev = c.get("severity").and_then(JsonValue::as_str).unwrap_or("info");
            let detail = c.get("detail").and_then(JsonValue::as_str).unwrap_or("");
            let tag = if ok {
                "OK  "
            } else if sev == "warn" {
                "WARN"
            } else {
                "FAIL"
            };
            println!("  [{tag}] {id:<18} {detail}");
            if let Some(prob) = c.get("problem").and_then(JsonValue::as_str) {
                println!("         problem: {prob}");
            }
            if let Some(fix) = c.get("fix").and_then(JsonValue::as_str) {
                println!("         fix:     {fix}");
            }
        }
    }
    if all_ok {
        Ok(())
    } else {
        Err(CliError::general(
            "doctor reported one or more blocking problems",
        ))
    }
}

/// Resolve the target config file path for `config set` / `config unset`:
/// explicit `--config` > `./audiophile-player.toml` (if exists) > `%APPDATA%\audiophile-player\config.toml`.
pub fn resolve_writable_config_path(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let local = PathBuf::from(LOCAL_CONFIG_FILENAME);
    if local.is_file() {
        return Ok(local);
    }
    PlayerConfig::default_user_config_path().ok_or_else(|| {
        CliError::config("could not determine default config.toml path (%APPDATA% not set)")
    })
}

/// Build `config show` JSON payload (`schema: "ap.config/1"`).
pub fn config_show_json(
    config_path: Option<&Path>,
    include_defaults: bool,
) -> Result<JsonValue, CliError> {
    let loaded = PlayerConfig::discover_and_load(config_path).map_err(CliError::config)?;
    let defaults = PlayerConfig::default();
    let source_label = loaded
        .source_path
        .as_ref()
        .map(|p| format!("file:{}", p.display()))
        .unwrap_or_else(|| "default".to_string());

    let mut entries = Vec::new();
    for &key in ALL_KNOWN_CONFIG_KEYS {
        let cur = loaded.config.get_value_str(key);
        let def = defaults.get_value_str(key);
        let from_file = loaded.source_path.is_some() && cur != def;
        if !include_defaults && !from_file {
            continue;
        }
        entries.push(JsonValue::obj(vec![
            ("key", JsonValue::from(key)),
            ("value", JsonValue::opt_str(cur.as_deref())),
            ("default", JsonValue::opt_str(def.as_deref())),
            (
                "source",
                JsonValue::from(if from_file {
                    source_label.as_str()
                } else {
                    "default"
                }),
            ),
            (
                "restart_required",
                JsonValue::from(crate::cli_spec::config_key_requires_restart(key)),
            ),
        ]));
    }

    Ok(JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.config/1")),
        (
            "config_file",
            JsonValue::opt_str(
                loaded
                    .source_path
                    .as_ref()
                    .map(|p| p.to_string_lossy())
                    .as_deref(),
            ),
        ),
        ("entries", JsonValue::Array(entries)),
    ]))
}

/// Perform validated, comment-preserving `config set <key> <value>` with `.bak` backup.
pub fn config_set_value(
    config_path: Option<&Path>,
    raw_key: &str,
    raw_value: &str,
) -> Result<JsonValue, CliError> {
    let canonical = canonicalize_config_key(raw_key).ok_or_else(|| {
        CliError::usage(
            format!("unknown config key '{raw_key}'"),
            "run `audiophile-player config show --effective` or `audiophile-player help --json`",
        )
    })?;
    let toml_val = format_toml_value_for_key(canonical, raw_value)
        .map_err(|e| CliError::usage(e, "run `audiophile-player help` to inspect valid values"))?;
    let path = resolve_writable_config_path(config_path)?;
    let existing = if path.is_file() {
        Some(
            fs::read_to_string(&path)
                .map_err(|e| CliError::config(format!("failed to read '{}': {e}", path.display())))?,
        )
    } else {
        None
    };
    let updated = upsert_toml_key_preserving_comments(existing.as_deref(), canonical, &toml_val)
        .map_err(CliError::config)?;
    write_config_with_backup(&path, &updated).map_err(CliError::config)?;

    Ok(JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.config_set/1")),
        ("ok", JsonValue::from(true)),
        ("key", JsonValue::from(canonical)),
        ("value", JsonValue::from(raw_value.trim())),
        ("config_file", JsonValue::from(path.to_string_lossy().into_owned())),
        (
            "restart_required",
            JsonValue::from(crate::cli_spec::config_key_requires_restart(canonical)),
        ),
    ]))
}

/// Perform comment-preserving `config unset <key>` with `.bak` backup.
pub fn config_unset_value(
    config_path: Option<&Path>,
    raw_key: &str,
) -> Result<JsonValue, CliError> {
    let canonical = canonicalize_config_key(raw_key).ok_or_else(|| {
        CliError::usage(
            format!("unknown config key '{raw_key}'"),
            "run `audiophile-player config show --effective`",
        )
    })?;
    let path = resolve_writable_config_path(config_path)?;
    if !path.is_file() {
        return Ok(JsonValue::obj(vec![
            ("schema", JsonValue::from("ap.config_unset/1")),
            ("ok", JsonValue::from(true)),
            ("key", JsonValue::from(canonical)),
            ("removed", JsonValue::from(false)),
            ("config_file", JsonValue::from(path.to_string_lossy().into_owned())),
            ("restart_required", JsonValue::from(false)),
        ]));
    }
    let existing = fs::read_to_string(&path)
        .map_err(|e| CliError::config(format!("failed to read '{}': {e}", path.display())))?;
    let (updated, removed) =
        unset_toml_key_preserving_comments(&existing, canonical).map_err(CliError::config)?;
    if removed {
        write_config_with_backup(&path, &updated).map_err(CliError::config)?;
    }
    Ok(JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.config_unset/1")),
        ("ok", JsonValue::from(true)),
        ("key", JsonValue::from(canonical)),
        ("removed", JsonValue::from(removed)),
        ("config_file", JsonValue::from(path.to_string_lossy().into_owned())),
        (
            "restart_required",
            JsonValue::from(removed && crate::cli_spec::config_key_requires_restart(canonical)),
        ),
    ]))
}

pub fn run_config_cli(cmd: ConfigCommand) -> Result<(), CliError> {
    match cmd {
        ConfigCommand::Show {
            config_path,
            effective,
            as_json,
        } => {
            let json = config_show_json(config_path.as_deref(), effective)?;
            if as_json {
                println!("{}", json.to_pretty_string());
                return Ok(());
            }
            let file = json
                .get("config_file")
                .and_then(JsonValue::as_str)
                .unwrap_or("(none — using built-in defaults)");
            println!("Config file: {file}");
            if let Some(entries) = json.get("entries").and_then(JsonValue::as_array) {
                for e in entries {
                    let k = e.get("key").and_then(JsonValue::as_str).unwrap_or("?");
                    let v = e
                        .get("value")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("(unset)");
                    let src = e.get("source").and_then(JsonValue::as_str).unwrap_or("?");
                    println!("  {k:<26} = {v:<24} [{src}]");
                }
            }
            Ok(())
        }
        ConfigCommand::Set {
            key,
            value,
            config_path,
            as_json,
        } => {
            let res = config_set_value(config_path.as_deref(), &key, &value)?;
            if as_json {
                println!("{}", res.to_pretty_string());
            } else {
                let ck = res.get("key").and_then(JsonValue::as_str).unwrap_or(&key);
                let cf = res
                    .get("config_file")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                println!("Set {ck} = {} in {cf} (backup saved as .bak).", value.trim());
                if res
                    .get("restart_required")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(true)
                {
                    println!("Note: run `audiophile-player restart` for the running player to pick up this change.");
                }
            }
            Ok(())
        }
        ConfigCommand::Unset {
            key,
            config_path,
            as_json,
        } => {
            let res = config_unset_value(config_path.as_deref(), &key)?;
            if as_json {
                println!("{}", res.to_pretty_string());
            } else {
                let ck = res.get("key").and_then(JsonValue::as_str).unwrap_or(&key);
                let removed = res
                    .get("removed")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                if removed {
                    println!("Removed {ck} from config.toml (backup saved as .bak).");
                } else {
                    println!("{ck} was not set in config.toml.");
                }
            }
            Ok(())
        }
    }
}
