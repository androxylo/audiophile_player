// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Minimal zero-dependency HTTP/1.1 framing primitives shared across `ap-control`, `ap-upnp`,
//! `ap-source`, and `ap-cli` (Review #9 O2).
//!
//! Provides one consistent implementation of:
//! - Header/body boundary detection (`find_header_end`) and status-line parsing (`parse_status_code`)
//! - Cooperative deadline-bounded request reading (`read_http_request`) with strict `Content-Length`,
//!   optional `Transfer-Encoding: chunked` decoding, optional `Expect: 100-continue`, duplicate header
//!   preservation (for strict `Host` validation), and truncated-body rejection
//! - Cooperative deadline-bounded socket writes (`write_all_bounded`)

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Locate the byte offset of `\r\n\r\n` separating HTTP headers from body.
#[inline]
pub fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse the 3-digit HTTP status code from the first line of an HTTP/1.x response header block
/// (e.g. `"HTTP/1.1 200 OK\r\n..."` -> `Some(200)`).
pub fn parse_status_code(header_block: &str) -> Option<u16> {
    let first_line = header_block.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let proto = parts.next()?;
    if !proto.starts_with("HTTP/") {
        return None;
    }
    let code_str = parts.next()?;
    if code_str.len() != 3 || !code_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    code_str.parse::<u16>().ok()
}

/// Write `data` to `stream` with cooperative `stop` polling and a hard wall-clock `deadline`.
pub fn write_all_bounded(
    stream: &mut TcpStream,
    mut data: &[u8],
    deadline: Instant,
    stop: Option<&AtomicBool>,
    chunk_bytes: usize,
) -> io::Result<()> {
    let step = chunk_bytes.max(1024);
    while !data.is_empty() {
        if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            return Err(io::Error::new(
                ErrorKind::Interrupted,
                "HTTP write cancelled by shutdown",
            ));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "HTTP write deadline exceeded",
            ));
        };
        let _ = stream.set_write_timeout(Some(remaining.min(Duration::from_millis(500))));
        let n_want = data.len().min(step);
        match stream.write(&data[..n_want]) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "HTTP write returned 0 bytes",
                ));
            }
            Ok(n) => data = &data[n..],
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        ErrorKind::TimedOut,
                        "HTTP write deadline exceeded",
                    ));
                }
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Configurable bounds and framing flags for [`read_http_request`].
#[derive(Debug, Clone, Copy)]
pub struct HttpReadLimits {
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub first_byte_deadline: Duration,
    pub read_deadline: Duration,
    pub allow_chunked: bool,
    pub honor_expect_100: bool,
}

/// Preserved raw HTTP header `(lowercase_name, trimmed_value)`.
#[derive(Debug, Clone)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

/// Parsed HTTP/1.1 request line, headers (preserving duplicates), and decoded body bytes.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<HttpHeader>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// Return the first header value matching ASCII lowercase `name`.
    pub fn header(&self, lower_name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.name == lower_name)
            .map(|h| h.value.as_str())
    }

    /// Return `true` if `lower_name` appeared more than once in the request headers.
    pub fn has_duplicate_header(&self, lower_name: &str) -> bool {
        self.headers.iter().filter(|h| h.name == lower_name).count() > 1
    }

    /// Decode the request body lossily as UTF-8.
    pub fn body_lossy(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Structured HTTP/1.1 request framing failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpReadError {
    ClosedOrMalformed,
    Timeout,
    HeaderTooLarge,
    BodyTooLarge,
    BadContentLength,
    TransferEncodingNotSupported,
    TruncatedBody,
}

impl HttpReadError {
    /// Map to `(HTTP status line, human-readable message)`, or `None` for silent close.
    pub fn http_status(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::ClosedOrMalformed => None,
            Self::Timeout => Some(("408 Request Timeout", "Request Timeout")),
            Self::HeaderTooLarge => Some((
                "431 Request Header Fields Too Large",
                "Request Header Fields Too Large",
            )),
            Self::BodyTooLarge => Some(("413 Payload Too Large", "Payload Too Large")),
            Self::BadContentLength => Some(("400 Bad Request", "Invalid Content-Length")),
            Self::TransferEncodingNotSupported => Some((
                "501 Not Implemented",
                "Transfer-Encoding is not supported; send Content-Length",
            )),
            Self::TruncatedBody => Some(("400 Bad Request", "Truncated request body")),
        }
    }
}

fn read_chunk_with_deadline(
    stream: &mut TcpStream,
    buf: &mut [u8],
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<usize, HttpReadError> {
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(HttpReadError::ClosedOrMalformed);
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(HttpReadError::Timeout);
        };
        let _ = stream.set_read_timeout(Some(remaining.min(Duration::from_millis(200))));
        match stream.read(buf) {
            Ok(n) => return Ok(n),
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if Instant::now() >= deadline {
                    return Err(HttpReadError::Timeout);
                }
                continue;
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(HttpReadError::ClosedOrMalformed),
        }
    }
}

/// Read one bounded HTTP/1.1 request from `stream` enforcing deadlines, header/body caps,
/// `Content-Length` / optional `Transfer-Encoding: chunked`, and truncated-body rejection.
pub fn read_http_request(
    stream: &mut TcpStream,
    limits: &HttpReadLimits,
    stop: &AtomicBool,
) -> Result<HttpRequest, HttpReadError> {
    let now = Instant::now();
    let first_byte_deadline = now + limits.first_byte_deadline;
    let deadline = now + limits.read_deadline;

    let mut raw = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];

    let header_end = loop {
        let active_deadline = if raw.is_empty() {
            first_byte_deadline.min(deadline)
        } else {
            deadline
        };
        let n = read_chunk_with_deadline(stream, &mut chunk, active_deadline, stop)?;
        if n == 0 {
            return Err(HttpReadError::ClosedOrMalformed);
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&raw) {
            let end = pos + 4;
            if end > limits.max_header_bytes {
                return Err(HttpReadError::HeaderTooLarge);
            }
            break pos;
        }
        if raw.len() > limits.max_header_bytes {
            return Err(HttpReadError::HeaderTooLarge);
        }
    };

    let header_text = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = header_text.split("\r\n");
    let req_line = lines.next().ok_or(HttpReadError::ClosedOrMalformed)?;
    let mut parts = req_line.split_whitespace();
    let method = parts
        .next()
        .ok_or(HttpReadError::ClosedOrMalformed)?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or(HttpReadError::ClosedOrMalformed)?
        .to_string();

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push(HttpHeader {
                name: k.trim().to_ascii_lowercase(),
                value: v.trim().to_string(),
            });
        }
    }

    if limits.honor_expect_100
        && headers
            .iter()
            .any(|h| h.name == "expect" && h.value.to_ascii_lowercase().contains("100-continue"))
    {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        let _ = stream.flush();
    }

    let mut body = raw[header_end + 4..].to_vec();

    if let Some(te) = headers.iter().find(|h| h.name == "transfer-encoding") {
        if !limits.allow_chunked || !te.value.to_ascii_lowercase().contains("chunked") {
            return Err(HttpReadError::TransferEncodingNotSupported);
        }
        let decoded = read_chunked_body(stream, &mut body, limits.max_body_bytes, deadline, stop)?;
        return Ok(HttpRequest {
            method,
            target,
            headers,
            body: decoded,
        });
    }

    let content_length = match headers.iter().find(|h| h.name == "content-length") {
        Some(h) => {
            let parsed = h
                .value
                .parse::<usize>()
                .map_err(|_| HttpReadError::BadContentLength)?;
            if parsed > limits.max_body_bytes {
                return Err(HttpReadError::BodyTooLarge);
            }
            parsed
        }
        None => 0,
    };

    while body.len() < content_length {
        let want = (content_length - body.len()).min(chunk.len());
        let n = read_chunk_with_deadline(stream, &mut chunk[..want], deadline, stop)?;
        if n == 0 {
            return Err(HttpReadError::TruncatedBody);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

fn read_chunked_body(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    max_body_bytes: usize,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Vec<u8>, HttpReadError> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut tmp = [0u8; 2048];

    loop {
        let crlf_rel = loop {
            if let Some(rel) = buf[pos..].windows(2).position(|w| w == b"\r\n") {
                break rel;
            }
            if buf.len() > max_body_bytes + 4096 {
                return Err(HttpReadError::BodyTooLarge);
            }
            let n = read_chunk_with_deadline(stream, &mut tmp, deadline, stop)?;
            if n == 0 {
                return Err(HttpReadError::TruncatedBody);
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        let line = String::from_utf8_lossy(&buf[pos..pos + crlf_rel]);
        let hex_str = line.split(';').next().unwrap_or("").trim();
        let chunk_len = usize::from_str_radix(hex_str, 16)
            .map_err(|_| HttpReadError::BadContentLength)?;
        pos += crlf_rel + 2;
        if chunk_len == 0 {
            break;
        }
        if out.len().saturating_add(chunk_len) > max_body_bytes {
            return Err(HttpReadError::BodyTooLarge);
        }
        let need_end = pos + chunk_len + 2;
        while buf.len() < need_end {
            let n = read_chunk_with_deadline(stream, &mut tmp, deadline, stop)?;
            if n == 0 {
                return Err(HttpReadError::TruncatedBody);
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        out.extend_from_slice(&buf[pos..pos + chunk_len]);
        pos = need_end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parse_status_code_parses_valid_lines_and_ignores_body_digits() {
        assert_eq!(parse_status_code("HTTP/1.1 200 OK\r\nContent-Length: 412\r\n"), Some(200));
        assert_eq!(parse_status_code("HTTP/1.1 412 Precondition Failed\r\n"), Some(412));
        assert_eq!(parse_status_code("HTTP/1.0 404 Not Found"), Some(404));
        assert_eq!(parse_status_code("GARBAGE 200 OK"), None);
        assert_eq!(parse_status_code("HTTP/1.1 20 OK"), None);
    }

    #[test]
    fn read_http_request_handles_chunked_rejects_truncated_and_detects_duplicate_host() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = AtomicBool::new(false);
        let limits = HttpReadLimits {
            max_header_bytes: 8192,
            max_body_bytes: 16384,
            first_byte_deadline: Duration::from_secs(2),
            read_deadline: Duration::from_secs(2),
            allow_chunked: true,
            honor_expect_100: false,
        };

        // 1. Chunked body + duplicate Host detection.
        let mut c1 = TcpStream::connect(addr).unwrap();
        let (mut s1, _) = listener.accept().unwrap();
        c1.write_all(
            b"POST /soap HTTP/1.1\r\nHost: 127.0.0.1\r\nHost: evil.com\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
        )
        .unwrap();
        drop(c1);
        let req = read_http_request(&mut s1, &limits, &stop).unwrap();
        assert_eq!(req.body_lossy(), "hello world");
        assert!(req.has_duplicate_header("host"));

        // 2. Truncated Content-Length body rejected cleanly.
        let mut c2 = TcpStream::connect(addr).unwrap();
        let (mut s2, _) = listener.accept().unwrap();
        c2.write_all(b"POST /api HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 20\r\n\r\nshort")
            .unwrap();
        drop(c2);
        assert_eq!(
            read_http_request(&mut s2, &limits, &stop).unwrap_err(),
            HttpReadError::TruncatedBody
        );
    }
}
