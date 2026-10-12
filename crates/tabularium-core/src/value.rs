use std::fmt;

use base64::Engine;
use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

use crate::dialect::Dialect;

/// A single cell value, decoded from any supported database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "t", content = "v")]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    /// Arbitrary-precision numeric kept as its textual representation.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(NaiveDate),
    Time(NaiveTime),
    DateTime(NaiveDateTime),
    DateTimeTz(DateTime<FixedOffset>),
    Uuid(uuid::Uuid),
    Json(serde_json::Value),
    Array(Vec<Value>),
    /// Values of types the driver does not decode natively (geometry, ranges, intervals...),
    /// rendered as text by the server or driver.
    Other { type_name: String, text: String },
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::UInt(_) | Value::Float(_) | Value::Decimal(_))
    }

    /// Numeric view for aggregation (sum/avg/min/max on selections).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Float(f) => Some(*f),
            Value::Decimal(d) => d.parse().ok(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Value::Text(t) => t.trim().parse().ok(),
            _ => None,
        }
    }

    /// Short name of the variant, useful for diagnostics.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::UInt(_) => "uint",
            Value::Float(_) => "float",
            Value::Decimal(_) => "decimal",
            Value::Text(_) => "text",
            Value::Bytes(_) => "bytes",
            Value::Date(_) => "date",
            Value::Time(_) => "time",
            Value::DateTime(_) => "datetime",
            Value::DateTimeTz(_) => "datetimetz",
            Value::Uuid(_) => "uuid",
            Value::Json(_) => "json",
            Value::Array(_) => "array",
            Value::Other { .. } => "other",
        }
    }

    /// Text shown in a grid cell. `None` for NULL so callers can render it specially.
    pub fn display_text(&self) -> Option<String> {
        if self.is_null() {
            None
        } else {
            Some(self.to_string())
        }
    }

    /// Text used when editing a value: same as display, but NULL becomes empty.
    pub fn edit_text(&self) -> String {
        self.display_text().unwrap_or_default()
    }

    /// Render as a SQL literal suitable for the given dialect.
    pub fn to_sql_literal(&self, dialect: Dialect) -> String {
        match self {
            Value::Null => "NULL".into(),
            Value::Bool(b) => dialect.bool_literal(*b).into(),
            Value::Int(i) => i.to_string(),
            Value::UInt(u) => u.to_string(),
            Value::Float(f) => {
                if f.is_finite() {
                    format_float(*f)
                } else {
                    dialect.quote_string(&f.to_string())
                }
            }
            Value::Decimal(d) => d.clone(),
            Value::Text(s) => dialect.quote_string(s),
            Value::Bytes(b) => dialect.quote_bytes(b),
            Value::Date(_) | Value::Time(_) | Value::DateTime(_) | Value::DateTimeTz(_) => {
                dialect.quote_string(&self.to_string())
            }
            Value::Uuid(u) => dialect.quote_string(&u.to_string()),
            Value::Json(j) => dialect.quote_string(&j.to_string()),
            Value::Array(items) => match dialect {
                Dialect::Postgres => {
                    let inner: Vec<String> =
                        items.iter().map(|v| v.to_sql_literal(dialect)).collect();
                    format!("ARRAY[{}]", inner.join(", "))
                }
                _ => dialect.quote_string(&self.to_string()),
            },
            Value::Other { text, .. } => dialect.quote_string(text),
        }
    }

    /// JSON representation used by exporters.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Int(i) => J::from(*i),
            Value::UInt(u) => J::from(*u),
            Value::Float(f) => serde_json::Number::from_f64(*f).map(J::Number).unwrap_or(J::Null),
            Value::Decimal(d) => {
                d.parse::<serde_json::Number>().map(J::Number).unwrap_or_else(|_| J::String(d.clone()))
            }
            Value::Json(j) => j.clone(),
            Value::Array(items) => J::Array(items.iter().map(Value::to_json).collect()),
            Value::Bytes(b) => J::String(base64::engine::general_purpose::STANDARD.encode(b)),
            other => J::String(other.to_string()),
        }
    }

    /// Parse user-entered text into a value, guided by the column's database type name.
    /// Empty input is *not* treated as NULL here; the UI decides that explicitly.
    pub fn parse_for_type(text: &str, type_name: &str) -> Value {
        let t = type_name.to_ascii_lowercase();
        let trimmed = text.trim();
        let base = t.split(['(', '[']).next().unwrap_or("").trim();
        let is = |names: &[&str]| names.iter().any(|n| base == *n || base.ends_with(n));
        if t.ends_with("[]") || t.starts_with('_') {
            return Value::Text(text.to_string());
        }
        if is(&["bool", "boolean", "bit"]) {
            match trimmed.to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" | "y" | "on" => return Value::Bool(true),
                "false" | "f" | "0" | "no" | "n" | "off" => return Value::Bool(false),
                _ => {}
            }
        }
        if is(&[
            "int", "integer", "int2", "int4", "int8", "smallint", "bigint", "tinyint", "mediumint",
            "serial", "bigserial", "smallserial",
        ]) && !base.contains("interval")
        {
            if let Ok(i) = trimmed.parse::<i64>() {
                return Value::Int(i);
            }
            if let Ok(u) = trimmed.parse::<u64>() {
                return Value::UInt(u);
            }
        }
        if is(&["float", "float4", "float8", "double", "double precision", "real"]) {
            if let Ok(f) = trimmed.parse::<f64>() {
                return Value::Float(f);
            }
        }
        if is(&["numeric", "decimal", "money", "number"]) && trimmed.parse::<f64>().is_ok() {
            return Value::Decimal(trimmed.to_string());
        }
        if base == "uuid" || base == "uniqueidentifier" {
            if let Ok(u) = uuid::Uuid::parse_str(trimmed) {
                return Value::Uuid(u);
            }
        }
        if base == "json" || base == "jsonb" {
            if let Ok(j) = serde_json::from_str(trimmed) {
                return Value::Json(j);
            }
        }
        if base == "bytea" || base.contains("blob") || base.contains("binary") {
            let h = trimmed.trim_start_matches("\\x").trim_start_matches("0x");
            if let Ok(b) = hex::decode(h) {
                return Value::Bytes(b);
            }
        }
        if base == "date" {
            if let Ok(d) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
                return Value::Date(d);
            }
        }
        Value::Text(text.to_string())
    }
}

fn format_float(f: f64) -> String {
    let s = f.to_string();
    if s.contains(['.', 'e', 'E']) || s.contains("inf") || s.contains("NaN") {
        s
    } else {
        format!("{s}.0")
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("<null>"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::UInt(u) => write!(f, "{u}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Decimal(d) => f.write_str(d),
            Value::Text(s) => f.write_str(s),
            Value::Bytes(b) => {
                if b.len() <= 64 {
                    write!(f, "0x{}", hex::encode_upper(b))
                } else {
                    write!(f, "0x{}… ({} bytes)", hex::encode_upper(&b[..32]), b.len())
                }
            }
            Value::Date(d) => write!(f, "{}", d.format("%Y-%m-%d")),
            Value::Time(t) => write!(f, "{}", t.format("%H:%M:%S%.f")),
            Value::DateTime(dt) => write!(f, "{}", dt.format("%Y-%m-%d %H:%M:%S%.f")),
            Value::DateTimeTz(dt) => write!(f, "{}", dt.format("%Y-%m-%d %H:%M:%S%.f %:z")),
            Value::Uuid(u) => write!(f, "{u}"),
            Value::Json(j) => write!(f, "{j}"),
            Value::Array(items) => {
                f.write_str("{")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    match v {
                        Value::Null => f.write_str("NULL")?,
                        Value::Text(s) if s.contains([',', '{', '}', ' ', '"']) || s.is_empty() => {
                            write!(f, "\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))?
                        }
                        other => write!(f, "{other}")?,
                    }
                }
                f.write_str("}")
            }
            Value::Other { text, .. } => f.write_str(text),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Text(s)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}
impl From<i32> for Value {
    fn from(i: i32) -> Self {
        Value::Int(i as i64)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Self {
        o.map(Into::into).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals() {
        assert_eq!(Value::Null.to_sql_literal(Dialect::Postgres), "NULL");
        assert_eq!(Value::from("a'b").to_sql_literal(Dialect::Postgres), "'a''b'");
        assert_eq!(Value::Bool(true).to_sql_literal(Dialect::MySql), "1");
        assert_eq!(Value::Float(2.0).to_sql_literal(Dialect::Postgres), "2.0");
        assert_eq!(Value::Bytes(vec![1, 255]).to_sql_literal(Dialect::Sqlite), "X'01ff'");
        assert_eq!(
            Value::Array(vec![Value::Int(1), Value::Int(2)]).to_sql_literal(Dialect::Postgres),
            "ARRAY[1, 2]"
        );
    }

    #[test]
    fn parse() {
        assert_eq!(Value::parse_for_type("42", "integer"), Value::Int(42));
        assert_eq!(Value::parse_for_type("t", "boolean"), Value::Bool(true));
        assert_eq!(Value::parse_for_type("1.5", "numeric(10,2)"), Value::Decimal("1.5".into()));
        assert_eq!(Value::parse_for_type("x", "varchar(10)"), Value::Text("x".into()));
        assert_eq!(Value::parse_for_type("2024-01-02", "date"),
            Value::Date(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()));
        assert_eq!(Value::parse_for_type("1 day", "interval"), Value::Text("1 day".into()));
    }

    #[test]
    fn serde_roundtrip() {
        let v = Value::Array(vec![Value::Int(1), Value::Null, Value::Text("x".into())]);
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap(), v);
    }
}
