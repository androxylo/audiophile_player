// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Engine process lifecycle management (`QUICK_START_AI_DESIGN.md` §§5.2, 5.4):
//! - Single-instance named Windows mutex `Local\AudiophilePlayer.Engine`
//! - Named fallback shutdown event `Local\AudiophilePlayer.Shutdown`
//! - `%LOCALAPPDATA%\audiophile-player\run.json` with 256-bit CSPRNG Bearer token & user-only ACL
//! - Zero-dep localhost HTTP/1.1 client for `/api/v1/*`
//! - CLI commands `status [--json]`, `stop [--when ...] [--json]`, `restart [OPTIONS...] [--when ...] [--json]`

use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ap_core::json::JsonValue;
use ap_core::track_db::atomic_write_yaml;

use crate::exit_codes::CliError;

pub const ENGINE_MUTEX_NAME: &str = "Local\\AudiophilePlayer.Engine";
pub const SHUTDOWN_EVENT_NAME: &str = "Local\\AudiophilePlayer.Shutdown";

/// Path to `%LOCALAPPDATA%\audiophile-player\run.json` (Review #9 Q10: sole runtime discovery file).
pub fn run_state_file_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let trimmed = local.trim();
            if !trimmed.is_empty() {
                return Some(
                    PathBuf::from(trimmed)
                        .join("audiophile-player")
                        .join("run.json"),
                );
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .or_else(|| std::env::var_os("HOME"))
            .map(|dir| PathBuf::from(dir).join("audiophile-player").join("run.json"))
    }
}

/// Path to `%LOCALAPPDATA%\audiophile-player\mcp-audit.log`.
pub fn mcp_audit_log_path() -> Option<PathBuf> {
    run_state_file_path().and_then(|p| p.parent().map(|d| d.join("mcp-audit.log")))
}

/// Parsed contents of `%LOCALAPPDATA%\audiophile-player\run.json`.
#[derive(Debug, Clone)]
pub struct RunStateFile {
    pub pid: u32,
    pub url: String,
    pub token: String,
    pub argv: Vec<String>,
    pub effective_options: JsonValue,
    pub started_unix_s: u64,
    pub version: String,
}

impl RunStateFile {
    pub fn to_json(&self) -> JsonValue {
        JsonValue::obj(vec![
            ("schema", JsonValue::from("ap.run_state/1")),
            ("pid", JsonValue::from(self.pid)),
            ("url", JsonValue::from(self.url.clone())),
            ("token", JsonValue::from(self.token.clone())),
            (
                "argv",
                JsonValue::Array(self.argv.iter().cloned().map(JsonValue::from).collect()),
            ),
            ("effective_options", self.effective_options.clone()),
            ("started_unix_s", JsonValue::from(self.started_unix_s)),
            ("version", JsonValue::from(self.version.clone())),
        ])
    }

    pub fn from_json(val: &JsonValue) -> Result<Self, String> {
        let pid = val
            .get("pid")
            .and_then(JsonValue::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| "run.json missing valid 'pid'".to_string())?;
        let url = val
            .get("url")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "run.json missing 'url'".to_string())?
            .to_string();
        let token = val
            .get("token")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "run.json missing 'token'".to_string())?
            .to_string();
        let argv = val
            .get("argv")
            .and_then(JsonValue::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let effective_options = val
            .get("effective_options")
            .cloned()
            .unwrap_or_else(|| JsonValue::Object(Vec::new()));
        let started_unix_s = val
            .get("started_unix_s")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let version = val
            .get("version")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown")
            .to_string();
        Ok(Self {
            pid,
            url,
            token,
            argv,
            effective_options,
            started_unix_s,
            version,
        })
    }

    pub fn read_default() -> Option<Self> {
        let path = run_state_file_path()?;
        Self::read_from(&path)
    }

    pub fn read_from(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let val = JsonValue::parse(&text).ok()?;
        Self::from_json(&val).ok()
    }
}

/// Generate a cryptographically random 256-bit (64-hex-char) Bearer token.
/// Review #9 Q16: fails closed if the OS CSPRNG fails (never falls back to weak xorshift).
pub fn generate_api_token_hex() -> Result<String, CliError> {
    let mut bytes = [0u8; 32];
    fill_random_bytes(&mut bytes)?;
    let mut hex = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{b:02x}");
    }
    Ok(hex)
}

#[cfg(windows)]
fn fill_random_bytes(dst: &mut [u8]) -> Result<(), CliError> {
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut std::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x00000002;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            dst.as_mut_ptr(),
            dst.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(CliError::general(format!(
            "OS CSPRNG (BCryptGenRandom) failed with NTSTATUS 0x{:08x}; refusing to start with weak API token",
            status as u32
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn fill_random_bytes(dst: &mut [u8]) -> Result<(), CliError> {
    let mut f = fs::File::open("/dev/urandom")
        .map_err(|e| CliError::general(format!("failed to open /dev/urandom: {e}")))?;
    f.read_exact(dst)
        .map_err(|e| CliError::general(format!("failed reading /dev/urandom: {e}")))?;
    Ok(())
}

/// Restrict `run.json` NTFS permissions to the current user (`OWNER_RIGHTS` / current user only).
#[cfg(windows)]
fn restrict_file_acl_to_current_user(path: &Path) {
    // Use `icacls` only if needed? Even cleaner: pure Win32 SDDL via Advapi32 (zero child process!).
    // SDDL "D:P(A;;FA;;;OW)" grants Full Access strictly to Owner (and Protected DACL strips inherited ACEs).
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: u32,
            security_descriptor: *mut *mut std::ffi::c_void,
            security_descriptor_size: *mut u32,
        ) -> i32;
        fn GetSecurityDescriptorDacl(
            p_security_descriptor: *mut std::ffi::c_void,
            lpb_dacl_present: *mut i32,
            p_dacl: *mut *mut std::ffi::c_void,
            lpb_dacl_defaulted: *mut i32,
        ) -> i32;
        fn SetNamedSecurityInfoW(
            p_object_name: *const u16,
            object_type: u32,
            security_info: u32,
            psid_owner: *mut std::ffi::c_void,
            psid_group: *mut std::ffi::c_void,
            p_dacl: *mut std::ffi::c_void,
            p_sacl: *mut std::ffi::c_void,
        ) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LocalFree(h_mem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    }

    const SDDL_REVISION_1: u32 = 1;
    const SE_FILE_OBJECT: u32 = 1;
    const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;
    const PROTECTED_DACL_SECURITY_INFORMATION: u32 = 0x8000_0000;

    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)\0".encode_utf16().collect();
    let mut sd: *mut std::ffi::c_void = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) == 0
            || sd.is_null()
        {
            return;
        }
        let mut present: i32 = 0;
        let mut defaulted: i32 = 0;
        let mut dacl: *mut std::ffi::c_void = std::ptr::null_mut();
        if GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) != 0
            && present != 0
        {
            let mut wide_path: Vec<u16> = path
                .as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .collect();
            wide_path.push(0);
            let _ = SetNamedSecurityInfoW(
                wide_path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            );
        }
        LocalFree(sd);
    }
}

#[cfg(not(windows))]
fn restrict_file_acl_to_current_user(_path: &Path) {}

/// RAII guard writing `run.json` on startup and removing it (if still ours) on teardown/drop.
pub struct RunStateGuard {
    path: Option<PathBuf>,
    pid: u32,
}

impl RunStateGuard {
    pub fn write(path: Option<PathBuf>, state: &RunStateFile) -> Self {
        if let Some(ref p) = path {
            let content = state.to_json().to_pretty_string();
            // Review #9 Q16: create a private temp file, restrict its NTFS ACL to the current user
            // BEFORE writing the secret token or renaming into `run.json`, eliminating any
            // post-rename exposure window.
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let tmp = p.with_file_name(format!("run.json.tmp.{}", state.pid));
            let wrote_privately = (|| -> std::io::Result<()> {
                let _ = fs::remove_file(&tmp);
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&tmp)?;
                restrict_file_acl_to_current_user(&tmp);
                f.write_all(content.as_bytes())?;
                f.sync_all()?;
                drop(f);
                if fs::rename(&tmp, p).is_err() {
                    let _ = fs::remove_file(p);
                    fs::rename(&tmp, p)?;
                }
                Ok(())
            })();
            if wrote_privately.is_err() {
                let _ = fs::remove_file(&tmp);
                if atomic_write_yaml(p, &content).is_ok() {
                    restrict_file_acl_to_current_user(p);
                }
            }
        }
        Self {
            path,
            pid: state.pid,
        }
    }

    pub fn remove_now(&mut self) {
        if let Some(p) = self.path.take() {
            if let Some(cur) = RunStateFile::read_from(&p) {
                if cur.pid == self.pid {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }
}

impl Drop for RunStateGuard {
    fn drop(&mut self) {
        self.remove_now();
    }
}

/// RAII single-instance Windows named mutex (`Local\AudiophilePlayer.Engine`).
#[cfg(windows)]
pub struct SingleInstanceGuard {
    handle: *mut std::ffi::c_void,
}

#[cfg(windows)]
unsafe impl Send for SingleInstanceGuard {}

#[cfg(windows)]
impl SingleInstanceGuard {
    /// Acquire `Local\AudiophilePlayer.Engine`. Returns `Err(CliError::instance)` (exit code 5)
    /// when another engine instance already owns the mutex.
    pub fn acquire() -> Result<Self, CliError> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateMutexW(
                lp_mutex_attributes: *mut std::ffi::c_void,
                b_initial_owner: i32,
                lp_name: *const u16,
            ) -> *mut std::ffi::c_void;
            fn GetLastError() -> u32;
            fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
        }
        const ERROR_ALREADY_EXISTS: u32 = 183;
        let name: Vec<u16> = ENGINE_MUTEX_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            let h = CreateMutexW(std::ptr::null_mut(), 1, name.as_ptr());
            if h.is_null() {
                return Err(CliError::general("failed creating single-instance mutex"));
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                let url_hint = RunStateFile::read_default()
                    .map(|r| format!(" (PID {}, Web UI: {})", r.pid, r.url))
                    .unwrap_or_default();
                return Err(CliError::instance(
                    format!("Audiophile Player is already running{url_hint}"),
                    "run `audiophile-player status`, `audiophile-player stop`, or `audiophile-player restart`",
                ));
            }
            Ok(Self { handle: h })
        }
    }
}

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn ReleaseMutex(h: *mut std::ffi::c_void) -> i32;
            fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            if !self.handle.is_null() {
                ReleaseMutex(self.handle);
                CloseHandle(self.handle);
                self.handle = std::ptr::null_mut();
            }
        }
    }
}

#[cfg(not(windows))]
pub struct SingleInstanceGuard;

#[cfg(not(windows))]
impl SingleInstanceGuard {
    pub fn acquire() -> Result<Self, CliError> {
        Ok(Self)
    }
}

/// Signal the fallback named Windows shutdown event `Local\AudiophilePlayer.Shutdown`.
#[cfg(windows)]
pub fn signal_named_shutdown_event() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenEventW(dw_desired_access: u32, b_inherit_handle: i32, lp_name: *const u16) -> *mut std::ffi::c_void;
        fn SetEvent(h_event: *mut std::ffi::c_void) -> i32;
        fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
    }
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    let name: Vec<u16> = SHUTDOWN_EVENT_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        let ok = SetEvent(h) != 0;
        CloseHandle(h);
        ok
    }
}

#[cfg(not(windows))]
pub fn signal_named_shutdown_event() -> bool {
    false
}

/// Check if a process ID is currently alive on Windows.
#[cfg(windows)]
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(dw_desired_access: u32, b_inherit_handle: i32, dw_process_id: u32) -> *mut std::ffi::c_void;
        fn GetExitCodeProcess(h_process: *mut std::ffi::c_void, lp_exit_code: *mut u32) -> i32;
        fn CloseHandle(h: *mut std::ffi::c_void) -> i32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
pub fn is_pid_alive(pid: u32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// Minimal zero-dependency HTTP/1.1 client strictly targeting localhost `127.0.0.1:<port>` `/api/v1/*`.
pub fn api_v1_request(
    base_url: &str,
    token: &str,
    method: &str,
    path_and_query: &str,
    json_body: Option<&JsonValue>,
) -> Result<(u16, JsonValue), String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let host_port = trimmed
        .strip_prefix("http://")
        .ok_or_else(|| format!("unsupported control URL '{base_url}'"))?;
    if !host_port.starts_with("127.0.0.1:") && !host_port.starts_with("localhost:") {
        return Err(format!("refusing non-localhost control URL '{base_url}'"));
    }

    let mut stream = TcpStream::connect(host_port)
        .map_err(|e| format!("failed connecting to {host_port}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let body_str = json_body
        .map(JsonValue::to_compact_string)
        .unwrap_or_default();
    let req = format!(
        "{method} {path_and_query} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n\
         {body_str}",
        body_str.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("HTTP write failed: {e}"))?;
    let _ = stream.flush();

    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("HTTP read failed: {e}"))?;
    let Some(head_end) = ap_core::http1::find_header_end(&raw) else {
        return Err("malformed HTTP response (missing header separator)".to_string());
    };
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let body = String::from_utf8_lossy(&raw[head_end + 4..]);
    let status_code = ap_core::http1::parse_status_code(&head)
        .ok_or_else(|| "malformed HTTP status line".to_string())?;
    let parsed = if body.trim().is_empty() {
        JsonValue::Object(Vec::new())
    } else {
        JsonValue::parse(&body)?
    };
    Ok((status_code, parsed))
}

/// Query running instance status (returns `Err(CliError::instance)` code 5 when not running).
pub fn query_running_status() -> Result<(RunStateFile, JsonValue), CliError> {
    let Some(rs) = RunStateFile::read_default() else {
        return Err(CliError::instance(
            "Audiophile Player is not running (run.json not found)",
            "start it with `audiophile-player` or `scripts\\launch-audiophile-player.ps1`",
        ));
    };
    if !is_pid_alive(rs.pid) {
        return Err(CliError::instance(
            format!("Audiophile Player is not running (stale run.json for PID {})", rs.pid),
            "start it with `audiophile-player`",
        ));
    }
    match api_v1_request(&rs.url, &rs.token, "GET", "/api/v1/status", None) {
        Ok((200, mut val)) => {
            val.insert("url", JsonValue::from(rs.url.clone()));
            Ok((rs, val))
        }
        Ok((code, val)) => Err(CliError::general(format!(
            "status endpoint returned HTTP {code}: {}",
            val.to_compact_string()
        ))),
        Err(err) => Err(CliError::instance(
            format!("could not reach running player (PID {} at {}): {err}", rs.pid, rs.url),
            "check if the player is still starting up or stopped",
        )),
    }
}

pub fn run_status_cli(as_json: bool) -> Result<(), CliError> {
    match query_running_status() {
        Ok((_rs, val)) => {
            if as_json {
                println!("{}", val.to_pretty_string());
            } else {
                let pid = val.get("pid").and_then(JsonValue::as_u64).unwrap_or(0);
                let url = val.get("url").and_then(JsonValue::as_str).unwrap_or("?");
                let mode = val.get("mode").and_then(JsonValue::as_str).unwrap_or("?");
                let dac = val.get("dac").and_then(JsonValue::as_str).unwrap_or("?");
                let playing = val
                    .get("is_playing")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let track = val
                    .get("current_track")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("(none)");
                println!("Audiophile Player: RUNNING (PID {pid})");
                println!("  Web UI:   {url}");
                println!("  DAC:      {dac} ({})", if playing { "claimed" } else { "released" });
                println!("  Mode:     {mode}");
                println!("  Track:    {track}");
                if let Some(fmt) = val.get("current_format").and_then(JsonValue::as_str) {
                    println!("  Format:   {fmt}");
                }
                if let Some(dec) = val.get("current_decision").and_then(JsonValue::as_str) {
                    println!("  Decision: {dec}");
                }
            }
            Ok(())
        }
        Err(err) => {
            if as_json {
                let not_running = JsonValue::obj(vec![
                    ("schema", JsonValue::from("ap.status/1")),
                    ("running", JsonValue::from(false)),
                    ("message", JsonValue::from(err.message.clone())),
                ]);
                println!("{}", not_running.to_pretty_string());
            }
            Err(err)
        }
    }
}

/// Schedule deferred `after_track` shutdown without blocking the caller for the entire track
/// (used by single-threaded MCP `player_stop(when="after_track")`, Review #9 Q5).
pub fn request_deferred_stop() -> Result<JsonValue, CliError> {
    let Some(rs) = RunStateFile::read_default() else {
        return Err(CliError::instance(
            "Audiophile Player is not running",
            "nothing to stop",
        ));
    };
    if !is_pid_alive(rs.pid) {
        return Err(CliError::instance(
            format!("Audiophile Player (PID {}) is already stopped", rs.pid),
            "nothing to stop",
        ));
    }
    match api_v1_request(
        &rs.url,
        &rs.token,
        "POST",
        "/api/v1/shutdown?when=after_track",
        None,
    ) {
        Ok((code, body)) if code < 400 => {
            let immediate = body
                .get("immediate")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            Ok(JsonValue::obj(vec![
                ("schema", JsonValue::from("ap.stop/1")),
                ("ok", JsonValue::from(true)),
                ("pid", JsonValue::from(rs.pid)),
                ("when", JsonValue::from("after_track")),
                ("pending", JsonValue::from(!immediate)),
                ("exited", JsonValue::from(false)),
            ]))
        }
        _ => stop_running_instance(false),
    }
}

/// Gracefully stop the running engine and wait for process exit (never force-kills!).
///
/// Review #9 Q4 + Q5:
/// - Falls back to the named shutdown event on ANY HTTP error OR HTTP status `>= 400`.
/// - Centralizes timeout policy here: `when=now` waits up to 30 s; `when=after_track` polls
///   `/api/v1/status` and continues waiting while the engine is alive and actively playing a track
///   with `shutdown_after_track: true` (supporting tracks > 10 minutes cleanly!), timing out only
///   if the process stops responding or stays idle without exiting for 30 s.
pub fn stop_running_instance(after_track: bool) -> Result<JsonValue, CliError> {
    let Some(rs) = RunStateFile::read_default() else {
        return Err(CliError::instance(
            "Audiophile Player is not running",
            "nothing to stop",
        ));
    };
    if !is_pid_alive(rs.pid) {
        return Err(CliError::instance(
            format!("Audiophile Player (PID {}) is already stopped", rs.pid),
            "nothing to stop",
        ));
    }

    let when_str = if after_track { "after_track" } else { "now" };
    let api_res = api_v1_request(
        &rs.url,
        &rs.token,
        "POST",
        &format!("/api/v1/shutdown?when={when_str}"),
        None,
    );
    let api_accepted = matches!(api_res, Ok((code, _)) if code < 400);
    if !api_accepted {
        // Fallback: signal Local\AudiophilePlayer.Shutdown event directly.
        if !signal_named_shutdown_event() {
            return Err(CliError::general(format!(
                "could not send graceful shutdown to PID {} via HTTP or named event",
                rs.pid
            )));
        }
    }

    let idle_grace = Duration::from_secs(30);
    let mut idle_deadline = Instant::now() + idle_grace;
    let mut last_poll = Instant::now() - Duration::from_secs(1);
    loop {
        if !is_pid_alive(rs.pid) {
            return Ok(JsonValue::obj(vec![
                ("schema", JsonValue::from("ap.stop/1")),
                ("ok", JsonValue::from(true)),
                ("pid", JsonValue::from(rs.pid)),
                ("when", JsonValue::from(when_str)),
                ("exited", JsonValue::from(true)),
            ]));
        }
        if after_track && api_accepted && last_poll.elapsed() >= Duration::from_millis(500) {
            last_poll = Instant::now();
            if let Ok((200, status)) =
                api_v1_request(&rs.url, &rs.token, "GET", "/api/v1/status", None)
            {
                let still_playing = status
                    .get("is_playing")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                let still_pending = status
                    .get("restart_pending")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
                if still_playing && still_pending {
                    idle_deadline = Instant::now() + idle_grace;
                }
            }
        }
        if Instant::now() >= idle_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Never silently force-kill! Report clearly to caller/agent.
    Err(CliError::general(format!(
        "PID {} did not exit within {} s after becoming idle (when={when_str}); refusing to force-kill silently",
        rs.pid,
        idle_grace.as_secs()
    )))
}

pub fn run_stop_cli(after_track: bool, as_json: bool) -> Result<(), CliError> {
    let res = stop_running_instance(after_track)?;
    if as_json {
        println!("{}", res.to_pretty_string());
    } else {
        let pid = res.get("pid").and_then(JsonValue::as_u64).unwrap_or(0);
        println!("Stopped Audiophile Player (PID {pid}); DAC released.");
    }
    Ok(())
}

/// Spawn a detached `audiophile-player` engine process with `args` and wait until `run.json` +
/// `/api/v1/status` are ready (up to `ready_timeout`).
pub fn spawn_detached_player(args: &[String], ready_timeout: Duration) -> Result<RunStateFile, CliError> {
    let exe = std::env::current_exe()
        .map_err(|e| CliError::general(format!("could not locate current executable: {e}")))?;

    let mut cmd = Command::new(&exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let child = cmd
        .spawn()
        .map_err(|e| CliError::general(format!("failed spawning detached player: {e}")))?;
    let new_pid = child.id();

    let deadline = Instant::now() + ready_timeout;
    while Instant::now() < deadline {
        if !is_pid_alive(new_pid) {
            return Err(CliError::general(format!(
                "newly spawned player (PID {new_pid}) exited prematurely; run `audiophile-player doctor` to inspect"
            )));
        }
        if let Some(rs) = RunStateFile::read_default() {
            if rs.pid == new_pid {
                if let Ok((200, _)) =
                    api_v1_request(&rs.url, &rs.token, "GET", "/api/v1/status", None)
                {
                    return Ok(rs);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(120));
    }

    Err(CliError::general(format!(
        "spawned player (PID {new_pid}) did not become ready within {} s",
        ready_timeout.as_secs()
    )))
}

/// Merge new CLI flags onto a previous argv slice (replacing conflicting flags and applying
/// spec-driven negations cleanly, plus optional `clear_flags` for undoing negative flags on
/// restart — Review #9 Q7).
#[cfg(test)]
pub fn merge_play_argv(prev_argv: &[String], new_flags: &[String]) -> Vec<String> {
    merge_play_argv_with_clears(prev_argv, new_flags, &[])
}

pub fn merge_play_argv_with_clears(
    prev_argv: &[String],
    new_flags: &[String],
    clear_flags: &[String],
) -> Vec<String> {
    // Strip flags in `prev_argv` that are explicitly overridden by `new_flags`, negated via
    // `cli_spec::negated_options`, or explicitly cleared via `clear_flags`.
    let mut override_names: Vec<String> = clear_flags.to_vec();
    let mut i = 0usize;
    while i < new_flags.len() {
        let tok = &new_flags[i];
        if tok.starts_with("--") {
            override_names.push(tok.clone());
            for &neg in crate::cli_spec::negated_options(tok) {
                override_names.push(neg.to_string());
            }
            if crate::cli_spec::find_option(tok)
                .is_some_and(|o| o.value != crate::cli_spec::ValueKind::Flag)
            {
                i += 1;
            }
        }
        i += 1;
    }

    let mut merged = Vec::new();
    let mut j = 0usize;
    while j < prev_argv.len() {
        let tok = &prev_argv[j];
        let takes_val = crate::cli_spec::find_option(tok)
            .is_some_and(|o| o.value != crate::cli_spec::ValueKind::Flag);
        if override_names.iter().any(|o| o == tok) {
            j += if takes_val { 2 } else { 1 };
            continue;
        }
        merged.push(tok.clone());
        if takes_val && j + 1 < prev_argv.len() {
            merged.push(prev_argv[j + 1].clone());
            j += 2;
        } else {
            j += 1;
        }
    }
    merged.extend(new_flags.iter().cloned());
    merged
}

/// Restart the running player gracefully (`after_track` by default), resuming active queue if playing.
pub fn restart_running_instance(
    new_flags: &[String],
    after_track: bool,
) -> Result<JsonValue, CliError> {
    restart_running_instance_with_clears(new_flags, &[], after_track)
}

pub fn restart_running_instance_with_clears(
    new_flags: &[String],
    clear_flags: &[String],
    after_track: bool,
) -> Result<JsonValue, CliError> {
    let (old_rs, old_status) = query_running_status()?;

    // Review #9 Q1: compute AND validate the merged playback argv BEFORE stopping the running
    // player so invalid options (`--version`, `scan`, `--idle`, `--dac-bits 17`, bad config)
    // NEVER leave the player stopped!
    let merged_argv = merge_play_argv_with_clears(&old_rs.argv, new_flags, clear_flags);
    crate::cli_args::validate_play_argv(&merged_argv)
        .map_err(|e| CliError::usage(e, "run `audiophile-player help restart`"))?;

    let was_playing = old_status
        .get("is_playing")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let old_mode = old_status
        .get("mode")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();
    let old_folder = old_status
        .get("active_folder")
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let old_options = old_status
        .get("effective_options")
        .cloned()
        .unwrap_or_else(|| JsonValue::Object(Vec::new()));

    let _ = stop_running_instance(after_track)?;

    let new_rs = spawn_detached_player(&merged_argv, Duration::from_secs(15))?;

    let mut resumed = false;
    if was_playing {
        if let Some(folder) = old_folder {
            let order = if old_mode.to_ascii_lowercase().contains("sequential") {
                "sequential"
            } else {
                "random"
            };
            let play_body = JsonValue::obj(vec![
                ("folder", JsonValue::from(folder)),
                ("order", JsonValue::from(order)),
            ]);
            if api_v1_request(&new_rs.url, &new_rs.token, "POST", "/api/v1/play", Some(&play_body))
                .is_ok()
            {
                resumed = true;
            }
        }
    }

    let new_options = api_v1_request(&new_rs.url, &new_rs.token, "GET", "/api/v1/status", None)
        .ok()
        .and_then(|(_, v)| v.get("effective_options").cloned())
        .unwrap_or_else(|| new_rs.effective_options.clone());

    Ok(JsonValue::obj(vec![
        ("schema", JsonValue::from("ap.restart/1")),
        ("ok", JsonValue::from(true)),
        ("old_pid", JsonValue::from(old_rs.pid)),
        ("new_pid", JsonValue::from(new_rs.pid)),
        ("url", JsonValue::from(new_rs.url)),
        ("when", JsonValue::from(if after_track { "after_track" } else { "now" })),
        ("resumed", JsonValue::from(resumed)),
        ("old_options", old_options),
        ("new_options", new_options),
    ]))
}

pub fn run_restart_cli(
    new_flags: Vec<String>,
    after_track: bool,
    as_json: bool,
) -> Result<(), CliError> {
    let res = restart_running_instance(&new_flags, after_track)?;
    if as_json {
        println!("{}", res.to_pretty_string());
    } else {
        let old_pid = res.get("old_pid").and_then(JsonValue::as_u64).unwrap_or(0);
        let new_pid = res.get("new_pid").and_then(JsonValue::as_u64).unwrap_or(0);
        let url = res.get("url").and_then(JsonValue::as_str).unwrap_or("?");
        println!("Restarted Audiophile Player (PID {old_pid} -> {new_pid}, Web UI: {url}).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_play_argv_replaces_conflicting_and_mutually_exclusive_flags() {
        let prev = vec![
            "--ultimate".to_string(),
            "--filters".to_string(),
            "suggested".to_string(),
            "--conv-dir".to_string(),
            r"C:\REW".to_string(),
            "--device".to_string(),
            "Gungnir 2".to_string(),
        ];
        let changes = vec![
            "--no-ultimate".to_string(),
            "--filters".to_string(),
            "mine".to_string(),
            "--no-conv".to_string(),
        ];
        let merged = merge_play_argv(&prev, &changes);
        assert_eq!(
            merged,
            vec![
                "--device",
                "Gungnir 2",
                "--no-ultimate",
                "--filters",
                "mine",
                "--no-conv",
            ]
        );
    }

    #[test]
    fn run_state_roundtrips_json_and_token_is_64_hex() {
        let tok = generate_api_token_hex().unwrap();
        assert_eq!(tok.len(), 64);
        assert!(tok.bytes().all(|b| b.is_ascii_hexdigit()));

        let rs = RunStateFile {
            pid: 12345,
            url: "http://127.0.0.1:8085".to_string(),
            token: tok.clone(),
            argv: vec!["--ultimate".to_string()],
            effective_options: JsonValue::obj(vec![("ultimate", JsonValue::from(true))]),
            started_unix_s: 1_700_000_000,
            version: "0.2.0".to_string(),
        };
        let reparsed = RunStateFile::from_json(&rs.to_json()).unwrap();
        assert_eq!(reparsed.pid, 12345);
        assert_eq!(reparsed.token, tok);
        assert_eq!(reparsed.argv, vec!["--ultimate"]);
    }

    #[test]
    fn review9_q1_q6_q7_restart_validation_and_negative_flag_undo() {
        // Q1: non-play modes and invalid dac_bits MUST be rejected before stopping the player!
        for bad in &[
            vec!["--version".to_string()],
            vec!["help".to_string()],
            vec!["scan".to_string()],
            vec!["--idle".to_string()],
            vec!["--dac-bits".to_string(), "17".to_string()],
        ] {
            assert!(
                crate::cli_args::validate_play_argv(bad).is_err(),
                "Q1: expected validate_play_argv to reject {bad:?}"
            );
        }
        // Valid play argv passes (including `restart -- --ultimate`).
        assert!(crate::cli_args::validate_play_argv(&[
            "--ultimate".to_string(),
            "--dac-bits".to_string(),
            "20".to_string(),
        ])
        .is_ok());

        // Q6: `restart -- --ultimate` parses cleanly and rejects unknown --when.
        let parsed = crate::cli_args::parse_args([
            "audiophile-player",
            "restart",
            "--when",
            "now",
            "--",
            "--ultimate",
        ])
        .unwrap();
        assert!(matches!(
            parsed,
            crate::cli_args::RunMode::Restart {
                after_track: false,
                ..
            }
        ));
        assert!(crate::cli_args::parse_when("later").is_err());

        // Q7: clear_flags undoes `--no-upnp` / `--no-capture` cleanly across restarts!
        let prev = vec![
            "--no-upnp".to_string(),
            "--no-capture".to_string(),
            "--ultimate".to_string(),
        ];
        let merged = merge_play_argv_with_clears(
            &prev,
            &[],
            &["--no-upnp".to_string(), "--no-capture".to_string()],
        );
        assert_eq!(merged, vec!["--ultimate"]);
    }
}
