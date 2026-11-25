//! Pure-Rust bounded-memory HTTP/1.1 streaming lossless audio source (`AudioBlockSource`).
//!
//! Designed for UPnP AV MediaRenderer playback (foobar2000 ≥ 2.25 built-in UPnP output,
//! BubbleUPnP, LAN media servers) with zero external HTTP/container dependencies:
//! - HTTP/1.1 streaming client supporting `Content-Length`, `Transfer-Encoding: chunked`,
//!   EOF-delimited streams, HTTP redirects (up to 5), and cooperative cancellation.
//! - Auto-detects container format from stream magic bytes (with `Content-Type` /
//!   `protocolInfo` fallback) and decodes directly into pure IEEE 754 `f64` [`StereoF64Buffer`]
//!   chunks:
//!   1. **Native FLAC** (`fLaC`, 1..=32-bit mono/stereo, 44.1..384 kHz)
//!   2. **Ogg FLAC** (`OggS`, Xiph Ogg FLAC mapping v1.0, including chained Ogg bitstreams)
//!   3. **Streaming RIFF WAV** (`RIFF....WAVE`, PCM 8/16/24/32-bit LE, IEEE float 32/64-bit,
//!      `WAVE_FORMAT_EXTENSIBLE`, and streaming sentinel sizes `0xFFFFFFFF` / `0x7FFFFFFF` / `0`)
//!   4. **Raw LPCM** (`audio/L16` and `audio/L24`, big-endian signed PCM per RFC 2586 / DLNA)

use std::collections::VecDeque;
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ap_core::buffer::StereoF64Buffer;
use ap_core::format::StreamSpec;
use ap_core::headroom::TrackPeakMetadata;
use flac_codec::decode::{FlacSampleReader, Metadata};
use flac_codec::metadata::VorbisComment;

use crate::flac::{
    AudioBlockSource, parse_peak_metadata_fields, validate_flac_stream_params,
};

/// Maximum URL length accepted by the HTTP streaming client.
pub const MAX_STREAM_URL_BYTES: usize = 4096;
/// Maximum HTTP response header block size (16 KiB).
const MAX_HTTP_HEADER_BYTES: usize = 16 * 1024;
/// Maximum HTTP redirects followed.
const MAX_HTTP_REDIRECTS: usize = 5;
/// TCP connect timeout per candidate address.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Per-read socket poll slice so cooperative cancellation aborts stalled streams promptly.
const HTTP_READ_POLL_SLICE: Duration = Duration::from_millis(250);
/// Maximum continuous stall waiting for data from the HTTP server before erroring.
const HTTP_IDLE_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Exact power-of-two normalization factor (`1 / 2^31`) matching `FlacBlockReader`.
const INV_SCALE_I32: f64 = 1.0 / ((1_u64 << 31) as f64);

/// Optional metadata hints supplied by UPnP DIDL-Lite / `protocolInfo` for raw LPCM streams
/// that do not carry an in-band container header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpStreamHints {
    pub protocol_info: Option<String>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u16>,
    pub bits_per_sample: Option<u16>,
}

/// Detected container / wire format for an HTTP audio stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamContainerFormat {
    Flac,
    OggFlac,
    Wav,
    Lpcm16Be,
    Lpcm24Be,
}

impl std::fmt::Display for StreamContainerFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flac => write!(f, "FLAC"),
            Self::OggFlac => write!(f, "Ogg-FLAC"),
            Self::Wav => write!(f, "WAV"),
            Self::Lpcm16Be => write!(f, "L16-BE"),
            Self::Lpcm24Be => write!(f, "L24-BE"),
        }
    }
}

/// Parsed `http://` URL (Phase 1 supports plain HTTP LAN/localhost streams).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHttpUrl {
    pub host: String,
    pub port: u16,
    pub path_and_query: String,
    pub host_header: String,
}

impl ParsedHttpUrl {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("empty HTTP stream URL".to_string());
        }
        if trimmed.len() > MAX_STREAM_URL_BYTES {
            return Err(format!(
                "HTTP stream URL exceeds maximum length ({MAX_STREAM_URL_BYTES} bytes)"
            ));
        }
        if trimmed
            .bytes()
            .any(|b| b < 0x20 || b == 0x7F)
        {
            return Err("HTTP stream URL contains control characters".to_string());
        }

        let rest = if let Some(r) = strip_prefix_ignore_ascii_case(trimmed, "http://") {
            r
        } else if strip_prefix_ignore_ascii_case(trimmed, "https://").is_some() {
            return Err(
                "HTTPS stream URLs are not yet enabled in Phase 1 (scheduled for Phase 2 Tidal/TLS)"
                    .to_string(),
            );
        } else {
            return Err(format!(
                "unsupported stream URL scheme (only http:// is allowed): '{trimmed}'"
            ));
        };

        // Strip any URL fragment `#...`.
        let rest = rest.split('#').next().unwrap_or(rest);
        let (authority, path_and_query) = match rest.find(['/', '?']) {
            Some(idx) => {
                let auth = &rest[..idx];
                let tail = &rest[idx..];
                let pq = if tail.starts_with('/') {
                    tail.to_string()
                } else {
                    format!("/{tail}")
                };
                (auth, pq)
            }
            None => (rest, "/".to_string()),
        };

        if authority.is_empty() {
            return Err("HTTP URL missing host".to_string());
        }
        if authority.contains('@') {
            return Err("HTTP URL userinfo credentials are not allowed".to_string());
        }

        let (host, port, host_header) = if let Some(after_bracket) = authority.strip_prefix('[') {
            let close = after_bracket
                .find(']')
                .ok_or_else(|| "malformed IPv6 host in HTTP URL".to_string())?;
            let ip6 = &after_bracket[..close];
            let rem = &after_bracket[close + 1..];
            let port = if rem.is_empty() {
                80
            } else if let Some(p_str) = rem.strip_prefix(':') {
                parse_port(p_str)?
            } else {
                return Err("malformed port after IPv6 host in HTTP URL".to_string());
            };
            let hdr = if port == 80 {
                format!("[{ip6}]")
            } else {
                format!("[{ip6}]:{port}")
            };
            (ip6.to_string(), port, hdr)
        } else if let Some((h, p_str)) = authority.rsplit_once(':') {
            if h.is_empty() {
                return Err("HTTP URL missing host before port".to_string());
            }
            let port = parse_port(p_str)?;
            let hdr = if port == 80 {
                h.to_string()
            } else {
                format!("{h}:{port}")
            };
            (h.to_string(), port, hdr)
        } else {
            (authority.to_string(), 80, authority.to_string())
        };

        if host.is_empty() || host.contains(char::is_whitespace) {
            return Err("HTTP URL contains invalid host".to_string());
        }

        Ok(Self {
            host,
            port,
            path_and_query,
            host_header,
        })
    }

    /// Resolve a `Location` redirect header relative to this URL.
    pub fn resolve_redirect(&self, location: &str) -> Result<Self, String> {
        let loc = location.trim();
        if loc.is_empty() {
            return Err("empty HTTP redirect Location".to_string());
        }
        if strip_prefix_ignore_ascii_case(loc, "http://").is_some()
            || strip_prefix_ignore_ascii_case(loc, "https://").is_some()
        {
            return Self::parse(loc);
        }
        if loc.starts_with("//") {
            return Self::parse(&format!("http:{loc}"));
        }
        if loc.starts_with('/') {
            let full = format!("http://{}{}", self.host_header, loc);
            return Self::parse(&full);
        }
        let base_dir = match self.path_and_query.rfind('/') {
            Some(pos) => &self.path_and_query[..=pos],
            None => "/",
        };
        let full = format!("http://{}{}{}", self.host_header, base_dir, loc);
        Self::parse(&full)
    }
}

fn parse_port(s: &str) -> Result<u16, String> {
    let p = s
        .parse::<u16>()
        .map_err(|_| format!("invalid port '{s}' in HTTP URL"))?;
    if p == 0 {
        return Err("port 0 is invalid in HTTP URL".to_string());
    }
    Ok(p)
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = s.as_bytes();
    let p = prefix.as_bytes();
    if bytes.len() >= p.len() && bytes[..p.len()].eq_ignore_ascii_case(p) {
        Some(&s[p.len()..])
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Cooperative-cancelling HTTP/1.1 streaming reader
// ---------------------------------------------------------------------------

enum BodyFraming {
    ContentLength(u64),
    Chunked {
        remaining_in_chunk: usize,
        need_chunk_crlf: bool,
        finished: bool,
    },
    UntilEof,
}

/// Streaming HTTP/1.1 response body reader implementing `std::io::Read`.
pub struct HttpBodyReader {
    resolved_url: ParsedHttpUrl,
    stream: TcpStream,
    buffered: VecDeque<u8>,
    framing: BodyFraming,
    cancel: Arc<AtomicBool>,
    content_type: Option<String>,
    content_length: Option<u64>,
    accept_ranges: bool,
    status_code: u16,
    bytes_consumed: u64,
    resumed_once: bool,
}

impl HttpBodyReader {
    /// Open an HTTP/1.1 GET stream to `url`, following up to 5 redirects.
    pub fn connect(url: &str, cancel: Arc<AtomicBool>) -> Result<Self, String> {
        Self::connect_with_range(url, None, cancel)
    }

    /// Open an HTTP/1.1 GET stream to `url`, optionally requesting `Range: bytes=<start>-`.
    pub fn connect_with_range(
        url: &str,
        start_byte: Option<u64>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let mut current = ParsedHttpUrl::parse(url)?;
        for _redirect in 0..=MAX_HTTP_REDIRECTS {
            if cancel.load(Ordering::Acquire) {
                return Err("HTTP stream cancelled".to_string());
            }
            let stream = connect_with_timeout(&current.host, current.port, &cancel)?;
            let mut reader =
                send_get_and_read_headers(stream, &current, start_byte, Arc::clone(&cancel))?;
            match reader.status_code {
                200 | 206 => {
                    return Ok(HttpBodyReader {
                        resolved_url: current,
                        stream: reader.stream,
                        buffered: reader.leftover,
                        framing: reader.framing,
                        cancel,
                        content_type: reader.content_type,
                        content_length: reader.content_length,
                        accept_ranges: reader.accept_ranges,
                        status_code: reader.status_code,
                        bytes_consumed: start_byte.filter(|_| reader.status_code == 206).unwrap_or(0),
                        resumed_once: false,
                    });
                }
                301 | 302 | 303 | 307 | 308 => {
                    let loc = reader.location.take().ok_or_else(|| {
                        format!(
                            "HTTP redirect {} without Location header",
                            reader.status_code
                        )
                    })?;
                    current = current.resolve_redirect(&loc)?;
                    continue;
                }
                code => {
                    return Err(format!("HTTP server returned status {code} for '{url}'"));
                }
            }
        }
        Err(format!("too many HTTP redirects (>{MAX_HTTP_REDIRECTS}) for '{url}'"))
    }

    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    pub fn accept_ranges(&self) -> bool {
        self.accept_ranges
    }

    pub fn is_partial_content(&self) -> bool {
        self.status_code == 206
    }

    fn try_resume_content_length(&mut self, remaining: u64) -> io::Result<bool> {
        if !self.accept_ranges || self.resumed_once || self.bytes_consumed == 0 {
            return Ok(false);
        }
        self.resumed_once = true;
        let stream = match connect_with_timeout(
            &self.resolved_url.host,
            self.resolved_url.port,
            &self.cancel,
        ) {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        let parsed = match send_get_and_read_headers(
            stream,
            &self.resolved_url,
            Some(self.bytes_consumed),
            Arc::clone(&self.cancel),
        ) {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };
        if parsed.status_code != 206 {
            return Ok(false);
        }
        // Review #6 Finding #15: if the server sent a `Content-Range` header, require its
        // byte-range start to match `self.bytes_consumed` exactly so mismatched offsets never
        // splice corrupted audio into the PCM stream.
        if parsed.content_range_seen {
            if parsed.content_range_start != Some(self.bytes_consumed) {
                return Ok(false);
            }
        }
        // Verify resumed response supplies the exact remaining byte count when declared.
        if let Some(cl) = parsed.content_length {
            if cl != remaining {
                return Ok(false);
            }
        }
        self.stream = parsed.stream;
        self.buffered = parsed.leftover;
        Ok(true)
    }

    fn read_raw_socket(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.buffered.is_empty() {
            let n = out.len().min(self.buffered.len());
            for slot in out.iter_mut().take(n) {
                *slot = self.buffered.pop_front().unwrap();
            }
            return Ok(n);
        }
        read_socket_cancellable(&mut self.stream, out, &self.cancel)
    }

    fn read_raw_line(&mut self, max_len: usize) -> io::Result<String> {
        let mut line = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        loop {
            let n = self.read_raw_socket(&mut byte)?;
            if n == 0 {
                if line.is_empty() {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "unexpected EOF reading HTTP chunk header",
                    ));
                }
                break;
            }
            if byte[0] == b'\n' {
                break;
            }
            if byte[0] != b'\r' {
                if line.len() >= max_len {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "HTTP chunk header line too long",
                    ));
                }
                line.push(byte[0]);
            }
        }
        Ok(String::from_utf8_lossy(&line).into_owned())
    }
}

impl Read for HttpBodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                ErrorKind::Interrupted,
                "HTTP stream cancelled",
            ));
        }

        match &mut self.framing {
            BodyFraming::UntilEof => self.read_raw_socket(buf),
            BodyFraming::ContentLength(rem) => {
                let remaining = *rem;
                if remaining == 0 {
                    return Ok(0);
                }
                let want = (buf.len() as u64).min(remaining) as usize;
                let read_res = self.read_raw_socket(&mut buf[..want]);
                let recoverable_drop = match &read_res {
                    Ok(0) => true,
                    Err(e) if !self.cancel.load(Ordering::Acquire) => matches!(
                        e.kind(),
                        ErrorKind::ConnectionReset
                            | ErrorKind::ConnectionAborted
                            | ErrorKind::BrokenPipe
                            | ErrorKind::TimedOut
                            | ErrorKind::UnexpectedEof
                    ),
                    _ => false,
                };
                if recoverable_drop {
                    // Review #5 #9 & Review #6 #15: premature EOF or mid-stream socket drop
                    // (RST/abort/pipe/stall timeout) before declared Content-Length attempts a
                    // single verified Range resume when supported; otherwise fails loudly.
                    if self.try_resume_content_length(remaining)? {
                        let n2 = self.read_raw_socket(&mut buf[..want])?;
                        if n2 == 0 {
                            return Err(io::Error::new(
                                ErrorKind::UnexpectedEof,
                                format!(
                                    "HTTP stream truncated after resume: {remaining} declared bytes remaining"
                                ),
                            ));
                        }
                        if let BodyFraming::ContentLength(r) = &mut self.framing {
                            *r = r.saturating_sub(n2 as u64);
                        }
                        self.bytes_consumed = self.bytes_consumed.saturating_add(n2 as u64);
                        return Ok(n2);
                    }
                    return match read_res {
                        Err(e) => Err(e),
                        Ok(_) => Err(io::Error::new(
                            ErrorKind::UnexpectedEof,
                            format!("HTTP stream truncated: {remaining} declared bytes remaining"),
                        )),
                    };
                }
                let n = read_res?;
                if let BodyFraming::ContentLength(r) = &mut self.framing {
                    *r = r.saturating_sub(n as u64);
                }
                self.bytes_consumed = self.bytes_consumed.saturating_add(n as u64);
                Ok(n)
            }
            BodyFraming::Chunked {
                remaining_in_chunk,
                need_chunk_crlf,
                finished,
            } => {
                if *finished {
                    return Ok(0);
                }
                let mut cur_rem = *remaining_in_chunk;
                let mut cur_crlf = *need_chunk_crlf;

                if cur_rem == 0 {
                    if cur_crlf {
                        // Consume trailing CRLF after the previous chunk payload.
                        let _ = self.read_raw_line(64)?;
                    }
                    let size_line = self.read_raw_line(256)?;
                    let hex_part = size_line.split(';').next().unwrap_or("").trim();
                    let chunk_len = usize::from_str_radix(hex_part, 16).map_err(|_| {
                        io::Error::new(
                            ErrorKind::InvalidData,
                            format!("invalid HTTP chunk size '{hex_part}'"),
                        )
                    })?;
                    if chunk_len == 0 {
                        // Drain optional trailer headers until blank line.
                        for _ in 0..32 {
                            match self.read_raw_line(1024) {
                                Ok(t) if t.is_empty() => break,
                                Ok(_) => continue,
                                Err(_) => break,
                            }
                        }
                        self.framing = BodyFraming::Chunked {
                            remaining_in_chunk: 0,
                            need_chunk_crlf: false,
                            finished: true,
                        };
                        return Ok(0);
                    }
                    cur_rem = chunk_len;
                    cur_crlf = true;
                }

                let want = buf.len().min(cur_rem);
                let n = self.read_raw_socket(&mut buf[..want])?;
                if n == 0 {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "unexpected EOF inside HTTP chunk payload",
                    ));
                }
                cur_rem -= n;
                self.framing = BodyFraming::Chunked {
                    remaining_in_chunk: cur_rem,
                    need_chunk_crlf: cur_crlf,
                    finished: false,
                };
                Ok(n)
            }
        }
    }
}

struct ParsedHttpResponse {
    status_code: u16,
    stream: TcpStream,
    leftover: VecDeque<u8>,
    framing: BodyFraming,
    content_type: Option<String>,
    content_length: Option<u64>,
    content_range_seen: bool,
    content_range_start: Option<u64>,
    accept_ranges: bool,
    location: Option<String>,
}

/// Parse `Content-Range: bytes <start>-<end>/<total>` and return `Some(start)`.
fn parse_content_range_start(val: &str) -> Option<u64> {
    let rest = val
        .trim()
        .strip_prefix("bytes ")
        .or_else(|| val.trim().strip_prefix("BYTES "))
        .or_else(|| {
            let trimmed = val.trim();
            if trimmed.len() > 6 && trimmed[..6].eq_ignore_ascii_case("bytes ") {
                Some(&trimmed[6..])
            } else {
                None
            }
        })?
        .trim_start();
    let (start_str, _) = rest.split_once('-')?;
    start_str.trim().parse::<u64>().ok()
}

fn connect_with_timeout(
    host: &str,
    port: u16,
    cancel: &AtomicBool,
) -> Result<TcpStream, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("failed resolving HTTP stream host '{host}:{port}': {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("no addresses resolved for '{host}:{port}'"));
    }
    let mut last_err = String::new();
    for addr in addrs {
        if cancel.load(Ordering::Acquire) {
            return Err("HTTP stream cancelled".to_string());
        }
        match TcpStream::connect_timeout(&addr, HTTP_CONNECT_TIMEOUT) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(HTTP_READ_POLL_SLICE));
                let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(format!(
        "failed connecting to HTTP stream '{host}:{port}': {last_err}"
    ))
}

fn read_socket_cancellable(
    stream: &mut TcpStream,
    out: &mut [u8],
    cancel: &AtomicBool,
) -> io::Result<usize> {
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                ErrorKind::Interrupted,
                "HTTP stream cancelled",
            ));
        }
        match stream.read(out) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                if started.elapsed() >= HTTP_IDLE_STALL_TIMEOUT {
                    return Err(io::Error::new(
                        ErrorKind::TimedOut,
                        "HTTP stream timed out waiting for audio data",
                    ));
                }
                continue;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn send_get_and_read_headers(
    mut stream: TcpStream,
    url: &ParsedHttpUrl,
    range_start: Option<u64>,
    cancel: Arc<AtomicBool>,
) -> Result<ParsedHttpResponse, String> {
    let range_line = match range_start {
        Some(start) => format!("Range: bytes={start}-\r\n"),
        None => String::new(),
    };
    let req = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: AudiophilePlayer/0.1 UPnP/1.0\r\n\
         Accept: */*\r\n\
         {range_line}\
         Connection: close\r\n\r\n",
        url.path_and_query, url.host_header
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("failed sending HTTP GET: {e}"))?;
    let _ = stream.flush();

    let mut raw = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = read_socket_cancellable(&mut stream, &mut tmp, &cancel)
            .map_err(|e| format!("failed reading HTTP response headers: {e}"))?;
        if n == 0 {
            return Err("HTTP server closed connection before completing headers".to_string());
        }
        raw.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_crlf_crlf(&raw) {
            break pos;
        }
        if raw.len() > MAX_HTTP_HEADER_BYTES {
            return Err("HTTP response headers exceed 16 KiB limit".to_string());
        }
    };

    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header_text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| "empty HTTP response status line".to_string())?;
    let mut parts = status_line.split_whitespace();
    let _http_ver = parts
        .next()
        .ok_or_else(|| format!("malformed HTTP status line '{status_line}'"))?;
    let status_code = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("invalid HTTP status code in '{status_line}'"))?;

    let mut content_type = None;
    let mut content_length = None;
    let mut content_range_seen = false;
    let mut content_range_start = None;
    let mut accept_ranges = false;
    let mut chunked = false;
    let mut location = None;

    for line in lines {
        let Some((name, val)) = line.split_once(':') else {
            continue;
        };
        let key = name.trim();
        let val = val.trim();
        if key.eq_ignore_ascii_case("content-type") {
            content_type = Some(val.to_string());
        } else if key.eq_ignore_ascii_case("content-length") {
            if let Ok(len) = val.parse::<u64>() {
                content_length = Some(len);
            }
        } else if key.eq_ignore_ascii_case("content-range") {
            content_range_seen = true;
            content_range_start = parse_content_range_start(val);
        } else if key.eq_ignore_ascii_case("accept-ranges") {
            if val.to_ascii_lowercase().contains("bytes") {
                accept_ranges = true;
            }
        } else if key.eq_ignore_ascii_case("transfer-encoding") {
            if val.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        } else if key.eq_ignore_ascii_case("location") {
            location = Some(val.to_string());
        }
    }

    let framing = if chunked {
        BodyFraming::Chunked {
            remaining_in_chunk: 0,
            need_chunk_crlf: false,
            finished: false,
        }
    } else if let Some(len) = content_length {
        BodyFraming::ContentLength(len)
    } else {
        BodyFraming::UntilEof
    };

    let leftover: VecDeque<u8> = raw[header_end + 4..].iter().copied().collect();
    Ok(ParsedHttpResponse {
        status_code,
        stream,
        leftover,
        framing,
        content_type,
        content_length,
        content_range_seen,
        content_range_start,
        accept_ranges,
        location,
    })
}

fn find_crlf_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Prefixed reader for non-seekable format sniffing
// ---------------------------------------------------------------------------

struct PrefixedReader<R> {
    prefix: VecDeque<u8>,
    inner: R,
}

impl<R> PrefixedReader<R> {
    fn new(prefix: &[u8], inner: R) -> Self {
        Self {
            prefix: prefix.iter().copied().collect(),
            inner,
        }
    }
}

impl<R: Read> Read for PrefixedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.prefix.is_empty() {
            let n = buf.len().min(self.prefix.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.prefix.pop_front().unwrap();
            }
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

// ---------------------------------------------------------------------------
// Ogg FLAC (Xiph Ogg FLAC mapping v1.0) page demuxer -> native FLAC bytes
// ---------------------------------------------------------------------------

struct SharedReader<R>(Arc<std::sync::Mutex<R>>);

impl<R> Clone for SharedReader<R> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<R: Read> Read for SharedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .read(buf)
    }
}

/// Unwraps one logical Ogg FLAC bitstream into a valid native FLAC byte stream (`fLaC...`).
///
/// Stops cleanly at the Ogg EOS flag (`0x04`) while leaving the underlying shared reader
/// positioned right at the start of any chained Ogg FLAC bitstream on the same HTTP connection.
struct OggFlacLogicalReader<R> {
    inner: R,
    serial: Option<u32>,
    out_buf: VecDeque<u8>,
    packet_buf: Vec<u8>,
    bos_parsed: bool,
    eos_reached: bool,
}

impl<R: Read> OggFlacLogicalReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            serial: None,
            out_buf: VecDeque::new(),
            packet_buf: Vec::new(),
            bos_parsed: false,
            eos_reached: false,
        }
    }

    fn read_next_page(&mut self) -> io::Result<bool> {
        if self.eos_reached {
            return Ok(false);
        }
        let mut hdr = [0u8; 27];
        match read_exact_or_eof(&mut self.inner, &mut hdr)? {
            false => {
                self.eos_reached = true;
                return Ok(false);
            }
            true => {}
        }
        if &hdr[0..4] != b"OggS" {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid Ogg page capture pattern (expected 'OggS')",
            ));
        }
        if hdr[4] != 0 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("unsupported Ogg stream structure version {}", hdr[4]),
            ));
        }
        let header_type = hdr[5];
        let serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
        let num_segments = hdr[26] as usize;

        let mut seg_table = vec![0u8; num_segments];
        self.inner.read_exact(&mut seg_table)?;
        let payload_len: usize = seg_table.iter().map(|&b| usize::from(b)).sum();
        let mut payload = vec![0u8; payload_len];
        self.inner.read_exact(&mut payload)?;

        if let Some(expected) = self.serial {
            if serial != expected {
                // Different multiplexed serial (ignore non-matching stream pages).
                return Ok(true);
            }
        } else {
            self.serial = Some(serial);
        }

        let mut offset = 0usize;
        for &lacing in &seg_table {
            let seg_len = usize::from(lacing);
            self.packet_buf
                .extend_from_slice(&payload[offset..offset + seg_len]);
            offset += seg_len;
            if lacing < 255 {
                self.finish_packet()?;
            }
        }

        if (header_type & 0x04) != 0 {
            if !self.packet_buf.is_empty() {
                self.finish_packet()?;
            }
            self.eos_reached = true;
        }
        Ok(true)
    }

    fn finish_packet(&mut self) -> io::Result<()> {
        if !self.bos_parsed {
            // Xiph Ogg FLAC mapping v1.0 BOS packet:
            // [0]    = 0x7F
            // [1..5] = "FLAC"
            // [5]    = major version (1)
            // [6]    = minor version (0)
            // [7..9] = big-endian u16 number of additional header packets
            // [9..]  = native FLAC signature "fLaC" + STREAMINFO metadata block
            if self.packet_buf.len() < 13
                || self.packet_buf[0] != 0x7F
                || &self.packet_buf[1..5] != b"FLAC"
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "Ogg stream is not Ogg FLAC (missing 0x7F 'FLAC' BOS header)",
                ));
            }
            if &self.packet_buf[9..13] != b"fLaC" {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "Ogg FLAC BOS packet missing native 'fLaC' signature",
                ));
            }
            self.out_buf.extend(&self.packet_buf[9..]);
            self.packet_buf.clear();
            self.bos_parsed = true;
        } else {
            self.out_buf.extend(self.packet_buf.drain(..));
        }
        Ok(())
    }
}

impl<R: Read> Read for OggFlacLogicalReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.out_buf.is_empty() {
            if !self.read_next_page()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.out_buf.len());
        for slot in buf.iter_mut().take(n) {
            *slot = self.out_buf.pop_front().unwrap();
        }
        Ok(n)
    }
}

/// Read exactly `buf.len()` bytes, returning `Ok(false)` ONLY if clean EOF occurred on byte 0.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut read = 0usize;
    while read < buf.len() {
        match r.read(&mut buf[read..])? {
            0 => {
                if read == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "unexpected EOF inside container header",
                ));
            }
            n => read += n,
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Streaming RIFF WAV decoder (PCM int8/16/24/32, IEEE float 32/64, EXTENSIBLE)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WavSampleEncoding {
    PcmU8,
    PcmI16Le,
    PcmI24Le,
    PcmI32Le,
    Float32Le,
    Float64Le,
}

struct StreamingWavDecoder<R> {
    reader: R,
    spec: StreamSpec,
    source_channels: usize,
    encoding: WavSampleEncoding,
    block_align: usize,
    /// `None` for streaming sentinel sizes (`0xFFFFFFFF`, `0x7FFFFFFF`, `0`, `>= 0xFFFF_F000`).
    remaining_data_bytes: Option<u64>,
    total_frames: Option<u64>,
    byte_scratch: Vec<u8>,
    eof: bool,
}

const KSDATAFORMAT_SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
    0x71,
];
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
    0x71,
];

impl<R: Read> StreamingWavDecoder<R> {
    fn open(mut reader: R) -> Result<Self, String> {
        let mut riff = [0u8; 12];
        reader
            .read_exact(&mut riff)
            .map_err(|e| format!("failed reading WAV RIFF header: {e}"))?;
        if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
            return Err("invalid WAV header (missing RIFF/WAVE signature)".to_string());
        }

        let mut parsed_fmt: Option<(StreamSpec, usize, WavSampleEncoding, usize)> = None;

        // Walk RIFF subchunks until "data". Cap unknown chunk walks to prevent DoS.
        for _chunk_idx in 0..128 {
            let mut ch_hdr = [0u8; 8];
            reader
                .read_exact(&mut ch_hdr)
                .map_err(|e| format!("failed reading WAV chunk header: {e}"))?;
            let id = &ch_hdr[0..4];
            let chunk_size = u32::from_le_bytes([ch_hdr[4], ch_hdr[5], ch_hdr[6], ch_hdr[7]]);

            if id == b"fmt " {
                if chunk_size < 16 || chunk_size > 4096 {
                    return Err(format!("invalid WAV fmt chunk size {chunk_size}"));
                }
                let mut fmt = vec![0u8; chunk_size as usize];
                reader
                    .read_exact(&mut fmt)
                    .map_err(|e| format!("failed reading WAV fmt chunk: {e}"))?;
                if chunk_size % 2 == 1 {
                    let mut pad = [0u8; 1];
                    let _ = reader.read_exact(&mut pad);
                }

                let fmt_tag = u16::from_le_bytes([fmt[0], fmt[1]]);
                let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                let sample_rate_hz = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                let block_align = u16::from_le_bytes([fmt[12], fmt[13]]) as usize;
                let bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);

                if channels != 1 && channels != 2 {
                    return Err(format!(
                        "unsupported WAV channel count {channels} (only mono and stereo are supported)"
                    ));
                }
                if sample_rate_hz == 0 {
                    return Err("invalid WAV sample rate 0 Hz".to_string());
                }

                let (is_float, valid_bits) = match fmt_tag {
                    0x0001 => (false, bits_per_sample),
                    0x0003 => (true, bits_per_sample),
                    0xFFFE => {
                        if fmt.len() < 40 {
                            return Err("truncated WAVE_FORMAT_EXTENSIBLE fmt chunk".to_string());
                        }
                        let valid = u16::from_le_bytes([fmt[18], fmt[19]]);
                        let subfmt = &fmt[24..40];
                        let is_flt = if subfmt == KSDATAFORMAT_SUBTYPE_PCM {
                            false
                        } else if subfmt == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                            true
                        } else {
                            return Err(
                                "unsupported WAVE_FORMAT_EXTENSIBLE subformat (only PCM/IEEE_FLOAT supported)"
                                    .to_string(),
                            );
                        };
                        let eff_bits = if valid > 0 && valid <= bits_per_sample {
                            valid
                        } else {
                            bits_per_sample
                        };
                        (is_flt, eff_bits)
                    }
                    other => {
                        return Err(format!("unsupported WAV format tag 0x{other:04X}"));
                    }
                };

                let encoding = match (is_float, bits_per_sample) {
                    (false, 8) => WavSampleEncoding::PcmU8,
                    (false, 16) => WavSampleEncoding::PcmI16Le,
                    (false, 24) => WavSampleEncoding::PcmI24Le,
                    (false, 32) => WavSampleEncoding::PcmI32Le,
                    (true, 32) => WavSampleEncoding::Float32Le,
                    (true, 64) => WavSampleEncoding::Float64Le,
                    _ => {
                        return Err(format!(
                            "unsupported WAV bit depth {bits_per_sample} (float={is_float})"
                        ));
                    }
                };
                let expected_align = usize::from(channels) * usize::from(bits_per_sample / 8);
                if block_align < expected_align || expected_align == 0 {
                    return Err(format!(
                        "invalid WAV block_align {block_align} (expected >= {expected_align})"
                    ));
                }

                let spec = StreamSpec {
                    sample_rate_hz,
                    bits_per_sample: valid_bits,
                    channels: 2,
                };
                parsed_fmt = Some((spec, usize::from(channels), encoding, block_align));
            } else if id == b"data" {
                let (spec, source_channels, encoding, block_align) = parsed_fmt
                    .ok_or_else(|| "WAV 'data' chunk appeared before 'fmt ' chunk".to_string())?;
                // Treat streaming sentinel sizes (foobar2000 live streaming uses 0xFFFFFFFF,
                // 0x7FFFFFFF, 0, or near-u32::MAX) as unbounded until HTTP EOF.
                let is_streaming_sentinel = chunk_size == 0
                    || chunk_size == 0x7FFF_FFFF
                    || chunk_size >= 0xFFFF_F000;
                let remaining_data_bytes = if is_streaming_sentinel {
                    None
                } else {
                    Some(u64::from(chunk_size))
                };
                let total_frames =
                    remaining_data_bytes.map(|b| b / (block_align as u64));
                return Ok(Self {
                    reader,
                    spec,
                    source_channels,
                    encoding,
                    block_align,
                    remaining_data_bytes,
                    total_frames,
                    byte_scratch: Vec::new(),
                    eof: false,
                });
            } else {
                // Skip unknown RIFF subchunk (pad to even length).
                let skip_len = u64::from(chunk_size) + u64::from(chunk_size & 1);
                if skip_len > 16 * 1024 * 1024 {
                    return Err(format!("WAV metadata chunk too large ({skip_len} bytes)"));
                }
                let mut skipped = 0u64;
                let mut discard = [0u8; 4096];
                while skipped < skip_len {
                    let want = ((skip_len - skipped) as usize).min(discard.len());
                    reader
                        .read_exact(&mut discard[..want])
                        .map_err(|e| format!("failed skipping WAV chunk: {e}"))?;
                    skipped += want as u64;
                }
            }
        }

        Err("WAV stream exceeded chunk header limit without reaching 'data'".to_string())
    }

    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
        if self.eof || max_frames == 0 {
            return Ok(None);
        }
        let mut want_bytes = max_frames.saturating_mul(self.block_align);
        if let Some(rem) = self.remaining_data_bytes {
            if rem == 0 {
                self.eof = true;
                return Ok(None);
            }
            want_bytes = (want_bytes as u64).min(rem) as usize;
        }
        // Align request to whole frames.
        want_bytes -= want_bytes % self.block_align;
        if want_bytes == 0 {
            self.eof = true;
            return Ok(None);
        }

        if self.byte_scratch.len() < want_bytes {
            self.byte_scratch.resize(want_bytes, 0);
        }
        let mut filled = 0usize;
        while filled < want_bytes {
            match self.reader.read(&mut self.byte_scratch[filled..want_bytes]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    filled += n;
                    if let Some(rem) = &mut self.remaining_data_bytes {
                        *rem = rem.saturating_sub(n as u64);
                    }
                    // For live streaming, once we have at least 1024 frames ready, we don't need
                    // to force extra TCP waits if the socket delivered a smaller burst? Actually,
                    // waiting up to max_frames (8192 frames ~185ms at 44.1k) gives smooth DSP blocks.
                }
                Err(e) => return Err(format!("WAV HTTP stream read error: {e}")),
            }
        }

        let frames = filled / self.block_align;
        if frames == 0 {
            return Ok(None);
        }

        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        let bytes_per_sample = match self.encoding {
            WavSampleEncoding::PcmU8 => 1,
            WavSampleEncoding::PcmI16Le => 2,
            WavSampleEncoding::PcmI24Le => 3,
            WavSampleEncoding::PcmI32Le | WavSampleEncoding::Float32Le => 4,
            WavSampleEncoding::Float64Le => 8,
        };

        for frame_bytes in self.byte_scratch[..frames * self.block_align]
            .chunks_exact(self.block_align)
        {
            let s0 = decode_wav_sample(&frame_bytes[0..bytes_per_sample], self.encoding);
            if self.source_channels == 1 {
                left.push(s0);
                right.push(s0);
            } else {
                let s1 = decode_wav_sample(
                    &frame_bytes[bytes_per_sample..2 * bytes_per_sample],
                    self.encoding,
                );
                left.push(s0);
                right.push(s1);
            }
        }

        Ok(Some(StereoF64Buffer {
            sample_rate_hz: self.spec.sample_rate_hz,
            left,
            right,
        }))
    }
}

#[inline]
fn decode_wav_sample(b: &[u8], enc: WavSampleEncoding) -> f64 {
    match enc {
        WavSampleEncoding::PcmU8 => {
            let signed = (b[0] as i32) - 128;
            (signed.wrapping_shl(24) as f64) * INV_SCALE_I32
        }
        WavSampleEncoding::PcmI16Le => {
            let s = i16::from_le_bytes([b[0], b[1]]) as i32;
            (s.wrapping_shl(16) as f64) * INV_SCALE_I32
        }
        WavSampleEncoding::PcmI24Le => {
            // Pack 24-bit signed little-endian directly into top 24 bits of i32.
            let s = i32::from_le_bytes([0, b[0], b[1], b[2]]);
            (s as f64) * INV_SCALE_I32
        }
        WavSampleEncoding::PcmI32Le => {
            let s = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            (s as f64) * INV_SCALE_I32
        }
        WavSampleEncoding::Float32Le => {
            let v = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64;
            if v.is_finite() { v } else { 0.0 }
        }
        WavSampleEncoding::Float64Le => {
            let v = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            if v.is_finite() { v } else { 0.0 }
        }
    }
}

// ---------------------------------------------------------------------------
// Raw LPCM big-endian decoder (audio/L16 & audio/L24 per RFC 2586 / DLNA)
// ---------------------------------------------------------------------------

struct RawLpcmBeDecoder<R> {
    reader: R,
    spec: StreamSpec,
    source_channels: usize,
    bytes_per_sample: usize, // 2 for L16, 3 for L24
    total_frames: Option<u64>,
    byte_scratch: Vec<u8>,
    eof: bool,
}

impl<R: Read> RawLpcmBeDecoder<R> {
    fn new(
        reader: R,
        sample_rate_hz: u32,
        source_channels: u16,
        bits_per_sample: u16,
        content_length: Option<u64>,
    ) -> Result<Self, String> {
        if source_channels != 1 && source_channels != 2 {
            return Err(format!(
                "unsupported LPCM channel count {source_channels} (only mono/stereo supported)"
            ));
        }
        if sample_rate_hz == 0 {
            return Err("invalid LPCM sample rate 0 Hz".to_string());
        }
        let bytes_per_sample = match bits_per_sample {
            16 => 2,
            24 => 3,
            other => {
                return Err(format!(
                    "unsupported raw LPCM bit depth {other} (expected 16 or 24)"
                ));
            }
        };
        let frame_bytes = usize::from(source_channels) * bytes_per_sample;
        let total_frames = content_length.map(|len| len / (frame_bytes as u64));
        Ok(Self {
            reader,
            spec: StreamSpec {
                sample_rate_hz,
                bits_per_sample,
                channels: 2,
            },
            source_channels: usize::from(source_channels),
            bytes_per_sample,
            total_frames,
            byte_scratch: Vec::new(),
            eof: false,
        })
    }

    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
        if self.eof || max_frames == 0 {
            return Ok(None);
        }
        let frame_bytes = self.source_channels * self.bytes_per_sample;
        let want_bytes = max_frames.saturating_mul(frame_bytes);
        if self.byte_scratch.len() < want_bytes {
            self.byte_scratch.resize(want_bytes, 0);
        }
        let mut filled = 0usize;
        while filled < want_bytes {
            match self.reader.read(&mut self.byte_scratch[filled..want_bytes]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) => return Err(format!("LPCM HTTP stream read error: {e}")),
            }
        }
        let frames = filled / frame_bytes;
        if frames == 0 {
            return Ok(None);
        }
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        let bps = self.bytes_per_sample;

        for frame in self.byte_scratch[..frames * frame_bytes].chunks_exact(frame_bytes) {
            let s0 = decode_lpcm_be(&frame[0..bps], bps);
            if self.source_channels == 1 {
                left.push(s0);
                right.push(s0);
            } else {
                let s1 = decode_lpcm_be(&frame[bps..2 * bps], bps);
                left.push(s0);
                right.push(s1);
            }
        }

        Ok(Some(StereoF64Buffer {
            sample_rate_hz: self.spec.sample_rate_hz,
            left,
            right,
        }))
    }
}

#[inline]
fn decode_lpcm_be(b: &[u8], bps: usize) -> f64 {
    if bps == 2 {
        let s = i16::from_be_bytes([b[0], b[1]]) as i32;
        (s.wrapping_shl(16) as f64) * INV_SCALE_I32
    } else {
        // 24-bit signed big-endian -> top 24 bits of i32.
        let s = i32::from_be_bytes([b[0], b[1], b[2], 0]);
        (s as f64) * INV_SCALE_I32
    }
}

// ---------------------------------------------------------------------------
// Streaming FLAC / Ogg-FLAC wrapper over generic Read
// ---------------------------------------------------------------------------

struct GenericFlacDecoder<R: Read> {
    reader: FlacSampleReader<BufReader<R>>,
    source_channels: usize,
    spec: StreamSpec,
    peak_meta: TrackPeakMetadata,
    total_frames: Option<u64>,
    shift: u32,
    scratch: Vec<i32>,
    eof: bool,
    pending_error: Option<String>,
}

impl<R: Read> GenericFlacDecoder<R> {
    fn open(inner: R, label: &str) -> Result<Self, String> {
        let reader = FlacSampleReader::new(BufReader::new(inner))
            .map_err(|e| format!("failed to parse streaming FLAC ({label}): {e}"))?;
        let source_channels = reader.channel_count();
        let sample_rate_hz = reader.sample_rate();
        let bits_per_sample = reader.bits_per_sample();
        let spec = validate_flac_stream_params(
            source_channels,
            sample_rate_hz,
            bits_per_sample,
            Path::new(label),
        )?;
        let peak_meta = reader
            .metadata()
            .get::<VorbisComment>()
            .map(|vc| parse_peak_metadata_fields(&vc.fields))
            .unwrap_or_default();
        let total_frames = reader.total_samples();
        let shift = 32 - bits_per_sample;
        Ok(Self {
            reader,
            source_channels: usize::from(source_channels),
            spec,
            peak_meta,
            total_frames,
            shift,
            scratch: Vec::new(),
            eof: false,
            pending_error: None,
        })
    }

    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
        if let Some(err) = self.pending_error.take() {
            self.eof = true;
            return Err(err);
        }
        if self.eof || max_frames == 0 {
            return Ok(None);
        }
        let ch = self.source_channels;
        let target_samples = max_frames.saturating_mul(ch);
        if self.scratch.len() < target_samples {
            self.scratch.resize(target_samples, 0);
        }

        let mut filled = 0usize;
        while filled < target_samples {
            match self.reader.read(&mut self.scratch[filled..target_samples]) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => filled += n,
                Err(e) => {
                    let msg = format!("streaming FLAC decode error: {e}");
                    if filled >= ch {
                        self.pending_error = Some(msg);
                        break;
                    }
                    self.eof = true;
                    return Err(msg);
                }
            }
        }
        let frames = filled / ch;
        if frames == 0 {
            return Ok(None);
        }
        let mut left = Vec::with_capacity(frames);
        let mut right = Vec::with_capacity(frames);
        let shift = self.shift;
        if ch == 1 {
            for &s in &self.scratch[..frames] {
                let v = (s.wrapping_shl(shift) as f64) * INV_SCALE_I32;
                left.push(v);
                right.push(v);
            }
        } else {
            for pair in self.scratch[..frames * 2].chunks_exact(2) {
                left.push((pair[0].wrapping_shl(shift) as f64) * INV_SCALE_I32);
                right.push((pair[1].wrapping_shl(shift) as f64) * INV_SCALE_I32);
            }
        }
        Ok(Some(StereoF64Buffer {
            sample_rate_hz: self.spec.sample_rate_hz,
            left,
            right,
        }))
    }
}

// ---------------------------------------------------------------------------
// Unified HTTP Streaming AudioBlockSource (with chained Ogg FLAC support)
// ---------------------------------------------------------------------------

type SharedPrefixedHttp = SharedReader<PrefixedReader<HttpBodyReader>>;

enum ActiveHttpDecoder {
    Flac(GenericFlacDecoder<PrefixedReader<HttpBodyReader>>),
    OggFlac {
        decoder: Option<GenericFlacDecoder<OggFlacLogicalReader<SharedPrefixedHttp>>>,
        shared_http: SharedPrefixedHttp,
    },
    Wav(StreamingWavDecoder<PrefixedReader<HttpBodyReader>>),
    LpcmBe(RawLpcmBeDecoder<PrefixedReader<HttpBodyReader>>),
}

/// Unified streaming HTTP audio source implementing [`AudioBlockSource`].
///
/// Always reports `is_streaming() == true` so unknown-peak `-3 dB` headroom applies per
/// `HEADROOM_DESIGN.md` & Issue #8 §2.1/§2.2.
pub struct HttpAudioSource {
    format: StreamContainerFormat,
    spec: StreamSpec,
    peak_meta: TrackPeakMetadata,
    total_frames: Option<u64>,
    decoder: ActiveHttpDecoder,
    /// When a chained Ogg FLAC stream switches sample rate or bit depth mid-HTTP-connection,
    /// the next opened segment is staged here so the caller can hand off to a fresh DSP session.
    next_chained_segment: Option<Box<HttpAudioSource>>,
}

impl HttpAudioSource {
    /// Open an `http://` audio stream, sniff its format, and initialize the streaming decoder.
    pub fn open(
        url: &str,
        hints: &HttpStreamHints,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        Self::open_at_offset(url, hints, None, cancel)
    }

    /// Open an `http://` audio stream and position playback at `seek_offset_s` (when `Some(> 0)`).
    ///
    /// Uses HTTP `Range: bytes=<offset>-` when seeking into finite raw LPCM streams on servers
    /// advertising `Accept-Ranges: bytes`, and exact decoded-frame skipping for containerized
    /// FLAC / Ogg-FLAC / WAV streams (guaranteeing zero frame-sync corruption or noise bursts).
    pub fn open_at_offset(
        url: &str,
        hints: &HttpStreamHints,
        seek_offset_s: Option<f64>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let mut body = HttpBodyReader::connect(url, Arc::clone(&cancel))?;
        let content_type = body.content_type().map(str::to_string);
        let content_length = body.content_length();
        let accept_ranges = body.accept_ranges();

        // Peek up to 12 bytes to detect container magic reliably.
        let mut peek = [0u8; 12];
        let mut peeked = read_up_to(&mut body, &mut peek)?;
        if peeked == 0 {
            return Err(format!("HTTP audio stream '{url}' returned 0 bytes"));
        }

        // Review #5 Finding #1: optionally skip a leading ID3v2 tag (some FLAC files carry one)
        // before sniffing for `fLaC`. Never fall through to raw LPCM noise if not FLAC.
        if peeked >= 10
            && &peek[0..3] == b"ID3"
            && peek[3] < 0xFF
            && peek[4] < 0xFF
            && (peek[6] & 0x80) == 0
            && (peek[7] & 0x80) == 0
            && (peek[8] & 0x80) == 0
            && (peek[9] & 0x80) == 0
        {
            let tag_size = ((peek[6] as u64) << 21)
                | ((peek[7] as u64) << 14)
                | ((peek[8] as u64) << 7)
                | (peek[9] as u64);
            let footer_len = if (peek[5] & 0x10) != 0 { 10u64 } else { 0u64 };
            let total_id3 = 10u64.saturating_add(tag_size).saturating_add(footer_len);
            if total_id3 <= 16 * 1024 * 1024 {
                let mut to_discard = total_id3.saturating_sub(peeked as u64);
                let mut discard_buf = [0u8; 4096];
                while to_discard > 0 {
                    let want = (to_discard as usize).min(discard_buf.len());
                    body.read_exact(&mut discard_buf[..want])
                        .map_err(|e| format!("failed skipping ID3v2 header in '{url}': {e}"))?;
                    to_discard -= want as u64;
                }
                peeked = read_up_to(&mut body, &mut peek)?;
                if peeked == 0 {
                    return Err(format!("HTTP stream '{url}' ended immediately after ID3v2 tag"));
                }
            }
        }

        let prefixed = PrefixedReader::new(&peek[..peeked], body);

        let mut source = if peeked >= 4 && &peek[0..4] == b"fLaC" {
            let dec = GenericFlacDecoder::open(prefixed, url)?;
            Self {
                format: StreamContainerFormat::Flac,
                spec: dec.spec,
                peak_meta: dec.peak_meta,
                total_frames: dec.total_frames,
                decoder: ActiveHttpDecoder::Flac(dec),
                next_chained_segment: None,
            }
        } else if peeked >= 4 && &peek[0..4] == b"OggS" {
            let shared_http = SharedReader(Arc::new(std::sync::Mutex::new(prefixed)));
            let ogg = OggFlacLogicalReader::new(shared_http.clone());
            let dec = GenericFlacDecoder::open(ogg, url)?;
            Self {
                format: StreamContainerFormat::OggFlac,
                spec: dec.spec,
                peak_meta: dec.peak_meta,
                total_frames: dec.total_frames,
                decoder: ActiveHttpDecoder::OggFlac {
                    decoder: Some(dec),
                    shared_http,
                },
                next_chained_segment: None,
            }
        } else if peeked >= 12 && &peek[0..4] == b"RIFF" && &peek[8..12] == b"WAVE" {
            let dec = StreamingWavDecoder::open(prefixed)?;
            Self {
                format: StreamContainerFormat::Wav,
                spec: dec.spec,
                peak_meta: TrackPeakMetadata::default(),
                total_frames: dec.total_frames,
                decoder: ActiveHttpDecoder::Wav(dec),
                next_chained_segment: None,
            }
        } else if let Some((bits, rate, ch)) =
            parse_lpcm_params(content_type.as_deref(), hints)
        {
            let frame_bytes = u64::from(ch) * u64::from(bits / 8);
            let target_s = seek_offset_s.unwrap_or(0.0);
            if target_s > 0.0 && accept_ranges && frame_bytes > 0 {
                let target_frames = (target_s * f64::from(rate)).round().max(0.0) as u64;
                let byte_offset = target_frames.saturating_mul(frame_bytes);
                if byte_offset > 0 {
                    if let Ok(ranged_body) =
                        HttpBodyReader::connect_with_range(url, Some(byte_offset), cancel)
                    {
                        if ranged_body.is_partial_content() {
                            let rem_len = ranged_body.content_length();
                            let ranged_prefixed = PrefixedReader::new(&[], ranged_body);
                            let dec =
                                RawLpcmBeDecoder::new(ranged_prefixed, rate, ch, bits, rem_len)?;
                            let format = if bits == 16 {
                                StreamContainerFormat::Lpcm16Be
                            } else {
                                StreamContainerFormat::Lpcm24Be
                            };
                            return Ok(Self {
                                format,
                                spec: dec.spec,
                                peak_meta: TrackPeakMetadata::default(),
                                total_frames: content_length.map(|l| l / frame_bytes),
                                decoder: ActiveHttpDecoder::LpcmBe(dec),
                                next_chained_segment: None,
                            });
                        }
                    }
                }
            }
            let dec = RawLpcmBeDecoder::new(prefixed, rate, ch, bits, content_length)?;
            let format = if bits == 16 {
                StreamContainerFormat::Lpcm16Be
            } else {
                StreamContainerFormat::Lpcm24Be
            };
            Self {
                format,
                spec: dec.spec,
                peak_meta: TrackPeakMetadata::default(),
                total_frames: dec.total_frames,
                decoder: ActiveHttpDecoder::LpcmBe(dec),
                next_chained_segment: None,
            }
        } else {
            let ct_disp = content_type.as_deref().unwrap_or("<none>");
            return Err(format!(
                "unsupported HTTP audio stream format (Content-Type: '{ct_disp}', magic: {:02X?})",
                &peek[..peeked.min(4)]
            ));
        };

        if let Some(target_s) = seek_offset_s {
            if target_s.is_finite() && target_s > 0.0 {
                let mut skip_frames =
                    (target_s * f64::from(source.spec.sample_rate_hz)).round().max(0.0) as u64;
                while skip_frames > 0 {
                    let step = (skip_frames as usize).min(16_384);
                    match source.read_next_block(step)? {
                        Some(b) if !b.is_empty() => {
                            skip_frames = skip_frames.saturating_sub(b.len() as u64);
                        }
                        _ => break,
                    }
                }
            }
        }

        Ok(source)
    }

    pub fn container_format(&self) -> StreamContainerFormat {
        self.format
    }

    /// If a chained Ogg FLAC stream ended one logical bitstream and immediately began another
    /// with a different sample rate or bit depth, take that next segment for fresh DSP handoff.
    pub fn take_next_chained_segment(&mut self) -> Option<HttpAudioSource> {
        self.next_chained_segment.take().map(|b| *b)
    }

    fn try_advance_chained_ogg(&mut self) -> Result<bool, String> {
        let ActiveHttpDecoder::OggFlac {
            decoder: slot,
            shared_http,
        } = &mut self.decoder
        else {
            return Ok(false);
        };
        let Some(old_dec) = slot.take() else {
            return Ok(false);
        };
        drop(old_dec);

        // Peek 4 bytes from the shared HTTP stream to check if another chained "OggS" page follows.
        let mut magic = [0u8; 4];
        {
            let mut guard = shared_http.0.lock().unwrap_or_else(|e| e.into_inner());
            match read_exact_or_eof(&mut *guard, &mut magic) {
                Ok(false) => return Ok(false), // Clean EOF after final Ogg logical bitstream.
                Ok(true) => {}
                Err(_) => return Ok(false),
            }
            if &magic != b"OggS" {
                return Ok(false);
            }
            for &b in magic.iter().rev() {
                guard.prefix.push_front(b);
            }
        }

        let next_ogg = OggFlacLogicalReader::new(shared_http.clone());
        let next_dec = GenericFlacDecoder::open(next_ogg, "chained-ogg-flac")?;
        if next_dec.spec == self.spec {
            // Same format -> continue seamlessly inside the current DSP session!
            *slot = Some(next_dec);
            Ok(true)
        } else {
            // Different sample rate or bit depth -> stage as a new segment for rate-following handoff.
            let seg = HttpAudioSource {
                format: StreamContainerFormat::OggFlac,
                spec: next_dec.spec,
                peak_meta: next_dec.peak_meta,
                total_frames: next_dec.total_frames,
                decoder: ActiveHttpDecoder::OggFlac {
                    decoder: Some(next_dec),
                    shared_http: shared_http.clone(),
                },
                next_chained_segment: None,
            };
            self.next_chained_segment = Some(Box::new(seg));
            Ok(false)
        }
    }
}

impl AudioBlockSource for HttpAudioSource {
    fn spec(&self) -> StreamSpec {
        self.spec
    }

    fn peak_meta(&self) -> TrackPeakMetadata {
        self.peak_meta
    }

    fn is_streaming(&self) -> bool {
        true
    }

    fn total_frames(&self) -> Option<u64> {
        self.total_frames
    }

    fn read_next_block(&mut self, max_frames: usize) -> Result<Option<StereoF64Buffer>, String> {
        loop {
            let res = match &mut self.decoder {
                ActiveHttpDecoder::Flac(d) => d.read_next_block(max_frames),
                ActiveHttpDecoder::OggFlac {
                    decoder: Some(d), ..
                } => d.read_next_block(max_frames),
                ActiveHttpDecoder::OggFlac { decoder: None, .. } => return Ok(None),
                ActiveHttpDecoder::Wav(d) => d.read_next_block(max_frames),
                ActiveHttpDecoder::LpcmBe(d) => d.read_next_block(max_frames),
            }?;
            if res.is_some() {
                return Ok(res);
            }
            // If an Ogg FLAC logical bitstream reached EOS, check for a chained continuation.
            if matches!(
                self.decoder,
                ActiveHttpDecoder::OggFlac {
                    decoder: Some(_),
                    ..
                }
            ) && self.try_advance_chained_ogg()?
            {
                continue;
            }
            return Ok(None);
        }
    }
}

fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize, String> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => return Err(format!("failed peeking HTTP audio stream header: {e}")),
        }
    }
    Ok(filled)
}

/// Extract `(bits_per_sample, sample_rate_hz, channels)` for `audio/L16` or `audio/L24`
/// ONLY when HTTP `Content-Type` or UPnP `protocolInfo` explicitly names `audio/L16` or
/// `audio/L24` (Review #5 Finding #1: never infer raw LPCM from DIDL `bitsPerSample` alone).
pub fn parse_lpcm_params(
    content_type: Option<&str>,
    hints: &HttpStreamHints,
) -> Option<(u16, u32, u16)> {
    let mut bits: Option<u16> = None;
    let mut rate: Option<u32> = None;
    let mut channels: Option<u16> = None;

    let mut inspect = |s: &str| {
        let lower = s.to_ascii_lowercase();
        if bits.is_none() {
            if lower.contains("audio/l16") {
                bits = Some(16);
            } else if lower.contains("audio/l24") {
                bits = Some(24);
            }
        }
        for part in lower.split([';', ':']) {
            let part = part.trim();
            if rate.is_none() {
                if let Some(v) = part.strip_prefix("rate=") {
                    if let Ok(r) = v.trim().parse::<u32>() {
                        if r > 0 {
                            rate = Some(r);
                        }
                    }
                }
            }
            if channels.is_none() {
                if let Some(v) = part.strip_prefix("channels=") {
                    if let Ok(c) = v.trim().parse::<u16>() {
                        if c > 0 {
                            channels = Some(c);
                        }
                    }
                }
            }
        }
    };

    if let Some(ct) = content_type {
        inspect(ct);
    }
    if let Some(pi) = hints.protocol_info.as_deref() {
        inspect(pi);
    }

    // Strict gate: raw LPCM requires explicit `audio/L16` or `audio/L24` MIME declaration.
    let final_bits = bits?;
    let final_rate = rate.or(hints.sample_rate_hz).unwrap_or(44_100);
    let final_ch = channels.or(hints.channels).unwrap_or(2);
    Some((final_bits, final_rate, final_ch))
}
