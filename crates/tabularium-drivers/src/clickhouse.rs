//! ClickHouse driver over the HTTP interface (reqwest + rustls, no OpenSSL).
//!
//! * Every statement is POSTed with a fresh `query_id` inside an HTTP session
//!   (`session_id`), so `SET`, temporary tables and `USE` behave like a console session.
//!   Requests of one connection are serialized.
//! * Results are requested as `JSONCompactEachRowWithNamesAndTypes` through the
//!   `default_format` setting, so a user-written `FORMAT X` clause still works (its output
//!   is shown as text). Rows are decoded with the column types from the second line.
//! * Row limits use `max_result_rows` + `result_overflow_mode=break` (the server stops
//!   after the block that crosses the limit; subqueries are not affected) and the client
//!   keeps the first `max_rows`.
//! * Cancel sends `KILL QUERY WHERE query_id = ...` on a separate request.
//! * ClickHouse has no interactive transactions: manual mode is `Unsupported`.
//! * Databases act as schemas (like MySQL).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use tabularium_core::driver::first_keyword;
use tabularium_core::error::DatabaseError;
use tabularium_core::model::*;
use tabularium_core::{
    CancelHandle, ColumnMeta, Connection, ConnectionConfig, Dialect, DriverKind, Error, ExecOptions, Result,
    SslMode, StatementResult, TransactionMode, Value,
};
use tokio::sync::Mutex;

use crate::common::*;
use crate::ssh::SshTunnel;

/// Output format requested for every statement.
pub(crate) const FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";

const D: Dialect = Dialect::Generic;

const SYSTEM_DATABASES: [&str; 2] = ["system", "information_schema"];

/// Error codes with special meaning.
const QUERY_WAS_CANCELLED: &str = "394";
const SESSION_IS_LOCKED: &str = "373";
const READONLY: &str = "164";

// ----- HTTP plumbing -------------------------------------------------------------------------

#[derive(Clone)]
struct Http {
    client: reqwest::Client,
    /// `http(s)://host:port/` (path included when the user gave one).
    base: String,
    user: String,
    password: Option<String>,
}

impl Http {
    fn post(&self, sql: String, params: &[(String, String)]) -> reqwest::RequestBuilder {
        let mut rb = self.client.post(&self.base).query(params).body(sql);
        if !self.user.is_empty() || self.password.is_some() {
            rb = rb.basic_auth(&self.user, self.password.as_deref());
        }
        rb
    }

    /// Run a statement outside the session (used for KILL QUERY) and return its body.
    async fn simple(&self, sql: &str) -> Result<String> {
        let params = vec![("http_write_exception_in_output_format".to_string(), "0".to_string())];
        let resp = self.post(sql.to_string(), &params).send().await.map_err(map_http_err)?;
        let status = resp.status();
        let body = resp.text().await.map_err(map_http_err)?;
        if !status.is_success() {
            return Err(parse_error(&body));
        }
        Ok(body)
    }
}

/// Error for a result stream that ended abruptly; `partial` is whatever unparsed text was
/// received last (it may hold the server's exception).
fn interrupted_error(partial: &str, cause: &str) -> Error {
    if partial.contains("Code: ") {
        return parse_error(partial);
    }
    Error::Database(tabularium_core::error::DatabaseError {
        message: format!("query aborted by the server while streaming results ({cause})"),
        ..Default::default()
    })
}

fn map_http_err(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Connection(format!("timed out: {e}"))
    } else if e.is_connect() || e.is_request() || e.is_body() || e.is_decode() {
        // Include the source chain: reqwest's top-level message is just "error sending request".
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg.push_str(": ");
            msg.push_str(&s.to_string());
            src = s.source();
        }
        Error::Connection(msg)
    } else {
        Error::Other(e.to_string())
    }
}

/// Parse a ClickHouse error text such as
/// `Code: 60. DB::Exception: Table default.x does not exist. (UNKNOWN_TABLE) (version 24.3.1.1)`.
pub(crate) fn parse_error(body: &str) -> Error {
    let text = body.trim();
    let mut code: Option<String> = None;
    let mut msg = text.to_string();
    if let Some(i) = text.find("Code: ") {
        let rest = &text[i + 6..];
        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !num.is_empty() {
            code = Some(num.clone());
        }
        msg = match rest.find("DB::Exception: ") {
            Some(j) => rest[j + 15..].to_string(),
            None => rest[num.len()..].trim_start_matches(['.', ' ']).to_string(),
        };
    }
    // Stack traces (only with `send_stack_trace`) and the version suffix are noise.
    if let Some(i) = msg.find(", Stack trace") {
        msg.truncate(i);
    }
    let trimmed = msg.trim_end();
    if trimmed.ends_with(')') {
        if let Some(i) = trimmed.rfind(" (version ") {
            msg.truncate(i);
        }
    }
    let mut name = None;
    let trimmed = msg.trim_end().to_string();
    if trimmed.ends_with(')') {
        if let Some(i) = trimmed.rfind(" (") {
            let n = &trimmed[i + 2..trimmed.len() - 1];
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                name = Some(n.to_string());
                msg = trimmed[..i].to_string();
            }
        }
    }
    if code.as_deref() == Some(QUERY_WAS_CANCELLED) {
        return Error::Cancelled;
    }
    let position = msg
        .find("failed at position ")
        .and_then(|i| msg[i + 19..].split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok());
    if msg.trim().is_empty() {
        msg = "ClickHouse returned an error without a message".into();
    }
    Error::Database(DatabaseError {
        message: msg.trim().to_string(),
        code: code.map(|c| match name {
            Some(n) => format!("{c} ({n})"),
            None => c,
        }),
        position,
        ..Default::default()
    })
}

/// Numeric part of a database error code (`"60 (UNKNOWN_TABLE)"` -> `"60"`).
fn error_number(e: &Error) -> Option<&str> {
    match e {
        Error::Database(d) => d.code.as_deref().and_then(|c| c.split_whitespace().next()),
        _ => None,
    }
}

// ----- types ---------------------------------------------------------------------------------

/// Decoding class of a ClickHouse type.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ChType {
    Int,
    UInt,
    /// Int128/256, UInt128/256: kept as decimal text.
    BigInt,
    Float,
    Decimal,
    Text,
    /// Zero-padded; trailing NULs are stripped.
    FixedString,
    Bool,
    Date,
    DateTime,
    Time,
    Uuid,
    Array(Box<ChType>),
    /// Map, Tuple, JSON, Variant, Dynamic, Nested: shown as JSON.
    Json,
    Nothing,
    Other(String),
}

/// Split the arguments of `Name(a, b(c, d), 'e,f')` at top-level commas.
pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => {
                quote = Some(c);
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// `("Nullable", "Int32")` for `Nullable(Int32)`; `("Int32", "")` for `Int32`.
fn type_head(t: &str) -> (&str, &str) {
    let t = t.trim();
    match t.find('(') {
        Some(i) if t.ends_with(')') => (t[..i].trim(), &t[i + 1..t.len() - 1]),
        _ => (t, ""),
    }
}

/// Classify a ClickHouse type name.
pub(crate) fn parse_type(t: &str) -> ChType {
    let (head, args) = type_head(t);
    match head {
        "Nullable" | "LowCardinality" => parse_type(args),
        "SimpleAggregateFunction" => split_args(args).get(1).map(|a| parse_type(a)).unwrap_or(ChType::Other(t.into())),
        "Int8" | "Int16" | "Int32" | "Int64" => ChType::Int,
        "UInt8" | "UInt16" | "UInt32" | "UInt64" => ChType::UInt,
        "Int128" | "Int256" | "UInt128" | "UInt256" => ChType::BigInt,
        "Float32" | "Float64" | "BFloat16" => ChType::Float,
        "Decimal" | "Decimal32" | "Decimal64" | "Decimal128" | "Decimal256" => ChType::Decimal,
        "String" => ChType::Text,
        "FixedString" => ChType::FixedString,
        "Bool" | "Boolean" => ChType::Bool,
        "Date" | "Date32" => ChType::Date,
        "DateTime" | "DateTime32" | "DateTime64" => ChType::DateTime,
        "Time" | "Time64" => ChType::Time,
        "UUID" => ChType::Uuid,
        "IPv4" | "IPv6" | "Enum8" | "Enum16" | "Enum" => ChType::Text,
        "Array" => ChType::Array(Box::new(parse_type(args))),
        "Map" | "Tuple" | "JSON" | "Object" | "Variant" | "Dynamic" | "Nested" | "Point" | "Ring" | "Polygon"
        | "MultiPolygon" | "LineString" | "MultiLineString" => ChType::Json,
        "Nothing" => ChType::Nothing,
        _ => ChType::Other(t.trim().to_string()),
    }
}

/// `true` for `Nullable(...)` (also inside `LowCardinality`).
pub(crate) fn is_nullable_type(t: &str) -> bool {
    let (head, args) = type_head(t);
    match head {
        "Nullable" => true,
        "LowCardinality" => is_nullable_type(args),
        _ => false,
    }
}

fn json_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
        .ok()
}

/// Decode one JSON cell of a `JSONCompact*` row.
pub(crate) fn decode(ty: &ChType, type_name: &str, v: &serde_json::Value) -> Value {
    use serde_json::Value as J;
    if v.is_null() {
        return Value::Null;
    }
    let other = || Value::Other { type_name: type_name.to_string(), text: json_text(v) };
    match ty {
        ChType::Nothing => Value::Null,
        ChType::Int => match v {
            J::Number(n) => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Decimal(n.to_string())),
            J::String(s) => s.parse().map(Value::Int).unwrap_or_else(|_| Value::Decimal(s.clone())),
            _ => other(),
        },
        ChType::UInt => match v {
            J::Number(n) => n.as_u64().map(Value::UInt).unwrap_or_else(|| Value::Decimal(n.to_string())),
            J::String(s) => s.parse().map(Value::UInt).unwrap_or_else(|_| Value::Decimal(s.clone())),
            J::Bool(b) => Value::UInt(u64::from(*b)),
            _ => other(),
        },
        ChType::BigInt | ChType::Decimal => match v {
            J::Number(_) | J::String(_) => Value::Decimal(json_text(v)),
            _ => other(),
        },
        ChType::Float => match v {
            J::Number(n) => n.as_f64().map(Value::Float).unwrap_or_else(other),
            J::String(s) => parse_float(s).map(Value::Float).unwrap_or_else(other),
            _ => other(),
        },
        ChType::Text => match v {
            J::String(s) => Value::Text(s.clone()),
            _ => Value::Text(v.to_string()),
        },
        ChType::FixedString => match v {
            J::String(s) => Value::Text(s.trim_end_matches('\0').to_string()),
            _ => other(),
        },
        ChType::Bool => match v {
            J::Bool(b) => Value::Bool(*b),
            J::Number(n) => Value::Bool(n.as_i64().unwrap_or(0) != 0),
            J::String(s) => match s.as_str() {
                "true" | "1" => Value::Bool(true),
                "false" | "0" => Value::Bool(false),
                _ => other(),
            },
            _ => other(),
        },
        ChType::Date => match v {
            J::String(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d").map(Value::Date).unwrap_or_else(|_| other()),
            _ => other(),
        },
        ChType::DateTime => match v {
            J::String(s) => parse_datetime(s).map(Value::DateTime).unwrap_or_else(other),
            _ => other(),
        },
        ChType::Time => match v {
            J::String(s) => NaiveTime::parse_from_str(s, "%H:%M:%S%.f").map(Value::Time).unwrap_or_else(|_| other()),
            _ => other(),
        },
        ChType::Uuid => match v {
            J::String(s) => uuid::Uuid::parse_str(s).map(Value::Uuid).unwrap_or_else(|_| other()),
            _ => other(),
        },
        ChType::Array(inner) => match v {
            J::Array(items) => {
                let inner_name = type_head(type_name).1;
                Value::Array(items.iter().map(|i| decode(inner, inner_name, i)).collect())
            }
            _ => other(),
        },
        ChType::Json => match v {
            // `JSON` columns may be serialized as a string.
            J::String(s) => serde_json::from_str(s).map(Value::Json).unwrap_or_else(|_| Value::Text(s.clone())),
            _ => Value::Json(v.clone()),
        },
        ChType::Other(_) => other(),
    }
}

/// Render a parameter as a ClickHouse literal (backslash escapes are significant).
pub(crate) fn literal(v: &Value) -> String {
    fn quote(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for c in s.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\'' => out.push_str("\\'"),
                '\0' => out.push_str("\\0"),
                _ => out.push(c),
            }
        }
        out.push('\'');
        out
    }
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) if f.is_nan() => "nan".into(),
        Value::Float(f) if f.is_infinite() => if *f > 0.0 { "inf" } else { "-inf" }.into(),
        Value::Float(f) => {
            let s = f.to_string();
            if s.contains(['.', 'e', 'E']) {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Decimal(d) => d.clone(),
        Value::Bytes(b) => {
            let mut s = String::from("'");
            for byte in b {
                s.push_str(&format!("\\x{byte:02X}"));
            }
            s.push('\'');
            s
        }
        Value::Array(items) => format!("[{}]", items.iter().map(literal).collect::<Vec<_>>().join(", ")),
        Value::Json(j) => quote(&j.to_string()),
        other => quote(&other.to_string()),
    }
}

/// Replace `?` placeholders outside literals/comments with literals of `params`.
pub(crate) fn bind_params(sql: &str, params: &[Value]) -> Result<String> {
    if params.is_empty() {
        return Ok(sql.to_string());
    }
    let mut out = String::with_capacity(sql.len() + params.len() * 8);
    let mut chars = sql.chars().peekable();
    let mut next = 0usize;
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                out.push(c);
                let mut escaped = false;
                for n in chars.by_ref() {
                    out.push(n);
                    if escaped {
                        escaped = false;
                    } else if n == '\\' {
                        escaped = true;
                    } else if n == c {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                out.push(c);
                for n in chars.by_ref() {
                    out.push(n);
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                out.push(c);
                let mut prev = ' ';
                for n in chars.by_ref() {
                    out.push(n);
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            '?' => {
                let v = params
                    .get(next)
                    .ok_or_else(|| Error::Other(format!("statement has more placeholders than the {} parameters given", params.len())))?;
                out.push_str(&literal(v));
                next += 1;
            }
            _ => out.push(c),
        }
    }
    if next != params.len() {
        return Err(Error::Other(format!("{} parameters given but the statement has {next} placeholders", params.len())));
    }
    Ok(out)
}

/// Statements that return a result set.
pub(crate) fn returns_rows(sql: &str) -> bool {
    matches!(
        first_keyword(sql).to_ascii_uppercase().as_str(),
        "SELECT" | "WITH" | "SHOW" | "DESCRIBE" | "DESC" | "EXISTS" | "EXPLAIN" | "VALUES"
    )
}

// ----- result parsing ------------------------------------------------------------------------

/// Incremental parser of a `JSONCompactEachRowWithNamesAndTypes` body.
#[derive(Default)]
pub(crate) struct RowParser {
    buf: Vec<u8>,
    pub names: Option<Vec<String>>,
    pub type_names: Option<Vec<String>>,
    types: Vec<ChType>,
    pub rows: Vec<Vec<Value>>,
    pub truncated: bool,
    max_rows: Option<usize>,
    /// Text after an `__exception__` marker (exception raised mid-stream, ClickHouse 24.11+).
    exception: Option<String>,
}

impl RowParser {
    pub(crate) fn new(max_rows: Option<usize>) -> Self {
        Self { max_rows, ..Default::default() }
    }

    fn line(&mut self, line: &[u8]) -> Result<()> {
        let line = trim_ascii(line);
        if let Some(exc) = self.exception.as_mut() {
            exc.push_str(&String::from_utf8_lossy(line));
            exc.push('\n');
            return Ok(());
        }
        if line.starts_with(b"__exception__") {
            self.exception = Some(String::new());
            return Ok(());
        }
        if line.is_empty() {
            return Ok(());
        }
        if self.type_names.is_some() && self.max_rows.is_some_and(|m| self.rows.len() >= m) {
            // Draining past the limit: skip decoding.
            self.truncated = true;
            return Ok(());
        }
        let parsed: serde_json::Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            // Invalid UTF-8 inside a String column: retry lossily.
            Err(_) => match serde_json::from_str(&String::from_utf8_lossy(line)) {
                Ok(v) => v,
                Err(_) => {
                    let text = String::from_utf8_lossy(line);
                    // An exception written mid-stream after the headers were sent.
                    if text.contains("Code: ") {
                        return Err(parse_error(&text));
                    }
                    return Err(Error::Other(format!("unexpected ClickHouse output: {}", truncate_text(&text, 300))));
                }
            },
        };
        let serde_json::Value::Array(items) = parsed else {
            if let Some(e) = parsed.get("exception").and_then(|e| e.as_str()) {
                return Err(parse_error(e));
            }
            return Err(Error::Other(format!("unexpected ClickHouse output: {}", truncate_text(&parsed.to_string(), 300))));
        };
        if self.names.is_none() {
            self.names = Some(items.iter().map(json_text).collect());
        } else if self.type_names.is_none() {
            let tn: Vec<String> = items.iter().map(json_text).collect();
            self.types = tn.iter().map(|t| parse_type(t)).collect();
            self.type_names = Some(tn);
        } else {
            let names = self.type_names.as_deref().unwrap_or_default();
            let row = items
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let ty = self.types.get(i).cloned().unwrap_or(ChType::Other(String::new()));
                    decode(&ty, names.get(i).map(String::as_str).unwrap_or(""), v)
                })
                .collect();
            self.rows.push(row);
        }
        Ok(())
    }

    /// Feed a chunk of the body.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|b| *b == b'\n') {
            let end = start + pos;
            let line = self.buf[start..end].to_vec();
            self.line(&line)?;
            start = end + 1;
        }
        self.buf.drain(..start);
        Ok(())
    }

    /// `true` once more than `max_rows` rows were seen.
    pub(crate) fn full(&self) -> bool {
        self.truncated
    }

    /// The body broke off after the server had answered successfully. ClickHouse does this
    /// when a query fails mid-stream and it can't write the exception into the output, so
    /// report a query error (with the server's message when part of it arrived) rather
    /// than a connection failure: the HTTP session itself is fine.
    pub(crate) fn interrupted(&mut self, cause: &reqwest::Error) -> Error {
        let partial = format!(
            "{}{}",
            self.exception.take().unwrap_or_default(),
            String::from_utf8_lossy(&std::mem::take(&mut self.buf))
        );
        interrupted_error(&partial, &cause.to_string())
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        let rest = std::mem::take(&mut self.buf);
        self.line(&rest)?;
        match self.exception.take() {
            Some(text) => Err(parse_error(&text)),
            None => Ok(()),
        }
    }

    pub(crate) fn columns(&self) -> Vec<ColumnMeta> {
        let names = self.names.clone().unwrap_or_default();
        let types = self.type_names.clone().unwrap_or_default();
        names
            .into_iter()
            .enumerate()
            .map(|(i, n)| {
                let t = types.get(i).cloned().unwrap_or_default();
                let mut c = ColumnMeta::new(n, t.clone());
                c.nullable = Some(is_nullable_type(&t));
                c
            })
            .collect()
    }
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map(|e| e + 1).unwrap_or(start);
    &b[start..end.max(start)]
}

fn truncate_text(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Outcome of one HTTP statement.
struct Outcome {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
    written_rows: Option<u64>,
}

// ----- connection ----------------------------------------------------------------------------

/// One ClickHouse "session" (HTTP session id + current database).
pub struct ClickHouseConnection {
    cfg: ConnectionConfig,
    http: Http,
    session_id: String,
    session_timeout: Option<u32>,
    /// User settings from `cfg.options`.
    settings: Vec<(String, String)>,
    /// The user's profile is `readonly=1`: no settings may be passed with queries.
    limited: bool,
    database: RwLock<Option<String>>,
    /// Serializes requests (an HTTP session runs one query at a time). `true` = closed.
    lock: Mutex<bool>,
    running: Arc<std::sync::Mutex<Option<String>>>,
    cancelled: Arc<AtomicBool>,
    version: String,
    _tunnel: Option<Arc<SshTunnel>>,
}

struct ClickHouseCancel {
    http: Http,
    running: Arc<std::sync::Mutex<Option<String>>>,
    cancelled: Arc<AtomicBool>,
    _tunnel: Option<Arc<SshTunnel>>,
}

fn kill_sql(query_id: &str, sync: bool) -> String {
    format!(
        "KILL QUERY WHERE query_id = {} {}",
        literal(&Value::from(query_id)),
        if sync { "SYNC" } else { "ASYNC" }
    )
}

#[async_trait]
impl CancelHandle for ClickHouseCancel {
    async fn cancel(&self) -> Result<()> {
        let id = self.running.lock().map_err(|_| Error::other("lock poisoned"))?.clone();
        let Some(id) = id else { return Ok(()) };
        self.cancelled.store(true, Ordering::SeqCst);
        match tokio::time::timeout(Duration::from_secs(15), self.http.simple(&kill_sql(&id, false))).await {
            Ok(r) => r.map(|_| ()),
            Err(_) => Err(Error::Connection("timed out sending KILL QUERY".into())),
        }
    }
}

/// Where and how to connect, derived from the config.
struct Target {
    https: bool,
    host: String,
    port: u16,
    path: String,
    user: String,
    password: Option<String>,
    database: String,
    options: Vec<(String, String)>,
}

fn target(cfg: &ConnectionConfig) -> Result<Target> {
    let url = cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty());
    let mut eff = cfg.clone();
    let mut path = "/".to_string();
    let mut scheme_https: Option<bool> = None;
    if let Some(url) = url {
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            // Raw HTTP endpoint: parse it with the generic URL parser.
            let https = lower.starts_with("https://");
            let rest = &url[if https { 8 } else { 7 }..];
            let (authority, p) = match rest.find(['/', '?']) {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            let (p, query) = match p.split_once('?') {
                Some((a, b)) => (a, Some(b)),
                None => (p, None),
            };
            let mut parsed = ConnectionConfig::from_url(&format!(
                "clickhouse://{authority}/{}",
                query.map(|q| format!("?{q}")).unwrap_or_default()
            ))?;
            if !authority.contains('@') {
                parsed.user = cfg.user.clone();
                parsed.password = cfg.password.clone();
            }
            if !has_port(authority) {
                parsed.port = if https { 443 } else { 80 };
            }
            parsed.database = parsed.options.remove("database").unwrap_or_else(|| cfg.database.clone());
            if !p.is_empty() && p != "/" {
                path = p.to_string();
            }
            scheme_https = Some(https);
            eff = ConnectionConfig { ssl: cfg.ssl.clone(), ..parsed };
        } else {
            let parsed = ConnectionConfig::from_url(url)?;
            if parsed.driver != DriverKind::ClickHouse {
                return Err(Error::Config(format!("not a ClickHouse URL: {url}")));
            }
            let mut ssl = parsed.ssl.clone();
            if ssl.mode == SslMode::Prefer {
                ssl.mode = cfg.ssl.mode;
            }
            ssl.ca_cert = ssl.ca_cert.or_else(|| cfg.ssl.ca_cert.clone());
            ssl.client_cert = ssl.client_cert.or_else(|| cfg.ssl.client_cert.clone());
            ssl.client_key = ssl.client_key.or_else(|| cfg.ssl.client_key.clone());
            let password = parsed.password.clone().or_else(|| cfg.password.clone());
            eff = ConnectionConfig { ssl, password, ..parsed };
        }
    }
    let port = if eff.port == 0 { DriverKind::ClickHouse.default_port() } else { eff.port };
    let https = scheme_https.unwrap_or(match eff.ssl.mode {
        SslMode::Disable => false,
        SslMode::Prefer => matches!(port, 443 | 8443),
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => true,
    });
    let host = if eff.host.trim().is_empty() { "localhost".to_string() } else { eff.host.trim().to_string() };
    let mut options: Vec<(String, String)> = cfg.options.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in &eff.options {
        if !options.iter().any(|(ok, _)| ok == k) {
            options.push((k.clone(), v.clone()));
        }
    }
    Ok(Target {
        https,
        host,
        port,
        path,
        user: eff.user.clone(),
        password: eff.password.clone().filter(|p| !p.is_empty()),
        database: eff.database.clone(),
        options,
    })
}

fn has_port(authority: &str) -> bool {
    let hostport = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    match hostport.rfind(']') {
        Some(i) => hostport[i..].contains(':'),
        None => hostport.contains(':'),
    }
}

fn build_client(cfg: &ConnectionConfig, https: bool, resolve: Option<(&str, std::net::SocketAddr)>) -> Result<reqwest::Client> {
    let secs = if cfg.connect_timeout_secs == 0 { 30 } else { cfg.connect_timeout_secs };
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(secs))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_max_idle_per_host(2)
        .user_agent(concat!("Tabularium/", env!("CARGO_PKG_VERSION")));
    if let Some((host, addr)) = resolve {
        b = b.resolve(host, addr);
    }
    if https {
        b = b.use_rustls_tls();
        if let Some(ca) = &cfg.ssl.ca_cert {
            let pem = std::fs::read(ca).map_err(|e| Error::Config(format!("cannot read CA certificate {}: {e}", ca.display())))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| Error::Config(format!("invalid CA certificate {}: {e}", ca.display())))?;
            for c in certs {
                b = b.add_root_certificate(c);
            }
        }
        if let (Some(cert), Some(key)) = (&cfg.ssl.client_cert, &cfg.ssl.client_key) {
            let mut pem = std::fs::read(cert).map_err(|e| Error::Config(format!("cannot read client certificate: {e}")))?;
            pem.push(b'\n');
            pem.extend(std::fs::read(key).map_err(|e| Error::Config(format!("cannot read client key: {e}")))?);
            let id = reqwest::Identity::from_pem(&pem).map_err(|e| Error::Config(format!("invalid client certificate/key: {e}")))?;
            b = b.identity(id);
        }
        match cfg.ssl.mode {
            // libpq semantics: `require` encrypts without verifying the server certificate
            // (unless a CA was given); verify-ca / verify-full verify the chain and host.
            SslMode::Require | SslMode::Prefer if cfg.ssl.ca_cert.is_none() => b = b.danger_accept_invalid_certs(true),
            _ => {}
        }
    }
    b.build().map_err(|e| Error::Config(format!("cannot create HTTP client: {e}")))
}

impl ClickHouseConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let t = target(cfg)?;
        let endpoint = crate::resolve_endpoint(cfg, &t.host, t.port).await?;
        let tunnelled = endpoint.tunnel.is_some();
        let host_is_ip = t.host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok();
        // Through a tunnel keep the original host name in the URL (TLS SNI / verification)
        // and resolve it to the local tunnel end.
        let (url_host, resolve) = if tunnelled && !host_is_ip && t.https {
            let addr: std::net::SocketAddr = ([127, 0, 0, 1], endpoint.port).into();
            (t.host.clone(), Some((t.host.as_str(), addr)))
        } else {
            (endpoint.host.clone(), None)
        };
        let client = build_client(cfg, t.https, resolve)?;
        let host_part = if url_host.contains(':') && !url_host.starts_with('[') { format!("[{url_host}]") } else { url_host };
        let path = if t.path.starts_with('/') { t.path.clone() } else { format!("/{}", t.path) };
        let http = Http {
            client,
            base: format!("{}://{}:{}{}", if t.https { "https" } else { "http" }, host_part, endpoint.port, path),
            user: if t.user.is_empty() && t.password.is_none() { String::new() } else { t.user.clone() },
            password: t.password.clone(),
        };
        let mut conn = Self {
            cfg: cfg.clone(),
            http,
            session_id: uuid::Uuid::new_v4().to_string(),
            session_timeout: Some(3600),
            settings: t.options.clone(),
            limited: false,
            database: RwLock::new(Some(t.database.clone()).filter(|d| !d.is_empty())),
            lock: Mutex::new(false),
            running: Arc::new(std::sync::Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            version: String::new(),
            _tunnel: endpoint.tunnel,
        };
        let probe = "SELECT version(), currentDatabase()";
        let first = with_connect_timeout(cfg, conn.request(probe, None)).await;
        let out = match first {
            Ok(o) => o,
            Err(Error::Database(d)) => {
                // Profiles with readonly=1 reject any setting; servers with a low
                // max_session_timeout reject ours. Retry minimally before giving up.
                log::debug!("ClickHouse probe failed ({d}); retrying without settings");
                conn.session_timeout = None;
                if d.code.as_deref().is_some_and(|c| c.starts_with(READONLY)) || d.message.contains("readonly") {
                    conn.limited = true;
                }
                match with_connect_timeout(cfg, conn.request(probe, None)).await {
                    Ok(o) => o,
                    Err(Error::Database(d2)) if !conn.limited => {
                        conn.limited = true;
                        with_connect_timeout(cfg, conn.request(probe, None)).await.map_err(|e| match e {
                            Error::Database(_) => Error::Connection(d2.to_string()),
                            other => other,
                        })?
                    }
                    Err(Error::Database(d2)) => return Err(Error::Connection(d2.to_string())),
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        let row = out.rows.first().cloned().unwrap_or_default();
        conn.version = c_string(&row, 0);
        if let Ok(mut g) = conn.database.write() {
            *g = c_nonempty(&row, 1);
        }
        for stmt in init_statements(cfg) {
            conn.execute(&stmt, &ExecOptions::unlimited()).await?;
        }
        if !cfg.auto_commit {
            log::info!("ClickHouse has no transactions; ignoring manual-commit mode");
        }
        Ok(conn)
    }

    fn params(&self, query_id: &str, server_limit: Option<usize>) -> Vec<(String, String)> {
        let mut p: Vec<(String, String)> = vec![
            ("session_id".into(), self.session_id.clone()),
            ("query_id".into(), query_id.to_string()),
        ];
        if let Some(t) = self.session_timeout {
            p.push(("session_timeout".into(), t.to_string()));
        }
        if let Some(db) = self.current_database() {
            p.push(("database".into(), db));
        }
        if self.limited {
            return p;
        }
        for (k, v) in [
            ("default_format", FORMAT),
            ("output_format_json_quote_64bit_integers", "1"),
            ("output_format_json_quote_decimals", "1"),
            ("output_format_json_quote_denormals", "1"),
            ("http_write_exception_in_output_format", "0"),
        ] {
            p.push((k.into(), v.into()));
        }
        if self.cfg.read_only && !self.settings.iter().any(|(k, _)| k == "readonly") {
            p.push(("readonly".into(), "2".into()));
        }
        for (k, v) in &self.settings {
            p.push((k.clone(), v.clone()));
        }
        if let Some(n) = server_limit {
            p.retain(|(k, _)| k != "max_result_rows" && k != "result_overflow_mode");
            p.push(("max_result_rows".into(), (n + 1).to_string()));
            p.push(("result_overflow_mode".into(), "break".into()));
        }
        p
    }

    /// Send one statement and collect its result.
    async fn request(&self, sql: &str, max_rows: Option<usize>) -> Result<Outcome> {
        let guard = self.lock.lock().await;
        if *guard {
            return Err(closed());
        }
        let rows_stmt = returns_rows(sql);
        let mut body = trim_statement(sql).to_string();
        if self.limited && rows_stmt && !contains_word(&body, "FORMAT") {
            body = format!("{body}\nFORMAT {FORMAT}");
        }
        let server_limit = if rows_stmt { max_rows } else { None };
        let mut attempt = 0;
        let result = loop {
            let query_id = uuid::Uuid::new_v4().to_string();
            if let Ok(mut g) = self.running.lock() {
                *g = Some(query_id.clone());
            }
            self.cancelled.store(false, Ordering::SeqCst);
            let r = self.send(&body, &query_id, max_rows, server_limit).await;
            if let Ok(mut g) = self.running.lock() {
                *g = None;
            }
            match r {
                // The session is still busy with a query we abandoned; wait for it.
                Err(e) if error_number(&e) == Some(SESSION_IS_LOCKED) && attempt < 50 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) if self.cancelled.load(Ordering::SeqCst) && !matches!(e, Error::Cancelled) => {
                    log::debug!("query failed after cancel: {e}");
                    break Err(Error::Cancelled);
                }
                other => break other,
            }
        };
        drop(guard);
        result
    }

    async fn send(&self, body: &str, query_id: &str, max_rows: Option<usize>, server_limit: Option<usize>) -> Result<Outcome> {
        let params = self.params(query_id, server_limit);
        let mut resp = self.http.post(body.to_string(), &params).send().await.map_err(map_http_err)?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let auth = status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN;
            if auth && !text.contains("Code: ") {
                return Err(Error::Connection(format!("HTTP {status}: {}", text.trim())));
            }
            if !text.contains("Code: ") {
                return Err(Error::Other(format!("HTTP {status}: {}", truncate_text(text.trim(), 500))));
            }
            return Err(parse_error(&text));
        }
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let format = header("x-clickhouse-format");
        let written_rows = header("x-clickhouse-summary")
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|j| j.get("written_rows").map(json_text))
            .and_then(|s| s.parse::<u64>().ok());
        let ours = format.as_deref().is_none_or(|f| f == FORMAT);
        if !ours {
            // The statement chose its own FORMAT: show the raw output, one row per line.
            let text = resp.text().await.map_err(map_http_err)?;
            if let Some(i) = text.find("Code: ").filter(|i| text[*i..].contains("DB::Exception")) {
                return Err(parse_error(&text[i..]));
            }
            let rows: Vec<Vec<Value>> = text.lines().map(|l| vec![Value::Text(l.to_string())]).collect();
            let (rows, truncated) = match max_rows {
                Some(m) if rows.len() > m => (rows.into_iter().take(m).collect(), true),
                _ => (rows, false),
            };
            return Ok(Outcome {
                columns: vec![ColumnMeta::new(format.unwrap_or_else(|| "result".into()), "String")],
                rows,
                truncated,
                written_rows,
            });
        }
        let mut parser = RowParser::new(max_rows);
        let mut abandoned = false;
        loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) if e.is_timeout() => return Err(map_http_err(e)),
                Err(e) => return Err(parser.interrupted(&e)),
            };
            parser.feed(&chunk)?;
            // With a server-side limit the rest is at most one block: drain it so the
            // session is free. Otherwise stop reading and kill the query.
            if parser.full() && server_limit.is_none() {
                abandoned = true;
                break;
            }
        }
        if abandoned {
            drop(resp);
            let kill_stmt = kill_sql(query_id, true);
            let kill = self.http.simple(&kill_stmt);
            if let Err(e) = tokio::time::timeout(Duration::from_secs(10), kill).await.unwrap_or(Ok(String::new())) {
                log::debug!("could not stop abandoned query {query_id}: {e}");
            }
        } else {
            parser.finish()?;
        }
        Ok(Outcome { columns: parser.columns(), truncated: parser.truncated, rows: parser.rows, written_rows })
    }

    /// Internal catalog query (no read-only check); returns rows.
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        let sql = bind_params(sql, params)?;
        Ok(self.request(&sql, None).await?.rows)
    }

    fn schema_or_current(&self, schema: Option<&str>) -> Option<String> {
        schema.filter(|s| !s.is_empty()).map(str::to_string).or_else(|| self.current_database())
    }

    fn require_schema(&self, schema: Option<&str>) -> Result<String> {
        self.schema_or_current(schema).ok_or_else(|| Error::Other("no database selected".into()))
    }

    async fn describe(&self, object: &ObjectRef) -> Result<TableInfo> {
        let db = self.require_schema(object.schema.as_deref())?;
        let name = object.name.as_str();
        let rows = self
            .query(
                "SELECT engine, comment, total_rows, primary_key, sorting_key, as_select \
                 FROM system.tables WHERE database = ? AND name = ?",
                &[Value::from(db.as_str()), Value::from(name)],
            )
            .await?;
        let t = rows.first().ok_or_else(|| not_found("table", &format!("{db}.{name}")))?;
        let engine = c_string(t, 0);
        let kind = engine_kind(&engine);
        let mut info = TableInfo::new(ObjectRef::new(Some(&db), name, kind));
        info.comment = c_nonempty(t, 1);
        info.row_estimate = c_i64(t, 2);
        if kind != ObjectKind::Table {
            info.view_definition = c_nonempty(t, 5);
        }
        let cols = self
            .query(
                "SELECT name, type, default_kind, default_expression, comment, position \
                 FROM system.columns WHERE database = ? AND table = ? ORDER BY position",
                &[Value::from(db.as_str()), Value::from(name)],
            )
            .await?;
        for r in &cols {
            let data_type = c_string(r, 1);
            let default_kind = c_string(r, 2);
            let expr = c_nonempty(r, 3);
            let (default, generated) = match default_kind.as_str() {
                "DEFAULT" => (expr, None),
                "" => (None, None),
                // MATERIALIZED / ALIAS / EPHEMERAL: computed columns.
                other => (None, Some(match expr {
                    Some(e) => format!("{other} {e}"),
                    None => other.to_string(),
                })),
            };
            info.columns.push(ColumnInfo {
                name: c_string(r, 0),
                nullable: is_nullable_type(&data_type),
                data_type,
                default,
                auto_increment: false,
                generated,
                comment: c_nonempty(r, 4),
                collation: None,
                ordinal: c_i64(r, 5).unwrap_or(0) as u32,
            });
        }
        if kind == ObjectKind::Table {
            // The MergeTree primary key (a prefix of the sorting key; not unique).
            let pk = c_nonempty(t, 3).or_else(|| c_nonempty(t, 4));
            if let Some(pk) = pk {
                let columns = split_args(&pk);
                if !columns.is_empty() {
                    info.primary_key = Some(KeyInfo { name: None, columns });
                }
            }
            let idx_params = [Value::from(db.as_str()), Value::from(name)];
            // `type_full` (with arguments, e.g. `set(100)`) is missing on old servers.
            let idx = match self
                .query(
                    "SELECT name, type_full, expr, granularity FROM system.data_skipping_indices \
                     WHERE database = ? AND table = ? ORDER BY name",
                    &idx_params,
                )
                .await
            {
                Ok(r) => r,
                Err(_) => self
                    .query(
                        "SELECT name, type, expr, granularity FROM system.data_skipping_indices \
                         WHERE database = ? AND table = ? ORDER BY name",
                        &idx_params,
                    )
                    .await
                    .unwrap_or_default(),
            };
            for r in idx {
                let iname = c_string(&r, 0);
                let ty = c_string(&r, 1);
                let expr = c_string(&r, 2);
                let gran = c_i64(&r, 3).unwrap_or(1);
                info.indexes.push(IndexInfo {
                    definition: Some(format!(
                        "ALTER TABLE {} ADD INDEX {} {expr} TYPE {ty} GRANULARITY {gran}",
                        D.qualified(Some(&db), name),
                        D.quote_ident_if_needed(&iname)
                    )),
                    name: iname,
                    columns: vec![expr],
                    unique: false,
                    primary: false,
                    method: Some(ty),
                    predicate: None,
                });
            }
        }
        Ok(info)
    }
}

/// Map a table engine to an explorer object kind.
pub(crate) fn engine_kind(engine: &str) -> ObjectKind {
    match engine {
        "View" | "LiveView" | "WindowView" => ObjectKind::View,
        "MaterializedView" => ObjectKind::MaterializedView,
        _ => ObjectKind::Table,
    }
}

/// Database name of a `USE db` statement.
fn use_target(sql: &str) -> Option<String> {
    let s = trim_statement(sql);
    let kw = first_keyword(s);
    if !kw.eq_ignore_ascii_case("USE") {
        return None;
    }
    let start = kw.as_ptr() as usize - s.as_ptr() as usize + kw.len();
    let name = s[start..].trim();
    let unq = match name.chars().next() {
        Some(q @ ('`' | '"')) if name.len() >= 2 && name.ends_with(q) => {
            name[1..name.len() - 1].replace(&format!("{q}{q}"), &q.to_string())
        }
        _ => name.to_string(),
    };
    Some(unq).filter(|n| !n.is_empty())
}

#[async_trait]
impl Connection for ClickHouseConnection {
    fn driver(&self) -> DriverKind {
        DriverKind::ClickHouse
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.query("SELECT version()", &[]).await?;
        let v = rows.first().map(|r| c_string(r, 0)).unwrap_or_else(|| self.version.clone());
        Ok(format!("ClickHouse {v}"))
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
        check_read_only(self.cfg.read_only, sql)?;
        let start = Instant::now();
        if let Some(db) = use_target(sql) {
            self.use_database(&db).await?;
            return Ok(StatementResult { sql: sql.to_string(), elapsed: start.elapsed(), ..Default::default() });
        }
        let body = bind_params(sql, &opts.params)?;
        let out = self.request(&body, opts.max_rows).await?;
        let has_rows = !out.columns.is_empty();
        let insert = first_keyword(sql).eq_ignore_ascii_case("INSERT");
        Ok(StatementResult {
            sql: sql.to_string(),
            columns: out.columns,
            rows: out.rows,
            rows_affected: if !has_rows && insert { out.written_rows } else { None },
            truncated: out.truncated,
            elapsed: start.elapsed(),
            notices: vec![],
        })
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(ClickHouseCancel {
            http: self.http.clone(),
            running: self.running.clone(),
            cancelled: self.cancelled.clone(),
            _tunnel: self._tunnel.clone(),
        }))
    }

    async fn set_transaction_mode(&self, mode: TransactionMode) -> Result<()> {
        match mode {
            TransactionMode::Auto => Ok(()),
            TransactionMode::Manual => Err(Error::Unsupported("ClickHouse has no transactions; manual commit mode is unavailable".into())),
        }
    }

    fn transaction_mode(&self) -> TransactionMode {
        TransactionMode::Auto
    }

    fn in_transaction(&self) -> bool {
        false
    }

    async fn commit(&self) -> Result<()> {
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = self.query("SELECT name FROM system.databases ORDER BY name", &[]).await?;
        Ok(rows.iter().map(|r| c_string(r, 0)).collect())
    }

    fn current_database(&self) -> Option<String> {
        self.database.read().ok().and_then(|g| g.clone())
    }

    async fn use_database(&self, database: &str) -> Result<()> {
        let rows = self.query("SELECT name FROM system.databases WHERE name = ?", &[Value::from(database)]).await?;
        if rows.is_empty() {
            return Err(Error::Database(DatabaseError {
                message: format!("Database {database} does not exist"),
                code: Some("81 (UNKNOWN_DATABASE)".into()),
                ..Default::default()
            }));
        }
        if let Ok(mut g) = self.database.write() {
            *g = Some(database.to_string());
        }
        Ok(())
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let mut dbs = self.list_databases().await?;
        if !self.cfg.show_system_objects {
            dbs.retain(|d| !SYSTEM_DATABASES.contains(&d.to_ascii_lowercase().as_str()));
        }
        if !self.cfg.schema_filter.is_empty() {
            dbs.retain(|d| self.cfg.schema_filter.iter().any(|f| f == d));
        }
        Ok(dbs)
    }

    async fn current_schema(&self) -> Result<Option<String>> {
        Ok(self.current_database())
    }

    async fn list_objects(&self, schema: Option<&str>, kinds: &[ObjectKind]) -> Result<Vec<ObjectInfo>> {
        let want = |k: ObjectKind| kinds.contains(&k);
        let mut out = vec![];
        if want(ObjectKind::Schema) {
            for d in self.list_schemas().await? {
                out.push(ObjectInfo::new(ObjectRef::new(None, d, ObjectKind::Schema)));
            }
        }
        let Some(db) = self.schema_or_current(schema) else { return Ok(out) };
        if want(ObjectKind::Table) || want(ObjectKind::View) || want(ObjectKind::MaterializedView) {
            let rows = self
                .query(
                    "SELECT name, engine, total_rows, comment FROM system.tables \
                     WHERE database = ? AND NOT is_temporary ORDER BY name",
                    &[Value::from(db.as_str())],
                )
                .await?;
            for r in rows {
                let name = c_string(&r, 0);
                // Inner tables of materialized views.
                if !self.cfg.show_system_objects && name.starts_with(".inner") {
                    continue;
                }
                let kind = engine_kind(&c_string(&r, 1));
                if !want(kind) {
                    continue;
                }
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&db), name, kind));
                info.row_estimate = if kind == ObjectKind::Table { c_i64(&r, 2) } else { None };
                info.comment = c_nonempty(&r, 3);
                out.push(info);
            }
        }
        if want(ObjectKind::Index) {
            let rows = self
                .query(
                    "SELECT name, table FROM system.data_skipping_indices WHERE database = ? ORDER BY table, name",
                    &[Value::from(db.as_str())],
                )
                .await
                .unwrap_or_default();
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&db), c_string(&r, 0), ObjectKind::Index));
                info.parent = c_str(&r, 1);
                out.push(info);
            }
        }
        Ok(out)
    }

    async fn describe_table(&self, object: &ObjectRef) -> Result<TableInfo> {
        self.describe(object).await
    }

    async fn describe_routine(&self, object: &ObjectRef, _signature: Option<&str>) -> Result<RoutineInfo> {
        let rows = self
            .query(
                "SELECT create_query FROM system.functions WHERE name = ? AND origin = 'SQLUserDefined'",
                &[Value::from(object.name.as_str())],
            )
            .await?;
        let def = rows.first().and_then(|r| c_nonempty(r, 0)).ok_or_else(|| not_found("function", &object.name))?;
        Ok(RoutineInfo {
            object: ObjectRef::new(object.schema.as_deref(), object.name.clone(), ObjectKind::Function),
            signature: None,
            return_type: None,
            language: Some("SQL".into()),
            definition: format!("{};", def.trim_end_matches(';')),
        })
    }

    async fn object_ddl(&self, object: &ObjectRef) -> Result<String> {
        let db = self.schema_or_current(object.schema.as_deref());
        let sql = match object.kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                format!("SHOW CREATE TABLE {}", D.qualified(db.as_deref(), &object.name))
            }
            ObjectKind::Database | ObjectKind::Schema => {
                format!("SHOW CREATE DATABASE {}", D.quote_ident_if_needed(&object.name))
            }
            ObjectKind::Function => return Ok(self.describe_routine(object, None).await?.definition + "\n"),
            ObjectKind::Index => {
                let db = db.ok_or_else(|| Error::Other("no database selected".into()))?;
                let rows = self
                    .query(
                        "SELECT table FROM system.data_skipping_indices WHERE database = ? AND name = ?",
                        &[Value::from(db.as_str()), Value::from(object.name.as_str())],
                    )
                    .await?;
                let mut out = vec![];
                for r in rows {
                    let t = self.describe(&ObjectRef::table(Some(&db), c_string(&r, 0))).await?;
                    if let Some(i) = t.indexes.iter().find(|i| i.name == object.name) {
                        out.push(format!("{};", i.definition.clone().unwrap_or_default()));
                    }
                }
                if out.is_empty() {
                    return Err(not_found("index", &object.display_name()));
                }
                return Ok(out.join("\n") + "\n");
            }
            other => return Err(Error::Unsupported(format!("DDL for {} objects", other.label()))),
        };
        let rows = self.query(&sql, &[]).await?;
        let ddl = rows
            .first()
            .and_then(|r| c_nonempty(r, 0))
            .ok_or_else(|| Error::Other(format!("no DDL available for {}", object.display_name())))?;
        Ok(format!("{};\n", ddl.trim_end().trim_end_matches(';')))
    }

    async fn list_roles(&self) -> Result<Vec<RoleInfo>> {
        let users = self.query("SELECT name FROM system.users ORDER BY name", &[]).await?;
        let roles = self.query("SELECT name FROM system.roles ORDER BY name", &[]).await.unwrap_or_default();
        let grants = self
            .query("SELECT coalesce(user_name, role_name), granted_role_name FROM system.role_grants", &[])
            .await
            .unwrap_or_default();
        let member_of = |n: &str| -> Vec<String> {
            grants.iter().filter(|g| c_string(g, 0) == n).map(|g| c_string(g, 1)).collect()
        };
        let mut out: Vec<RoleInfo> = users
            .iter()
            .map(|r| {
                let name = c_string(r, 0);
                RoleInfo { member_of: member_of(&name), name, superuser: false, can_login: true }
            })
            .collect();
        out.extend(roles.iter().map(|r| {
            let name = c_string(r, 0);
            RoleInfo { member_of: member_of(&name), name, superuser: false, can_login: false }
        }));
        Ok(out)
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let rows = self
            .query(
                "SELECT query_id, user, current_database, toString(address), is_cancelled, query, elapsed \
                 FROM system.processes ORDER BY elapsed DESC",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| SessionInfo {
                id: c_string(r, 0),
                user: c_str(r, 1),
                database: c_str(r, 2),
                client: c_str(r, 3),
                state: Some(if c_bool(r, 4) { "cancelling".into() } else { "running".into() }),
                query: c_str(r, 5),
                started: match cell(r, 6) {
                    Value::Float(f) => Some(format!("{f:.1} s ago")),
                    _ => c_str(r, 6).map(|s| format!("{s} s ago")),
                },
            })
            .collect())
    }

    async fn kill_session(&self, id: &str) -> Result<()> {
        self.http.simple(&kill_sql(id, false)).await.map(|_| ())
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<StatementResult> {
        let prefix = if analyze { "EXPLAIN PIPELINE " } else { "EXPLAIN PLAN indexes = 1 " };
        let body = trim_statement(sql);
        match self.execute(&format!("{prefix}{body}"), &ExecOptions::unlimited()).await {
            Err(Error::Database(_)) if !analyze => self.execute(&format!("EXPLAIN PLAN {body}"), &ExecOptions::unlimited()).await,
            other => other,
        }
    }

    async fn close(&self) -> Result<()> {
        let mut g = self.lock.lock().await;
        *g = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_stream_is_a_query_error() {
        match interrupted_error("", "unexpected EOF during chunk size line") {
            Error::Database(d) => assert!(d.message.contains("aborted by the server"), "{d}"),
            other => panic!("{other:?}"),
        }
        match interrupted_error("Code: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)", "eof") {
            Error::Database(d) => assert!(d.code.as_deref().unwrap().starts_with("395"), "{d}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn types() {
        assert_eq!(parse_type("Nullable(Int32)"), ChType::Int);
        assert_eq!(parse_type("LowCardinality(Nullable(String))"), ChType::Text);
        assert_eq!(parse_type("UInt64"), ChType::UInt);
        assert_eq!(parse_type("Int128"), ChType::BigInt);
        assert_eq!(parse_type("Decimal(18, 3)"), ChType::Decimal);
        assert_eq!(parse_type("DateTime64(3, 'Europe/Berlin')"), ChType::DateTime);
        assert_eq!(parse_type("DateTime('UTC')"), ChType::DateTime);
        assert_eq!(parse_type("Array(Array(Nullable(UInt8)))"), ChType::Array(Box::new(ChType::Array(Box::new(ChType::UInt)))));
        assert_eq!(parse_type("Map(String, UInt8)"), ChType::Json);
        assert_eq!(parse_type("Tuple(x UInt8, z String)"), ChType::Json);
        assert_eq!(parse_type("Enum8('a' = 1, 'b,c' = 2)"), ChType::Text);
        assert_eq!(parse_type("SimpleAggregateFunction(sum, UInt64)"), ChType::UInt);
        assert_eq!(parse_type("AggregateFunction(uniq, String)"), ChType::Other("AggregateFunction(uniq, String)".into()));
        assert!(is_nullable_type("LowCardinality(Nullable(String))"));
        assert!(!is_nullable_type("Array(Nullable(Int8))"));
        assert_eq!(split_args("a, b(c, d), 'e,f', `g,h`"), vec!["a", "b(c, d)", "'e,f'", "`g,h`"]);
        assert_eq!(split_args("id, toDate(ts)"), vec!["id", "toDate(ts)"]);
    }

    #[test]
    fn decoding() {
        use serde_json::json;
        let d = |t: &str, v: serde_json::Value| decode(&parse_type(t), t, &v);
        assert_eq!(d("UInt64", json!("18446744073709551615")), Value::UInt(u64::MAX));
        assert_eq!(d("Int64", json!("-2")), Value::Int(-2));
        assert_eq!(d("Int8", json!(-2)), Value::Int(-2));
        assert_eq!(d("UInt256", json!("123456789012345678901234567890")), Value::Decimal("123456789012345678901234567890".into()));
        assert_eq!(d("Decimal(18, 3)", json!("1.500")), Value::Decimal("1.500".into()));
        assert_eq!(d("Float64", json!("nan")).as_f64().map(f64::is_nan), Some(true));
        assert_eq!(d("Float32", json!(1.5)), Value::Float(1.5));
        assert_eq!(d("FixedString(3)", json!("x\u{0}\u{0}")), Value::Text("x".into()));
        assert_eq!(d("Date32", json!("2024-01-02")), Value::Date(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()));
        assert_eq!(d("DateTime64(3)", json!("2024-01-02 03:04:05.678")).to_string(), "2024-01-02 03:04:05.678");
        assert_eq!(d("DateTime", json!("2024-01-02 03:04:05")).kind_name(), "datetime");
        assert_eq!(d("Bool", json!(true)), Value::Bool(true));
        assert_eq!(d("Nullable(String)", json!(null)), Value::Null);
        assert_eq!(d("UUID", json!("6af3923e-6a11-4f6a-98a1-1414653cc1c0")).kind_name(), "uuid");
        assert_eq!(d("Array(Nullable(Int8))", json!([1, null])), Value::Array(vec![Value::Int(1), Value::Null]));
        assert_eq!(d("Map(String, UInt8)", json!({"k": 1})), Value::Json(json!({"k": 1})));
        assert_eq!(d("IPv4", json!("1.2.3.4")), Value::Text("1.2.3.4".into()));
        assert_eq!(
            d("AggregateFunction(uniq, String)", json!("\u{1}")),
            Value::Other { type_name: "AggregateFunction(uniq, String)".into(), text: "\u{1}".into() }
        );
    }

    #[test]
    fn errors() {
        let e = parse_error("Code: 60. DB::Exception: Table default.nope does not exist. (UNKNOWN_TABLE) (version 24.3.1.1 (official build))\n");
        match e {
            Error::Database(d) => {
                assert_eq!(d.code.as_deref(), Some("60 (UNKNOWN_TABLE)"));
                assert_eq!(d.message, "Table default.nope does not exist.");
            }
            other => panic!("{other:?}"),
        }
        match parse_error("Code: 62. DB::Exception: Syntax error: failed at position 1 (SELEC): SELEC 1. Expected one of: Query. (SYNTAX_ERROR) (version 25.8.2.29 (official build))") {
            Error::Database(d) => {
                assert_eq!(d.position, Some(1));
                assert!(d.message.starts_with("Syntax error"));
                assert_eq!(error_number(&Error::Database(d)), Some("62"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_error("Code: 394. DB::Exception: Query was cancelled. (QUERY_WAS_CANCELLED)"), Error::Cancelled));
        match parse_error("plain failure") {
            Error::Database(d) => assert_eq!((d.code, d.message.as_str()), (None, "plain failure")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn row_parser_streams_and_truncates() {
        let mut p = RowParser::new(Some(2));
        p.feed(b"[\"a\", \"b\"]\n[\"UInt64\", \"Nullable(String)\"]\n[\"1\", \"x\"]\n[\"2\", nu").unwrap();
        p.feed(b"ll]\n[\"3\", \"z\"]\n").unwrap();
        p.finish().unwrap();
        assert!(p.truncated);
        assert_eq!(p.rows, vec![vec![Value::UInt(1), Value::Text("x".into())], vec![Value::UInt(2), Value::Null]]);
        let cols = p.columns();
        assert_eq!(cols[1].nullable, Some(true));
        assert_eq!(cols[0].type_name, "UInt64");

        let mut p = RowParser::new(None);
        p.feed(b"[\"x\"]\n[\"UInt8\"]\n[1]\nCode: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)\n").unwrap_err();
        let mut p = RowParser::new(Some(1));
        p.feed(b"[\"x\"]\n[\"UInt8\"]\n[1]\n[2]\n__exception__\r\nCode: 395. DB::Exception: boom. (X) (version 25.8)\n").unwrap();
        match p.finish().unwrap_err() {
            Error::Database(d) => assert_eq!(d.code.as_deref(), Some("395 (X)")),
            other => panic!("{other:?}"),
        }
        let mut p = RowParser::new(None);
        p.feed(b"[\"x\"]\n[\"String\"]\n[\"\xff\"]\n").unwrap();
        assert_eq!(p.rows.len(), 1);
    }

    #[test]
    fn params_and_literals() {
        assert_eq!(literal(&Value::from("it's \\ ok")), "'it\\'s \\\\ ok'");
        assert_eq!(literal(&Value::Bytes(vec![0, 255])), "'\\x00\\xFF'");
        assert_eq!(literal(&Value::Float(2.0)), "2.0");
        assert_eq!(literal(&Value::Array(vec![Value::Int(1), Value::Null])), "[1, NULL]");
        assert_eq!(
            bind_params("SELECT ?, '?', `?` -- ?\n, ?", &[Value::Int(1), Value::Bool(true)]).unwrap(),
            "SELECT 1, '?', `?` -- ?\n, true"
        );
        assert!(bind_params("SELECT ?", &[Value::Int(1), Value::Int(2)]).is_err());
        assert!(bind_params("SELECT ?, ?", &[Value::Int(1)]).is_err());
        assert!(returns_rows("  /* x */ select 1"));
        assert!(returns_rows("EXISTS TABLE t"));
        assert!(!returns_rows("INSERT INTO t VALUES (1)"));
        assert_eq!(use_target("USE `my db`;").as_deref(), Some("my db"));
        assert_eq!(use_target("use system").as_deref(), Some("system"));
        assert_eq!(use_target("select 1"), None);
        assert_eq!(engine_kind("MaterializedView"), ObjectKind::MaterializedView);
        assert_eq!(engine_kind("MergeTree"), ObjectKind::Table);
    }

    #[test]
    fn targets() {
        let mut cfg = ConnectionConfig::new(DriverKind::ClickHouse);
        let t = target(&cfg).unwrap();
        assert!(!t.https);
        assert_eq!((t.host.as_str(), t.port, t.user.as_str()), ("localhost", 8123, "default"));
        cfg.ssl.mode = SslMode::Require;
        assert!(target(&cfg).unwrap().https);
        cfg.ssl.mode = SslMode::Prefer;
        cfg.port = 8443;
        assert!(target(&cfg).unwrap().https);
        cfg.url = Some("clickhouses://bob:pw@ch.example.com/logs?max_threads=2".into());
        let t = target(&cfg).unwrap();
        assert!(t.https);
        assert_eq!((t.host.as_str(), t.port, t.database.as_str()), ("ch.example.com", 8443, "logs"));
        assert_eq!(t.password.as_deref(), Some("pw"));
        assert!(t.options.contains(&("max_threads".to_string(), "2".to_string())));
        cfg.url = Some("https://ch.example.com/proxy/?database=db1".into());
        let t = target(&cfg).unwrap();
        assert!(t.https);
        assert_eq!((t.port, t.path.as_str(), t.database.as_str()), (443, "/proxy/", "db1"));
    }
}
