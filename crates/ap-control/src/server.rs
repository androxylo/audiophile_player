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

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::browser::{
    decode_path_token, encode_path_token, list_folder_entries_in_root, parse_form_param,
    url_encode,
};
use crate::html::{render_control_page, render_status_json};
use crate::security::{
    is_allowed_fetch_dest, is_allowed_fetch_site, is_valid_localhost_host,
    is_valid_localhost_origin, validate_path_within_root, validate_playable_flac_path,
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
/// Overall wall-clock deadline for an entire HTTP connection (read + browse + chunked write)
/// so slow-reading clients cannot pin worker slots indefinitely.
pub const CONNECTION_TOTAL_DEADLINE: Duration = Duration::from_secs(10);
/// Chunk size used when writing HTTP response bodies under the overall connection deadline.
const RESPONSE_WRITE_CHUNK_BYTES: usize = 16 * 1024;
/// Maximum concurrent connection worker threads.
pub const MAX_CONCURRENT_WORKERS: usize = 16;

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

/// Best-effort write of the active localhost control UI URL for launcher discovery.
pub fn write_runtime_url_file(url: &str) {
    if let Some(path) = runtime_url_file_path() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, format!("{url}\n"));
    }
}

/// Best-effort cleanup of the runtime URL file if it still points to `url`.
pub fn clear_runtime_url_file(url: &str) {
    if let Some(path) = runtime_url_file_path() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if contents.trim() == url.trim() {
                let _ = std::fs::remove_file(&path);
            }
        }
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
            let browse_dir = match raw_dir_param {
                Some(raw) => match decode_and_validate_dir(&raw, &allowed_root) {
                    Ok(valid) => valid,
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
                },
                None => snap.active_folder.clone(),
            };

            let listing = list_folder_entries_in_root(&browse_dir, &allowed_root);
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
            let dir = match extract_validated_dir(&req.body, shared, &allowed_root) {
                Ok(d) => d,
                Err(reason) => {
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
            };
            shared.submit_command(ControlCommand::PlayRandom(dir.clone()));
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/play-sequential") => {
            let dir = match extract_validated_dir(&req.body, shared, &allowed_root) {
                Ok(d) => d,
                Err(reason) => {
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
            };
            shared.submit_command(ControlCommand::PlaySequential(dir.clone()));
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/play-file") => {
            let dir = match extract_validated_dir(&req.body, shared, &allowed_root) {
                Ok(d) => d,
                Err(reason) => {
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
            let valid_file = match validate_playable_flac_path(&decoded, Some(&allowed_root)) {
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
            shared.submit_command(ControlCommand::PlayFile(valid_file));
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/next") => {
            // Next does not act on a user-supplied filesystem path; fall back cleanly to
            // active_folder if the hidden redirect `dir` token is missing or stale.
            let dir = extract_validated_dir(&req.body, shared, &allowed_root)
                .unwrap_or_else(|_| shared.snapshot().active_folder);
            shared.submit_command(ControlCommand::Next);
            redirect_to_dir(stream, &dir, conn_deadline, stop);
        }
        ("POST", "/action/pause") => {
            // Pause MUST always succeed to release the exclusive DAC pin, even if the hidden
            // redirect `dir` token is missing or stale.
            let dir = extract_validated_dir(&req.body, shared, &allowed_root)
                .unwrap_or_else(|_| shared.snapshot().active_folder);
            shared.submit_command(ControlCommand::Pause);
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

fn extract_validated_dir(
    body: &str,
    shared: &SharedControl,
    allowed_root: &Path,
) -> Result<PathBuf, String> {
    match parse_form_param(body, "dir").filter(|s| !s.trim().is_empty()) {
        Some(raw) => decode_and_validate_dir(&raw, allowed_root),
        None => Ok(shared.snapshot().active_folder),
    }
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
