// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Dedicated bounded HTTP/1.1 server for UPnP XML, SOAP control, and GENA eventing.
//!
//! Strictly separate from `ap-control` (which stays 127.0.0.1-only with CSRF/Origin checks).

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ap_control::state::SharedControl;
use ap_core::http1::{HttpReadLimits, HttpRequest, read_http_request, write_all_bounded};

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
            .spawn(move || accept_loop(listener, port, state_bg, stop_bg, workers_bg))
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
    bound_port: u16,
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
                        handle_connection(&mut stream, bound_port, &state_w, &stop_w);
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

/// Validate `Host` header against DNS rebinding on the LAN UPnP HTTP server (Review #9 O16).
/// Accepts `localhost:<port>` or any loopback / RFC 1918 / link-local IP literal matching `bound_port`.
fn is_valid_upnp_host(host: &str, bound_port: u16) -> bool {
    let trimmed = host.trim();
    let (host_part, port_part) = if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((ip6, tail)) = rest.split_once(']') else {
            return false;
        };
        let p = match tail.strip_prefix(':') {
            Some(ps) => ps.parse::<u16>().ok(),
            None if tail.is_empty() => Some(80),
            None => None,
        };
        (ip6, p)
    } else if let Some((h, ps)) = trimmed.rsplit_once(':') {
        (h, ps.parse::<u16>().ok())
    } else {
        (trimmed, Some(80))
    };
    if port_part != Some(bound_port) {
        return false;
    }
    if host_part.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host_part
        .parse::<IpAddr>()
        .is_ok_and(is_private_or_loopback)
}

const UPNP_READ_LIMITS: HttpReadLimits = HttpReadLimits {
    max_header_bytes: MAX_HEADER_BYTES,
    max_body_bytes: MAX_BODY_BYTES,
    first_byte_deadline: FIRST_BYTE_DEADLINE,
    read_deadline: READ_DEADLINE,
    allow_chunked: true,
    honor_expect_100: true,
};

const SCPD_ROUTES: &[(&str, &str)] = &[
    ("/scpd/AVTransport.xml", AVTRANSPORT_SCPD_XML),
    ("/scpd/ConnectionManager.xml", CONNECTION_MANAGER_SCPD_XML),
    ("/scpd/RenderingControl.xml", RENDERING_CONTROL_SCPD_XML),
];

const EVENT_ROUTES: &[(&str, GenaService)] = &[
    ("/upnp/event/AVTransport", GenaService::AvTransport),
    ("/upnp/event/ConnectionManager", GenaService::ConnectionManager),
    ("/upnp/event/RenderingControl", GenaService::RenderingControl),
];

fn reply_412(stream: &mut TcpStream) {
    let _ = write_http_reply(stream, "412 Precondition Failed", "text/plain", &[], b"", false);
}

fn reply_soap(stream: &mut TcpStream, resp: SoapResponse) {
    let _ = write_http_reply(
        stream,
        resp.status,
        "text/xml; charset=\"utf-8\"",
        &[("EXT", "")],
        resp.xml.as_bytes(),
        false,
    );
}

fn handle_connection(
    stream: &mut TcpStream,
    bound_port: u16,
    state: &UpnpRendererState,
    stop: &AtomicBool,
) {
    let req = match read_http_request(stream, &UPNP_READ_LIMITS, stop) {
        Ok(r) => r,
        Err(err) => {
            if let Some((status, msg)) = err.http_status() {
                let _ = write_http_reply(
                    stream,
                    status,
                    "text/plain; charset=\"utf-8\"",
                    &[],
                    msg.as_bytes(),
                    false,
                );
            }
            return;
        }
    };

    // Review #9 O16: block DNS rebinding via Host header validation (must be loopback/LAN IP or localhost).
    if req.has_duplicate_header("host")
        || !req
            .header("host")
            .is_some_and(|h| is_valid_upnp_host(h, bound_port))
    {
        let _ = write_http_reply(
            stream,
            "403 Forbidden",
            "text/plain; charset=\"utf-8\"",
            &[],
            b"Forbidden: invalid UPnP Host header",
            false,
        );
        return;
    }

    let path = req.target.split('?').next().unwrap_or(req.target.as_str());
    let is_head = req.method == "HEAD";

    // Table-driven UPnP dispatch (Review #9 O13).
    if matches!(req.method.as_str(), "GET" | "HEAD") {
        if path == "/description.xml" {
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
            return;
        }
        if let Some(&(_, scpd_xml)) = SCPD_ROUTES.iter().find(|&&(p, _)| p == path) {
            let _ = write_http_reply(
                stream,
                "200 OK",
                "text/xml; charset=\"utf-8\"",
                &[],
                scpd_xml.as_bytes(),
                is_head,
            );
            return;
        }
    }

    if req.method == "POST" && path.starts_with("/upnp/control/") {
        let body = req.body_lossy();
        let soap_resp = match parse_soap_action_name(req.header("soapaction")) {
            None => Some(SoapResponse::fault(401, "Missing SOAPACTION header")),
            Some(action) => match path {
                "/upnp/control/AVTransport" => Some(handle_av_transport(&action, &body, state)),
                "/upnp/control/ConnectionManager" => {
                    Some(handle_connection_manager(&action, state))
                }
                "/upnp/control/RenderingControl" => Some(handle_rendering_control(&action, state)),
                _ => None,
            },
        };
        if let Some(resp) = soap_resp {
            reply_soap(stream, resp);
            return;
        }
    }

    if let Some(&(_, service)) = EVENT_ROUTES.iter().find(|&&(p, _)| p == path) {
        match req.method.as_str() {
            "SUBSCRIBE" => {
                handle_subscribe(stream, service, &req, state);
                return;
            }
            "UNSUBSCRIBE" => {
                if req.header("sid").is_some_and(|sid| state.gena().unsubscribe(sid)) {
                    let _ = write_http_reply(stream, "200 OK", "text/plain", &[], b"", false);
                } else {
                    reply_412(stream);
                }
                return;
            }
            _ => {}
        }
    }

    let _ = write_http_reply(
        stream,
        "404 Not Found",
        "text/plain; charset=\"utf-8\"",
        &[],
        b"Not Found",
        is_head,
    );
}

fn handle_subscribe(
    stream: &mut TcpStream,
    service: GenaService,
    req: &HttpRequest,
    state: &UpnpRendererState,
) {
    let timeout_hdr = req.header("timeout");
    if let Some(sid) = req.header("sid") {
        // Subscription renewal.
        match state.gena().renew(sid, timeout_hdr) {
            Ok(secs) => {
                let timeout_val = format!("Second-{secs}");
                let _ = write_http_reply(
                    stream,
                    "200 OK",
                    "text/plain",
                    &[("SID", sid), ("TIMEOUT", timeout_val.as_str())],
                    b"",
                    false,
                );
            }
            Err(_) => reply_412(stream),
        }
        return;
    }

    let Some(cb) = req.header("callback") else {
        reply_412(stream);
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
        Err(SubscribeError::InvalidRequest(_)) => reply_412(stream),
    }
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

    write_all_bounded(stream, head.as_bytes(), deadline, None, 16 * 1024)?;
    if !head_only && !body.is_empty() {
        write_all_bounded(stream, body, deadline, None, 16 * 1024)?;
    }
    let _ = std::io::Write::flush(stream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_valid_upnp_host;

    #[test]
    fn upnp_host_header_blocks_dns_rebinding_and_allows_lan_o16() {
        assert!(is_valid_upnp_host("127.0.0.1:49152", 49152));
        assert!(is_valid_upnp_host("localhost:49152", 49152));
        assert!(is_valid_upnp_host("192.168.1.50:49152", 49152));
        assert!(is_valid_upnp_host("10.0.0.5:49152", 49152));
        assert!(is_valid_upnp_host("[::1]:49152", 49152));
        assert!(!is_valid_upnp_host("evil.example.com:49152", 49152));
        assert!(!is_valid_upnp_host("8.8.8.8:49152", 49152));
        assert!(!is_valid_upnp_host("192.168.1.50:9999", 49152));
    }
}
