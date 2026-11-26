// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Localhost-only HTTP/1.1 control server strictly decoupled from the audio output thread.
//!
//! Port selection policy:
//! - Starts at a reasonable conventional localhost range (`8085..=8199`).
//! - **Never** binds or touches port `80` or `8080`, and **never** kills or hijacks any
//!   other process listening on a candidate port.
//! - Accurately probes each candidate port (detecting both `127.0.0.1:port` and wildcard
//!   `0.0.0.0:port` / `[::]:port` listeners before binding) and automatically advances to
//!   the next free port when busy.
//!
//! Security & robustness guarantees:
//! - Validates `Host` strictly matches `127.0.0.1:<port>` or `localhost:<port>` (`403` otherwise)
//!   to block DNS rebinding and cross-site GET abuse.
//! - Validates `Origin` and `Sec-Fetch-Site` on state-changing `POST /action/*` routes (and
//!   blocks cross-site `GET /?dir=...` / `GET /api/status`).
//! - Rejects UNC, verbatim, device-namespace, reserved DOS device, and out-of-root paths before
//!   any filesystem syscall, and requires regular `.flac` files for `/action/play-file`.
//! - Caps headers at 8 KiB, enforces an overall per-request deadline (5 s), and dispatches
//!   connections to bounded short-lived worker threads so slow clients or drives never stall
//!   `/api/status` or `ControlServer::shutdown`.

use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ap_core::http1::{HttpReadLimits, HttpRequest, read_http_request, write_all_bounded};
use ap_core::json::JsonValue;

use crate::browser::{
    FolderListing, decode_path_token, encode_path_token, list_folder_entries_in_root,
    parse_form_param, url_encode,
};
use crate::html::{render_control_page, render_status_json};
use crate::security::{
    extract_unc_share, is_allowed_fetch_dest, is_allowed_fetch_site, is_valid_localhost_host,
    is_valid_localhost_origin, lexically_within_root, normalize_allowed_root,
    validate_path_syntax_for_root, validate_path_within_root, validate_playable_flac_path,
};
use crate::state::{ControlCommand, SharedControl};

/// Reasonable default localhost port range (avoids 80, 443, 8080; auto-skips busy ports).
pub const DEFAULT_PORT_START: u16 = 8085;
pub const DEFAULT_PORT_END: u16 = 8199;

/// Maximum allowed HTTP request header block size (8 KiB).
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Maximum allowed HTTP request body size (24 KiB).
pub const MAX_BODY_BYTES: usize = 24 * 1024;
/// Overall wall-clock deadline for reading a single HTTP request (prevents trickle slowloris).
pub const REQUEST_READ_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum time a worker will wait on a blocking filesystem browse/validation operation
/// (strictly below `CONNECTION_TOTAL_DEADLINE` so a stalled NAS returns `504 Gateway Timeout`
/// cleanly instead of pinning worker slots or dropping an empty reply).
pub const FS_OP_DEADLINE: Duration = Duration::from_secs(4);
/// Overall wall-clock deadline for an entire HTTP connection (read + browse + chunked write)
/// so slow-reading clients cannot pin worker slots indefinitely.
pub const CONNECTION_TOTAL_DEADLINE: Duration = Duration::from_secs(10);
/// Chunk size used when writing HTTP response bodies under the overall connection deadline.
const RESPONSE_WRITE_CHUNK_BYTES: usize = 16 * 1024;
/// Maximum concurrent connection worker threads.
pub const MAX_CONCURRENT_WORKERS: usize = 16;
/// Maximum concurrent blocking filesystem helpers allowed for a single storage root/volume
/// (`MAX_FS_HELPERS_PER_ROOT < MAX_CONCURRENT_WORKERS`), so one stalled NAS share or dead USB
/// drive can NEVER saturate the global helper pool and starve local drives (Review #4 Finding #3).
pub const MAX_FS_HELPERS_PER_ROOT: usize = 4;

#[derive(Default)]
struct FsHelperPoolState {
    total: usize,
    per_root: HashMap<String, usize>,
}

fn fs_helper_pool() -> &'static Mutex<FsHelperPoolState> {
    static POOL: OnceLock<Mutex<FsHelperPoolState>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(FsHelperPoolState::default()))
}

fn lexical_volume_key(path: &Path) -> String {
    if let Some((srv, share)) = extract_unc_share(path) {
        return format!("unc:\\\\{srv}\\{share}");
    }
    if let Some(Component::Prefix(p)) = path.components().next() {
        match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                return format!("drive:{}", (d as char).to_ascii_lowercase());
            }
            Prefix::Verbatim(v) => {
                return format!("vol:{}", v.to_string_lossy().to_ascii_lowercase());
            }
            _ => {}
        }
    }
    "local".to_string()
}

const REPARSE_KEY_CACHE_MAX_ENTRIES: usize = 512;

fn reparse_key_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn has_unsafe_stat_prefix(path: &Path) -> bool {
    let Some(Component::Prefix(p)) = path.components().next() else {
        return false;
    };
    match p.kind() {
        Prefix::DeviceNS(_) => true,
        Prefix::Verbatim(v) => {
            let s = v.to_string_lossy();
            !(s.len() >= 7 && s[..7].eq_ignore_ascii_case("Volume{"))
        }
        _ => false,
    }
}

fn normalized_cache_path_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn lookup_cached_reparse_volume_key(path: &Path) -> Option<String> {
    let Ok(cache) = reparse_key_cache().lock() else {
        return None;
    };
    if cache.is_empty() {
        return None;
    }
    // Check from full path up to ancestors so the deepest cached reparse hop wins.
    let mut cur = path;
    loop {
        let key = normalized_cache_path_key(cur);
        if let Some(bucket) = cache.get(&key) {
            return Some(bucket.clone());
        }
        match cur.parent() {
            Some(parent) if parent != cur => cur = parent,
            _ => break,
        }
    }
    None
}

/// Populate the in-memory reparse bucket cache strictly INSIDE a bounded filesystem worker thread
/// (never on the HTTP connection thread, Review #6 Finding #9).
fn populate_reparse_volume_cache_bounded(path: &Path) {
    if has_unsafe_stat_prefix(path) || extract_unc_share(path).is_some() {
        return;
    }
    let lexical = lexical_volume_key(path);
    let resolved = fs_volume_key_with(path, &|p| {
        if has_unsafe_stat_prefix(p) {
            return None;
        }
        let meta = std::fs::symlink_metadata(p).ok()?;
        if meta.file_type().is_symlink() {
            std::fs::read_link(p).ok()
        } else {
            None
        }
    });
    if resolved != lexical {
        if let Ok(mut cache) = reparse_key_cache().lock() {
            if cache.len() >= REPARSE_KEY_CACHE_MAX_ENTRIES {
                cache.clear();
            }
            cache.insert(normalized_cache_path_key(path), resolved);
        }
    }
}

/// Pure-lexical pre-validation (ZERO filesystem syscalls) enforcing syntax & allowed-root
/// containment BEFORE any bounded helper thread or bucket lookup runs (Review #6 Finding #9).
fn prevalidate_lexical_path_for_root(decoded: &Path, allowed_root: &Path) -> Result<PathBuf, String> {
    let norm_root = normalize_allowed_root(allowed_root);
    let allowed_unc = extract_unc_share(&norm_root);
    let share_ref = allowed_unc
        .as_ref()
        .map(|(s, sh)| (s.as_str(), sh.as_str()));
    let norm = validate_path_syntax_for_root(decoded, share_ref)?;
    if !lexically_within_root(&norm, &norm_root) {
        return Err(format!(
            "path '{}' escapes allowed music root '{}'",
            norm.display(),
            norm_root.display()
        ));
    }
    Ok(norm)
}

/// Derive a stable volume/share concurrency bucket key for `path` (e.g. `unc:\\nas\music` or
/// `drive:c`) with **ZERO filesystem syscalls** on the HTTP connection thread (Review #6 #9):
/// - Consults the in-memory reparse-target cache (populated strictly inside bounded worker threads)
///   so known local junctions pointing to another volume (e.g. `C:\Music\Usb -> D:\Archive`) are
///   isolated to the target volume's bucket without ever risking an unbounded `symlink_metadata`
///   hang on the HTTP thread.
/// - Falls back immediately to purely lexical [`lexical_volume_key`].
pub fn fs_volume_key(path: &Path) -> String {
    if has_unsafe_stat_prefix(path) || extract_unc_share(path).is_some() {
        return lexical_volume_key(path);
    }
    lookup_cached_reparse_volume_key(path).unwrap_or_else(|| lexical_volume_key(path))
}

fn fs_volume_key_with(path: &Path, read_reparse_target: &dyn Fn(&Path) -> Option<PathBuf>) -> String {
    fn resolve_key(
        path: &Path,
        remaining_hops: usize,
        read_reparse_target: &dyn Fn(&Path) -> Option<PathBuf>,
    ) -> String {
        if has_unsafe_stat_prefix(path)
            || extract_unc_share(path).is_some()
            || ap_source::library::is_remote_network_dir(path)
        {
            return lexical_volume_key(path);
        }
        if remaining_hops == 0 {
            return lexical_volume_key(path);
        }
        let mut cur = PathBuf::new();
        let mut comps = path.components();
        while let Some(comp) = comps.next() {
            cur.push(comp.as_os_str());
            if matches!(comp, Component::Prefix(_) | Component::RootDir) {
                continue;
            }
            if let Some(target) = read_reparse_target(&cur) {
                let mut next = if target.is_absolute() {
                    target
                } else {
                    cur.parent().map(|p| p.join(&target)).unwrap_or(target)
                };
                for rest in comps {
                    next.push(rest.as_os_str());
                }
                return resolve_key(&next, remaining_hops - 1, read_reparse_target);
            }
        }
        lexical_volume_key(path)
    }

    resolve_key(path, 4, read_reparse_target)
}

/// Run a potentially blocking filesystem validation or directory-listing closure with a strict
/// wall-clock timeout, a per-volume concurrency cap ([`MAX_FS_HELPERS_PER_ROOT`]), and a global
/// cap ([`MAX_CONCURRENT_WORKERS`]) so a stalled NAS/USB drive can never starve local drives or
/// prevent `POST /action/pause` from releasing the DAC.
pub fn run_fs_op_bounded_for_key<T, F>(key: &str, timeout: Duration, op: F) -> Result<T, ()>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let key_owned = key.to_string();
    {
        let mut pool = fs_helper_pool().lock().unwrap_or_else(|e| e.into_inner());
        let root_active = pool.per_root.get(&key_owned).copied().unwrap_or(0);
        if pool.total >= MAX_CONCURRENT_WORKERS || root_active >= MAX_FS_HELPERS_PER_ROOT {
            return Err(());
        }
        pool.total += 1;
        *pool.per_root.entry(key_owned.clone()).or_insert(0) += 1;
    }

    struct HelperGuard {
        key: String,
    }
    impl Drop for HelperGuard {
        fn drop(&mut self) {
            let mut pool = fs_helper_pool().lock().unwrap_or_else(|e| e.into_inner());
            pool.total = pool.total.saturating_sub(1);
            if let Some(cnt) = pool.per_root.get_mut(&self.key) {
                *cnt = cnt.saturating_sub(1);
                if *cnt == 0 {
                    pool.per_root.remove(&self.key);
                }
            }
        }
    }

    let (tx, rx) = mpsc::sync_channel(1);
    let key_for_thread = key_owned.clone();
    let spawned = thread::Builder::new()
        .name("ap-http-fs".to_string())
        .spawn(move || {
            let _guard = HelperGuard {
                key: key_for_thread,
            };
            let _ = tx.send(op());
        });
    if spawned.is_err() {
        let mut pool = fs_helper_pool().lock().unwrap_or_else(|e| e.into_inner());
        pool.total = pool.total.saturating_sub(1);
        if let Some(cnt) = pool.per_root.get_mut(&key_owned) {
            *cnt = cnt.saturating_sub(1);
            if *cnt == 0 {
                pool.per_root.remove(&key_owned);
            }
        }
        return Err(());
    }

    rx.recv_timeout(timeout).map_err(|_| ())
}

/// Non-destructively check whether another service is already accepting TCP connections
/// on localhost at `port` (catches services bound to `0.0.0.0:port`, `[::]:port`, or
/// `127.0.0.1:port` so Windows' dual-bind quirk cannot hijack another process's traffic).
fn is_localhost_port_accepting(port: u16) -> bool {
    let v4 = SocketAddr::from(([127, 0, 0, 1], port));
    if TcpStream::connect_timeout(&v4, Duration::from_millis(20)).is_ok() {
        return true;
    }
    let v6 = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], port));
    TcpStream::connect_timeout(&v6, Duration::from_millis(20)).is_ok()
}

/// Bind a localhost `TcpListener` on the first genuinely unused port in `start_port..=end_port`.
///
/// Guarantees:
/// - Never terminates or interferes with existing services.
/// - Detects listeners bound to `0.0.0.0:port`, `[::]:port`, or `127.0.0.1:port` and skips them.
pub fn bind_first_available_port(
    start_port: u16,
    end_port: u16,
) -> Result<(TcpListener, u16), String> {
    if start_port == 0 || start_port > end_port {
        return Err(format!("invalid port range {start_port}..={end_port}"));
    }
    let mut last_err = String::new();
    for port in start_port..=end_port {
        if is_localhost_port_accepting(port) {
            last_err = format!("port {port} is already accepting connections");
            continue;
        }
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok((listener, port)),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!(
        "could not bind any free localhost HTTP port in {start_port}..={end_port}: {last_err}"
    ))
}

/// Running rudimentary HTTP control server handle. Stops cleanly on [`ControlServer::shutdown`] or drop.
pub struct ControlServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    active_workers: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl ControlServer {
    /// Start the HTTP server on `explicit_port` (if given) or the first free port in
    /// [`DEFAULT_PORT_START`]..=[`DEFAULT_PORT_END`].
    pub fn start(
        shared: Arc<SharedControl>,
        explicit_port: Option<u16>,
    ) -> Result<Self, String> {
        let (listener, port) = match explicit_port {
            Some(p) => bind_first_available_port(p, p)?,
            None => bind_first_available_port(DEFAULT_PORT_START, DEFAULT_PORT_END)?,
        };
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed setting nonblocking listener: {e}"))?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let stop = Arc::new(AtomicBool::new(false));
        let active_workers = Arc::new(AtomicUsize::new(0));
        let stop_bg = Arc::clone(&stop);
        let workers_bg = Arc::clone(&active_workers);

        shared.record_action(format!("HTTP control server listening on http://{addr}"));

        let handle = thread::Builder::new()
            .name("ap-http-control".to_string())
            .spawn(move || serve_loop(listener, port, shared, stop_bg, workers_bg))
            .map_err(|e| format!("failed spawning HTTP control thread: {e}"))?;

        Ok(Self {
            addr,
            stop,
            active_workers,
            handle: Some(handle),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Joining the accept loop drops the TcpListener promptly (within <= 25 ms),
        // releasing the port without blocking on stuck client worker threads.
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Give active fast workers a brief grace period to finish writing responses,
        // without ever blocking shutdown indefinitely on a stuck client or dead drive.
        let drain_deadline = Instant::now() + Duration::from_millis(200);
        while self.active_workers.load(Ordering::Acquire) > 0 && Instant::now() < drain_deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct WorkerSlotGuard {
    active_workers: Arc<AtomicUsize>,
}

impl Drop for WorkerSlotGuard {
    fn drop(&mut self) {
        self.active_workers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_worker_slot(active_workers: &Arc<AtomicUsize>) -> Option<WorkerSlotGuard> {
    let mut cur = active_workers.load(Ordering::Acquire);
    while cur < MAX_CONCURRENT_WORKERS {
        match active_workers.compare_exchange_weak(
            cur,
            cur + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                return Some(WorkerSlotGuard {
                    active_workers: Arc::clone(active_workers),
                });
            }
            Err(observed) => cur = observed,
        }
    }
    None
}

fn serve_loop(
    listener: TcpListener,
    bound_port: u16,
    shared: Arc<SharedControl>,
    stop: Arc<AtomicBool>,
    active_workers: Arc<AtomicUsize>,
) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

                let Some(slot_guard) = try_acquire_worker_slot(&active_workers) else {
                    let deadline = Instant::now() + Duration::from_secs(1);
                    write_response(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=utf-8",
                        b"Too many concurrent HTTP requests",
                        None,
                        deadline,
                        &stop,
                    );
                    continue;
                };

                let shared_w = Arc::clone(&shared);
                let stop_w = Arc::clone(&stop);
                let spawn_res = thread::Builder::new()
                    .name("ap-http-worker".to_string())
                    .spawn(move || {
                        let _guard = slot_guard;
                        handle_connection(&mut stream, bound_port, &shared_w, &stop_w);
                    });
                if spawn_res.is_err() {
                    // `slot_guard` was moved into closure or dropped if spawn failed before moving;
                    // thread::Builder::spawn drops the closure on Err so the slot is released.
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

const CONTROL_READ_LIMITS: HttpReadLimits = HttpReadLimits {
    max_header_bytes: MAX_HEADER_BYTES,
    max_body_bytes: MAX_BODY_BYTES,
    first_byte_deadline: REQUEST_READ_DEADLINE,
    read_deadline: REQUEST_READ_DEADLINE,
    allow_chunked: false,
    honor_expect_100: false,
};

enum BoundedValidationError {
    Rejected(String),
    TimedOut,
}

fn text_err(
    stream: &mut TcpStream,
    status: &str,
    msg: &str,
    deadline: Instant,
    stop: &AtomicBool,
) {
    write_response(
        stream,
        status,
        "text/plain; charset=utf-8",
        msg.as_bytes(),
        None,
        deadline,
        stop,
    );
}

fn json_err(
    stream: &mut TcpStream,
    status: &str,
    msg: impl Into<String>,
    deadline: Instant,
    stop: &AtomicBool,
) {
    write_json_response(
        stream,
        status,
        &JsonValue::obj(vec![
            ("ok", JsonValue::from(false)),
            ("error", JsonValue::from(msg.into())),
        ]),
        deadline,
        stop,
    );
}

/// Lexical pre-validation + per-volume bounded filesystem validation shared across HTML & `/api/v1`
/// folder/file routes (Review #9 O1).
fn validate_path_bounded(
    raw: &Path,
    allowed_root: &Path,
    is_file: bool,
) -> Result<PathBuf, BoundedValidationError> {
    let prevalidated = prevalidate_lexical_path_for_root(raw, allowed_root)
        .map_err(BoundedValidationError::Rejected)?;
    let root_for_fs = allowed_root.to_path_buf();
    let key = fs_volume_key(&prevalidated);
    match run_fs_op_bounded_for_key(&key, FS_OP_DEADLINE, move || {
        populate_reparse_volume_cache_bounded(&prevalidated);
        if is_file {
            validate_playable_flac_path(&prevalidated, Some(&root_for_fs))
        } else {
            validate_path_within_root(&prevalidated, &root_for_fs)
        }
    }) {
        Ok(Ok(valid)) => Ok(valid),
        Ok(Err(reason)) => Err(BoundedValidationError::Rejected(reason)),
        Err(()) => Err(BoundedValidationError::TimedOut),
    }
}

fn handle_connection(
    stream: &mut TcpStream,
    bound_port: u16,
    shared: &SharedControl,
    stop: &AtomicBool,
) {
    let conn_deadline = Instant::now() + CONNECTION_TOTAL_DEADLINE;
    let req = match read_http_request(stream, &CONTROL_READ_LIMITS, stop) {
        Ok(r) => r,
        Err(err) => {
            if let Some((status, msg)) = err.http_status() {
                text_err(stream, status, msg, conn_deadline, stop);
            }
            return;
        }
    };

    // Strict `Host` validation blocks DNS rebinding and cross-site GET/POST abuse.
    let valid_host = !req.has_duplicate_header("host")
        && req
            .header("host")
            .is_some_and(|h| is_valid_localhost_host(h, bound_port));
    if !valid_host {
        text_err(
            stream,
            "403 Forbidden",
            "Forbidden: invalid Host header",
            conn_deadline,
            stop,
        );
        return;
    }

    // Block framing / clickjacking attempts (iframe, frame, object, embed) on EVERY route.
    if req
        .header("sec-fetch-dest")
        .is_some_and(|dest| !is_allowed_fetch_dest(dest))
    {
        text_err(
            stream,
            "403 Forbidden",
            "Forbidden: embedded framing rejected",
            conn_deadline,
            stop,
        );
        return;
    }

    let (path, query) = req
        .target
        .split_once('?')
        .unwrap_or((req.target.as_str(), ""));

    // Enforce Origin / Sec-Fetch-Site checks on all state-changing POST routes.
    if req.method == "POST" {
        if req
            .header("origin")
            .is_some_and(|origin| !is_valid_localhost_origin(origin, bound_port))
        {
            text_err(
                stream,
                "403 Forbidden",
                "Forbidden: cross-origin POST rejected",
                conn_deadline,
                stop,
            );
            return;
        }
        if req
            .header("sec-fetch-site")
            .is_some_and(|site| !is_allowed_fetch_site(site))
        {
            text_err(
                stream,
                "403 Forbidden",
                "Forbidden: cross-site POST rejected",
                conn_deadline,
                stop,
            );
            return;
        }
    }

    let allowed_root = shared.allowed_root();

    // Token-authenticated JSON API (`/api/v1/*`) used by CLI `status`/`stop`/`restart` & MCP supervisor.
    if path.starts_with("/api/v1/") {
        handle_api_v1(
            stream,
            &req,
            path,
            query,
            shared,
            &allowed_root,
            conn_deadline,
            stop,
        );
        return;
    }

    let body_str = req.body_lossy();
    match (req.method.as_str(), path) {
        ("GET", "/") => {
            let raw_dir_param = parse_form_param(query, "dir").filter(|s| !s.trim().is_empty());
            if raw_dir_param.is_some()
                && req
                    .header("sec-fetch-site")
                    .is_some_and(|site| !is_allowed_fetch_site(site))
            {
                text_err(
                    stream,
                    "403 Forbidden",
                    "Forbidden: cross-site directory probe rejected",
                    conn_deadline,
                    stop,
                );
                return;
            }

            let snap = shared.snapshot();
            let root_for_fs = allowed_root.clone();
            let fallback_dir = snap.active_folder.clone();
            // Review #6 Finding #9: purely lexical pre-validation before any bounded worker or
            // bucket lookup so illegal prefixes / out-of-root paths trigger ZERO syscalls.
            let display_dir_on_timeout = if let Some(raw) = raw_dir_param.as_deref() {
                let decoded = match decode_path_token(raw) {
                    Ok(p) => p,
                    Err(reason) => {
                        text_err(
                            stream,
                            "403 Forbidden",
                            &format!("Forbidden path: {reason}"),
                            conn_deadline,
                            stop,
                        );
                        return;
                    }
                };
                match prevalidate_lexical_path_for_root(&decoded, &allowed_root) {
                    Ok(norm) => norm,
                    Err(reason) => {
                        text_err(
                            stream,
                            "403 Forbidden",
                            &format!("Forbidden path: {reason}"),
                            conn_deadline,
                            stop,
                        );
                        return;
                    }
                }
            } else {
                fallback_dir.clone()
            };
            let fs_key = fs_volume_key(&display_dir_on_timeout);
            let prevalidated_for_fs = display_dir_on_timeout.clone();
            let has_raw_dir = raw_dir_param.is_some();
            let fs_res = run_fs_op_bounded_for_key(&fs_key, FS_OP_DEADLINE, move || {
                let browse_dir = if has_raw_dir {
                    match validate_path_within_root(&prevalidated_for_fs, &root_for_fs) {
                        Ok(valid) => {
                            populate_reparse_volume_cache_bounded(&valid);
                            valid
                        }
                        Err(reason) => return Err(reason),
                    }
                } else {
                    fallback_dir
                };
                Ok(list_folder_entries_in_root(&browse_dir, &root_for_fs))
            });
            let html = match fs_res {
                Ok(Ok(listing)) => render_control_page(&snap, &listing),
                Ok(Err(reason)) => {
                    text_err(
                        stream,
                        "403 Forbidden",
                        &format!("Forbidden path: {reason}"),
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(()) => render_timeout_control_html(
                    &snap,
                    display_dir_on_timeout,
                    &allowed_root,
                    "Storage timed out or is busy while reading this folder. Playback controls (Pause / Next / Play) and other folders remain available.",
                ),
            };
            write_response(
                stream,
                "200 OK",
                "text/html; charset=utf-8",
                html.as_bytes(),
                None,
                conn_deadline,
                stop,
            );
        }
        ("GET", "/api/status") => {
            if req
                .header("sec-fetch-site")
                .is_some_and(|site| !is_allowed_fetch_site(site))
            {
                text_err(
                    stream,
                    "403 Forbidden",
                    "Forbidden: cross-site status fetch rejected",
                    conn_deadline,
                    stop,
                );
                return;
            }
            let json = render_status_json(&shared.snapshot());
            write_response(
                stream,
                "200 OK",
                "application/json; charset=utf-8",
                json.as_bytes(),
                None,
                conn_deadline,
                stop,
            );
        }
        ("POST", action_path @ ("/action/play-random" | "/action/play-sequential")) => {
            let dir = match extract_validated_dir_bounded(&body_str, shared, &allowed_root) {
                Ok(d) => d,
                Err(BoundedValidationError::Rejected(reason)) => {
                    text_err(
                        stream,
                        "403 Forbidden",
                        &format!("Forbidden folder: {reason}"),
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(BoundedValidationError::TimedOut) => {
                    write_storage_timeout_control_page(
                        stream,
                        shared,
                        &allowed_root,
                        &body_str,
                        "Storage timed out or is busy validating folder path. Playback controls (Pause / Next / Play) and other folders remain available.",
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            let cmd = if action_path == "/action/play-sequential" {
                ControlCommand::PlaySequential(dir.clone())
            } else {
                ControlCommand::PlayRandom(dir.clone())
            };
            shared.submit_command(cmd);
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/play-file") => {
            let dir = match extract_validated_dir_bounded(&body_str, shared, &allowed_root) {
                Ok(d) => d,
                Err(BoundedValidationError::Rejected(reason)) => {
                    text_err(
                        stream,
                        "403 Forbidden",
                        &format!("Forbidden folder: {reason}"),
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(BoundedValidationError::TimedOut) => {
                    write_storage_timeout_control_page(
                        stream,
                        shared,
                        &allowed_root,
                        &body_str,
                        "Storage timed out or is busy validating folder path. Playback controls (Pause / Next / Play) and other folders remain available.",
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            let Some(raw_path) =
                parse_form_param(&body_str, "path").filter(|s| !s.trim().is_empty())
            else {
                text_err(stream, "400 Bad Request", "Missing file path", conn_deadline, stop);
                return;
            };
            let decoded = match decode_path_token(&raw_path) {
                Ok(p) => p,
                Err(reason) => {
                    text_err(
                        stream,
                        "400 Bad Request",
                        &format!("Invalid path token: {reason}"),
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            let valid_file = match validate_path_bounded(&decoded, &allowed_root, true) {
                Ok(p) => p,
                Err(BoundedValidationError::Rejected(reason)) => {
                    text_err(
                        stream,
                        "403 Forbidden",
                        &format!("Rejected file path: {reason}"),
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(BoundedValidationError::TimedOut) => {
                    write_storage_timeout_control_page(
                        stream,
                        shared,
                        &allowed_root,
                        &body_str,
                        "Storage timed out or is busy validating file path. Playback controls (Pause / Next / Play) and other folders remain available.",
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            shared.submit_command(ControlCommand::PlayFile(valid_file));
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", action_path @ ("/action/next" | "/action/pause" | "/action/resume")) => {
            // Review #3 Finding #4: submit Next/Pause/Resume IMMEDIATELY with ZERO blocking FS I/O
            // so a stalled NAS/USB drive can never delay skipping or releasing the exclusive DAC pin.
            let cmd = match action_path {
                "/action/next" => ControlCommand::Next,
                "/action/pause" => ControlCommand::Pause,
                _ => ControlCommand::Resume,
            };
            shared.submit_command(cmd);
            let dir = extract_syntax_only_dir(&body_str, shared, &allowed_root);
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        _ => text_err(stream, "404 Not Found", "Not Found", conn_deadline, stop),
    }
}

fn extract_validated_dir_bounded(
    body: &str,
    shared: &SharedControl,
    allowed_root: &Path,
) -> Result<PathBuf, BoundedValidationError> {
    match parse_form_param(body, "dir").filter(|s| !s.trim().is_empty()) {
        Some(raw) => {
            let decoded = decode_path_token(&raw).map_err(BoundedValidationError::Rejected)?;
            validate_path_bounded(&decoded, allowed_root, false)
        }
        None => Ok(shared.snapshot().active_folder),
    }
}

/// Pure-memory redirect folder extraction performing **zero** filesystem syscalls (Review #9 O12:
/// delegates directly to `prevalidate_lexical_path_for_root`).
fn extract_syntax_only_dir(body: &str, shared: &SharedControl, allowed_root: &Path) -> PathBuf {
    parse_form_param(body, "dir")
        .filter(|s| !s.trim().is_empty())
        .and_then(|raw| decode_path_token(&raw).ok())
        .and_then(|decoded| prevalidate_lexical_path_for_root(&decoded, allowed_root).ok())
        .unwrap_or_else(|| shared.snapshot().active_folder)
}

/// Build the full interactive HTML control page with an error banner when storage times out
/// (shared by `GET /` and `POST /action/play-*` timeouts, Review #9 O12).
fn render_timeout_control_html(
    snap: &crate::state::ControlSnapshot,
    display_dir: PathBuf,
    allowed_root: &Path,
    error_message: &str,
) -> String {
    let norm_root = normalize_allowed_root(allowed_root);
    let parent_dir = display_dir
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| lexically_within_root(p, &norm_root));
    let listing = FolderListing {
        current_dir: display_dir,
        parent_dir,
        subdirs: Vec::new(),
        flac_files: Vec::new(),
        error: Some(error_message.to_string()),
    };
    render_control_page(snap, &listing)
}

fn write_storage_timeout_control_page(
    stream: &mut TcpStream,
    shared: &SharedControl,
    allowed_root: &Path,
    body: &str,
    error_message: &str,
    deadline: Instant,
    stop: &AtomicBool,
) {
    let snap = shared.snapshot();
    let display_dir = extract_syntax_only_dir(body, shared, allowed_root);
    let html = render_timeout_control_html(&snap, display_dir, allowed_root, error_message);
    write_response(
        stream,
        "504 Gateway Timeout",
        "text/html; charset=utf-8",
        html.as_bytes(),
        None,
        deadline,
        stop,
    );
}

fn redirect_to_dir(stream: &mut TcpStream, dir: &Path, deadline: Instant, stop: &AtomicBool) {
    let loc = format!("/?dir={}", url_encode(&encode_path_token(dir)));
    write_response(
        stream,
        "303 See Other",
        "text/plain; charset=utf-8",
        b"OK",
        Some(&loc),
        deadline,
        stop,
    );
}

fn write_json_response(
    stream: &mut TcpStream,
    status: &str,
    payload: &JsonValue,
    deadline: Instant,
    stop: &AtomicBool,
) {
    let body = payload.to_compact_string();
    write_response(
        stream,
        status,
        "application/json; charset=utf-8",
        body.as_bytes(),
        None,
        deadline,
        stop,
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_api_v1(
    stream: &mut TcpStream,
    req: &HttpRequest,
    path: &str,
    query: &str,
    shared: &SharedControl,
    allowed_root: &Path,
    deadline: Instant,
    stop: &AtomicBool,
) {
    if !shared.verify_bearer_token(req.header("authorization")) {
        json_err(
            stream,
            "401 Unauthorized",
            "unauthorized: missing or invalid Bearer token",
            deadline,
            stop,
        );
        return;
    }

    let body_str = req.body_lossy();
    match (req.method.as_str(), path) {
        ("GET", "/api/v1/status") => {
            write_json_response(stream, "200 OK", &shared.api_v1_status_json(), deadline, stop);
        }
        ("POST", route @ ("/api/v1/pause" | "/api/v1/resume" | "/api/v1/next")) => {
            let (cmd, action) = match route {
                "/api/v1/pause" => (ControlCommand::Pause, "pause"),
                "/api/v1/resume" => (ControlCommand::Resume, "resume"),
                _ => (ControlCommand::Next, "next"),
            };
            shared.submit_command(cmd);
            write_json_response(
                stream,
                "200 OK",
                &JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("action", JsonValue::from(action)),
                ]),
                deadline,
                stop,
            );
        }
        ("POST", "/api/v1/play") => {
            let parsed = if body_str.trim().is_empty() {
                Ok(JsonValue::Object(Vec::new()))
            } else {
                JsonValue::parse(&body_str)
            };
            let body_json = match parsed {
                Ok(v) => v,
                Err(err) => {
                    json_err(stream, "400 Bad Request", format!("invalid JSON body: {err}"), deadline, stop);
                    return;
                }
            };
            let order = body_json
                .get("order")
                .and_then(JsonValue::as_str)
                .unwrap_or("random")
                .to_ascii_lowercase();
            let folder_raw = body_json
                .get("folder")
                .and_then(JsonValue::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| shared.snapshot().active_folder);

            let valid_dir = match validate_path_bounded(&folder_raw, allowed_root, false) {
                Ok(d) => d,
                Err(BoundedValidationError::Rejected(reason)) => {
                    json_err(stream, "403 Forbidden", format!("forbidden folder: {reason}"), deadline, stop);
                    return;
                }
                Err(BoundedValidationError::TimedOut) => {
                    json_err(stream, "504 Gateway Timeout", "storage timed out validating folder", deadline, stop);
                    return;
                }
            };
            match order.as_str() {
                "sequential" => shared.submit_command(ControlCommand::PlaySequential(valid_dir.clone())),
                "random" => shared.submit_command(ControlCommand::PlayRandom(valid_dir.clone())),
                other => {
                    json_err(
                        stream,
                        "400 Bad Request",
                        format!("invalid order '{other}'; expected random or sequential"),
                        deadline,
                        stop,
                    );
                    return;
                }
            }
            write_json_response(
                stream,
                "200 OK",
                &JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("action", JsonValue::from("play")),
                    ("order", JsonValue::from(order)),
                    ("folder", JsonValue::from(valid_dir.to_string_lossy().into_owned())),
                ]),
                deadline,
                stop,
            );
        }
        ("POST", "/api/v1/play-file") => {
            let body_json = match JsonValue::parse(&body_str) {
                Ok(v) => v,
                Err(err) => {
                    json_err(stream, "400 Bad Request", format!("invalid JSON body: {err}"), deadline, stop);
                    return;
                }
            };
            let Some(path_str) = body_json.get("path").and_then(JsonValue::as_str) else {
                json_err(stream, "400 Bad Request", "missing 'path' field", deadline, stop);
                return;
            };
            let valid_file = match validate_path_bounded(Path::new(path_str), allowed_root, true) {
                Ok(p) => p,
                Err(BoundedValidationError::Rejected(reason)) => {
                    json_err(stream, "403 Forbidden", format!("rejected file path: {reason}"), deadline, stop);
                    return;
                }
                Err(BoundedValidationError::TimedOut) => {
                    json_err(stream, "504 Gateway Timeout", "storage timed out validating file", deadline, stop);
                    return;
                }
            };
            shared.submit_command(ControlCommand::PlayFile(valid_file.clone()));
            write_json_response(
                stream,
                "200 OK",
                &JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("action", JsonValue::from("play_file")),
                    ("path", JsonValue::from(valid_file.to_string_lossy().into_owned())),
                ]),
                deadline,
                stop,
            );
        }
        ("POST", "/api/v1/shutdown") => {
            let mut when = parse_form_param(query, "when").unwrap_or_default();
            if when.is_empty() && !body_str.trim().is_empty() {
                if let Ok(v) = JsonValue::parse(&body_str) {
                    if let Some(w) = v.get("when").and_then(JsonValue::as_str) {
                        when = w.to_string();
                    }
                }
            }
            let norm_when = when.trim().to_ascii_lowercase();
            let after_track = matches!(norm_when.as_str(), "after_track" | "after-track");
            if !norm_when.is_empty() && norm_when != "now" && !after_track {
                json_err(
                    stream,
                    "400 Bad Request",
                    format!("invalid 'when' value '{when}'; expected 'now' or 'after_track'"),
                    deadline,
                    stop,
                );
                return;
            }
            let immediate = if after_track {
                shared.request_shutdown_after_track()
            } else {
                shared.request_shutdown();
                true
            };
            write_json_response(
                stream,
                "202 Accepted",
                &JsonValue::obj(vec![
                    ("ok", JsonValue::from(true)),
                    ("accepted", JsonValue::from(true)),
                    (
                        "when",
                        JsonValue::from(if after_track { "after_track" } else { "now" }),
                    ),
                    ("immediate", JsonValue::from(immediate)),
                ]),
                deadline,
                stop,
            );
        }
        _ => json_err(
            stream,
            "404 Not Found",
            format!("unknown API route {} {path}", req.method),
            deadline,
            stop,
        ),
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    location: Option<&str>,
    deadline: Instant,
    stop: &AtomicBool,
) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Frame-Options: DENY\r\n\
         Content-Security-Policy: frame-ancestors 'none'\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(loc) = location {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    head.push_str("\r\n");
    if write_all_bounded(stream, head.as_bytes(), deadline, Some(stop), RESPONSE_WRITE_CHUNK_BYTES)
        .is_ok()
    {
        let _ = write_all_bounded(stream, body, deadline, Some(stop), RESPONSE_WRITE_CHUNK_BYTES);
        let _ = stream.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_volume_key_isolates_reparse_targets_to_destination_volume_and_caps_cycles() {
        let reader = |p: &Path| -> Option<PathBuf> {
            let s = p.to_string_lossy();
            if s.eq_ignore_ascii_case(r"C:\Music\NasLink") {
                Some(PathBuf::from(r"Z:\NasMusic"))
            } else if s.eq_ignore_ascii_case(r"C:\Music\UncLink") {
                Some(PathBuf::from(r"\\192.168.1.50\FLAC"))
            } else if s.eq_ignore_ascii_case(r"C:\Music\CycleA") {
                Some(PathBuf::from(r"C:\Music\CycleB"))
            } else if s.eq_ignore_ascii_case(r"C:\Music\CycleB") {
                Some(PathBuf::from(r"C:\Music\CycleA"))
            } else {
                None
            }
        };

        assert_eq!(
            fs_volume_key_with(Path::new(r"C:\Music\LocalAlbum"), &reader),
            "drive:c"
        );
        assert_eq!(
            fs_volume_key_with(Path::new(r"C:\Music\NasLink\Jazz"), &reader),
            "drive:z",
            "junction on C: pointing to mapped drive Z: must bill to drive:z"
        );
        assert_eq!(
            fs_volume_key_with(Path::new(r"C:\Music\UncLink\Rock"), &reader),
            r"unc:\\192.168.1.50\flac",
            "junction on C: pointing to UNC share must bill to that UNC bucket"
        );
        assert_eq!(
            fs_volume_key_with(Path::new(r"C:\Music\CycleA\Sub"), &reader),
            "drive:c",
            "cyclic junctions must terminate cleanly at hop limit"
        );

        // Review #6 Finding #9: DeviceNS and non-Volume{} Verbatim prefixes MUST NEVER invoke
        // the reparse/stat reader (prevents unsolicited SMB contact from raw POST paths).
        let panicking_reader = |p: &Path| -> Option<PathBuf> {
            panic!("must never stat unsafe prefix path: {}", p.display());
        };
        let _ = fs_volume_key_with(Path::new(r"\\.\UNC\evilhost\share\x.flac"), &panicking_reader);
        let _ = fs_volume_key_with(
            Path::new(r"\\?\GLOBALROOT\Device\Mup\evilhost\share\x.flac"),
            &panicking_reader,
        );
        assert!(
            prevalidate_lexical_path_for_root(
                Path::new(r"\\.\UNC\evilhost\share\x.flac"),
                Path::new(r"C:\Music")
            )
            .is_err()
        );
        assert!(
            prevalidate_lexical_path_for_root(
                Path::new(r"\\?\GLOBALROOT\Device\Mup\evilhost\share\x.flac"),
                Path::new(r"C:\Music")
            )
            .is_err()
        );
    }
}
