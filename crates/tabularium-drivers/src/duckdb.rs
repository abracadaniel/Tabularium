//! DuckDB driver (duckdb-rs with the bundled engine; opt-in feature `duckdb`).
//!
//! DuckDB's API is synchronous: every call runs on `tokio::task::spawn_blocking` with the
//! connection behind an `Arc<std::sync::Mutex<..>>`. Row-returning statements are executed
//! in streaming mode and converted from Arrow chunks, so a row limit never materializes a
//! whole table; other statements are executed normally to obtain the changed-row count.
//!
//! A database file can only be opened by one DuckDB instance per process, so connections
//! to the same file share one instance (a process-wide registry) and each Tabularium
//! connection is a separate DuckDB connection (`try_clone`) with its own transactions.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock, Weak};
use std::time::Instant;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime};
use duckdb::arrow::array::{Array, ArrayRef, AsArray};
use duckdb::arrow::datatypes::{self as adt, DataType, IntervalUnit, TimeUnit as ArrowTimeUnit};
use duckdb::core::{LogicalTypeHandle, LogicalTypeId};
use duckdb::types::{TimeUnit, Value as DValue};
use duckdb::{AccessMode, Config, InterruptHandle};
use tabularium_core::driver::first_keyword;
use tabularium_core::error::DatabaseError;
use tabularium_core::model::*;
use tabularium_core::{
    CancelHandle, ColumnMeta, Connection, ConnectionConfig, Dialect, DriverKind, Error, ExecOptions, Result,
    StatementResult, TransactionMode, Value,
};

use crate::common::*;

const D: Dialect = Dialect::Postgres;

const SYSTEM_SCHEMAS: [&str; 2] = ["information_schema", "pg_catalog"];

type Shared = Arc<StdMutex<Option<duckdb::Connection>>>;

/// Root connections of open database files, so a file is opened once per process.
/// (canonical path, read-only) -> root connection of the database instance.
type Registry = HashMap<(String, bool), Weak<StdMutex<duckdb::Connection>>>;

fn registry() -> &'static StdMutex<Registry> {
    static REG: OnceLock<StdMutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| StdMutex::new(HashMap::new()))
}

pub struct DuckDbConnection {
    cfg: ConnectionConfig,
    path: String,
    conn: Shared,
    /// Keeps the database instance open while this connection lives.
    _root: Option<Arc<StdMutex<duckdb::Connection>>>,
    tx: Arc<TxState>,
    interrupt: Arc<InterruptHandle>,
    running: Arc<AtomicBool>,
    database: RwLock<Option<String>>,
}

struct DuckCancel {
    interrupt: Arc<InterruptHandle>,
    running: Arc<AtomicBool>,
}

#[async_trait]
impl CancelHandle for DuckCancel {
    async fn cancel(&self) -> Result<()> {
        // An interrupt with no running query could abort the next one.
        if self.running.load(Ordering::SeqCst) {
            self.interrupt.interrupt();
        }
        Ok(())
    }
}

/// Strip `duckdb:` URL prefixes and expand `~`.
fn normalize_path(db: &str) -> String {
    let mut p = db.trim();
    for prefix in ["duckdb://", "duckdb:", "file://", "file:"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            p = rest;
            break;
        }
    }
    let p = p.split('?').next().unwrap_or(p);
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return std::path::PathBuf::from(home).join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

fn is_memory(path: &str) -> bool {
    path.is_empty() || path == ":memory:" || path == "memory"
}

/// Map a duckdb-rs error.
fn map_err(e: duckdb::Error) -> Error {
    match e {
        duckdb::Error::DuckDBFailure(ffi_err, msg) => {
            let text = msg.unwrap_or_else(|| ffi_err.to_string());
            if text.contains("INTERRUPT") || text.contains("Interrupted") {
                return Error::Cancelled;
            }
            // "Catalog Error: Table with name x does not exist!\nDid you mean ..."
            let (code, message) = match text.split_once(" Error: ") {
                Some((kind, rest)) if !kind.contains('\n') && kind.len() < 40 => (Some(format!("{kind} Error")), rest.to_string()),
                _ => (None, text.clone()),
            };
            let (message, detail) = match message.split_once('\n') {
                Some((first, rest)) if !rest.trim().is_empty() => (first.to_string(), Some(rest.trim().to_string())),
                _ => (message, None),
            };
            Error::Database(DatabaseError { message, code, detail, ..Default::default() })
        }
        duckdb::Error::InvalidPath(p) => Error::Config(format!("invalid DuckDB path: {}", p.display())),
        other => Error::Other(other.to_string()),
    }
}

fn map_connect_err(e: duckdb::Error) -> Error {
    match map_err(e) {
        Error::Database(d) => Error::Connection(d.to_string()),
        other => other,
    }
}

// ----- parameters ----------------------------------------------------------------------------

fn to_duck(v: &Value) -> DValue {
    match v {
        Value::Null => DValue::Null,
        Value::Bool(b) => DValue::Boolean(*b),
        Value::Int(i) => DValue::BigInt(*i),
        Value::UInt(u) => DValue::UBigInt(*u),
        Value::Float(f) => DValue::Double(*f),
        Value::Decimal(s) | Value::Text(s) => DValue::Text(s.clone()),
        Value::Bytes(b) => DValue::Blob(b.clone()),
        Value::Date(d) => {
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
            DValue::Date32(d.signed_duration_since(epoch).num_days() as i32)
        }
        Value::Time(t) => {
            let micros = t.signed_duration_since(NaiveTime::MIN).num_microseconds().unwrap_or(0);
            DValue::Time64(TimeUnit::Microsecond, micros)
        }
        Value::DateTime(dt) => DValue::Timestamp(TimeUnit::Microsecond, dt.and_utc().timestamp_micros()),
        Value::DateTimeTz(dt) => DValue::Timestamp(TimeUnit::Microsecond, dt.timestamp_micros()),
        Value::Uuid(u) => DValue::Text(u.to_string()),
        Value::Json(j) => DValue::Text(j.to_string()),
        Value::Array(_) | Value::Other { .. } => DValue::Text(v.to_string()),
    }
}

// ----- types & decoding ----------------------------------------------------------------------

/// DuckDB spelling of a logical type (`DECIMAL(10,2)`, `INTEGER[]`, `STRUCT(a INTEGER)`).
fn logical_name(t: &LogicalTypeHandle) -> String {
    if let Some(alias) = t.get_alias() {
        return alias;
    }
    let Ok(id) = t.try_id() else { return "UNKNOWN".into() };
    match id {
        LogicalTypeId::Boolean => "BOOLEAN".into(),
        LogicalTypeId::Tinyint => "TINYINT".into(),
        LogicalTypeId::Smallint => "SMALLINT".into(),
        LogicalTypeId::Integer => "INTEGER".into(),
        LogicalTypeId::Bigint => "BIGINT".into(),
        LogicalTypeId::UTinyint => "UTINYINT".into(),
        LogicalTypeId::USmallint => "USMALLINT".into(),
        LogicalTypeId::UInteger => "UINTEGER".into(),
        LogicalTypeId::UBigint => "UBIGINT".into(),
        LogicalTypeId::Hugeint => "HUGEINT".into(),
        LogicalTypeId::UHugeint => "UHUGEINT".into(),
        LogicalTypeId::Float => "FLOAT".into(),
        LogicalTypeId::Double => "DOUBLE".into(),
        LogicalTypeId::Decimal => format!("DECIMAL({},{})", t.decimal_width(), t.decimal_scale()),
        LogicalTypeId::Varchar => "VARCHAR".into(),
        LogicalTypeId::Blob => "BLOB".into(),
        LogicalTypeId::Date => "DATE".into(),
        LogicalTypeId::Time => "TIME".into(),
        LogicalTypeId::TimeNs => "TIME_NS".into(),
        LogicalTypeId::TimeTZ => "TIME WITH TIME ZONE".into(),
        LogicalTypeId::Timestamp => "TIMESTAMP".into(),
        LogicalTypeId::TimestampS => "TIMESTAMP_S".into(),
        LogicalTypeId::TimestampMs => "TIMESTAMP_MS".into(),
        LogicalTypeId::TimestampNs => "TIMESTAMP_NS".into(),
        LogicalTypeId::TimestampTZ => "TIMESTAMP WITH TIME ZONE".into(),
        LogicalTypeId::Interval => "INTERVAL".into(),
        LogicalTypeId::Uuid => "UUID".into(),
        LogicalTypeId::Enum => "ENUM".into(),
        LogicalTypeId::Bit => "BIT".into(),
        LogicalTypeId::Bignum => "BIGNUM".into(),
        LogicalTypeId::Geometry => "GEOMETRY".into(),
        LogicalTypeId::Variant => "VARIANT".into(),
        LogicalTypeId::SqlNull => "NULL".into(),
        LogicalTypeId::List | LogicalTypeId::Array => format!("{}[]", logical_name(&t.child(0))),
        LogicalTypeId::Map => format!("MAP({}, {})", logical_name(&t.child(0)), logical_name(&t.child(1))),
        LogicalTypeId::Struct | LogicalTypeId::Union => {
            let fields: Vec<String> = (0..t.num_children())
                .map(|i| format!("{} {}", D.quote_ident_if_needed(&t.child_name(i)), logical_name(&t.child(i))))
                .collect();
            format!("{}({})", if id == LogicalTypeId::Struct { "STRUCT" } else { "UNION" }, fields.join(", "))
        }
        other => format!("{other:?}").to_ascii_uppercase(),
    }
}

/// Format an unscaled decimal.
pub(crate) fn fmt_decimal(v: i128, scale: i8) -> String {
    if scale <= 0 {
        let mut s = v.to_string();
        s.extend(std::iter::repeat_n('0', (-scale) as usize));
        return s;
    }
    let scale = scale as usize;
    let neg = v < 0;
    let digits = v.unsigned_abs().to_string();
    let digits = if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale - digits.len() + 1)) } else { digits };
    let (int, frac) = digits.split_at(digits.len() - scale);
    format!("{}{int}.{frac}", if neg { "-" } else { "" })
}

/// DuckDB-style interval text: `1 year 2 months 3 days 04:05:06.5`.
pub(crate) fn fmt_interval(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = vec![];
    let (y, m) = (months / 12, months % 12);
    let plural = |n: i64, unit: &str| format!("{n} {unit}{}", if n.abs() == 1 { "" } else { "s" });
    if y != 0 {
        parts.push(plural(y as i64, "year"));
    }
    if m != 0 {
        parts.push(plural(m as i64, "month"));
    }
    if days != 0 {
        parts.push(plural(days as i64, "day"));
    }
    if nanos != 0 || parts.is_empty() {
        let neg = nanos < 0;
        let n = nanos.unsigned_abs();
        let secs = n / 1_000_000_000;
        let frac = n % 1_000_000_000;
        let mut t = format!("{}{:02}:{:02}:{:02}", if neg { "-" } else { "" }, secs / 3600, (secs / 60) % 60, secs % 60);
        if frac != 0 {
            let f = format!("{frac:09}");
            t.push('.');
            t.push_str(f.trim_end_matches('0'));
        }
        parts.push(t);
    }
    parts.join(" ")
}

fn display(arr: &dyn Array, row: usize) -> String {
    duckdb::arrow::util::display::array_value_to_string(arr, row).unwrap_or_else(|e| format!("<error: {e}>"))
}

fn ts_value(v: i64, unit: &ArrowTimeUnit, tz: bool, type_name: &str) -> Value {
    let (secs, nanos) = match unit {
        ArrowTimeUnit::Second => (v, 0),
        ArrowTimeUnit::Millisecond => (v.div_euclid(1000), (v.rem_euclid(1000) * 1_000_000) as u32),
        ArrowTimeUnit::Microsecond => (v.div_euclid(1_000_000), (v.rem_euclid(1_000_000) * 1000) as u32),
        ArrowTimeUnit::Nanosecond => (v.div_euclid(1_000_000_000), v.rem_euclid(1_000_000_000) as u32),
    };
    match DateTime::from_timestamp(secs, nanos) {
        Some(dt) if tz => Value::DateTimeTz(dt.fixed_offset()),
        Some(dt) => Value::DateTime(dt.naive_utc()),
        // ±infinity and out-of-range values
        None => Value::Other { type_name: type_name.into(), text: if v > 0 { "infinity".into() } else { "-infinity".into() } },
    }
}

fn time_value(nanos_since_midnight: i64, type_name: &str) -> Value {
    let secs = nanos_since_midnight.div_euclid(1_000_000_000);
    let nanos = nanos_since_midnight.rem_euclid(1_000_000_000) as u32;
    match NaiveTime::from_num_seconds_from_midnight_opt(secs as u32, nanos) {
        Some(t) if (0..86_400).contains(&secs) => Value::Time(t),
        // 24:00:00 is a legal DuckDB TIME
        _ => Value::Other { type_name: type_name.into(), text: fmt_interval(0, 0, nanos_since_midnight) },
    }
}

/// Convert one Arrow cell into a [`Value`]. `type_name` is the DuckDB type of the column.
pub(crate) fn arrow_value(arr: &dyn Array, row: usize, type_name: &str) -> Value {
    if arr.is_null(row) {
        return Value::Null;
    }
    let dt = arr.data_type();
    match dt {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(arr.as_boolean().value(row)),
        DataType::Int8 => Value::Int(arr.as_primitive::<adt::Int8Type>().value(row).into()),
        DataType::Int16 => Value::Int(arr.as_primitive::<adt::Int16Type>().value(row).into()),
        DataType::Int32 => Value::Int(arr.as_primitive::<adt::Int32Type>().value(row).into()),
        DataType::Int64 => Value::Int(arr.as_primitive::<adt::Int64Type>().value(row)),
        DataType::UInt8 => Value::UInt(arr.as_primitive::<adt::UInt8Type>().value(row).into()),
        DataType::UInt16 => Value::UInt(arr.as_primitive::<adt::UInt16Type>().value(row).into()),
        DataType::UInt32 => Value::UInt(arr.as_primitive::<adt::UInt32Type>().value(row).into()),
        DataType::UInt64 => Value::UInt(arr.as_primitive::<adt::UInt64Type>().value(row)),
        DataType::Float16 => Value::Float(f64::from(arr.as_primitive::<adt::Float16Type>().value(row).to_f32())),
        DataType::Float32 => Value::Float(f32_to_f64(arr.as_primitive::<adt::Float32Type>().value(row))),
        DataType::Float64 => Value::Float(arr.as_primitive::<adt::Float64Type>().value(row)),
        DataType::Decimal128(_, s) => {
            let v = arr.as_primitive::<adt::Decimal128Type>().value(row);
            // HUGEINT / UHUGEINT arrive as DECIMAL(38,0).
            if type_name == "UHUGEINT" {
                Value::Decimal((v as u128).to_string())
            } else {
                Value::Decimal(fmt_decimal(v, *s))
            }
        }
        DataType::Utf8 => {
            let s = arr.as_string::<i32>().value(row);
            text_value(s, type_name)
        }
        DataType::LargeUtf8 => text_value(arr.as_string::<i64>().value(row), type_name),
        DataType::Utf8View => text_value(arr.as_string_view().value(row), type_name),
        DataType::Binary => Value::Bytes(arr.as_binary::<i32>().value(row).to_vec()),
        DataType::LargeBinary => Value::Bytes(arr.as_binary::<i64>().value(row).to_vec()),
        DataType::BinaryView => Value::Bytes(arr.as_binary_view().value(row).to_vec()),
        DataType::FixedSizeBinary(16) if type_name == "UUID" => {
            let b = arr.as_fixed_size_binary().value(row);
            uuid::Uuid::from_slice(b).map(Value::Uuid).unwrap_or_else(|_| Value::Bytes(b.to_vec()))
        }
        DataType::FixedSizeBinary(_) => Value::Bytes(arr.as_fixed_size_binary().value(row).to_vec()),
        DataType::Date32 => {
            let days = arr.as_primitive::<adt::Date32Type>().value(row);
            NaiveDate::from_ymd_opt(1970, 1, 1)
                .and_then(|e| e.checked_add_signed(chrono::Duration::days(days.into())))
                .map(Value::Date)
                .unwrap_or_else(|| Value::Other { type_name: type_name.into(), text: if days > 0 { "infinity" } else { "-infinity" }.into() })
        }
        DataType::Date64 => {
            let ms = arr.as_primitive::<adt::Date64Type>().value(row);
            match DateTime::from_timestamp_millis(ms) {
                Some(d) => Value::Date(d.date_naive()),
                None => Value::Other { type_name: type_name.into(), text: display(arr, row) },
            }
        }
        DataType::Time32(ArrowTimeUnit::Second) => {
            time_value(i64::from(arr.as_primitive::<adt::Time32SecondType>().value(row)) * 1_000_000_000, type_name)
        }
        DataType::Time32(_) => {
            time_value(i64::from(arr.as_primitive::<adt::Time32MillisecondType>().value(row)) * 1_000_000, type_name)
        }
        DataType::Time64(ArrowTimeUnit::Nanosecond) => {
            time_value(arr.as_primitive::<adt::Time64NanosecondType>().value(row), type_name)
        }
        DataType::Time64(_) => time_value(arr.as_primitive::<adt::Time64MicrosecondType>().value(row) * 1000, type_name),
        DataType::Timestamp(unit, tz) => {
            let v = match unit {
                ArrowTimeUnit::Second => arr.as_primitive::<adt::TimestampSecondType>().value(row),
                ArrowTimeUnit::Millisecond => arr.as_primitive::<adt::TimestampMillisecondType>().value(row),
                ArrowTimeUnit::Microsecond => arr.as_primitive::<adt::TimestampMicrosecondType>().value(row),
                ArrowTimeUnit::Nanosecond => arr.as_primitive::<adt::TimestampNanosecondType>().value(row),
            };
            ts_value(v, unit, tz.is_some(), type_name)
        }
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            let v = arr.as_primitive::<adt::IntervalMonthDayNanoType>().value(row);
            Value::Other { type_name: "INTERVAL".into(), text: fmt_interval(v.months, v.days, v.nanoseconds) }
        }
        DataType::List(_) => list_value(arr.as_list::<i32>().value(row), type_name),
        DataType::LargeList(_) => list_value(arr.as_list::<i64>().value(row), type_name),
        DataType::FixedSizeList(_, _) => list_value(arr.as_fixed_size_list().value(row), type_name),
        DataType::Struct(_) | DataType::Map(_, _) | DataType::Union(_, _) => Value::Json(arrow_json(arr, row)),
        DataType::Dictionary(_, _) => Value::Text(display(arr, row)),
        _ => Value::Other { type_name: type_name.into(), text: display(arr, row) },
    }
}

fn text_value(s: &str, type_name: &str) -> Value {
    match type_name {
        "JSON" => serde_json::from_str(s).map(Value::Json).unwrap_or_else(|_| Value::Text(s.to_string())),
        "UUID" => uuid::Uuid::parse_str(s).map(Value::Uuid).unwrap_or_else(|_| Value::Text(s.to_string())),
        "VARCHAR" | "ENUM" | "" => Value::Text(s.to_string()),
        t if t.starts_with("VARCHAR") => Value::Text(s.to_string()),
        // BIT, TIME WITH TIME ZONE, BIGNUM ... exported as strings
        t => Value::Other { type_name: t.to_string(), text: s.to_string() },
    }
}

fn list_value(child: ArrayRef, type_name: &str) -> Value {
    let inner = type_name.strip_suffix("[]").unwrap_or("");
    Value::Array((0..child.len()).map(|i| arrow_value(child.as_ref(), i, inner)).collect())
}

/// JSON rendering of nested Arrow values (STRUCT, MAP, UNION).
fn arrow_json(arr: &dyn Array, row: usize) -> serde_json::Value {
    use serde_json::Value as J;
    if arr.is_null(row) {
        return J::Null;
    }
    match arr.data_type() {
        DataType::Struct(fields) => {
            let s = arr.as_struct();
            let mut m = serde_json::Map::new();
            for (i, f) in fields.iter().enumerate() {
                m.insert(f.name().clone(), arrow_json(s.column(i).as_ref(), row));
            }
            J::Object(m)
        }
        DataType::Map(_, _) => {
            let entries = arr.as_map().value(row);
            let keys = entries.column(0);
            let vals = entries.column(1);
            let string_keys = matches!(keys.data_type(), DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View);
            if string_keys {
                let mut m = serde_json::Map::new();
                for i in 0..entries.len() {
                    m.insert(display(keys.as_ref(), i), arrow_json(vals.as_ref(), i));
                }
                J::Object(m)
            } else {
                J::Array(
                    (0..entries.len())
                        .map(|i| J::Array(vec![arrow_json(keys.as_ref(), i), arrow_json(vals.as_ref(), i)]))
                        .collect(),
                )
            }
        }
        DataType::Union(_, _) => {
            let u = arr.as_union();
            let tid = u.type_id(row);
            arrow_json(u.child(tid).as_ref(), u.value_offset(row))
        }
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(_, _) => {
            let child = match arr.data_type() {
                DataType::List(_) => arr.as_list::<i32>().value(row),
                DataType::LargeList(_) => arr.as_list::<i64>().value(row),
                _ => arr.as_fixed_size_list().value(row),
            };
            J::Array((0..child.len()).map(|i| arrow_json(child.as_ref(), i)).collect())
        }
        _ => arrow_value(arr, row, "").to_json(),
    }
}

// ----- blocking execution --------------------------------------------------------------------

/// Statements that are executed in streaming mode (pure queries).
fn is_streamable(sql: &str) -> bool {
    let kw = first_keyword(sql).to_ascii_uppercase();
    match kw.as_str() {
        "SELECT" | "VALUES" | "TABLE" | "FROM" | "SHOW" | "DESCRIBE" | "DESC" | "SUMMARIZE" | "PIVOT" | "UNPIVOT" => true,
        "WITH" => is_plain_select(sql),
        _ => false,
    }
}

fn is_dml(sql: &str) -> bool {
    matches!(first_keyword(sql).to_ascii_uppercase().as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE")
        || (first_keyword(sql).eq_ignore_ascii_case("WITH") && !is_plain_select(sql))
}

fn columns_of(stmt: &duckdb::Statement<'_>) -> Vec<ColumnMeta> {
    let n = stmt.column_count();
    (0..n)
        .map(|i| {
            let name = stmt.column_name(i).map(|s| s.to_string()).unwrap_or_else(|_| format!("column{i}"));
            ColumnMeta::new(name, logical_name(&stmt.column_logical_type(i)))
        })
        .collect()
}

/// Collect rows from an executed statement (materialized or streaming).
fn collect_rows(stmt: &duckdb::Statement<'_>, columns: &[ColumnMeta], max_rows: Option<usize>) -> Result<(Vec<Vec<Value>>, bool)> {
    let mut rows = Vec::new();
    let mut truncated = false;
    'outer: while let Some(chunk) = stmt.step().map_err(map_err)? {
        let cols = chunk.columns();
        for r in 0..chunk.len() {
            if max_rows.is_some_and(|m| rows.len() >= m) {
                truncated = true;
                break 'outer;
            }
            rows.push(
                cols.iter()
                    .enumerate()
                    .map(|(i, c)| arrow_value(c.as_ref(), r, columns.get(i).map(|c| c.type_name.as_str()).unwrap_or("")))
                    .collect(),
            );
        }
    }
    Ok((rows, truncated))
}

fn run_blocking(conn: &duckdb::Connection, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
    let start = Instant::now();
    let body = trim_statement(sql);
    let params: Vec<DValue> = opts.params.iter().map(to_duck).collect();
    let mut stmt = conn.prepare(body).map_err(map_err)?;
    if is_streamable(body) {
        drop(stmt.stream_arrow(duckdb::params_from_iter(params.iter())).map_err(map_err)?);
        let columns = columns_of(&stmt);
        let (rows, truncated) = collect_rows(&stmt, &columns, opts.max_rows)?;
        return Ok(StatementResult {
            sql: sql.to_string(),
            columns,
            rows,
            rows_affected: None,
            truncated,
            elapsed: start.elapsed(),
            notices: vec![],
        });
    }
    let changed = stmt.execute(duckdb::params_from_iter(params.iter())).map_err(map_err)?;
    let returning = contains_word(body, "RETURNING");
    let mut columns = columns_of(&stmt);
    // DML without RETURNING reports a single "Count" column; DDL a "Success" column.
    let status_only = (is_dml(body) && !returning)
        || columns.is_empty()
        || (!returning && columns.len() == 1 && matches!(columns[0].name.as_str(), "Success" | "Count"));
    if status_only {
        columns.clear();
        return Ok(StatementResult {
            sql: sql.to_string(),
            rows_affected: Some(changed as u64),
            elapsed: start.elapsed(),
            ..Default::default()
        });
    }
    let (rows, truncated) = collect_rows(&stmt, &columns, opts.max_rows)?;
    Ok(StatementResult {
        sql: sql.to_string(),
        columns,
        rows,
        rows_affected: None,
        truncated,
        elapsed: start.elapsed(),
        notices: vec![],
    })
}

impl DuckDbConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let path = normalize_path(&cfg.database);
        let memory = is_memory(&path);
        let read_only = cfg.read_only && !memory;
        let options: Vec<(String, String)> = cfg.options.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let open_path = path.clone();
        let opened = tokio::task::spawn_blocking(move || -> Result<(duckdb::Connection, Option<Arc<StdMutex<duckdb::Connection>>>)> {
            let config = || -> Result<Config> {
                let mut c = Config::default();
                if read_only {
                    c = c.access_mode(AccessMode::ReadOnly).map_err(map_connect_err)?;
                }
                for (k, v) in &options {
                    c = c.with(k, v).map_err(map_connect_err)?;
                }
                Ok(c)
            };
            if memory {
                return Ok((duckdb::Connection::open_in_memory_with_flags(config()?).map_err(map_connect_err)?, None));
            }
            let key = (
                std::fs::canonicalize(&open_path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| open_path.clone()),
                read_only,
            );
            let mut reg = registry().lock().map_err(|_| Error::other("registry lock poisoned"))?;
            reg.retain(|_, w| w.strong_count() > 0);
            if let Some(root) = reg.get(&key).and_then(Weak::upgrade) {
                let c = root.lock().map_err(|_| Error::other("lock poisoned"))?.try_clone().map_err(map_connect_err)?;
                return Ok((c, Some(root)));
            }
            let root = duckdb::Connection::open_with_flags(&open_path, config()?).map_err(map_connect_err)?;
            let c = root.try_clone().map_err(map_connect_err)?;
            let root = Arc::new(StdMutex::new(root));
            // The canonical path exists now that the file was created.
            let key = (
                std::fs::canonicalize(&open_path).map(|p| p.to_string_lossy().into_owned()).unwrap_or(key.0),
                read_only,
            );
            reg.insert(key, Arc::downgrade(&root));
            Ok((c, Some(root)))
        });
        let (conn, root) = with_connect_timeout(cfg, async { opened.await.map_err(|e| Error::other(format!("DuckDB open task failed: {e}")))? }).await?;
        let interrupt = conn.interrupt_handle();
        let mode = if cfg.auto_commit { TransactionMode::Auto } else { TransactionMode::Manual };
        let me = Self {
            cfg: cfg.clone(),
            path: if memory { ":memory:".into() } else { path },
            conn: Arc::new(StdMutex::new(Some(conn))),
            _root: root,
            tx: Arc::new(TxState::new(mode)),
            interrupt,
            running: Arc::new(AtomicBool::new(false)),
            database: RwLock::new(None),
        };
        for stmt in init_statements(cfg) {
            me.blocking(move |c| c.execute_batch(&stmt).map_err(map_err)).await?;
        }
        me.refresh_database().await;
        Ok(me)
    }

    /// The database file path (`:memory:` for in-memory databases).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Run `f` with the connection on a blocking thread.
    async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&duckdb::Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let running = self.running.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| Error::other("DuckDB connection lock poisoned"))?;
            let c = guard.as_ref().ok_or_else(closed)?;
            running.store(true, Ordering::SeqCst);
            let r = f(c);
            running.store(false, Ordering::SeqCst);
            r
        })
        .await
        .map_err(|e| Error::other(format!("DuckDB task failed: {e}")))?
    }

    /// Internal query (no transaction bookkeeping) returning rows.
    async fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        let sql = sql.to_string();
        let opts = ExecOptions { max_rows: None, params: params.to_vec() };
        Ok(self.blocking(move |c| run_blocking(c, &sql, &opts)).await?.rows)
    }

    async fn refresh_database(&self) {
        if let Ok(rows) = self.query("SELECT current_database()", &[]).await {
            if let Ok(mut g) = self.database.write() {
                *g = rows.first().and_then(|r| c_str(r, 0));
            }
        }
    }

    async fn schema_or_current(&self, schema: Option<&str>) -> Result<String> {
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => Ok(s.to_string()),
            None => Ok(self.current_schema().await?.unwrap_or_else(|| "main".into())),
        }
    }

    fn db_param(&self) -> Value {
        Value::from(self.current_database().unwrap_or_else(|| "memory".into()))
    }

    async fn describe(&self, object: &ObjectRef) -> Result<TableInfo> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let name = object.name.as_str();
        let db = self.db_param();
        let p = [db.clone(), Value::from(schema.as_str()), Value::from(name)];
        let t = self
            .query(
                "SELECT 'table', comment, estimated_size, NULL FROM duckdb_tables() \
                 WHERE database_name = ? AND schema_name = ? AND table_name = ? \
                 UNION ALL SELECT 'view', comment, NULL, sql FROM duckdb_views() \
                 WHERE database_name = ? AND schema_name = ? AND view_name = ?",
                &[p.clone(), p.clone()].concat(),
            )
            .await?;
        let t = t.first().ok_or_else(|| not_found("table", &format!("{schema}.{name}")))?;
        let kind = if c_string(t, 0) == "view" { ObjectKind::View } else { ObjectKind::Table };
        let mut info = TableInfo::new(ObjectRef::new(Some(&schema), name, kind));
        info.comment = c_nonempty(t, 1);
        info.row_estimate = c_i64(t, 2);
        if kind == ObjectKind::View {
            info.view_definition = c_str(t, 3).as_deref().and_then(view_body);
        }
        let cols = self
            .query(
                "SELECT column_name, data_type, is_nullable, column_default, comment, column_index \
                 FROM duckdb_columns() WHERE database_name = ? AND schema_name = ? AND table_name = ? \
                 ORDER BY column_index",
                &p,
            )
            .await?;
        for r in &cols {
            let default = c_nonempty(r, 3);
            info.columns.push(ColumnInfo {
                name: c_string(r, 0),
                data_type: c_string(r, 1),
                nullable: c_bool(r, 2),
                auto_increment: default.as_deref().is_some_and(|d| d.to_ascii_lowercase().starts_with("nextval(")),
                default,
                generated: None,
                comment: c_nonempty(r, 4),
                collation: None,
                ordinal: c_i64(r, 5).unwrap_or(0) as u32,
            });
        }
        if kind != ObjectKind::Table {
            return Ok(info);
        }
        let cons = self
            .query(
                "SELECT constraint_type, constraint_name, constraint_column_names, expression, \
                        referenced_table, referenced_column_names \
                 FROM duckdb_constraints() WHERE database_name = ? AND schema_name = ? AND table_name = ? \
                 ORDER BY constraint_index",
                &p,
            )
            .await?;
        for r in &cons {
            let name = c_nonempty(r, 1);
            let columns = c_list(r, 2);
            match c_string(r, 0).as_str() {
                "PRIMARY KEY" => info.primary_key = Some(KeyInfo { name, columns }),
                "UNIQUE" => info.unique_keys.push(KeyInfo { name, columns }),
                "CHECK" => info.checks.push(CheckInfo { name, expression: c_string(r, 3) }),
                "FOREIGN KEY" => info.foreign_keys.push(ForeignKeyInfo {
                    name,
                    columns,
                    ref_schema: Some(schema.clone()),
                    ref_table: c_string(r, 4),
                    ref_columns: c_list(r, 5),
                    on_update: FkAction::NoAction,
                    on_delete: FkAction::NoAction,
                }),
                _ => {}
            }
        }
        let idx = self
            .query(
                "SELECT index_name, is_unique, is_primary, expressions, sql FROM duckdb_indexes() \
                 WHERE database_name = ? AND schema_name = ? AND table_name = ? ORDER BY index_name",
                &p,
            )
            .await
            .unwrap_or_default();
        for r in idx {
            let columns = {
                let raw = c_list(&r, 3);
                raw.into_iter().map(|c| c.trim_matches(['[', ']', '\'']).to_string()).collect()
            };
            info.indexes.push(IndexInfo {
                name: c_string(&r, 0),
                columns,
                unique: c_bool(&r, 1),
                primary: c_bool(&r, 2),
                method: Some("art".into()),
                predicate: None,
                definition: c_nonempty(&r, 4).map(|s| s.trim_end_matches(';').to_string()),
            });
        }
        Ok(info)
    }
}

/// SELECT body of a CREATE VIEW statement.
fn view_body(create: &str) -> Option<String> {
    let stripped = strip_literals_and_comments(create).to_ascii_uppercase();
    if stripped.len() != create.len() {
        return Some(create.to_string());
    }
    let mut from = 0;
    while let Some(pos) = stripped[from..].find("AS") {
        let at = from + pos;
        let end = at + 2;
        let before_ok = at == 0 || !stripped.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after_ok = end >= stripped.len() || !stripped.as_bytes()[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(create[end..].trim().trim_end_matches(';').trim().to_string());
        }
        from = end;
    }
    None
}

#[async_trait]
impl Connection for DuckDbConnection {
    fn driver(&self) -> DriverKind {
        DriverKind::DuckDb
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.query("SELECT version()", &[]).await?;
        let v = rows.first().map(|r| c_string(r, 0)).unwrap_or_default();
        Ok(format!("DuckDB {}", v.trim_start_matches('v')))
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
        check_read_only(self.cfg.read_only, sql)?;
        let ctl = tx_control(sql, false);
        let sql_owned = sql.to_string();
        let opts = opts.clone();
        let tx = self.tx.clone();
        let res = self
            .blocking(move |c| {
                if tx.needs_begin(ctl) {
                    c.execute_batch("BEGIN TRANSACTION").map_err(map_err)?;
                    tx.began();
                }
                let res = run_blocking(c, &sql_owned, &opts);
                // A failed statement leaves a DuckDB transaction open (aborted) until
                // ROLLBACK, so the bookkeeping is the same as for other drivers.
                tx.after_statement(ctl, res.is_ok());
                res
            })
            .await;
        if first_keyword(sql).eq_ignore_ascii_case("USE") && res.is_ok() {
            self.refresh_database().await;
        }
        res
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(DuckCancel { interrupt: self.interrupt.clone(), running: self.running.clone() }))
    }

    async fn set_transaction_mode(&self, mode: TransactionMode) -> Result<()> {
        if mode == TransactionMode::Auto && self.tx.is_active() {
            self.commit().await?;
        }
        self.tx.set_mode(mode);
        Ok(())
    }

    fn transaction_mode(&self) -> TransactionMode {
        self.tx.mode()
    }

    fn in_transaction(&self) -> bool {
        self.tx.in_transaction()
    }

    async fn commit(&self) -> Result<()> {
        let tx = self.tx.clone();
        self.blocking(move |c| {
            if tx.is_active() {
                let r = c.execute_batch("COMMIT").map_err(map_err);
                tx.ended();
                r?;
            }
            Ok(())
        })
        .await
    }

    async fn rollback(&self) -> Result<()> {
        let tx = self.tx.clone();
        self.blocking(move |c| {
            if tx.is_active() {
                if let Err(e) = c.execute_batch("ROLLBACK") {
                    log::debug!("rollback: {e}");
                }
                tx.ended();
            }
            Ok(())
        })
        .await
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = self
            .query("SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name", &[])
            .await?;
        Ok(rows.iter().map(|r| c_string(r, 0)).collect())
    }

    fn current_database(&self) -> Option<String> {
        self.database.read().ok().and_then(|g| g.clone())
    }

    async fn use_database(&self, database: &str) -> Result<()> {
        let sql = format!("USE {}", D.quote_ident(database));
        self.blocking(move |c| c.execute_batch(&sql).map_err(map_err)).await?;
        self.refresh_database().await;
        Ok(())
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let rows = self
            .query(
                "SELECT DISTINCT schema_name FROM duckdb_schemas() WHERE database_name = ? ORDER BY schema_name",
                &[self.db_param()],
            )
            .await?;
        let mut out: Vec<String> = rows.iter().map(|r| c_string(r, 0)).collect();
        if !self.cfg.show_system_objects {
            out.retain(|s| !SYSTEM_SCHEMAS.contains(&s.as_str()));
        }
        if !self.cfg.schema_filter.is_empty() {
            out.retain(|s| self.cfg.schema_filter.iter().any(|f| f == s));
        }
        Ok(out)
    }

    async fn current_schema(&self) -> Result<Option<String>> {
        let rows = self.query("SELECT current_schema()", &[]).await?;
        Ok(rows.first().and_then(|r| c_str(r, 0)))
    }

    async fn list_objects(&self, schema: Option<&str>, kinds: &[ObjectKind]) -> Result<Vec<ObjectInfo>> {
        let want = |k: ObjectKind| kinds.contains(&k);
        let mut out = vec![];
        if want(ObjectKind::Schema) {
            for s in self.list_schemas().await? {
                out.push(ObjectInfo::new(ObjectRef::new(None, s, ObjectKind::Schema)));
            }
        }
        let schema = self.schema_or_current(schema).await?;
        let s = schema.as_str();
        let p = [self.db_param(), Value::from(s)];
        let system = self.cfg.show_system_objects;
        if want(ObjectKind::Table) {
            let rows = self
                .query(
                    "SELECT table_name, comment, estimated_size, internal FROM duckdb_tables() \
                     WHERE database_name = ? AND schema_name = ? ORDER BY table_name",
                    &p,
                )
                .await?;
            for r in rows.iter().filter(|r| system || !c_bool(r, 3)) {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(r, 0), ObjectKind::Table));
                info.comment = c_nonempty(r, 1);
                info.row_estimate = c_i64(r, 2);
                out.push(info);
            }
        }
        if want(ObjectKind::View) {
            let rows = self
                .query(
                    "SELECT view_name, comment, internal FROM duckdb_views() \
                     WHERE database_name = ? AND schema_name = ? ORDER BY view_name",
                    &p,
                )
                .await?;
            for r in rows.iter().filter(|r| system || !c_bool(r, 2)) {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(r, 0), ObjectKind::View));
                info.comment = c_nonempty(r, 1);
                out.push(info);
            }
        }
        if want(ObjectKind::Sequence) {
            let rows = self
                .query(
                    "SELECT sequence_name, comment FROM duckdb_sequences() \
                     WHERE database_name = ? AND schema_name = ? ORDER BY sequence_name",
                    &p,
                )
                .await?;
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), ObjectKind::Sequence));
                info.comment = c_nonempty(&r, 1);
                out.push(info);
            }
        }
        if want(ObjectKind::Index) {
            let rows = self
                .query(
                    "SELECT index_name, table_name FROM duckdb_indexes() \
                     WHERE database_name = ? AND schema_name = ? ORDER BY table_name, index_name",
                    &p,
                )
                .await?;
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), ObjectKind::Index));
                info.parent = c_str(&r, 1);
                out.push(info);
            }
        }
        if want(ObjectKind::Function) {
            // User macros (scalar and table).
            let rows = self
                .query(
                    "SELECT function_name, parameters, comment FROM duckdb_functions() \
                     WHERE database_name = ? AND schema_name = ? AND NOT internal \
                       AND function_type IN ('macro', 'table_macro') ORDER BY function_name",
                    &p,
                )
                .await
                .unwrap_or_default();
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), ObjectKind::Function));
                info.signature = Some(format!("({})", c_list(&r, 1).join(", ")));
                info.comment = c_nonempty(&r, 2);
                out.push(info);
            }
        }
        if want(ObjectKind::Type) {
            let rows = self
                .query(
                    "SELECT type_name, comment FROM duckdb_types() \
                     WHERE database_name = ? AND schema_name = ? AND NOT internal ORDER BY type_name",
                    &p,
                )
                .await
                .unwrap_or_default();
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), ObjectKind::Type));
                info.comment = c_nonempty(&r, 1);
                out.push(info);
            }
        }
        Ok(out)
    }

    async fn describe_table(&self, object: &ObjectRef) -> Result<TableInfo> {
        self.describe(object).await
    }

    async fn describe_routine(&self, object: &ObjectRef, _signature: Option<&str>) -> Result<RoutineInfo> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let rows = self
            .query(
                "SELECT function_type, parameters, macro_definition FROM duckdb_functions() \
                 WHERE database_name = ? AND schema_name = ? AND function_name = ? AND NOT internal \
                   AND function_type IN ('macro', 'table_macro')",
                &[self.db_param(), Value::from(schema.as_str()), Value::from(object.name.as_str())],
            )
            .await?;
        let r = rows.first().ok_or_else(|| not_found("macro", &format!("{schema}.{}", object.name)))?;
        let sig = format!("({})", c_list(r, 1).join(", "));
        let body = c_string(r, 2);
        let table = c_string(r, 0) == "table_macro";
        let definition = format!(
            "CREATE MACRO {}{sig} AS {}{body};",
            D.qualified(Some(&schema), &object.name),
            if table { "TABLE " } else { "" }
        );
        Ok(RoutineInfo {
            object: ObjectRef::new(Some(&schema), object.name.clone(), ObjectKind::Function),
            signature: Some(sig),
            return_type: None,
            language: Some("SQL".into()),
            definition,
        })
    }

    async fn object_ddl(&self, object: &ObjectRef) -> Result<String> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let p = [self.db_param(), Value::from(schema.as_str()), Value::from(object.name.as_str())];
        let one = |rows: Vec<Vec<Value>>| rows.first().and_then(|r| c_nonempty(r, 0));
        let ddl = match object.kind {
            ObjectKind::Table => {
                let t = one(self
                    .query("SELECT sql FROM duckdb_tables() WHERE database_name = ? AND schema_name = ? AND table_name = ?", &p)
                    .await?)
                .ok_or_else(|| not_found("table", &object.display_name()))?;
                let mut out = format!("{};\n", t.trim_end().trim_end_matches(';'));
                let idx = self
                    .query(
                        "SELECT sql FROM duckdb_indexes() WHERE database_name = ? AND schema_name = ? AND table_name = ? \
                         AND sql IS NOT NULL ORDER BY index_name",
                        &p,
                    )
                    .await
                    .unwrap_or_default();
                for r in idx {
                    out.push_str(&format!("\n{};\n", c_string(&r, 0).trim_end().trim_end_matches(';')));
                }
                return Ok(out);
            }
            ObjectKind::View => one(self
                .query("SELECT sql FROM duckdb_views() WHERE database_name = ? AND schema_name = ? AND view_name = ?", &p)
                .await?),
            ObjectKind::Sequence => one(self
                .query("SELECT sql FROM duckdb_sequences() WHERE database_name = ? AND schema_name = ? AND sequence_name = ?", &p)
                .await?),
            ObjectKind::Index => one(self
                .query("SELECT sql FROM duckdb_indexes() WHERE database_name = ? AND schema_name = ? AND index_name = ?", &p)
                .await?),
            ObjectKind::Function => return Ok(self.describe_routine(object, None).await?.definition + "\n"),
            ObjectKind::Schema => Some(format!("CREATE SCHEMA {}", D.quote_ident_if_needed(&object.name))),
            other => return Err(Error::Unsupported(format!("DDL for {} objects", other.label()))),
        };
        let ddl = ddl.ok_or_else(|| not_found(object.kind.label(), &object.display_name()))?;
        Ok(format!("{};\n", ddl.trim_end().trim_end_matches(';')))
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<StatementResult> {
        let prefix = if analyze { "EXPLAIN ANALYZE " } else { "EXPLAIN " };
        self.execute(&format!("{prefix}{}", trim_statement(sql)), &ExecOptions::unlimited()).await
    }

    async fn close(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            if let Ok(mut g) = conn.lock() {
                g.take();
            }
        })
        .await
        .map_err(|e| Error::other(format!("DuckDB task failed: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(fmt_decimal(12345, 2), "123.45");
        assert_eq!(fmt_decimal(-5, 3), "-0.005");
        assert_eq!(fmt_decimal(7, 0), "7");
        assert_eq!(fmt_decimal(7, -2), "700");
        assert_eq!(fmt_interval(14, 3, 3_723_500_000_000), "1 year 2 months 3 days 01:02:03.5");
        assert_eq!(fmt_interval(0, 0, 0), "00:00:00");
        assert_eq!(fmt_interval(0, 1, 0), "1 day");
        assert_eq!(normalize_path("duckdb:///tmp/a.duckdb"), "/tmp/a.duckdb");
        assert!(is_memory(""));
        assert!(is_streamable("FROM t"));
        assert!(is_streamable("with x as (select 1) select * from x"));
        assert!(!is_streamable("insert into t select 1"));
        assert!(is_dml("DELETE FROM t"));
        assert_eq!(view_body("CREATE VIEW v AS SELECT 1;").as_deref(), Some("SELECT 1"));
    }
}
