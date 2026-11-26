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
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// Convenience wrapper around [`run_fs_op_bounded_for_key`] using the default bucket key.
pub fn run_fs_op_bounded<T, F>(timeout: Duration, op: F) -> Result<T, ()>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    run_fs_op_bounded_for_key("default", timeout, op)
}

/// Path to the runtime URL discovery file (`%LOCALAPPDATA%\audiophile-player\server.url` on
/// Windows) read by launcher scripts so they find the active port even beyond 8105 without
/// launching a duplicate instance.
pub fn runtime_url_file_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|dir| PathBuf::from(dir).join("audiophile-player").join("server.url"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .or_else(|| std::env::var_os("HOME"))
            .map(|dir| PathBuf::from(dir).join("audiophile-player").join("server.url"))
    }
}

/// Best-effort write of the active localhost control UI URL to an explicit file path.
pub fn write_runtime_url_file_at(path: &Path, url: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{url}\n"));
}

/// Best-effort cleanup of an explicit runtime URL file if it still points to `url`.
pub fn clear_runtime_url_file_at(path: &Path, url: &str) {
    if let Ok(contents) = std::fs::read_to_string(path) {
        if contents.trim() == url.trim() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Best-effort write of the active localhost control UI URL for launcher discovery.
pub fn write_runtime_url_file(url: &str) {
    if let Some(path) = runtime_url_file_path() {
        write_runtime_url_file_at(&path, url);
    }
}

/// Best-effort cleanup of the runtime URL file if it still points to `url`.
pub fn clear_runtime_url_file(url: &str) {
    if let Some(path) = runtime_url_file_path() {
        clear_runtime_url_file_at(&path, url);
    }
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

struct HttpRequest {
    method: String,
    target: String,
    host: Option<String>,
    duplicate_host: bool,
    origin: Option<String>,
    sec_fetch_site: Option<String>,
    sec_fetch_dest: Option<String>,
    body: String,
}

enum ReadRequestError {
    ClosedOrMalformed,
    Timeout,
    HeaderTooLarge,
    BodyTooLarge,
    BadContentLength,
}

fn handle_connection(
    stream: &mut TcpStream,
    bound_port: u16,
    shared: &SharedControl,
    stop: &AtomicBool,
) {
    let conn_deadline = Instant::now() + CONNECTION_TOTAL_DEADLINE;
    let req = match read_http_request(stream, stop) {
        Ok(r) => r,
        Err(ReadRequestError::ClosedOrMalformed) => return,
        Err(ReadRequestError::Timeout) => {
            write_response(
                stream,
                "408 Request Timeout",
                "text/plain; charset=utf-8",
                b"Request Timeout",
                None,
                conn_deadline,
                stop,
            );
            return;
        }
        Err(ReadRequestError::HeaderTooLarge) => {
            write_response(
                stream,
                "431 Request Header Fields Too Large",
                "text/plain; charset=utf-8",
                b"Request Header Fields Too Large",
                None,
                conn_deadline,
                stop,
            );
            return;
        }
        Err(ReadRequestError::BodyTooLarge) => {
            write_response(
                stream,
                "413 Payload Too Large",
                "text/plain; charset=utf-8",
                b"Payload Too Large",
                None,
                conn_deadline,
                stop,
            );
            return;
        }
        Err(ReadRequestError::BadContentLength) => {
            write_response(
                stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                b"Invalid Content-Length",
                None,
                conn_deadline,
                stop,
            );
            return;
        }
    };

    // Strict `Host` validation blocks DNS rebinding and cross-site GET/POST abuse.
    let valid_host = !req.duplicate_host
        && req
            .host
            .as_deref()
            .is_some_and(|h| is_valid_localhost_host(h, bound_port));
    if !valid_host {
        write_response(
            stream,
            "403 Forbidden",
            "text/plain; charset=utf-8",
            b"Forbidden: invalid Host header",
            None,
            conn_deadline,
            stop,
        );
        return;
    }

    // Block framing / clickjacking attempts (iframe, frame, object, embed) on EVERY route.
    if let Some(dest) = req.sec_fetch_dest.as_deref() {
        if !is_allowed_fetch_dest(dest) {
            write_response(
                stream,
                "403 Forbidden",
                "text/plain; charset=utf-8",
                b"Forbidden: embedded framing rejected",
                None,
                conn_deadline,
                stop,
            );
            return;
        }
    }

    let (path, query) = req
        .target
        .split_once('?')
        .unwrap_or((req.target.as_str(), ""));

    // Enforce Origin / Sec-Fetch-Site checks on all state-changing POST routes.
    if req.method == "POST" {
        if let Some(origin) = req.origin.as_deref() {
            if !is_valid_localhost_origin(origin, bound_port) {
                write_response(
                    stream,
                    "403 Forbidden",
                    "text/plain; charset=utf-8",
                    b"Forbidden: cross-origin POST rejected",
                    None,
                    conn_deadline,
                    stop,
                );
                return;
            }
        }
        if let Some(site) = req.sec_fetch_site.as_deref() {
            if !is_allowed_fetch_site(site) {
                write_response(
                    stream,
                    "403 Forbidden",
                    "text/plain; charset=utf-8",
                    b"Forbidden: cross-site POST rejected",
                    None,
                    conn_deadline,
                    stop,
                );
                return;
            }
        }
    }

    let allowed_root = shared.allowed_root();

    match (req.method.as_str(), path) {
        ("GET", "/") => {
            let raw_dir_param = parse_form_param(query, "dir").filter(|s| !s.trim().is_empty());
            if raw_dir_param.is_some() {
                if let Some(site) = req.sec_fetch_site.as_deref() {
                    if !is_allowed_fetch_site(site) {
                        write_response(
                            stream,
                            "403 Forbidden",
                            "text/plain; charset=utf-8",
                            b"Forbidden: cross-site directory probe rejected",
                            None,
                            conn_deadline,
                            stop,
                        );
                        return;
                    }
                }
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
                        write_response(
                            stream,
                            "403 Forbidden",
                            "text/plain; charset=utf-8",
                            format!("Forbidden path: {reason}").as_bytes(),
                            None,
                            conn_deadline,
                            stop,
                        );
                        return;
                    }
                };
                match prevalidate_lexical_path_for_root(&decoded, &allowed_root) {
                    Ok(norm) => norm,
                    Err(reason) => {
                        write_response(
                            stream,
                            "403 Forbidden",
                            "text/plain; charset=utf-8",
                            format!("Forbidden path: {reason}").as_bytes(),
                            None,
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
            let fs_res = run_fs_op_bounded_for_key(&fs_key, FS_OP_DEADLINE, move || {
                let browse_dir = match raw_dir_param {
                    Some(raw) => match decode_and_validate_dir(&raw, &root_for_fs) {
                        Ok(valid) => {
                            populate_reparse_volume_cache_bounded(&valid);
                            valid
                        }
                        Err(reason) => return Err(reason),
                    },
                    None => fallback_dir,
                };
                Ok(list_folder_entries_in_root(&browse_dir, &root_for_fs))
            });
            let listing = match fs_res {
                Ok(Ok(l)) => l,
                Ok(Err(reason)) => {
                    write_response(
                        stream,
                        "403 Forbidden",
                        "text/plain; charset=utf-8",
                        format!("Forbidden path: {reason}").as_bytes(),
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(()) => {
                    // Review #4 Finding #3: NEVER strand the user on a bare 504 text page with no
                    // Pause/Next buttons! Render the full HTML control page with an empty listing
                    // and a clear error banner so Pause, Next, and other folders stay reachable.
                    let norm_root = normalize_allowed_root(&allowed_root);
                    let parent_dir = display_dir_on_timeout
                        .parent()
                        .map(Path::to_path_buf)
                        .filter(|p| lexically_within_root(p, &norm_root));
                    FolderListing {
                        current_dir: display_dir_on_timeout,
                        parent_dir,
                        subdirs: Vec::new(),
                        flac_files: Vec::new(),
                        error: Some(
                            "Storage timed out or is busy while reading this folder. Playback controls (Pause / Next / Play) and other folders remain available."
                                .to_string(),
                        ),
                    }
                }
            };
            let html = render_control_page(&snap, &listing);
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
            if let Some(site) = req.sec_fetch_site.as_deref() {
                if !is_allowed_fetch_site(site) {
                    write_response(
                        stream,
                        "403 Forbidden",
                        "text/plain; charset=utf-8",
                        b"Forbidden: cross-site status fetch rejected",
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            }
            let snap = shared.snapshot();
            let json = render_status_json(&snap);
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
        ("POST", "/action/play-random") => {
            let dir = match extract_validated_dir_bounded(&req.body, shared, &allowed_root) {
                Ok(Ok(d)) => d,
                Ok(Err(reason)) => {
                    write_response(
                        stream,
                        "403 Forbidden",
                        "text/plain; charset=utf-8",
                        format!("Forbidden folder: {reason}").as_bytes(),
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(()) => {
                    write_storage_timeout_control_page(
                        stream,
                        shared,
                        &allowed_root,
                        &req.body,
                        "Storage timed out or is busy validating folder path. Playback controls (Pause / Next / Play) and other folders remain available.",
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            shared.submit_command(ControlCommand::PlayRandom(dir.clone()));
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/play-sequential") => {
            let dir = match extract_validated_dir_bounded(&req.body, shared, &allowed_root) {
                Ok(Ok(d)) => d,
                Ok(Err(reason)) => {
                    write_response(
                        stream,
                        "403 Forbidden",
                        "text/plain; charset=utf-8",
                        format!("Forbidden folder: {reason}").as_bytes(),
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(()) => {
                    write_storage_timeout_control_page(
                        stream,
                        shared,
                        &allowed_root,
                        &req.body,
                        "Storage timed out or is busy validating folder path. Playback controls (Pause / Next / Play) and other folders remain available.",
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            shared.submit_command(ControlCommand::PlaySequential(dir.clone()));
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/play-file") => {
            let dir = match extract_validated_dir_bounded(&req.body, shared, &allowed_root) {
                Ok(Ok(d)) => d,
                Ok(Err(reason)) => {
                    write_response(
                        stream,
                        "403 Forbidden",
                        "text/plain; charset=utf-8",
                        format!("Forbidden folder: {reason}").as_bytes(),
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(()) => {
                    write_storage_timeout_control_page(
                        stream,
                        shared,
                        &allowed_root,
                        &req.body,
                        "Storage timed out or is busy validating folder path. Playback controls (Pause / Next / Play) and other folders remain available.",
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            let Some(raw_path) =
                parse_form_param(&req.body, "path").filter(|s| !s.trim().is_empty())
            else {
                write_response(
                    stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    b"Missing file path",
                    None,
                    conn_deadline,
                    stop,
                );
                return;
            };
            let decoded = match decode_path_token(&raw_path) {
                Ok(p) => p,
                Err(reason) => {
                    write_response(
                        stream,
                        "400 Bad Request",
                        "text/plain; charset=utf-8",
                        format!("Invalid path token: {reason}").as_bytes(),
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            // Review #6 Finding #9: validate syntax and root containment lexically FIRST (zero
            // syscalls on the HTTP thread) so raw DeviceNS / Verbatim / out-of-root paths never
            // trigger any filesystem stat or SMB contact.
            let prevalidated = match prevalidate_lexical_path_for_root(&decoded, &allowed_root) {
                Ok(p) => p,
                Err(reason) => {
                    write_response(
                        stream,
                        "403 Forbidden",
                        "text/plain; charset=utf-8",
                        format!("Rejected file path: {reason}").as_bytes(),
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
            };
            let root_for_fs = allowed_root.clone();
            let file_fs_key = fs_volume_key(&prevalidated);
            let valid_file = match run_fs_op_bounded_for_key(
                &file_fs_key,
                FS_OP_DEADLINE,
                move || {
                    populate_reparse_volume_cache_bounded(&prevalidated);
                    validate_playable_flac_path(&prevalidated, Some(&root_for_fs))
                },
            ) {
                Ok(Ok(p)) => p,
                Ok(Err(reason)) => {
                    write_response(
                        stream,
                        "403 Forbidden",
                        "text/plain; charset=utf-8",
                        format!("Rejected file path: {reason}").as_bytes(),
                        None,
                        conn_deadline,
                        stop,
                    );
                    return;
                }
                Err(()) => {
                    write_storage_timeout_control_page(
                        stream,
                        shared,
                        &allowed_root,
                        &req.body,
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
        ("POST", "/action/next") => {
            // Review #3 Finding #4: submit Next IMMEDIATELY with ZERO blocking filesystem I/O
            // so a stalled NAS/USB drive can never delay or drop track skipping.
            shared.submit_command(ControlCommand::Next);
            let dir = extract_syntax_only_dir(&req.body, shared, &allowed_root);
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/pause") => {
            // Review #3 Finding #4: submit Pause IMMEDIATELY with ZERO blocking filesystem I/O
            // so releasing the exclusive DAC pin (for Audirvana / etc.) never blocks on a stalled NAS.
            shared.submit_command(ControlCommand::Pause);
            let dir = extract_syntax_only_dir(&req.body, shared, &allowed_root);
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/resume") => {
            // Submit Resume immediately with zero blocking filesystem I/O (same as Pause / Next).
            shared.submit_command(ControlCommand::Resume);
            let dir = extract_syntax_only_dir(&req.body, shared, &allowed_root);
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        _ => {
            write_response(
                stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                b"Not Found",
                None,
                conn_deadline,
                stop,
            );
        }
    }
}

fn decode_and_validate_dir(raw: &str, allowed_root: &Path) -> Result<PathBuf, String> {
    let decoded = decode_path_token(raw)?;
    validate_path_within_root(&decoded, allowed_root)
}

fn extract_validated_dir_bounded(
    body: &str,
    shared: &SharedControl,
    allowed_root: &Path,
) -> Result<Result<PathBuf, String>, ()> {
    match parse_form_param(body, "dir").filter(|s| !s.trim().is_empty()) {
        Some(raw) => {
            // Review #6 Finding #9: decode + lexical syntax/root validation strictly BEFORE any
            // bucket key or bounded worker spawn (zero syscalls on invalid / DeviceNS paths!).
            let decoded = match decode_path_token(&raw) {
                Ok(p) => p,
                Err(reason) => return Ok(Err(reason)),
            };
            let prevalidated = match prevalidate_lexical_path_for_root(&decoded, allowed_root) {
                Ok(p) => p,
                Err(reason) => return Ok(Err(reason)),
            };
            let root = allowed_root.to_path_buf();
            let fs_key = fs_volume_key(&prevalidated);
            run_fs_op_bounded_for_key(&fs_key, FS_OP_DEADLINE, move || {
                populate_reparse_volume_cache_bounded(&prevalidated);
                validate_path_within_root(&prevalidated, &root)
            })
        }
        None => Ok(Ok(shared.snapshot().active_folder)),
    }
}

/// Pure-memory redirect folder extraction performing **zero** filesystem syscalls.
/// Used by `POST /action/pause` and `POST /action/next` after command submission so a stalled NAS
/// never blocks the redirect response. Any invalid/out-of-root `dir` falls back cleanly to
/// `active_folder`, and the subsequent `GET /?dir=...` still performs full filesystem validation.
fn extract_syntax_only_dir(body: &str, shared: &SharedControl, allowed_root: &Path) -> PathBuf {
    let Some(raw) = parse_form_param(body, "dir").filter(|s| !s.trim().is_empty()) else {
        return shared.snapshot().active_folder;
    };
    let Ok(decoded) = decode_path_token(&raw) else {
        return shared.snapshot().active_folder;
    };
    let norm_root = normalize_allowed_root(allowed_root);
    let allowed_unc = extract_unc_share(&norm_root);
    let share_ref = allowed_unc
        .as_ref()
        .map(|(s, sh)| (s.as_str(), sh.as_str()));
    match validate_path_syntax_for_root(&decoded, share_ref) {
        Ok(norm) if lexically_within_root(&norm, &norm_root) => norm,
        _ => shared.snapshot().active_folder,
    }
}

/// Render the full interactive HTML control page with an error banner when a `POST /action/play-*`
/// storage validation times out or its per-volume helper bucket is saturated, so Pause / Next /
/// parent-folder navigation stay clickable instead of stranding the user on a bare text 504 page
/// (Review #5 follow-up to Review #4 #3).
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
    let html = render_control_page(&snap, &listing);
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

fn read_chunk_with_deadline(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<usize, ReadRequestError> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(ReadRequestError::ClosedOrMalformed);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(ReadRequestError::Timeout);
        };
        let slice_timeout = remaining.min(Duration::from_millis(250));
        let _ = stream.set_read_timeout(Some(slice_timeout));
        match stream.read(buf) {
            Ok(n) => return Ok(n),
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if Instant::now() >= deadline {
                    return Err(ReadRequestError::Timeout);
                }
                continue;
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(ReadRequestError::ClosedOrMalformed),
        }
    }
}

fn read_http_request(
    stream: &mut TcpStream,
    stop: &AtomicBool,
) -> Result<HttpRequest, ReadRequestError> {
    let deadline = Instant::now() + REQUEST_READ_DEADLINE;
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];

    let header_end = loop {
        let n = read_chunk_with_deadline(stream, &mut chunk, deadline, stop)?;
        if n == 0 {
            return Err(ReadRequestError::ClosedOrMalformed);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            let end = pos + 4;
            if end > MAX_HEADER_BYTES {
                return Err(ReadRequestError::HeaderTooLarge);
            }
            break end;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(ReadRequestError::HeaderTooLarge);
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.lines();
    let req_line = lines.next().ok_or(ReadRequestError::ClosedOrMalformed)?;
    let mut parts = req_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(ReadRequestError::ClosedOrMalformed)?
        .to_string();
    let target = parts
        .next()
        .ok_or(ReadRequestError::ClosedOrMalformed)?
        .to_string();

    let mut content_length: usize = 0;
    let mut host: Option<String> = None;
    let mut duplicate_host = false;
    let mut origin: Option<String> = None;
    let mut sec_fetch_site: Option<String> = None;
    let mut sec_fetch_dest: Option<String> = None;

    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let name = k.trim();
            let val = v.trim();
            if name.eq_ignore_ascii_case("content-length") {
                let parsed = val
                    .parse::<usize>()
                    .map_err(|_| ReadRequestError::BadContentLength)?;
                if parsed > MAX_BODY_BYTES {
                    return Err(ReadRequestError::BodyTooLarge);
                }
                content_length = parsed;
            } else if name.eq_ignore_ascii_case("host") {
                if host.is_some() {
                    duplicate_host = true;
                } else {
                    host = Some(val.to_string());
                }
            } else if name.eq_ignore_ascii_case("origin") {
                origin = Some(val.to_string());
            } else if name.eq_ignore_ascii_case("sec-fetch-site") {
                sec_fetch_site = Some(val.to_string());
            } else if name.eq_ignore_ascii_case("sec-fetch-dest") {
                sec_fetch_dest = Some(val.to_string());
            }
        }
    }

    let total_needed = header_end + content_length;
    while buf.len() < total_needed {
        let n = read_chunk_with_deadline(stream, &mut chunk, deadline, stop)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > header_end + MAX_BODY_BYTES {
            return Err(ReadRequestError::BodyTooLarge);
        }
    }

    let body_end = total_needed.min(buf.len());
    let body = String::from_utf8_lossy(&buf[header_end..body_end]).into_owned();
    Ok(HttpRequest {
        method,
        target,
        host,
        duplicate_host,
        origin,
        sec_fetch_site,
        sec_fetch_dest,
        body,
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn write_all_with_deadline(
    stream: &mut TcpStream,
    mut data: &[u8],
    deadline: Instant,
    stop: &AtomicBool,
) -> bool {
    while !data.is_empty() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        let slice_timeout = remaining.min(Duration::from_millis(500));
        let _ = stream.set_write_timeout(Some(slice_timeout));
        let chunk_len = data.len().min(RESPONSE_WRITE_CHUNK_BYTES);
        match stream.write(&data[..chunk_len]) {
            Ok(0) => return false,
            Ok(n) => data = &data[n..],
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if Instant::now() >= deadline {
                    return false;
                }
                continue;
            }
            Err(_) => return false,
        }
    }
    true
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
    if write_all_with_deadline(stream, head.as_bytes(), deadline, stop) {
        let _ = write_all_with_deadline(stream, body, deadline, stop);
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
