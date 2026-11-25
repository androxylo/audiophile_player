//! Dedicated bounded HTTP/1.1 server for UPnP XML, SOAP control, and GENA eventing.
//!
//! Strictly separate from `ap-control` (which stays 127.0.0.1-only with CSRF/Origin checks).

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ap_control::state::SharedControl;

use crate::gena::{GenaEventHub, GenaService, SubscribeError, is_private_or_loopback};
use crate::services::{
    SoapResponse, handle_av_transport, handle_connection_manager, handle_rendering_control,
    parse_soap_action_name, render_connection_manager_event_xml,
};
use crate::ssdp::{SsdpService, detect_local_ipv4};
use crate::state::UpnpRendererState;
use crate::xml::{
    AVTRANSPORT_SCPD_XML, CONNECTION_MANAGER_SCPD_XML, RENDERING_CONTROL_SCPD_XML,
    deterministic_udn, render_device_description_xml,
};

pub const DEFAULT_UPNP_PORT_START: u16 = 49152;
pub const DEFAULT_UPNP_PORT_END: u16 = 49250;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 128 * 1024;
const MAX_WORKERS: usize = 16;
/// Maximum concurrent HTTP/SOAP connections allowed from a single peer IP (Review #5 #27).
const MAX_WORKERS_PER_PEER: usize = 4;
/// Tight deadline to receive the first request byte on an accepted TCP socket (Review #5 #27).
const FIRST_BYTE_DEADLINE: Duration = Duration::from_secs(2);
const READ_DEADLINE: Duration = Duration::from_secs(5);
const CONN_DEADLINE: Duration = Duration::from_secs(10);
const SERVER_HEADER: &str = "Windows/10 UPnP/1.0 AudiophilePlayer/0.1";

#[derive(Default)]
struct WorkerPoolState {
    total: usize,
    per_peer: HashMap<IpAddr, usize>,
}

/// Startup configuration for [`UpnpRendererServer`].
#[derive(Debug, Clone)]
pub struct UpnpRendererConfig {
    pub friendly_name: String,
    pub port: Option<u16>,
    pub bind_ip: Option<Ipv4Addr>,
    /// Enable UDP SSDP multicast discovery (`false` in isolated loopback tests).
    pub enable_ssdp: bool,
}

impl Default for UpnpRendererConfig {
    fn default() -> Self {
        Self {
            friendly_name: "Audiophile Player".to_string(),
            port: None,
            bind_ip: None,
            enable_ssdp: true,
        }
    }
}

/// Running UPnP MediaRenderer server (HTTP XML/SOAP/GENA + SSDP discovery + shared state).
pub struct UpnpRendererServer {
    addr: SocketAddr,
    advertised_ip: Ipv4Addr,
    state: Arc<UpnpRendererState>,
    stop: Arc<AtomicBool>,
    workers: Arc<Mutex<WorkerPoolState>>,
    http_handle: Option<JoinHandle<()>>,
    ssdp: Option<SsdpService>,
}

impl UpnpRendererServer {
    pub fn start(
        control: Arc<SharedControl>,
        cfg: UpnpRendererConfig,
    ) -> Result<Self, String> {
        // Review #5 Finding #26: bind specifically to the chosen/advertised interface rather than
        // 0.0.0.0 across VPN / public Wi-Fi / Hyper-V interfaces (explicit 0.0.0.0 still allowed).
        let advertised_ip = match cfg.bind_ip {
            Some(ip) if !ip.is_unspecified() => ip,
            _ => detect_local_ipv4(),
        };
        let bind_ip = cfg.bind_ip.unwrap_or(advertised_ip);
        let (listener, port) = match cfg.port {
            Some(p) => bind_upnp_port(bind_ip, p, p)?,
            None => bind_upnp_port(bind_ip, DEFAULT_UPNP_PORT_START, DEFAULT_UPNP_PORT_END)?,
        };
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("failed setting UPnP listener nonblocking: {e}"))?;

        let addr = SocketAddr::from((bind_ip, port));
        let udn = deterministic_udn(&cfg.friendly_name);
        let gena = GenaEventHub::start();
        let state = UpnpRendererState::new(
            Arc::clone(&control),
            gena,
            udn.clone(),
            cfg.friendly_name.clone(),
        );

        let location_url = format!("http://{advertised_ip}:{port}/description.xml");
        control.record_action(format!(
            "UPnP MediaRenderer '{}' listening on {location_url} ({udn})",
            cfg.friendly_name
        ));

        let stop = Arc::new(AtomicBool::new(false));
        let workers = Arc::new(Mutex::new(WorkerPoolState::default()));
        let stop_bg = Arc::clone(&stop);
        let workers_bg = Arc::clone(&workers);
        let state_bg = Arc::clone(&state);

        let http_handle = thread::Builder::new()
            .name("ap-upnp-http".to_string())
            .spawn(move || accept_loop(listener, state_bg, stop_bg, workers_bg))
            .map_err(|e| format!("failed spawning UPnP HTTP server thread: {e}"))?;

        let ssdp = if cfg.enable_ssdp {
            Some(SsdpService::start(advertised_ip, location_url, udn))
        } else {
            None
        };

        Ok(Self {
            addr,
            advertised_ip,
            state,
            stop,
            workers,
            http_handle: Some(http_handle),
            ssdp,
        })
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn location_url(&self) -> String {
        format!("http://{}:{}/description.xml", self.advertised_ip, self.addr.port())
    }

    pub fn udn(&self) -> &str {
        self.state.udn()
    }

    pub fn friendly_name(&self) -> &str {
        self.state.friendly_name()
    }

    pub fn state(&self) -> &Arc<UpnpRendererState> {
        &self.state
    }

    pub fn shutdown(&mut self) {
        if let Some(mut ssdp) = self.ssdp.take() {
            ssdp.shutdown();
        }
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.http_handle.take() {
            let _ = h.join();
        }
        self.state.gena().shutdown();
        let deadline = Instant::now() + Duration::from_millis(200);
        while self.workers.lock().unwrap_or_else(|e| e.into_inner()).total > 0
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for UpnpRendererServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn bind_upnp_port(
    bind_ip: Ipv4Addr,
    start: u16,
    end: u16,
) -> Result<(TcpListener, u16), String> {
    if start == 0 || start > end {
        return Err(format!("invalid UPnP port range {start}..={end}"));
    }
    let probe_ip = if bind_ip.is_unspecified() {
        Ipv4Addr::LOCALHOST
    } else {
        bind_ip
    };
    let mut last_err = String::new();
    for port in start..=end {
        // Avoid Windows' dual-bind quirk when another process already holds 0.0.0.0:port.
        if TcpStream::connect_timeout(&SocketAddr::from((probe_ip, port)), Duration::from_millis(20))
            .is_ok()
        {
            last_err = format!("port {port} is already accepting connections on {probe_ip}");
            continue;
        }
        match TcpListener::bind((bind_ip, port)) {
            Ok(l) => return Ok((l, port)),
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!(
        "could not bind UPnP HTTP server on {bind_ip}:{start}..={end}: {last_err}"
    ))
}

struct WorkerGuard {
    pool: Arc<Mutex<WorkerPoolState>>,
    peer: IpAddr,
}
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut st = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        st.total = st.total.saturating_sub(1);
        if let Some(cnt) = st.per_peer.get_mut(&self.peer) {
            *cnt = cnt.saturating_sub(1);
            if *cnt == 0 {
                st.per_peer.remove(&self.peer);
            }
        }
    }
}

fn try_acquire_worker(pool: &Arc<Mutex<WorkerPoolState>>, peer: IpAddr) -> Option<WorkerGuard> {
    let mut st = pool.lock().unwrap_or_else(|e| e.into_inner());
    let peer_active = st.per_peer.get(&peer).copied().unwrap_or(0);
    if st.total >= MAX_WORKERS || peer_active >= MAX_WORKERS_PER_PEER {
        return None;
    }
    st.total += 1;
    *st.per_peer.entry(peer).or_insert(0) += 1;
    Some(WorkerGuard {
        pool: Arc::clone(pool),
        peer,
    })
}

fn accept_loop(
    listener: TcpListener,
    state: Arc<UpnpRendererState>,
    stop: Arc<AtomicBool>,
    workers: Arc<Mutex<WorkerPoolState>>,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, peer_addr)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                // Review #5 Finding #26: reject connections from outside loopback/RFC1918/link-local.
                if !is_private_or_loopback(peer_addr.ip()) {
                    let _ = write_http_reply(
                        &mut stream,
                        "403 Forbidden",
                        "text/plain; charset=\"utf-8\"",
                        &[],
                        b"Forbidden: non-LAN UPnP peer rejected",
                        false,
                    );
                    continue;
                }
                let Some(guard) = try_acquire_worker(&workers, peer_addr.ip()) else {
                    let _ = write_http_reply(
                        &mut stream,
                        "503 Service Unavailable",
                        "text/plain; charset=\"utf-8\"",
                        &[],
                        b"UPnP server busy",
                        false,
                    );
                    continue;
                };
                let state_w = Arc::clone(&state);
                let stop_w = Arc::clone(&stop);
                let _ = thread::Builder::new()
                    .name("ap-upnp-worker".to_string())
                    .spawn(move || {
                        let _g = guard;
                        handle_connection(&mut stream, &state_w, &stop_w);
                    });
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

struct UpnpHttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

fn handle_connection(stream: &mut TcpStream, state: &UpnpRendererState, stop: &AtomicBool) {
    let req = match read_upnp_http_request(stream, stop) {
        Ok(r) => r,
        Err(status) => {
            if !status.is_empty() {
                let _ = write_http_reply(
                    stream,
                    status,
                    "text/plain; charset=\"utf-8\"",
                    &[],
                    status.as_bytes(),
                    false,
                );
            }
            return;
        }
    };

    let path = req.path.split('?').next().unwrap_or(req.path.as_str());
    let is_head = req.method == "HEAD";

    match (req.method.as_str(), path) {
        ("GET" | "HEAD", "/description.xml") => {
            let xml = render_device_description_xml(
                state.udn(),
                state.friendly_name(),
                &state.dac_name(),
            );
            let _ = write_http_reply(
                stream,
                "200 OK",
                "text/xml; charset=\"utf-8\"",
                &[],
                xml.as_bytes(),
                is_head,
            );
        }
        ("GET" | "HEAD", "/scpd/AVTransport.xml") => {
            let _ = write_http_reply(
                stream,
                "200 OK",
                "text/xml; charset=\"utf-8\"",
                &[],
                AVTRANSPORT_SCPD_XML.as_bytes(),
                is_head,
            );
        }
        ("GET" | "HEAD", "/scpd/ConnectionManager.xml") => {
            let _ = write_http_reply(
                stream,
                "200 OK",
                "text/xml; charset=\"utf-8\"",
                &[],
                CONNECTION_MANAGER_SCPD_XML.as_bytes(),
                is_head,
            );
        }
        ("GET" | "HEAD", "/scpd/RenderingControl.xml") => {
            let _ = write_http_reply(
                stream,
                "200 OK",
                "text/xml; charset=\"utf-8\"",
                &[],
                RENDERING_CONTROL_SCPD_XML.as_bytes(),
                is_head,
            );
        }
        ("POST", "/upnp/control/AVTransport") => {
            let resp = match parse_soap_action_name(
                req.headers.get("soapaction").map(String::as_str),
            ) {
                Some(action) => handle_av_transport(&action, &req.body, state),
                None => SoapResponse::fault(401, "Missing SOAPACTION header"),
            };
            let _ = write_http_reply(
                stream,
                resp.status,
                "text/xml; charset=\"utf-8\"",
                &[("EXT", "")],
                resp.xml.as_bytes(),
                false,
            );
        }
        ("POST", "/upnp/control/ConnectionManager") => {
            let resp = match parse_soap_action_name(
                req.headers.get("soapaction").map(String::as_str),
            ) {
                Some(action) => handle_connection_manager(&action, state),
                None => SoapResponse::fault(401, "Missing SOAPACTION header"),
            };
            let _ = write_http_reply(
                stream,
                resp.status,
                "text/xml; charset=\"utf-8\"",
                &[("EXT", "")],
                resp.xml.as_bytes(),
                false,
            );
        }
        ("POST", "/upnp/control/RenderingControl") => {
            let resp = match parse_soap_action_name(
                req.headers.get("soapaction").map(String::as_str),
            ) {
                Some(action) => handle_rendering_control(&action, state),
                None => SoapResponse::fault(401, "Missing SOAPACTION header"),
            };
            let _ = write_http_reply(
                stream,
                resp.status,
                "text/xml; charset=\"utf-8\"",
                &[("EXT", "")],
                resp.xml.as_bytes(),
                false,
            );
        }
        ("SUBSCRIBE", "/upnp/event/AVTransport") => {
            handle_subscribe(stream, GenaService::AvTransport, &req, state);
        }
        ("SUBSCRIBE", "/upnp/event/ConnectionManager") => {
            handle_subscribe(stream, GenaService::ConnectionManager, &req, state);
        }
        ("SUBSCRIBE", "/upnp/event/RenderingControl") => {
            handle_subscribe(stream, GenaService::RenderingControl, &req, state);
        }
        ("UNSUBSCRIBE", "/upnp/event/AVTransport")
        | ("UNSUBSCRIBE", "/upnp/event/ConnectionManager")
        | ("UNSUBSCRIBE", "/upnp/event/RenderingControl") => {
            if let Some(sid) = req.headers.get("sid") {
                if state.gena().unsubscribe(sid) {
                    let _ = write_http_reply(stream, "200 OK", "text/plain", &[], b"", false);
                } else {
                    let _ = write_http_reply(
                        stream,
                        "412 Precondition Failed",
                        "text/plain",
                        &[],
                        b"",
                        false,
                    );
                }
            } else {
                let _ = write_http_reply(
                    stream,
                    "412 Precondition Failed",
                    "text/plain",
                    &[],
                    b"",
                    false,
                );
            }
        }
        _ => {
            let _ = write_http_reply(
                stream,
                "404 Not Found",
                "text/plain; charset=\"utf-8\"",
                &[],
                b"Not Found",
                is_head,
            );
        }
    }
}

fn handle_subscribe(
    stream: &mut TcpStream,
    service: GenaService,
    req: &UpnpHttpRequest,
    state: &UpnpRendererState,
) {
    let timeout_hdr = req.headers.get("timeout").map(String::as_str);
    if let Some(sid) = req.headers.get("sid") {
        // Subscription renewal.
        match state.gena().renew(sid, timeout_hdr) {
            Ok(secs) => {
                let timeout_val = format!("Second-{secs}");
                let _ = write_http_reply(
                    stream,
                    "200 OK",
                    "text/plain",
                    &[("SID", sid.as_str()), ("TIMEOUT", timeout_val.as_str())],
                    b"",
                    false,
                );
            }
            Err(_) => {
                let _ = write_http_reply(
                    stream,
                    "412 Precondition Failed",
                    "text/plain",
                    &[],
                    b"",
                    false,
                );
            }
        }
        return;
    }

    let Some(cb) = req.headers.get("callback") else {
        let _ = write_http_reply(
            stream,
            "412 Precondition Failed",
            "text/plain",
            &[],
            b"",
            false,
        );
        return;
    };

    let initial_xml = match service {
        GenaService::AvTransport => state.render_avt_event_xml(),
        GenaService::ConnectionManager => render_connection_manager_event_xml(),
        GenaService::RenderingControl => state.render_rcs_event_xml(),
    };

    match state.gena().subscribe(service, cb, timeout_hdr, initial_xml) {
        Ok((sid, secs, initial_job)) => {
            let timeout_val = format!("Second-{secs}");
            // Review #5 Finding #33: write and flush the 200 OK response (with SID) BEFORE
            // enqueueing the initial SEQ: 0 NOTIFY so the control point already knows the SID.
            let write_ok = write_http_reply(
                stream,
                "200 OK",
                "text/plain",
                &[("SID", sid.as_str()), ("TIMEOUT", timeout_val.as_str())],
                b"",
                false,
            )
            .is_ok();
            if write_ok {
                state.gena().enqueue_initial(initial_job);
            }
        }
        Err(SubscribeError::CapacityExceeded) => {
            let _ = write_http_reply(
                stream,
                "503 Service Unavailable",
                "text/plain",
                &[],
                b"Too many active GENA subscriptions",
                false,
            );
        }
        Err(SubscribeError::InvalidRequest(_)) => {
            let _ = write_http_reply(
                stream,
                "412 Precondition Failed",
                "text/plain",
                &[],
                b"",
                false,
            );
        }
    }
}

fn read_upnp_http_request(
    stream: &mut TcpStream,
    stop: &AtomicBool,
) -> Result<UpnpHttpRequest, &'static str> {
    let now = Instant::now();
    let first_byte_deadline = now + FIRST_BYTE_DEADLINE;
    let deadline = now + READ_DEADLINE;
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));

    let mut raw = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        if stop.load(Ordering::Acquire) {
            return Err("");
        }
        let cur_now = Instant::now();
        if (raw.is_empty() && cur_now >= first_byte_deadline) || cur_now >= deadline {
            return Err("408 Request Timeout");
        }
        match stream.read(&mut tmp) {
            Ok(0) => return Err(""),
            Ok(n) => {
                raw.extend_from_slice(&tmp[..n]);
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos;
                }
                if raw.len() > MAX_HEADER_BYTES {
                    return Err("431 Request Header Fields Too Large");
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue;
            }
            Err(_) => return Err(""),
        }
    };

    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header_text.split("\r\n");
    let req_line = lines.next().ok_or("400 Bad Request")?;
    let mut req_parts = req_line.split_whitespace();
    let method = req_parts.next().ok_or("400 Bad Request")?.to_uppercase();
    let path = req_parts.next().ok_or("400 Bad Request")?.to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    // Honor `Expect: 100-continue` before reading SOAP request bodies.
    if headers
        .get("expect")
        .is_some_and(|v| v.to_ascii_lowercase().contains("100-continue"))
    {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = stream.flush();
    }

    let body_start = header_end + 4;
    let mut body_bytes = raw[body_start..].to_vec();

    let is_chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));

    if is_chunked {
        let decoded = read_chunked_body(stream, &mut body_bytes, deadline, stop)?;
        return Ok(UpnpHttpRequest {
            method,
            path,
            headers,
            body: String::from_utf8_lossy(&decoded).into_owned(),
        });
    }

    let content_len = match headers.get("content-length") {
        Some(v) => {
            let n = v.parse::<usize>().map_err(|_| "400 Bad Request")?;
            if n > MAX_BODY_BYTES {
                return Err("413 Payload Too Large");
            }
            n
        }
        None => 0,
    };

    while body_bytes.len() < content_len {
        if stop.load(Ordering::Acquire) {
            return Err("");
        }
        if Instant::now() >= deadline {
            return Err("408 Request Timeout");
        }
        let want = (content_len - body_bytes.len()).min(tmp.len());
        match stream.read(&mut tmp[..want]) {
            Ok(0) => return Err("400 Bad Request"),
            Ok(n) => body_bytes.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue;
            }
            Err(_) => return Err(""),
        }
    }
    body_bytes.truncate(content_len);

    Ok(UpnpHttpRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
    })
}

fn read_chunked_body(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut tmp = [0u8; 2048];

    loop {
        // Ensure we have a chunk-size line ending in `\r\n`.
        let crlf_rel = loop {
            if let Some(rel) = buf[pos..].windows(2).position(|w| w == b"\r\n") {
                break rel;
            }
            if stop.load(Ordering::Acquire) {
                return Err("");
            }
            if Instant::now() >= deadline {
                return Err("408 Request Timeout");
            }
            if buf.len() > MAX_BODY_BYTES + 4096 {
                return Err("413 Payload Too Large");
            }
            match stream.read(&mut tmp) {
                Ok(0) => return Err("400 Bad Request"),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => return Err(""),
            }
        };
        let line = String::from_utf8_lossy(&buf[pos..pos + crlf_rel]);
        let hex_str = line.split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(hex_str, 16).map_err(|_| "400 Bad Request")?;
        pos += crlf_rel + 2;
        if chunk_len == 0 {
            break;
        }
        if out.len().saturating_add(chunk_len) > MAX_BODY_BYTES {
            return Err("413 Payload Too Large");
        }
        let need_end = pos + chunk_len + 2;
        while buf.len() < need_end {
            if stop.load(Ordering::Acquire) {
                return Err("");
            }
            if Instant::now() >= deadline {
                return Err("408 Request Timeout");
            }
            match stream.read(&mut tmp) {
                Ok(0) => return Err("400 Bad Request"),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => return Err(""),
            }
        }
        out.extend_from_slice(&buf[pos..pos + chunk_len]);
        pos = need_end;
    }
    Ok(out)
}

fn write_http_reply(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    body: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    let deadline = Instant::now() + CONN_DEADLINE;
    let mut head = format!(
        "HTTP/1.1 {status}\r\n\
         SERVER: {SERVER_HEADER}\r\n\
         CONTENT-TYPE: {content_type}\r\n\
         CONTENT-LENGTH: {}\r\n\
         CONNECTION: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        if v.is_empty() {
            head.push_str(&format!("{k}:\r\n"));
        } else {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    head.push_str("\r\n");

    write_all_bounded(stream, head.as_bytes(), deadline)?;
    if !head_only && !body.is_empty() {
        write_all_bounded(stream, body, deadline)?;
    }
    let _ = stream.flush();
    Ok(())
}

fn write_all_bounded(
    stream: &mut TcpStream,
    data: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut written = 0usize;
    while written < data.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                "UPnP HTTP write deadline exceeded",
            ));
        }
        match stream.write(&data[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::WriteZero,
                    "UPnP HTTP socket closed during write",
                ));
            }
            Ok(n) => written += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
