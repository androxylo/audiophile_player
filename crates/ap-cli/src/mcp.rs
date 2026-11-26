// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Thin stdio MCP supervisor (`audiophile-player mcp`, `QUICK_START_AI_DESIGN.md` §5).
//!
//! Architecture & safety guarantees:
//! - Never opens a DAC or plays audio. Spawns `audiophile-player` as a detached child process
//!   that outlives the MCP session, and communicates via `%LOCALAPPDATA%\audiophile-player\run.json`
//!   and Bearer-authenticated `127.0.0.1` `/api/v1/*` JSON endpoints.
//! - Zero external crates: uses `ap_core::json::JsonValue` for newline-delimited JSON-RPC 2.0.
//! - Tool schemas for `player_start` / `player_restart` are generated directly from `cli_spec.rs`.
//! - Hard safety limits (§5.6):
//!   - Cannot change headroom policy, cannot write into the music library, cannot run arbitrary
//!     shell commands, and cannot install/touch the ACX driver, Test Mode, BitLocker, or certs.
//!   - `config_set` is restricted strictly to keys registered in `cli_spec.rs` / `PlayerConfig`.
//!   - Every mutating tool call is appended with a timestamp to `mcp-audit.log`.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ap_core::config::canonicalize_config_key;
use ap_core::json::JsonValue;
use ap_core::track_db::now_unix_secs;

use crate::cli_args::{CommonPaths, FiltersAction, FiltersCommand};
use crate::cli_spec::{
    self, OPTIONS, ValueKind, find_option, mcp_play_options_schema, render_detail_help,
    render_help_json,
};
use crate::diag::{config_set_value, config_show_json, devices_report_json, doctor_report_json};
use crate::exit_codes::CliError;
use crate::filters_cmd;
use crate::lifecycle::{
    api_v1_request, mcp_audit_log_path, query_running_status,
    spawn_detached_player, stop_running_instance,
};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

const DOC_GETTING_STARTED: &str = include_str!("../../../docs/GETTING_STARTED.md");
const DOC_ULTIMATE_MODE: &str = include_str!("../../../docs/ULTIMATE_MODE.md");
const DOC_SOUND_ENGINE: &str = include_str!("../../../docs/SOUND_ENGINE.md");

struct BackgroundScanJob {
    id: String,
    folder: String,
    started_unix_s: u64,
    child: Child,
    finished: bool,
    exit_code: Option<i32>,
    cancelled: bool,
}

pub struct McpServer {
    audit_path: Option<PathBuf>,
    scan_jobs: HashMap<String, BackgroundScanJob>,
    next_job_seq: u64,
}

impl McpServer {
    pub fn new(audit_path: Option<PathBuf>) -> Self {
        Self {
            audit_path,
            scan_jobs: HashMap::new(),
            next_job_seq: 1,
        }
    }

    fn audit(&self, tool: &str, args: &JsonValue) {
        let Some(ref path) = self.audit_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(
                f,
                "ts={} tool={} args={}",
                now_unix_secs(),
                tool,
                args.to_compact_string()
            );
        }
    }

    /// Process one newline-delimited JSON-RPC 2.0 request. Returns `None` for notifications.
    pub fn handle_json_line(&mut self, line: &str) -> Option<JsonValue> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        let req = match JsonValue::parse(trimmed) {
            Ok(v) => v,
            Err(err) => {
                return Some(jsonrpc_error(JsonValue::Null, -32700, &format!("parse error: {err}")));
            }
        };
        let id = req.get("id").cloned();
        let Some(method) = req.get("method").and_then(JsonValue::as_str) else {
            return id.map(|i| jsonrpc_error(i, -32600, "invalid request: missing 'method'"));
        };
        let params = req
            .get("params")
            .cloned()
            .unwrap_or_else(|| JsonValue::Object(Vec::new()));

        // Notifications have no `id` and expect no reply.
        if id.is_none() {
            return None;
        }
        let req_id = id.unwrap();

        match method {
            "initialize" => Some(jsonrpc_ok(
                req_id,
                JsonValue::obj(vec![
                    ("protocolVersion", JsonValue::from(MCP_PROTOCOL_VERSION)),
                    (
                        "capabilities",
                        JsonValue::obj(vec![
                            ("tools", JsonValue::Object(Vec::new())),
                            ("resources", JsonValue::Object(Vec::new())),
                            ("prompts", JsonValue::Object(Vec::new())),
                        ]),
                    ),
                    (
                        "serverInfo",
                        JsonValue::obj(vec![
                            ("name", JsonValue::from("audiophile-player")),
                            ("version", JsonValue::from(env!("CARGO_PKG_VERSION"))),
                        ]),
                    ),
                ]),
            )),
            "ping" => Some(jsonrpc_ok(req_id, JsonValue::Object(Vec::new()))),
            "tools/list" => Some(jsonrpc_ok(
                req_id,
                JsonValue::obj(vec![("tools", JsonValue::Array(tool_definitions()))]),
            )),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| JsonValue::Object(Vec::new()));
                match self.dispatch_tool(name, &arguments) {
                    Ok(payload) => Some(jsonrpc_ok(req_id, mcp_tool_text_result(&payload, false))),
                    Err(err_msg) => {
                        let err_obj = JsonValue::obj(vec![
                            ("ok", JsonValue::from(false)),
                            ("error", JsonValue::from(err_msg)),
                        ]);
                        Some(jsonrpc_ok(req_id, mcp_tool_text_result(&err_obj, true)))
                    }
                }
            }
            "resources/list" => Some(jsonrpc_ok(
                req_id,
                JsonValue::obj(vec![("resources", JsonValue::Array(resource_definitions()))]),
            )),
            "resources/read" => {
                let uri = params
                    .get("uri")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                match read_resource(uri) {
                    Ok((mime, text)) => Some(jsonrpc_ok(
                        req_id,
                        JsonValue::obj(vec![(
                            "contents",
                            JsonValue::Array(vec![JsonValue::obj(vec![
                                ("uri", JsonValue::from(uri)),
                                ("mimeType", JsonValue::from(mime)),
                                ("text", JsonValue::from(text)),
                            ])]),
                        )]),
                    )),
                    Err(msg) => Some(jsonrpc_error(req_id, -32602, &msg)),
                }
            }
            "prompts/list" => Some(jsonrpc_ok(
                req_id,
                JsonValue::obj(vec![("prompts", JsonValue::Array(prompt_definitions()))]),
            )),
            "prompts/get" => {
                let name = params
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                match get_prompt(name) {
                    Ok(prompt_val) => Some(jsonrpc_ok(req_id, prompt_val)),
                    Err(msg) => Some(jsonrpc_error(req_id, -32602, &msg)),
                }
            }
            other => Some(jsonrpc_error(
                req_id,
                -32601,
                &format!("method not found: {other}"),
            )),
        }
    }

    fn dispatch_tool(&mut self, name: &str, args: &JsonValue) -> Result<JsonValue, String> {
        match name {
            "player_status" => match query_running_status() {
                Ok((_rs, val)) => Ok(val),
                Err(err) => Ok(JsonValue::obj(vec![
                    ("schema", JsonValue::from("ap.status/1")),
                    ("running", JsonValue::from(false)),
                    ("message", JsonValue::from(err.message)),
                ])),
            },
            "player_start" => {
                self.audit(name, args);
                if query_running_status().is_ok() {
                    return Err(
                        "Audiophile Player is already running; use `player_restart` or `player_stop` first"
                            .to_string(),
                    );
                }
                let opts_val = args
                    .get("options")
                    .cloned()
                    .unwrap_or_else(|| JsonValue::Object(Vec::new()));
                let argv = options_json_to_argv(&opts_val)?;
                let rs = spawn_detached_player(&argv, Duration::from_secs(15))
                    .map_err(|e| e.message)?;
                Ok(JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("pid", JsonValue::from(rs.pid)),
                    ("url", JsonValue::from(rs.url)),
                    ("effective_options", rs.effective_options),
                ]))
            }
            "player_stop" => {
                self.audit(name, args);
                let when = args
                    .get("when")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("now");
                let after_track = crate::cli_args::parse_when(when)?;
                if after_track {
                    // Review #9 Q5: do not block the single-threaded MCP loop for 10+ minutes on
                    // long tracks; schedule deferred shutdown and return immediately if still playing.
                    crate::lifecycle::request_deferred_stop().map_err(|e| e.message)
                } else {
                    stop_running_instance(false).map_err(|e| e.message)
                }
            }
            "player_restart" => {
                self.audit(name, args);
                let when = args
                    .get("when")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("after_track");
                let after_track = crate::cli_args::parse_when(when)?;
                let changes = args
                    .get("changes")
                    .cloned()
                    .unwrap_or_else(|| JsonValue::Object(Vec::new()));
                let (flags, clear_flags) = options_json_to_argv_with_clears(&changes)?;
                crate::lifecycle::restart_running_instance_with_clears(
                    &flags,
                    &clear_flags,
                    after_track,
                )
                .map_err(|e| e.message)
            }
            "play" => {
                self.audit(name, args);
                call_running_api("POST", "/api/v1/play", Some(args))
            }
            "play_file" => {
                self.audit(name, args);
                call_running_api("POST", "/api/v1/play-file", Some(args))
            }
            "pause" | "resume" | "next" => {
                self.audit(name, args);
                call_running_api("POST", &format!("/api/v1/{name}"), None)
            }
            "options_list" => {
                let group_filter = args.get("group").and_then(JsonValue::as_str);
                let items: Vec<JsonValue> = OPTIONS
                    .iter()
                    .filter(|o| {
                        group_filter.is_none_or(|g| o.group.slug().eq_ignore_ascii_case(g))
                    })
                    .map(|o| {
                        JsonValue::obj(vec![
                            ("name", JsonValue::from(o.name)),
                            ("group", JsonValue::from(o.group.slug())),
                            ("config_key", JsonValue::opt_str(o.config_key)),
                            ("default", JsonValue::opt_str(o.default)),
                            ("restart", JsonValue::from(o.restart.tag())),
                            ("summary", JsonValue::from(o.summary)),
                        ])
                    })
                    .collect();
                Ok(JsonValue::obj(vec![
                    ("schema", JsonValue::from("ap.options_list/1")),
                    ("options", JsonValue::Array(items)),
                ]))
            }
            "option_describe" => {
                let opt_name = args
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'name' parameter".to_string())?;
                let detail = render_detail_help(opt_name)?;
                Ok(JsonValue::obj(vec![
                    ("name", JsonValue::from(opt_name)),
                    ("description", JsonValue::from(detail)),
                ]))
            }
            "config_get" => {
                let key_filter = args.get("key").and_then(JsonValue::as_str);
                let full = config_show_json(None, true).map_err(|e| e.message)?;
                if let Some(raw_k) = key_filter {
                    let canon = canonicalize_config_key(raw_k)
                        .ok_or_else(|| format!("unknown config key '{raw_k}'"))?;
                    let found = full
                        .get("entries")
                        .and_then(JsonValue::as_array)
                        .and_then(|arr| {
                            arr.iter()
                                .find(|e| e.get("key").and_then(JsonValue::as_str) == Some(canon))
                                .cloned()
                        })
                        .ok_or_else(|| format!("config key '{canon}' not found"))?;
                    Ok(found)
                } else {
                    Ok(full)
                }
            }
            "config_set" => {
                self.audit(name, args);
                let key = args
                    .get("key")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'key' parameter".to_string())?;
                let value = args
                    .get("value")
                    .and_then(|v| match v {
                        JsonValue::Str(s) => Some(s.clone()),
                        JsonValue::Bool(b) => Some(b.to_string()),
                        JsonValue::Int(n) => Some(n.to_string()),
                        _ => None,
                    })
                    .ok_or_else(|| "missing required 'value' parameter".to_string())?;
                let mut res = config_set_value(None, key, &value).map_err(|e| e.message)?;
                res.insert(
                    "suggested_action",
                    JsonValue::from("player_restart({changes: {}, when: \"after_track\"})"),
                );
                Ok(res)
            }
            "scan_start" => {
                self.audit(name, args);
                let folder = args
                    .get("folder")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'folder' parameter".to_string())?;
                let force = args
                    .get("force")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let post_chain = args
                    .get("post_chain")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let jobs = args
                    .get("jobs")
                    .and_then(JsonValue::as_u64)
                    .map(|n| n.clamp(1, 16) as usize);

                let exe = std::env::current_exe()
                    .map_err(|e| format!("failed locating audiophile-player exe: {e}"))?;
                let mut cmd = Command::new(exe);
                cmd.arg("scan").arg(folder);
                if force {
                    cmd.arg("--force");
                }
                if post_chain {
                    cmd.arg("--post-chain");
                }
                if let Some(j) = jobs {
                    cmd.arg("--jobs").arg(j.to_string());
                }
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let child = cmd
                    .spawn()
                    .map_err(|e| format!("failed spawning scan worker: {e}"))?;

                let job_id = format!("scan-{}", self.next_job_seq);
                self.next_job_seq += 1;
                let pid = child.id();
                self.scan_jobs.insert(
                    job_id.clone(),
                    BackgroundScanJob {
                        id: job_id.clone(),
                        folder: folder.to_string(),
                        started_unix_s: now_unix_secs(),
                        child,
                        finished: false,
                        exit_code: None,
                        cancelled: false,
                    },
                );
                Ok(JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("job", JsonValue::from(job_id)),
                    ("pid", JsonValue::from(pid)),
                    ("folder", JsonValue::from(folder)),
                ]))
            }
            "scan_progress" => {
                let job_id = args
                    .get("job")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'job' parameter".to_string())?;
                let job = self
                    .scan_jobs
                    .get_mut(job_id)
                    .ok_or_else(|| format!("unknown scan job '{job_id}'"))?;
                if !job.finished {
                    if let Ok(Some(st)) = job.child.try_wait() {
                        job.finished = true;
                        job.exit_code = st.code();
                    }
                }
                Ok(JsonValue::obj(vec![
                    ("job", JsonValue::from(job.id.clone())),
                    ("folder", JsonValue::from(job.folder.clone())),
                    ("started_unix_s", JsonValue::from(job.started_unix_s)),
                    ("running", JsonValue::from(!job.finished)),
                    ("cancelled", JsonValue::from(job.cancelled)),
                    (
                        "exit_code",
                        job.exit_code
                            .map(|c| JsonValue::from(i64::from(c)))
                            .unwrap_or(JsonValue::Null),
                    ),
                ]))
            }
            "scan_cancel" => {
                self.audit(name, args);
                let job_id = args
                    .get("job")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'job' parameter".to_string())?;
                let job = self
                    .scan_jobs
                    .get_mut(job_id)
                    .ok_or_else(|| format!("unknown scan job '{job_id}'"))?;
                if !job.finished {
                    let _ = job.child.kill();
                    let _ = job.child.wait();
                    job.finished = true;
                    job.cancelled = true;
                }
                Ok(JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("job", JsonValue::from(job_id)),
                    ("cancelled", JsonValue::from(true)),
                ]))
            }
            "filters_show" => {
                let path = args
                    .get("path")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'path' parameter".to_string())?;
                run_filters_tool(FiltersAction::Show { target: PathBuf::from(path) }, &[])
            }
            "filters_set" => {
                self.audit(name, args);
                let path = args
                    .get("path")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'path' parameter".to_string())?;
                let filter = args
                    .get("filter")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| "missing required 'filter' parameter".to_string())?;
                let note = args
                    .get("note")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string);
                run_filters_tool(
                    FiltersAction::Set {
                        target: PathBuf::from(path),
                        filter: filter.to_string(),
                        note,
                    },
                    &[("restart_required", JsonValue::from(false))],
                )
            }
            "filters_diff" => run_filters_tool(FiltersAction::Diff, &[]),
            "filters_check" => run_filters_tool(FiltersAction::Check, &[]),
            "doctor" => {
                let (report, _) = doctor_report_json(None);
                Ok(report)
            }
            "devices_list" => devices_report_json().map_err(|e| e.message),
            "logs_tail" => {
                let lines_req = args
                    .get("lines")
                    .and_then(JsonValue::as_u64)
                    .unwrap_or(25)
                    .clamp(1, 200) as usize;
                let (_, status) = query_running_status().map_err(|e| e.message)?;
                let mut actions: Vec<JsonValue> = status
                    .get("recent_actions")
                    .and_then(JsonValue::as_array)
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                if actions.len() > lines_req {
                    let keep_from = actions.len() - lines_req;
                    actions = actions.split_off(keep_from);
                }
                Ok(JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("lines", JsonValue::Array(actions)),
                ]))
            }
            other => Err(format!("unknown MCP tool '{other}'")),
        }
    }
}

/// Convert a validated JSON options object (keys matching `cli_spec.rs` play options) into argv flags.
pub fn options_json_to_argv(obj: &JsonValue) -> Result<Vec<String>, String> {
    options_json_to_argv_with_clears(obj).map(|(flags, _)| flags)
}

/// Convert a validated JSON options object into `(add_flags, clear_flags)` so `false` booleans on
/// negative flags (`--no-upnp`, `--no-capture`, `--no-conv`, etc.) cleanly undo them on restart
/// (Review #9 Q1 + Q7), and run full `validate_play_argv` so parser constraints (e.g. `--dac-bits`
/// in `{16, 18, 20, 24}`) are enforced upfront.
pub fn options_json_to_argv_with_clears(
    obj: &JsonValue,
) -> Result<(Vec<String>, Vec<String>), String> {
    let Some(entries) = obj.as_object() else {
        return Err("options must be a JSON object".to_string());
    };
    let mut argv = Vec::new();
    let mut clear_flags = Vec::new();
    for (raw_key, val) in entries {
        let flag_name = format!("--{}", raw_key.trim().replace('_', "-"));
        let Some(spec) = find_option(&flag_name) else {
            return Err(format!("unknown play option '{raw_key}'"));
        };
        if !spec.scope.contains(&cli_spec::Cmd::Play)
            || matches!(spec.name, "--help" | "--version" | "--json" | "--idle")
        {
            return Err(format!(
                "option '{}' is not allowed for player start/restart",
                spec.name
            ));
        }
        match spec.value {
            ValueKind::Flag => {
                let b = val
                    .as_bool()
                    .ok_or_else(|| format!("option '{raw_key}' expects a boolean"))?;
                if b {
                    argv.push(spec.name.to_string());
                } else {
                    // Undo this flag from previous argv (and emit its inverse if one exists).
                    clear_flags.push(spec.name.to_string());
                    if spec.name == "--ultimate" {
                        argv.push("--no-ultimate".to_string());
                    }
                }
            }
            ValueKind::Str(_) | ValueKind::Path(_) => {
                let s = val
                    .as_str()
                    .ok_or_else(|| format!("option '{raw_key}' expects a string"))?;
                argv.push(spec.name.to_string());
                argv.push(s.to_string());
            }
            ValueKind::Enum { values, .. } => {
                let s = val
                    .as_str()
                    .ok_or_else(|| format!("option '{raw_key}' expects a string"))?;
                if !values.iter().any(|v| v.eq_ignore_ascii_case(s)) {
                    return Err(format!(
                        "invalid value '{s}' for '{raw_key}'; expected one of: {}",
                        values.join(", ")
                    ));
                }
                argv.push(spec.name.to_string());
                argv.push(s.to_string());
            }
            ValueKind::Int { min, max, .. } => {
                let n = val
                    .as_i64()
                    .ok_or_else(|| format!("option '{raw_key}' expects an integer"))?;
                if n < min || n > max {
                    return Err(format!(
                        "value {n} for '{raw_key}' out of range {min}..={max}"
                    ));
                }
                argv.push(spec.name.to_string());
                argv.push(n.to_string());
            }
        }
    }
    if !argv.is_empty() {
        crate::cli_args::validate_play_argv(&argv)?;
    }
    Ok((argv, clear_flags))
}

fn jsonrpc_ok(id: JsonValue, result: JsonValue) -> JsonValue {
    JsonValue::obj(vec![
        ("jsonrpc", JsonValue::from("2.0")),
        ("id", id),
        ("result", result),
    ])
}

fn jsonrpc_error(id: JsonValue, code: i64, message: &str) -> JsonValue {
    JsonValue::obj(vec![
        ("jsonrpc", JsonValue::from("2.0")),
        ("id", id),
        (
            "error",
            JsonValue::obj(vec![
                ("code", JsonValue::from(code)),
                ("message", JsonValue::from(message)),
            ]),
        ),
    ])
}

fn mcp_tool_text_result(payload: &JsonValue, is_error: bool) -> JsonValue {
    JsonValue::obj(vec![
        (
            "content",
            JsonValue::Array(vec![JsonValue::obj(vec![
                ("type", JsonValue::from("text")),
                ("text", JsonValue::from(payload.to_pretty_string())),
            ])]),
        ),
        ("isError", JsonValue::from(is_error)),
    ])
}

fn call_running_api(
    method: &str,
    path: &str,
    body: Option<&JsonValue>,
) -> Result<JsonValue, String> {
    let (rs, _) = query_running_status().map_err(|e| e.message)?;
    let (code, resp) = api_v1_request(&rs.url, &rs.token, method, path, body)?;
    if code >= 400 {
        return Err(format!("HTTP {code}: {}", resp.to_compact_string()));
    }
    Ok(resp)
}

fn run_filters_tool(
    action: FiltersAction,
    extra: &[(&'static str, JsonValue)],
) -> Result<JsonValue, String> {
    let out = filters_cmd::run_filters_capture(FiltersCommand {
        paths: CommonPaths::default(),
        action,
    })?;
    let mut fields = Vec::with_capacity(2 + extra.len());
    fields.push(("ok", JsonValue::from(true)));
    fields.extend(extra.iter().cloned());
    fields.push(("output", JsonValue::from(out)));
    Ok(JsonValue::obj(fields))
}

fn tool_def(name: &str, description: &str, input_schema: JsonValue) -> JsonValue {
    JsonValue::obj(vec![
        ("name", JsonValue::from(name)),
        ("description", JsonValue::from(description)),
        ("inputSchema", input_schema),
    ])
}

fn prop(ty: &str) -> JsonValue {
    JsonValue::obj(vec![("type", JsonValue::from(ty))])
}

fn enum_prop(values: &[&str]) -> JsonValue {
    JsonValue::obj(vec![
        ("type", JsonValue::from("string")),
        (
            "enum",
            JsonValue::Array(values.iter().map(|&v| JsonValue::from(v)).collect()),
        ),
    ])
}

fn obj_schema(props: Vec<(&'static str, JsonValue)>, required: &[&str]) -> JsonValue {
    let mut fields = vec![("type", JsonValue::from("object"))];
    if !required.is_empty() {
        fields.push((
            "required",
            JsonValue::Array(required.iter().map(|&r| JsonValue::from(r)).collect()),
        ));
    }
    fields.push(("properties", JsonValue::obj(props)));
    JsonValue::obj(fields)
}

fn empty_schema() -> JsonValue {
    obj_schema(Vec::new(), &[])
}

fn tool_definitions() -> Vec<JsonValue> {
    let play_opts_schema = mcp_play_options_schema();
    vec![
        tool_def(
            "player_status",
            "Check whether Audiophile Player is running, its PID, Web UI URL, effective options, now playing track, and DAC lock status.",
            empty_schema(),
        ),
        tool_def(
            "player_start",
            "Start Audiophile Player as a detached process (outlives MCP session) with validated options from cli_spec.",
            obj_schema(vec![("options", play_opts_schema.clone())], &[]),
        ),
        tool_def(
            "player_stop",
            "Gracefully stop the running player and release the exclusive DAC pin.",
            obj_schema(vec![("when", enum_prop(&["now", "after_track"]))], &[]),
        ),
        tool_def(
            "player_restart",
            "Gracefully restart Audiophile Player with updated options (defaults to when='after_track' so the current track finishes cleanly).",
            obj_schema(
                vec![
                    ("changes", play_opts_schema),
                    ("when", enum_prop(&["after_track", "now"])),
                ],
                &[],
            ),
        ),
        tool_def(
            "play",
            "Play a folder under the configured music root in random or sequential order.",
            obj_schema(
                vec![
                    ("folder", prop("string")),
                    ("order", enum_prop(&["random", "sequential"])),
                ],
                &[],
            ),
        ),
        tool_def(
            "play_file",
            "Play one specific FLAC file inside the configured music root.",
            obj_schema(vec![("path", prop("string"))], &["path"]),
        ),
        tool_def("pause", "Pause playback immediately and release the WDM-KS DAC pin.", empty_schema()),
        tool_def("resume", "Resume playback after Pause.", empty_schema()),
        tool_def("next", "Skip cleanly to the next track.", empty_schema()),
        tool_def(
            "options_list",
            "List CLI options directly from cli_spec.rs (optionally filtered by group: output|dsp|ultimate|library|network|capture|general).",
            obj_schema(vec![("group", prop("string"))], &[]),
        ),
        tool_def(
            "option_describe",
            "Show detailed documentation, default, config key, and examples for any CLI option or command.",
            obj_schema(vec![("name", prop("string"))], &["name"]),
        ),
        tool_def(
            "config_get",
            "Read effective configuration values and their provenance from config.toml.",
            obj_schema(vec![("key", prop("string"))], &[]),
        ),
        tool_def(
            "config_set",
            "Validate and write a config.toml key (preserving comments + creating .bak). Returns restart_required.",
            obj_schema(
                vec![("key", prop("string")), ("value", prop("string"))],
                &["key", "value"],
            ),
        ),
        tool_def(
            "scan_start",
            "Start a read-only background library scan job writing Schema-2 filterdb album YAMLs.",
            obj_schema(
                vec![
                    ("folder", prop("string")),
                    ("force", prop("boolean")),
                    ("post_chain", prop("boolean")),
                    ("jobs", prop("integer")),
                ],
                &["folder"],
            ),
        ),
        tool_def(
            "scan_progress",
            "Check status of a background scan job started by scan_start.",
            obj_schema(vec![("job", prop("string"))], &["job"]),
        ),
        tool_def(
            "scan_cancel",
            "Cancel a running background scan job.",
            obj_schema(vec![("job", prop("string"))], &["job"]),
        ),
        tool_def(
            "filters_show",
            "Show active (mine) vs machine-suggested filter, rule, and why for a file or album folder.",
            obj_schema(vec![("path", prop("string"))], &["path"]),
        ),
        tool_def(
            "filters_set",
            "Set a per-folder or per-track filter choice in my-filters.yaml (hot-reloaded next track, no restart).",
            obj_schema(
                vec![
                    ("path", prop("string")),
                    ("filter", prop("string")),
                    ("note", prop("string")),
                ],
                &["path", "filter"],
            ),
        ),
        tool_def("filters_diff", "List all scanned tracks where user choices differ from machine suggestions.", empty_schema()),
        tool_def("filters_check", "Validate my-filters.yaml against library.yaml.", empty_schema()),
        tool_def("doctor", "Run non-destructive system, toolchain, DAC, config, and music folder checks.", empty_schema()),
        tool_def("devices_list", "List WDM-KS audio render devices and probe 352.8 / 384 kHz 8fS support.", empty_schema()),
        tool_def(
            "logs_tail",
            "Return the most recent action log lines from the running player.",
            obj_schema(vec![("lines", prop("integer"))], &[]),
        ),
    ]
}

fn resource_definitions() -> Vec<JsonValue> {
    vec![
        JsonValue::obj(vec![
            ("uri", JsonValue::from("ap://cli/spec")),
            ("name", JsonValue::from("CLI & Config Specification")),
            ("mimeType", JsonValue::from("application/json")),
        ]),
        JsonValue::obj(vec![
            ("uri", JsonValue::from("ap://status")),
            ("name", JsonValue::from("Live Player Status")),
            ("mimeType", JsonValue::from("application/json")),
        ]),
        JsonValue::obj(vec![
            ("uri", JsonValue::from("ap://docs/getting-started")),
            ("name", JsonValue::from("Getting Started Guide")),
            ("mimeType", JsonValue::from("text/markdown")),
        ]),
        JsonValue::obj(vec![
            ("uri", JsonValue::from("ap://docs/ultimate-mode")),
            ("name", JsonValue::from("Ultimate Mode Guide")),
            ("mimeType", JsonValue::from("text/markdown")),
        ]),
        JsonValue::obj(vec![
            ("uri", JsonValue::from("ap://docs/sound-engine")),
            ("name", JsonValue::from("Sound Engine Guide")),
            ("mimeType", JsonValue::from("text/markdown")),
        ]),
    ]
}

fn read_resource(uri: &str) -> Result<(&'static str, String), String> {
    match uri {
        "ap://cli/spec" => Ok(("application/json", render_help_json().to_pretty_string())),
        "ap://status" => {
            let val = match query_running_status() {
                Ok((_, v)) => v,
                Err(e) => JsonValue::obj(vec![
                    ("schema", JsonValue::from("ap.status/1")),
                    ("running", JsonValue::from(false)),
                    ("message", JsonValue::from(e.message)),
                ]),
            };
            Ok(("application/json", val.to_pretty_string()))
        }
        "ap://docs/getting-started" => Ok(("text/markdown", DOC_GETTING_STARTED.to_string())),
        "ap://docs/ultimate-mode" => Ok(("text/markdown", DOC_ULTIMATE_MODE.to_string())),
        "ap://docs/sound-engine" => Ok(("text/markdown", DOC_SOUND_ENGINE.to_string())),
        other => Err(format!("unknown MCP resource URI '{other}'")),
    }
}

fn prompt_definitions() -> Vec<JsonValue> {
    vec![
        JsonValue::obj(vec![
            ("name", JsonValue::from("ap-setup")),
            (
                "description",
                JsonValue::from("Guide first-time setup: run doctor, pick 8fS DAC and music folder, configure, and start."),
            ),
        ]),
        JsonValue::obj(vec![
            ("name", JsonValue::from("ap-ultimate-on")),
            (
                "description",
                JsonValue::from("Enable Ultimate Mode cleanly and restart after the current track finishes."),
            ),
        ]),
        JsonValue::obj(vec![
            ("name", JsonValue::from("ap-scan-library")),
            (
                "description",
                JsonValue::from("Run a read-only background library scan for Schema-2 filterdb caching."),
            ),
        ]),
    ]
}

fn get_prompt(name: &str) -> Result<JsonValue, String> {
    let text = match name {
        "ap-setup" => "Run `doctor` and `devices_list`, confirm the user's 8fS DAC and FLAC music folder, set them via `config_set`, then start playback with `player_start` and share the localhost Web UI URL.",
        "ap-ultimate-on" => "Set `ultimate.enabled` to `true` via `config_set`, then call `player_restart` with `when: \"after_track\"` so the current song finishes before Ultimate Mode activates.",
        "ap-scan-library" => "Read `player.music_dir` via `config_get`, start an offline read-only scan with `scan_start` (`post_chain: true`), and report progress with `scan_progress`.",
        other => return Err(format!("unknown MCP prompt '{other}'")),
    };
    Ok(JsonValue::obj(vec![
        ("description", JsonValue::from(name)),
        (
            "messages",
            JsonValue::Array(vec![JsonValue::obj(vec![
                ("role", JsonValue::from("user")),
                (
                    "content",
                    JsonValue::obj(vec![
                        ("type", JsonValue::from("text")),
                        ("text", JsonValue::from(text)),
                    ]),
                ),
            ])]),
        ),
    ]))
}

/// Main stdio loop for `audiophile-player mcp`.
pub fn run_mcp_stdio() -> Result<(), CliError> {
    let mut server = McpServer::new(mcp_audit_log_path());
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out_lock = stdout.lock();

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(err) => {
                eprintln!("mcp stdin read error: {err}");
                break;
            }
        };
        if let Some(resp) = server.handle_json_line(&line) {
            let encoded = resp.to_compact_string();
            if writeln!(out_lock, "{encoded}").is_err() || out_lock.flush().is_err() {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_stdio_initialize_tools_resources_prompts_and_options_validation_work() {
        let mut srv = McpServer::new(None);

        // 1. Initialize handshake
        let init_resp = srv
            .handle_json_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .unwrap();
        assert_eq!(
            init_resp
                .get("result")
                .and_then(|r| r.get("serverInfo"))
                .and_then(|s| s.get("name"))
                .and_then(JsonValue::as_str),
            Some("audiophile-player")
        );

        // 2. Notification returns None
        assert!(
            srv.handle_json_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );

        // 3. tools/list returns full toolset
        let tools_resp = srv
            .handle_json_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .unwrap();
        let tools = tools_resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(JsonValue::as_array)
            .unwrap();
        assert!(tools.len() >= 19);

        // 4. tools/call options_list & option_describe
        let opt_resp = srv
            .handle_json_line(
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"option_describe","arguments":{"name":"--filters"}}}"#,
            )
            .unwrap();
        let text = opt_resp
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(JsonValue::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(JsonValue::as_str)
            .unwrap();
        assert!(text.contains("filters.mode"));

        // 5. resources/read ap://cli/spec
        let res_resp = srv
            .handle_json_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"ap://cli/spec"}}"#,
            )
            .unwrap();
        let spec_text = res_resp
            .get("result")
            .and_then(|r| r.get("contents"))
            .and_then(JsonValue::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(JsonValue::as_str)
            .unwrap();
        assert!(spec_text.contains("ap.cli_spec/1"));

        // 6. prompts/get ap-setup
        let pr_resp = srv
            .handle_json_line(
                r#"{"jsonrpc":"2.0","id":5,"method":"prompts/get","params":{"name":"ap-setup"}}"#,
            )
            .unwrap();
        assert!(pr_resp.get("result").is_some());

        // 7. options_json_to_argv validates strictly against cli_spec
        let ok_argv = options_json_to_argv(&JsonValue::obj(vec![
            ("ultimate", JsonValue::from(true)),
            ("filters", JsonValue::from("mine")),
            ("dac_bits", JsonValue::from(20_i64)),
        ]))
        .unwrap();
        assert_eq!(ok_argv, vec!["--ultimate", "--filters", "mine", "--dac-bits", "20"]);
        assert!(
            options_json_to_argv(&JsonValue::obj(vec![("bogus_flag", JsonValue::from(true))]))
                .is_err()
        );
        // Review #9 Q1: invalid dac_bits (17) must be caught upfront by validate_play_argv!
        assert!(
            options_json_to_argv(&JsonValue::obj(vec![("dac_bits", JsonValue::from(17_i64))]))
                .is_err()
        );
        // Review #9 Q7: false on negative flag emits clear_flags so restart can undo it.
        let (add_f, clear_f) = options_json_to_argv_with_clears(&JsonValue::obj(vec![
            ("no_upnp", JsonValue::from(false)),
            ("no_capture", JsonValue::from(false)),
        ]))
        .unwrap();
        assert!(add_f.is_empty());
        assert_eq!(clear_f, vec!["--no-upnp", "--no-capture"]);
    }
}
