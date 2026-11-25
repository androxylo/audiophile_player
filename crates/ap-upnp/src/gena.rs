//! UPnP GENA eventing (`SUBSCRIBE`, renewal, `UNSUBSCRIBE`, and background `NOTIFY` delivery).
//!
//! Security & reliability hardening (Review #5 Findings #25 & #33):
//! - Caps active subscriptions ([`MAX_SUBSCRIPTIONS`]); evicts failing subscriptions first and
//!   rejects new ones with capacity error rather than evicting healthy subscribers.
//! - Strictly restricts callback URLs to `http://` targeting loopback, RFC 1918 private LAN,
//!   or link-local addresses (blocks WAN SSRF / reflection).
//! - Separates registration from queueing the initial `SEQ: 0` event so `server.rs` writes the
//!   `200 OK` (with `SID`) BEFORE `SEQ: 0` is dispatched (UDA ordering, Finding #33).
//! - Delivers `NOTIFY` via a bounded 4-worker pool with tight 1 s timeouts and drops dead
//!   subscribers automatically after 3 consecutive delivery failures (Finding #25).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ap_source::http_stream::ParsedHttpUrl;

const MAX_SUBSCRIPTIONS: usize = 16;
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const NOTIFY_WORKER_THREADS: usize = 4;
const DEFAULT_SUBSCRIPTION_TIMEOUT_SECS: u64 = 300;
const MIN_SUBSCRIPTION_TIMEOUT_SECS: u64 = 60;
const MAX_SUBSCRIPTION_TIMEOUT_SECS: u64 = 1800;
const NOTIFY_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const NOTIFY_IO_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenaService {
    AvTransport,
    ConnectionManager,
    RenderingControl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscribeError {
    InvalidRequest(String),
    CapacityExceeded,
}

#[derive(Debug, Clone)]
struct Subscription {
    sid: String,
    service: GenaService,
    callback: ParsedHttpUrl,
    expires_at: Instant,
    seq: u32,
    consecutive_failures: u32,
}

/// Pending initial or state-change GENA `NOTIFY` event.
#[derive(Debug, Clone)]
pub struct NotifyJob {
    callback: ParsedHttpUrl,
    sid: String,
    seq: u32,
    xml_body: String,
}

/// Thread-safe GENA subscription registry and asynchronous `NOTIFY` dispatcher.
pub struct GenaEventHub {
    subs: Arc<Mutex<HashMap<String, Subscription>>>,
    next_id: AtomicU64,
    tx: Mutex<Option<mpsc::SyncSender<NotifyJob>>>,
    stop: Arc<AtomicBool>,
    workers: Mutex<Vec<JoinHandle<()>>>,
}

impl GenaEventHub {
    pub fn start() -> Arc<Self> {
        let (tx, rx) = mpsc::sync_channel::<NotifyJob>(64);
        let shared_rx = Arc::new(Mutex::new(rx));
        let subs = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::with_capacity(NOTIFY_WORKER_THREADS);
        for idx in 0..NOTIFY_WORKER_THREADS {
            let rx_w = Arc::clone(&shared_rx);
            let subs_w = Arc::clone(&subs);
            let stop_w = Arc::clone(&stop);
            if let Ok(h) = thread::Builder::new()
                .name(format!("ap-upnp-gena-{idx}"))
                .spawn(move || notify_worker_loop(rx_w, subs_w, stop_w))
            {
                workers.push(h);
            }
        }

        Arc::new(Self {
            subs,
            next_id: AtomicU64::new(1),
            tx: Mutex::new(Some(tx)),
            stop,
            workers: Mutex::new(workers),
        })
    }

    /// Create an inactive hub for unit tests that do not need background threads.
    pub fn new_for_test() -> Arc<Self> {
        Arc::new(Self {
            subs: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
            tx: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
            workers: Mutex::new(Vec::new()),
        })
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut tx) = self.tx.lock() {
            *tx = None;
        }
        if let Ok(mut list) = self.workers.lock() {
            for h in list.drain(..) {
                let _ = h.join();
            }
        }
    }

    /// Register a new GENA subscription and return `(sid, timeout_secs, initial_job)` WITHOUT
    /// enqueueing `initial_job` yet. Call [`Self::enqueue_initial`] strictly AFTER writing the
    /// HTTP `200 OK` response containing `SID` (UDA conformance, Review #5 Finding #33).
    pub fn subscribe(
        &self,
        service: GenaService,
        callback_header: &str,
        timeout_header: Option<&str>,
        initial_xml: String,
    ) -> Result<(String, u64, NotifyJob), SubscribeError> {
        let callback_url =
            parse_first_callback_url(callback_header).map_err(SubscribeError::InvalidRequest)?;
        validate_lan_callback(&callback_url).map_err(SubscribeError::InvalidRequest)?;
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
            seq: 1, // SEQ 0 is returned as `initial_job` for post-200 dispatch.
            consecutive_failures: 0,
        };

        {
            let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            map.retain(|_, s| s.expires_at > now && s.consecutive_failures < MAX_CONSECUTIVE_FAILURES);
            if map.len() >= MAX_SUBSCRIPTIONS {
                // Evict a failing subscriber first; never evict a healthy subscriber at cap!
                let failing_key = map
                    .iter()
                    .filter(|(_, s)| s.consecutive_failures > 0)
                    .max_by_key(|(_, s)| s.consecutive_failures)
                    .map(|(k, _)| k.clone());
                if let Some(k) = failing_key {
                    map.remove(&k);
                } else {
                    return Err(SubscribeError::CapacityExceeded);
                }
            }
            map.insert(sid.clone(), sub);
        }

        let initial_job = NotifyJob {
            callback: callback_url,
            sid: sid.clone(),
            seq: 0,
            xml_body: initial_xml,
        };
        Ok((sid, timeout_secs, initial_job))
    }

    /// Enqueue the initial `SEQ: 0` event after the `SUBSCRIBE` `200 OK` response has been sent.
    pub fn enqueue_initial(&self, job: NotifyJob) {
        self.enqueue(job);
    }

    /// Renew an existing subscription by `SID`.
    pub fn renew(&self, sid: &str, timeout_header: Option<&str>) -> Result<u64, String> {
        let timeout_secs = parse_timeout_secs(timeout_header);
        let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(sub) = map.get_mut(sid.trim()) else {
            return Err("unknown subscription SID".to_string());
        };
        sub.expires_at = Instant::now() + Duration::from_secs(timeout_secs);
        sub.consecutive_failures = 0;
        Ok(timeout_secs)
    }

    /// Remove a subscription by `SID`.
    pub fn unsubscribe(&self, sid: &str) -> bool {
        let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(sid.trim()).is_some()
    }

    /// Active subscriber count (diagnostic / test helper).
    pub fn active_subscription_count(&self) -> usize {
        let map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        map.len()
    }

    /// Push a `propertyset` XML event to all active subscribers of `service`.
    pub fn notify_service(&self, service: GenaService, xml_body: String) {
        let jobs: Vec<NotifyJob> = {
            let mut map = self.subs.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            map.retain(|_, s| s.expires_at > now && s.consecutive_failures < MAX_CONSECUTIVE_FAILURES);
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

/// Check whether `ip` belongs to loopback, RFC 1918 private LAN, or link-local ranges.
pub fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
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

fn notify_worker_loop(
    rx: Arc<Mutex<mpsc::Receiver<NotifyJob>>>,
    subs: Arc<Mutex<HashMap<String, Subscription>>>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        let maybe_job = {
            let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
            guard.recv_timeout(Duration::from_millis(100))
        };
        let job = match maybe_job {
            Ok(j) => j,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        let ok = send_gena_notify(&job);
        let mut map = subs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sub) = map.get_mut(&job.sid) {
            if ok {
                sub.consecutive_failures = 0;
            } else {
                sub.consecutive_failures = sub.consecutive_failures.saturating_add(1);
                if sub.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    map.remove(&job.sid);
                }
            }
        }
    }
}

fn send_gena_notify(job: &NotifyJob) -> bool {
    let Ok(addrs) = (job.callback.host.as_str(), job.callback.port).to_socket_addrs() else {
        return false;
    };
    let Some(addr) = addrs.into_iter().find(|sa| is_private_or_loopback(sa.ip())) else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, NOTIFY_CONNECT_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(NOTIFY_IO_TIMEOUT));
    let _ = stream.set_read_timeout(Some(NOTIFY_IO_TIMEOUT));
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
    if stream.write_all(&full).is_err() {
        return false;
    }
    let _ = stream.flush();
    // Read brief HTTP status line to detect 412/connection failure cleanly.
    let mut resp = [0u8; 64];
    match stream.read(&mut resp) {
        Ok(0) => true,
        Ok(n) => {
            let head = String::from_utf8_lossy(&resp[..n]);
            !head.contains("412") && !head.contains("404")
        }
        Err(_) => true, // Some control points close without reply; TCP write succeeded.
    }
}
