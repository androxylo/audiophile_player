// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 androxylo

//! Minimal, zero-dependency, deterministic JSON value, serializer, and recursive-descent parser.
//!
//! Used across Audiophile Player for:
//! - CLI `--json` outputs (`help`, `doctor`, `devices`, `config`, `status`, `stop`, `restart`, `--version`)
//! - `%LOCALAPPDATA%\audiophile-player\run.json` lifecycle discovery
//! - `/api/v1/*` token-authenticated JSON control endpoints
//! - Stdio MCP JSON-RPC 2.0 server (`audiophile-player mcp`)
//!
//! Uses insertion-ordered `Vec<(String, JsonValue)>` for objects so generated schemas and reports
//! stay readable and byte-for-byte deterministic without external dependencies.

use std::fmt::Write as _;

/// Hard maximum nesting depth accepted by [`JsonValue::parse`] (prevents stack overflow on crafted input).
pub const MAX_JSON_DEPTH: usize = 64;

/// Hard maximum JSON document size accepted by [`JsonValue::parse`] (4 MiB).
pub const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    pub fn obj(entries: Vec<(&str, JsonValue)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    pub fn obj_owned(entries: Vec<(String, JsonValue)>) -> Self {
        Self::Object(entries)
    }

    pub fn str_val(s: impl Into<String>) -> Self {
        Self::Str(s.into())
    }

    pub fn opt_str(s: Option<&str>) -> Self {
        match s {
            Some(v) => Self::Str(v.to_string()),
            None => Self::Null,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(n) => Some(*n),
            Self::Float(f) if f.is_finite() && f.fract() == 0.0 => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_i64().and_then(|n| u64::try_from(n).ok())
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Float(f) => Some(*f),
            Self::Int(n) => Some(*n as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            Self::Object(o) => Some(o.as_slice()),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        let Self::Object(entries) = self else {
            return None;
        };
        entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn insert(&mut self, key: impl Into<String>, value: JsonValue) {
        if let Self::Object(entries) = self {
            let k = key.into();
            if let Some(existing) = entries.iter_mut().find(|(ek, _)| *ek == k) {
                existing.1 = value;
            } else {
                entries.push((k, value));
            }
        }
    }

    /// Serialize to compact single-line JSON (suitable for JSON-RPC stdio / HTTP bodies).
    pub fn to_compact_string(&self) -> String {
        let mut out = String::with_capacity(256);
        self.write_fmt(&mut out, None, 0);
        out
    }

    /// Serialize to deterministic 2-space-indented JSON (suitable for CLI `--json` & `run.json`).
    pub fn to_pretty_string(&self) -> String {
        let mut out = String::with_capacity(512);
        self.write_fmt(&mut out, Some(2), 0);
        out
    }

    fn write_fmt(&self, out: &mut String, indent_step: Option<usize>, depth: usize) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Float(f) => {
                if f.is_finite() {
                    let _ = write!(out, "{f}");
                } else {
                    out.push_str("null");
                }
            }
            Self::Str(s) => write_json_string(out, s),
            Self::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Some(step) = indent_step {
                        out.push('\n');
                        push_spaces(out, (depth + 1) * step);
                    }
                    item.write_fmt(out, indent_step, depth + 1);
                }
                if let Some(step) = indent_step {
                    out.push('\n');
                    push_spaces(out, depth * step);
                }
                out.push(']');
            }
            Self::Object(entries) => {
                if entries.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    if let Some(step) = indent_step {
                        out.push('\n');
                        push_spaces(out, (depth + 1) * step);
                    }
                    write_json_string(out, k);
                    if indent_step.is_some() {
                        out.push_str(": ");
                    } else {
                        out.push(':');
                    }
                    v.write_fmt(out, indent_step, depth + 1);
                }
                if let Some(step) = indent_step {
                    out.push('\n');
                    push_spaces(out, depth * step);
                }
                out.push('}');
            }
        }
    }

    /// Strict RFC 8259 recursive-descent JSON parser with bounded depth & size.
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.len() > MAX_JSON_BYTES {
            return Err(format!(
                "JSON document exceeds {MAX_JSON_BYTES}-byte safety limit"
            ));
        }
        let text = input.strip_prefix('\u{feff}').unwrap_or(input);
        let mut p = JsonParser {
            bytes: text.as_bytes(),
            pos: 0,
        };
        let val = p.parse_value(0)?;
        p.skip_ws();
        if p.pos != p.bytes.len() {
            return Err(format!(
                "unexpected trailing character at byte {}",
                p.pos
            ));
        }
        Ok(val)
    }
}

impl From<bool> for JsonValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for JsonValue {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}

impl From<u64> for JsonValue {
    fn from(v: u64) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

impl From<u32> for JsonValue {
    fn from(v: u32) -> Self {
        Self::Int(i64::from(v))
    }
}

impl From<usize> for JsonValue {
    fn from(v: usize) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

impl From<f64> for JsonValue {
    fn from(v: f64) -> Self {
        Self::Float(v)
    }
}

impl From<&str> for JsonValue {
    fn from(v: &str) -> Self {
        Self::Str(v.to_string())
    }
}

impl From<String> for JsonValue {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}

fn push_spaces(out: &mut String, count: usize) {
    for _ in 0..count {
        out.push(' ');
    }
}

/// Escape and quote a Rust string as a valid RFC 8259 JSON string literal.
pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len()
            && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n')
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next_byte(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, String> {
        if depth > MAX_JSON_DEPTH {
            return Err(format!("JSON nesting depth exceeded {MAX_JSON_DEPTH}"));
        }
        self.skip_ws();
        let Some(b) = self.peek() else {
            return Err("unexpected end of JSON input".to_string());
        };
        match b {
            b'n' => {
                self.expect_literal(b"null")?;
                Ok(JsonValue::Null)
            }
            b't' => {
                self.expect_literal(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            b'f' => {
                self.expect_literal(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            b'"' => Ok(JsonValue::Str(self.parse_string()?)),
            b'[' => self.parse_array(depth + 1),
            b'{' => self.parse_object(depth + 1),
            b'-' | b'0'..=b'9' => self.parse_number(),
            other => Err(format!(
                "unexpected byte 0x{other:02x} at offset {}",
                self.pos
            )),
        }
    }

    fn expect_literal(&mut self, lit: &[u8]) -> Result<(), String> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(format!(
                "expected '{}' at byte {}",
                String::from_utf8_lossy(lit),
                self.pos
            ))
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.next_byte() != Some(b'"') {
            return Err(format!("expected '\"' at byte {}", self.pos));
        }
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let Some(esc) = self.next_byte() else {
                    return Err("unterminated escape in JSON string".to_string());
                };
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\x08'),
                    b'f' => out.push('\x0c'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) {
                            // UTF-16 surrogate pair
                            if !self.bytes[self.pos..].starts_with(b"\\u") {
                                return Err("unpaired high surrogate in JSON string".to_string());
                            }
                            self.pos += 2;
                            let low = self.parse_hex4()?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err("invalid low surrogate in JSON string".to_string());
                            }
                            let cp = 0x10000
                                + (((u32::from(code) - 0xD800) << 10)
                                    | (u32::from(low) - 0xDC00));
                            let ch = char::from_u32(cp)
                                .ok_or_else(|| "invalid Unicode code point".to_string())?;
                            out.push(ch);
                        } else if (0xDC00..=0xDFFF).contains(&code) {
                            return Err("unpaired low surrogate in JSON string".to_string());
                        } else {
                            let ch = char::from_u32(u32::from(code))
                                .ok_or_else(|| "invalid Unicode escape".to_string())?;
                            out.push(ch);
                        }
                    }
                    other => {
                        return Err(format!("invalid JSON escape '\\{}'", other as char));
                    }
                }
            } else if b < 0x20 {
                return Err(format!(
                    "unescaped control character 0x{b:02x} in JSON string"
                ));
            } else {
                // Review #10 CS7: scan entire run of plain unescaped bytes in O(n) rather than
                // re-validating `&self.bytes[self.pos..]` on every character (which was O(n^2)).
                let run_start = self.pos;
                while self.pos < self.bytes.len() {
                    let c = self.bytes[self.pos];
                    if c == b'"' || c == b'\\' || c < 0x20 {
                        break;
                    }
                    self.pos += 1;
                }
                let slice = std::str::from_utf8(&self.bytes[run_start..self.pos])
                    .map_err(|_| "invalid UTF-8 in JSON string".to_string())?;
                out.push_str(slice);
            }
        }
        Err("unterminated JSON string".to_string())
    }

    fn parse_hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("truncated \\uXXXX escape in JSON string".to_string());
        }
        let raw = &self.bytes[self.pos..self.pos + 4];
        if !raw.iter().all(|b| b.is_ascii_hexdigit()) {
            let slice = String::from_utf8_lossy(raw);
            return Err(format!("invalid hex escape '\\u{slice}'"));
        }
        let slice = std::str::from_utf8(raw)
            .map_err(|_| "invalid UTF-8 in \\uXXXX escape".to_string())?;
        self.pos += 4;
        u16::from_str_radix(slice, 16)
            .map_err(|_| format!("invalid hex escape '\\u{slice}'"))
    }

    fn parse_number(&mut self) -> Result<JsonValue, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut is_float = false;
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(format!("invalid JSON number at byte {start}")),
        }
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("expected digit after '.' at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(format!("expected exponent digit at byte {}", self.pos));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let raw = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "invalid UTF-8 in number".to_string())?;
        if !is_float {
            if let Ok(n) = raw.parse::<i64>() {
                return Ok(JsonValue::Int(n));
            }
        }
        let f = raw
            .parse::<f64>()
            .map_err(|_| format!("invalid JSON number '{raw}'"))?;
        if !f.is_finite() {
            return Err(format!("non-finite JSON number '{raw}'"));
        }
        Ok(JsonValue::Float(f))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, String> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value(depth)?);
            self.skip_ws();
            match self.next_byte() {
                Some(b',') => continue,
                Some(b']') => return Ok(JsonValue::Array(items)),
                _ => return Err(format!("expected ',' or ']' in array at byte {}", self.pos)),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, String> {
        self.pos += 1; // '{'
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.next_byte() != Some(b':') {
                return Err(format!("expected ':' after object key at byte {}", self.pos));
            }
            let val = self.parse_value(depth)?;
            entries.push((key, val));
            self.skip_ws();
            match self.next_byte() {
                Some(b',') => continue,
                Some(b'}') => return Ok(JsonValue::Object(entries)),
                _ => return Err(format!("expected ',' or '}}' in object at byte {}", self.pos)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_nested_objects_escapes_unicode_and_numbers() {
        let doc = JsonValue::obj(vec![
            ("schema", JsonValue::from("ap.test/1")),
            ("ok", JsonValue::from(true)),
            ("count", JsonValue::from(42_i64)),
            ("neg", JsonValue::from(-7_i64)),
            ("pi", JsonValue::from(3.14159_f64)),
            ("empty", JsonValue::Null),
            ("path", JsonValue::from(r#"D:\Music\Mahler\01 "Adagio".flac"#)),
            (
                "arr",
                JsonValue::Array(vec![
                    JsonValue::from("line1\nline2\t✓"),
                    JsonValue::obj(vec![("nested", JsonValue::from(false))]),
                ]),
            ),
        ]);

        let compact = doc.to_compact_string();
        let pretty = doc.to_pretty_string();
        assert_eq!(JsonValue::parse(&compact).unwrap(), doc);
        assert_eq!(JsonValue::parse(&pretty).unwrap(), doc);
        assert_eq!(doc.get("schema").and_then(JsonValue::as_str), Some("ap.test/1"));
        assert_eq!(doc.get("count").and_then(JsonValue::as_i64), Some(42));
    }

    #[test]
    fn parses_surrogate_pairs_and_rejects_malformed_or_deep_json() {
        let emoji = JsonValue::parse(r#""\uD83C\uDFB5""#).unwrap();
        assert_eq!(emoji.as_str(), Some("🎵"));

        assert!(JsonValue::parse("{trailing}").is_err());
        assert!(JsonValue::parse("123 extra").is_err());
        assert!(JsonValue::parse(r#""\uD83C""#).is_err());

        let too_deep = "[".repeat(MAX_JSON_DEPTH + 5) + &"]".repeat(MAX_JSON_DEPTH + 5);
        assert!(JsonValue::parse(&too_deep).is_err());
    }
}
