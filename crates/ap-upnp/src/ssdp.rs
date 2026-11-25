//! SSDP multicast discovery (`239.255.255.250:1900`) for UPnP AV MediaRenderer:1.
//!
//! - Uses `SO_REUSEADDR` on Windows via `Win32_Networking_WinSock` so the renderer coexists
//!   cleanly with Windows' built-in SSDP Discovery service (`SSDPSRV`) and local `foobar2000`.
//! - Enables `IP_MULTICAST_LOOP` so local control points on the same Windows PC discover the
//!   renderer immediately, alongside LAN control points (BubbleUPnP, remote foobar2000).
//! - Sends `NOTIFY ssdp:alive` at startup + periodic keepalive, answers unicast `M-SEARCH`
//!   queries, and sends `NOTIFY ssdp:byebye` cleanly on shutdown.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub const SSDP_MULTICAST_IP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
pub const SSDP_PORT: u16 = 1900;
pub const SSDP_MAX_AGE_SECS: u32 = 1800;
const SSDP_NOTIFY_INTERVAL: Duration = Duration::from_secs(300);
const SSDP_SERVER_HEADER: &str = "Windows/10 UPnP/1.0 AudiophilePlayer/0.1";

/// All UPnP MediaRenderer:1 notification types announced over SSDP.
pub fn advertised_targets(udn: &str) -> [String; 6] {
    [
        "upnp:rootdevice".to_string(),
        udn.to_string(),
        "urn:schemas-upnp-org:device:MediaRenderer:1".to_string(),
        "urn:schemas-upnp-org:service:AVTransport:1".to_string(),
        "urn:schemas-upnp-org:service:ConnectionManager:1".to_string(),
        "urn:schemas-upnp-org:service:RenderingControl:1".to_string(),
    ]
}

/// Format the SSDP `USN` header corresponding to `target` and `udn`.
pub fn format_usn(udn: &str, target: &str) -> String {
    if target == udn {
        udn.to_string()
    } else {
        format!("{udn}::{target}")
    }
}

/// Build an SSDP `NOTIFY` packet (`ssdp:alive` or `ssdp:byebye`).
pub fn build_ssdp_notify_packet(
    location_url: &str,
    udn: &str,
    nt: &str,
    alive: bool,
) -> String {
    let usn = format_usn(udn, nt);
    if alive {
        format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             CACHE-CONTROL: max-age={SSDP_MAX_AGE_SECS}\r\n\
             LOCATION: {location_url}\r\n\
             NT: {nt}\r\n\
             NTS: ssdp:alive\r\n\
             SERVER: {SSDP_SERVER_HEADER}\r\n\
             USN: {usn}\r\n\r\n"
        )
    } else {
        format!(
            "NOTIFY * HTTP/1.1\r\n\
             HOST: 239.255.255.250:1900\r\n\
             NT: {nt}\r\n\
             NTS: ssdp:byebye\r\n\
             USN: {usn}\r\n\r\n"
        )
    }
}

/// Build an SSDP unicast `HTTP/1.1 200 OK` response to an `M-SEARCH` request.
pub fn build_msearch_response_packet(location_url: &str, udn: &str, st: &str) -> String {
    let usn = format_usn(udn, st);
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age={SSDP_MAX_AGE_SECS}\r\n\
         EXT:\r\n\
         LOCATION: {location_url}\r\n\
         SERVER: {SSDP_SERVER_HEADER}\r\n\
         ST: {st}\r\n\
         USN: {usn}\r\n\r\n"
    )
}

/// Parse an incoming SSDP `M-SEARCH` datagram and return the matching `ST` values to answer.
pub fn match_msearch_request(packet: &str, udn: &str) -> Vec<String> {
    let mut lines = packet.split("\r\n");
    let Some(first) = lines.next() else {
        return Vec::new();
    };
    if !first.trim().to_ascii_uppercase().starts_with("M-SEARCH ") {
        return Vec::new();
    }
    let mut man_ok = false;
    let mut st: Option<String> = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim();
        if key.eq_ignore_ascii_case("MAN") {
            if val.trim_matches('"').eq_ignore_ascii_case("ssdp:discover") {
                man_ok = true;
            }
        } else if key.eq_ignore_ascii_case("ST") {
            st = Some(val.to_string());
        }
    }
    if !man_ok {
        return Vec::new();
    }
    let Some(st_val) = st else {
        return Vec::new();
    };
    let all = advertised_targets(udn);
    if st_val.eq_ignore_ascii_case("ssdp:all") {
        return all.to_vec();
    }
    all.into_iter()
        .filter(|t| t.eq_ignore_ascii_case(&st_val))
        .collect()
}

/// Detect the primary local LAN IPv4 address via zero-packet UDP route lookup (falling back to
/// `127.0.0.1` on air-gapped / loopback-only hosts).
pub fn detect_local_ipv4() -> Ipv4Addr {
    for target in ["192.0.2.1:80", "8.8.8.8:80", "10.255.255.1:80"] {
        if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
            if sock.connect(target).is_ok() {
                if let Ok(SocketAddr::V4(local)) = sock.local_addr() {
                    let ip = *local.ip();
                    if !ip.is_unspecified() && !ip.is_loopback() {
                        return ip;
                    }
                }
            }
        }
    }
    Ipv4Addr::LOCALHOST
}

/// Bind a UDP socket to `0.0.0.0:1900` with `SO_REUSEADDR = 1` and join `239.255.255.250`.
#[cfg(windows)]
fn bind_ssdp_listener(local_ip: Ipv4Addr) -> Result<UdpSocket, String> {
    use std::os::windows::io::{FromRawSocket, RawSocket};
    use windows_sys::Win32::Networking::WinSock::{
        AF_INET, IN_ADDR, IN_ADDR_0, INVALID_SOCKET, IPPROTO_UDP, SO_REUSEADDR, SOCK_DGRAM,
        SOCKADDR_IN, SOL_SOCKET, WSAGetLastError, bind, closesocket, setsockopt, socket,
    };

    // Ensure Winsock is initialized via std before calling raw WinSock APIs.
    let _ws_init = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0));

    // SAFETY: Standard WinSock2 IPv4 UDP socket creation with SO_REUSEADDR before bind;
    // ownership is transferred immediately to `UdpSocket::from_raw_socket` on success or closed on error.
    let raw = unsafe { socket(AF_INET as i32, SOCK_DGRAM, IPPROTO_UDP) };
    if raw == INVALID_SOCKET {
        let code = unsafe { WSAGetLastError() };
        return Err(format!("WinSock socket() failed (WSA error {code})"));
    }

    let reuse: i32 = 1;
    let rc_opt = unsafe {
        setsockopt(
            raw,
            SOL_SOCKET,
            SO_REUSEADDR,
            (&reuse as *const i32).cast::<u8>(),
            std::mem::size_of::<i32>() as i32,
        )
    };
    if rc_opt != 0 {
        let code = unsafe { WSAGetLastError() };
        unsafe { closesocket(raw) };
        return Err(format!("setsockopt(SO_REUSEADDR) failed (WSA error {code})"));
    }

    let addr = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_port: SSDP_PORT.to_be(),
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 { S_addr: 0 },
        },
        sin_zero: [0; 8],
    };
    let rc_bind = unsafe {
        bind(
            raw,
            (&addr as *const SOCKADDR_IN).cast(),
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        )
    };
    if rc_bind != 0 {
        let code = unsafe { WSAGetLastError() };
        unsafe { closesocket(raw) };
        return Err(format!("bind(0.0.0.0:1900) failed (WSA error {code})"));
    }

    let sock = unsafe { UdpSocket::from_raw_socket(raw as RawSocket) };
    let _ = sock.set_multicast_loop_v4(true);
    let _ = sock.set_multicast_ttl_v4(4);
    let _ = sock.join_multicast_v4(&SSDP_MULTICAST_IP, &local_ip);
    if local_ip != Ipv4Addr::UNSPECIFIED {
        let _ = sock.join_multicast_v4(&SSDP_MULTICAST_IP, &Ipv4Addr::UNSPECIFIED);
    }
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    Ok(sock)
}

#[cfg(not(windows))]
fn bind_ssdp_listener(local_ip: Ipv4Addr) -> Result<UdpSocket, String> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, SSDP_PORT))
        .map_err(|e| format!("failed binding SSDP port 1900: {e}"))?;
    let _ = sock.set_multicast_loop_v4(true);
    let _ = sock.set_multicast_ttl_v4(4);
    let _ = sock.join_multicast_v4(&SSDP_MULTICAST_IP, &local_ip);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    Ok(sock)
}

/// Running SSDP background responder/announcer thread handle.
pub struct SsdpService {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SsdpService {
    pub fn start(local_ip: Ipv4Addr, location_url: String, udn: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("ap-upnp-ssdp".to_string())
            .spawn(move || ssdp_loop(local_ip, location_url, udn, stop_bg))
            .ok();
        Self { stop, handle }
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SsdpService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn ssdp_loop(local_ip: Ipv4Addr, location_url: String, udn: String, stop: Arc<AtomicBool>) {
    let mcast_dest = SocketAddr::V4(SocketAddrV4::new(SSDP_MULTICAST_IP, SSDP_PORT));

    let send_sock = UdpSocket::bind((local_ip, 0))
        .or_else(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)))
        .ok();
    if let Some(ref s) = send_sock {
        let _ = s.set_multicast_loop_v4(true);
        let _ = s.set_multicast_ttl_v4(4);
    }

    let recv_sock = match bind_ssdp_listener(local_ip) {
        Ok(s) => Some(s),
        Err(err) => {
            eprintln!("  [upnp/ssdp] listener warning ({err}); continuing with periodic NOTIFY announcements");
            None
        }
    };

    // Announce ssdp:alive twice at startup (UDP best practice) so foobar2000 sees us immediately.
    if let Some(ref s) = send_sock {
        send_all_notifies(s, mcast_dest, &location_url, &udn, true);
        thread::sleep(Duration::from_millis(120));
        if !stop.load(Ordering::Acquire) {
            send_all_notifies(s, mcast_dest, &location_url, &udn, true);
        }
    }

    let mut last_notify = Instant::now();
    let mut buf = [0u8; 4096];

    while !stop.load(Ordering::Acquire) {
        if last_notify.elapsed() >= SSDP_NOTIFY_INTERVAL {
            if let Some(ref s) = send_sock {
                send_all_notifies(s, mcast_dest, &location_url, &udn, true);
            }
            last_notify = Instant::now();
        }

        if let Some(ref rs) = recv_sock {
            match rs.recv_from(&mut buf) {
                Ok((n, src)) => {
                    if let Ok(text) = std::str::from_utf8(&buf[..n]) {
                        let matched = match_msearch_request(text, &udn);
                        if !matched.is_empty() {
                            // If a local control point queried from loopback, ensure LOCATION is
                            // reachable; our HTTP server binds 0.0.0.0 so both local_ip and 127.0.0.1 work.
                            let _is_loopback = matches!(src.ip(), IpAddr::V4(v4) if v4.is_loopback());
                            let reply_sock = send_sock.as_ref().unwrap_or(rs);
                            for st in matched {
                                let resp =
                                    build_msearch_response_packet(&location_url, &udn, &st);
                                let _ = reply_sock.send_to(resp.as_bytes(), src);
                            }
                        }
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(_) => thread::sleep(Duration::from_millis(50)),
            }
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }

    // Graceful shutdown: broadcast ssdp:byebye so foobar2000 removes the device immediately.
    if let Some(ref s) = send_sock {
        send_all_notifies(s, mcast_dest, &location_url, &udn, false);
    }
}

fn send_all_notifies(
    sock: &UdpSocket,
    dest: SocketAddr,
    location_url: &str,
    udn: &str,
    alive: bool,
) {
    for nt in advertised_targets(udn) {
        let pkt = build_ssdp_notify_packet(location_url, udn, &nt, alive);
        let _ = sock.send_to(pkt.as_bytes(), dest);
    }
}
