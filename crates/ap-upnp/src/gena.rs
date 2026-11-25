//! UPnP GENA eventing (`SUBSCRIBE`, renewal, `UNSUBSCRIBE`, and background `NOTIFY` delivery).
//!
//! Security hardening:
//! - Caps active subscriptions ([`MAX_SUBSCRIPTIONS`]).
//! - Strictly restricts callback URLs to `http://` targeting loopback, RFC 1918 private LAN,
//!   or link-local IPv4 addresses (blocks WAN SSRF / reflection).
//! - Delivers `NOTIFY` asynchronously on a dedicated low-priority thread with short connect/write
//!   timeouts so unresponsive control points never stall SOAP replies or audio playback.

use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ap_source::http_stream::ParsedHttpUrl;

const MAX_SUBSCRIPTIONS: usize = 16;
const DEFAULT_SUBSCRIPTION_TIMEOUT_SECS: u64 = 300;
const MIN_SUBSCRIPTION_TIMEOUT_SECS: u64 = 60;
const MAX_SUBSCRIPTION_TIMEOUT_SECS: u64 = 1800;
const NOTIFY_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const NOTIFY_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenaService {
    AvTransport,
    ConnectionManager,
    RenderingControl,
}

#[derive(Debug, Clone)]
struct Subscription {
    sid: String,
    service: GenaService,
    callback: ParsedHttpUrl,
    expires_at: Instant,
    seq: u32,
}

struct NotifyJob {
    callback: ParsedHttpUrl,
    sid: String,
    seq: u32,
    xml_body: String,
}

/// Thread-safe GENA subscription registry and asynchronous `NOTIFY` dispatcher.
pub struct GenaEventHub {
    subs: Mutex<HashMap<String, Subscription>>,
    next_id: AtomicU64,
    tx: Mutex<Option<mpsc::SyncSender<NotifyJob>>>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl GenaEventHub {
    pub fn start() -> Arc<Self> {
        let (tx, rx) = mpsc::sync_channel::<NotifyJob>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("ap-upnp-gena".to_string())
            .spawn(move || notify_worker_loop(rx, stop_bg))
            .ok();
        Arc::new(Self {
            subs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            tx: Mutex::new(Some(tx)),
            stop,
            worker: Mutex::new(worker),
        })
    }

    /// Create an inactive hub for unit tests that do not need a background thread.
    pub fn new_for_test() -> Arc<Self> {
        Arc::new(Self {
            subs: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            tx: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
        })
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        // Drop sender so worker exits promptly.
        if let Ok(mut tx) = self.tx.lock() {
            *tx = None;
        }
        if let Ok(mut w) = self.worker.lock() {
            if let Some(h) = w.take() {
                let _ = h.join();
            }
        }
    }

    /// Register a new GENA subscription and immediately queue the initial `SEQ: 0` event.
    pub fn subscribe(
        &self,
        service: GenaService,
        callback_header: &str,
        timeout_header: Option<&str>,
        initial_xml: String,
    ) -> Result<(String, u64), String> {
        let callback_url = parse_first_callback_url(callback_header)?;
        validate_lan_callback(&callback_url)?;
        let timeout_secs = parse_timeout_secs(timeout_header);

        let sid = format!(
            "uuid:ap-gena-{:016x}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let sub = Subscription {
            sid: sid.clone(),
            service,
            callback: callback_url.clone(),
            expires_at: Instant::now() + Duration::from_secs(timeout_secs),
            seq: 1, // SEQ 0 is queued immediately below.
        };

        {
            let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            map.retain(|_, s| s.expires_at > now);
            if map.len() >= MAX_SUBSCRIPTIONS {
                // Evict soonest-expiring subscription to stay strictly bounded.
                if let Some(oldest_key) = map
                    .iter()
                    .min_by_key(|(_, s)| s.expires_at)
                    .map(|(k, _)| k.clone())
                {
                    map.remove(&oldest_key);
                }
            }
            map.insert(sid.clone(), sub);
        }

        self.enqueue(NotifyJob {
            callback: callback_url,
            sid: sid.clone(),
            seq: 0,
            xml_body: initial_xml,
        });

        Ok((sid, timeout_secs))
    }

    /// Renew an existing subscription by `SID`.
    pub fn renew(&self, sid: &str, timeout_header: Option<&str>) -> Result<u64, String> {
        let timeout_secs = parse_timeout_secs(timeout_header);
        let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sub) = map.get_mut(sid.trim()) else {
            return Err("unknown subscription SID".to_string());
        };
        sub.expires_at = Instant::now() + Duration::from_secs(timeout_secs);
        Ok(timeout_secs)
    }

    /// Remove a subscription by `SID`.
    pub fn unsubscribe(&self, sid: &str) -> bool {
        let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(sid.trim()).is_some()
    }

    /// Push a `propertyset` XML event to all active subscribers of `service`.
    pub fn notify_service(&self, service: GenaService, xml_body: String) {
        let jobs: Vec<NotifyJob> = {
            let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            map.retain(|_, s| s.expires_at > now);
            let mut out = Vec::new();
            for sub in map.values_mut() {
                if sub.service == service {
                    let seq = sub.seq;
                    sub.seq = sub.seq.wrapping_add(1).max(1);
                    out.push(NotifyJob {
                        callback: sub.callback.clone(),
                        sid: sub.sid.clone(),
                        seq,
                        xml_body: xml_body.clone(),
                    });
                }
            }
            out
        };
        for job in jobs {
            self.enqueue(job);
        }
    }

    fn enqueue(&self, job: NotifyJob) {
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                // Best-effort non-blocking enqueue; never stall SOAP or audio threads.
                let _ = tx.try_send(job);
            }
        }
    }
}

/// Extract the first `<http://...>` URL from a GENA `CALLBACK` header.
pub fn parse_first_callback_url(header: &str) -> Result<ParsedHttpUrl, String> {
    let trimmed = header.trim();
    let url_str = if let Some(start) = trimmed.find('<') {
        let after = &trimmed[start + 1..];
        let end = after
            .find('>')
            .ok_or_else(|| "malformed GENA CALLBACK header (missing '>')".to_string())?;
        &after[..end]
    } else {
        trimmed
    };
    ParsedHttpUrl::parse(url_str)
}

/// Reject GENA callbacks that resolve outside loopback / private LAN / link-local ranges.
pub fn validate_lan_callback(url: &ParsedHttpUrl) -> Result<(), String> {
    let addrs: Vec<SocketAddr> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("failed resolving GENA callback '{}': {e}", url.host))?
        .collect();
    if addrs.is_empty() {
        return Err("GENA callback host did not resolve".to_string());
    }
    if addrs.iter().all(|sa| is_private_or_loopback(sa.ip())) {
        Ok(())
    } else {
        Err(format!(
            "GENA callback host '{}' is outside private LAN/loopback range",
            url.host
        ))
    }
}

fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn parse_timeout_secs(header: Option<&str>) -> u64 {
    let Some(raw) = header else {
        return DEFAULT_SUBSCRIPTION_TIMEOUT_SECS;
    };
    let trimmed = raw.trim();
    let num = trimmed
        .strip_prefix("Second-")
        .or_else(|| trimmed.strip_prefix("second-"))
        .unwrap_or(trimmed);
    if num.eq_ignore_ascii_case("infinite") {
        return MAX_SUBSCRIPTION_TIMEOUT_SECS;
    }
    num.parse::<u64>()
        .unwrap_or(DEFAULT_SUBSCRIPTION_TIMEOUT_SECS)
        .clamp(MIN_SUBSCRIPTION_TIMEOUT_SECS, MAX_SUBSCRIPTION_TIMEOUT_SECS)
}

fn notify_worker_loop(rx: mpsc::Receiver<NotifyJob>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        let Ok(job) = rx.recv_timeout(Duration::from_millis(100)) else {
            if stop.load(Ordering::Acquire) {
                break;
            }
            continue;
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        send_gena_notify(&job);
    }
}

fn send_gena_notify(job: &NotifyJob) {
    let Ok(addrs) = (job.callback.host.as_str(), job.callback.port).to_socket_addrs() else {
        return;
    };
    let Some(addr) = addrs.into_iter().find(|sa| is_private_or_loopback(sa.ip())) else {
        return;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, NOTIFY_CONNECT_TIMEOUT) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(NOTIFY_WRITE_TIMEOUT));
    let _ = stream.set_read_timeout(Some(NOTIFY_WRITE_TIMEOUT));
    let body = job.xml_body.as_bytes();
    let mut full = format!(
        "NOTIFY {} HTTP/1.1\r\n\
         HOST: {}\r\n\
         CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
         CONTENT-LENGTH: {}\r\n\
         NT: upnp:event\r\n\
         NTS: upnp:propchange\r\n\
         SID: {}\r\n\
         SEQ: {}\r\n\
         CONNECTION: close\r\n\r\n",
        job.callback.path_and_query,
        job.callback.host_header,
        body.len(),
        job.sid,
        job.seq
    )
    .into_bytes();
    full.extend_from_slice(body);
    let _ = stream.write_all(&full);
    let _ = stream.flush();
}
