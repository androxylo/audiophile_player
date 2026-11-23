//! Localhost-only HTTP/1.1 control server strictly decoupled from the audio output thread.
//!
//! Port selection policy:
//! - Starts at a clean non-conflicting localhost range (`17441..=17499` — nod to 44.1 kHz).
//! - **Never** binds or touches conventional service ports like `80` or `8080`.
//! - Automatically advances to the next free port in range if the first port is occupied.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::browser::{list_folder_entries, parse_form_param, url_encode};
use crate::html::{render_control_page, render_status_json};
use crate::state::{ControlCommand, SharedControl};

/// Conventional non-conflicting localhost port range (far away from 80, 443, 3000, 8000, 8080).
pub const DEFAULT_PORT_START: u16 = 17_441;
pub const DEFAULT_PORT_END: u16 = 17_499;

/// Bind a localhost `TcpListener` starting at `start_port` up to `end_port`.
pub fn bind_first_available_port(start_port: u16, end_port: u16) -> Result<(TcpListener, u16), String> {
    if start_port > end_port {
        return Err(format!("invalid port range {start_port}..={end_port}"));
    }
    let mut last_err = None;
    for port in start_port..=end_port {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok((listener, port)),
            Err(e) => last_err = Some(e),
        }
    }
    Err(format!(
        "could not bind any localhost HTTP port in {start_port}..={end_port}: {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Running rudimentary HTTP control server handle. Stops cleanly on [`ControlServer::shutdown`] or drop.
pub struct ControlServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ControlServer {
    /// Start the HTTP server on `explicit_port` (if given) or the first free port in
    /// `DEFAULT_PORT_START..=DEFAULT_PORT_END` (`17441..=17499`).
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
        let stop_bg = Arc::clone(&stop);

        shared.record_action(format!("HTTP control server listening on http://{addr}"));

        let handle = thread::Builder::new()
            .name("ap-http-control".to_string())
            .spawn(move || serve_loop(listener, shared, stop_bg))
            .map_err(|e| format!("failed spawning HTTP control thread: {e}"))?;

        Ok(Self {
            addr,
            stop,
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
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn serve_loop(listener: TcpListener, shared: Arc<SharedControl>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                handle_connection(&mut stream, &shared);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_connection(stream: &mut TcpStream, shared: &SharedControl) {
    let Some((method, target, body)) = read_http_request(stream) else {
        return;
    };

    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));

    match (method.as_str(), path) {
        ("GET", "/") => {
            let snap = shared.snapshot();
            let browse_dir = parse_form_param(query, "dir")
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| snap.active_folder.clone());
            let listing = list_folder_entries(&browse_dir);
            let html = render_control_page(&snap, &listing);
            write_response(stream, "200 OK", "text/html; charset=utf-8", html.as_bytes(), None);
        }
        ("GET", "/api/status") => {
            let snap = shared.snapshot();
            let json = render_status_json(&snap);
            write_response(
                stream,
                "200 OK",
                "application/json; charset=utf-8",
                json.as_bytes(),
                None,
            );
        }
        ("POST", "/action/play-random") => {
            let dir = extract_dir(&body, shared);
            shared.submit_command(ControlCommand::PlayRandom(dir.clone()));
            redirect_to_dir(stream, &dir);
        }
        ("POST", "/action/play-sequential") => {
            let dir = extract_dir(&body, shared);
            shared.submit_command(ControlCommand::PlaySequential(dir.clone()));
            redirect_to_dir(stream, &dir);
        }
        ("POST", "/action/play-file") => {
            let dir = extract_dir(&body, shared);
            if let Some(p) = parse_form_param(&body, "path").filter(|s| !s.trim().is_empty()) {
                shared.submit_command(ControlCommand::PlayFile(PathBuf::from(p)));
            }
            redirect_to_dir(stream, &dir);
        }
        ("POST", "/action/pause") => {
            let dir = extract_dir(&body, shared);
            shared.submit_command(ControlCommand::Pause);
            redirect_to_dir(stream, &dir);
        }
        _ => {
            write_response(stream, "404 Not Found", "text/plain; charset=utf-8", b"Not Found", None);
        }
    }
}

fn extract_dir(body: &str, shared: &SharedControl) -> PathBuf {
    parse_form_param(body, "dir")
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| shared.snapshot().active_folder)
}

fn redirect_to_dir(stream: &mut TcpStream, dir: &PathBuf) {
    let loc = format!("/?dir={}", url_encode(&dir.to_string_lossy()));
    write_response(stream, "303 See Other", "text/plain; charset=utf-8", b"OK", Some(&loc));
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    const MAX_REQ_BYTES: usize = 32 * 1024;
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 1024];

    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() >= MAX_REQ_BYTES {
            return None;
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = header_text.lines();
    let req_line = lines.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut content_length: usize = 0;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse::<usize>().unwrap_or(0).min(MAX_REQ_BYTES);
            }
        }
    }

    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() >= MAX_REQ_BYTES {
            break;
        }
    }

    let body_end = (header_end + content_length).min(buf.len());
    let body = String::from_utf8_lossy(&buf[header_end..body_end]).into_owned();
    Some((method, target, body))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
    location: Option<&str>,
) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(loc) = location {
        head.push_str(&format!("Location: {loc}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}
