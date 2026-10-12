//! PostgreSQL value codec.
//!
//! sqlx returns rows of prepared statements in the binary wire format and rows of simple
//! queries in the text format. Rather than relying on sqlx's typed decoders (which reject
//! unknown types), values are decoded here straight from the raw bytes, so every type
//! produces *something*: builtins map to native [`Value`] variants, everything else falls
//! back to text or hex.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use sqlx::postgres::{PgTypeInfo, PgTypeKind, PgValueFormat, PgValueRef};
use sqlx::{TypeInfo, ValueRef};
use tabularium_core::{Error, Result, Value};

use crate::common::parse_float;

// ----- OIDs ------------------------------------------------------------------------------------

pub(crate) mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const CHAR: u32 = 18;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT2VECTOR: u32 = 22;
    pub const INT4: u32 = 23;
    pub const REGPROC: u32 = 24;
    pub const TEXT: u32 = 25;
    pub const OID: u32 = 26;
    pub const TID: u32 = 27;
    pub const XID: u32 = 28;
    pub const CID: u32 = 29;
    pub const OIDVECTOR: u32 = 30;
    pub const JSON: u32 = 114;
    pub const XML: u32 = 142;
    pub const PG_NODE_TREE: u32 = 194;
    pub const POINT: u32 = 600;
    pub const LSEG: u32 = 601;
    pub const PATH: u32 = 602;
    pub const BOX: u32 = 603;
    pub const POLYGON: u32 = 604;
    pub const LINE: u32 = 628;
    pub const CIDR: u32 = 650;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const CIRCLE: u32 = 718;
    pub const MACADDR8: u32 = 774;
    pub const MONEY: u32 = 790;
    pub const MACADDR: u32 = 829;
    pub const INET: u32 = 869;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const BIT: u32 = 1560;
    pub const VARBIT: u32 = 1562;
    pub const NUMERIC: u32 = 1700;
    pub const REFCURSOR: u32 = 1790;
    pub const REGPROCEDURE: u32 = 2202;
    pub const REGOPER: u32 = 2203;
    pub const REGOPERATOR: u32 = 2204;
    pub const REGCLASS: u32 = 2205;
    pub const REGTYPE: u32 = 2206;
    pub const RECORD: u32 = 2249;
    pub const CSTRING: u32 = 2275;
    pub const UUID: u32 = 2950;
    pub const PG_LSN: u32 = 3220;
    pub const TSVECTOR: u32 = 3614;
    pub const REGCONFIG: u32 = 3734;
    pub const JSONB: u32 = 3802;
    pub const JSONPATH: u32 = 4072;
    pub const REGNAMESPACE: u32 = 4089;
    pub const REGROLE: u32 = 4096;
    pub const XID8: u32 = 5069;
    pub const INT4RANGE: u32 = 3904;
    pub const NUMRANGE: u32 = 3906;
    pub const TSRANGE: u32 = 3908;
    pub const TSTZRANGE: u32 = 3910;
    pub const DATERANGE: u32 = 3912;
    pub const INT8RANGE: u32 = 3926;
}

/// Name of a builtin type, as `format_type` would print it (short form).
pub(crate) fn builtin_name(o: u32) -> Option<&'static str> {
    use oid::*;
    Some(match o {
        BOOL => "bool",
        BYTEA => "bytea",
        CHAR => "char",
        NAME => "name",
        INT8 => "int8",
        INT2 => "int2",
        INT2VECTOR => "int2vector",
        INT4 => "int4",
        REGPROC => "regproc",
        TEXT => "text",
        OID => "oid",
        TID => "tid",
        XID => "xid",
        CID => "cid",
        OIDVECTOR => "oidvector",
        JSON => "json",
        XML => "xml",
        PG_NODE_TREE => "pg_node_tree",
        POINT => "point",
        LSEG => "lseg",
        PATH => "path",
        BOX => "box",
        POLYGON => "polygon",
        LINE => "line",
        CIDR => "cidr",
        FLOAT4 => "float4",
        FLOAT8 => "float8",
        UNKNOWN => "unknown",
        CIRCLE => "circle",
        MACADDR8 => "macaddr8",
        MONEY => "money",
        MACADDR => "macaddr",
        INET => "inet",
        BPCHAR => "bpchar",
        VARCHAR => "varchar",
        DATE => "date",
        TIME => "time",
        TIMESTAMP => "timestamp",
        TIMESTAMPTZ => "timestamptz",
        INTERVAL => "interval",
        TIMETZ => "timetz",
        BIT => "bit",
        VARBIT => "varbit",
        NUMERIC => "numeric",
        REFCURSOR => "refcursor",
        REGPROCEDURE => "regprocedure",
        REGOPER => "regoper",
        REGOPERATOR => "regoperator",
        REGCLASS => "regclass",
        REGTYPE => "regtype",
        RECORD => "record",
        CSTRING => "cstring",
        UUID => "uuid",
        PG_LSN => "pg_lsn",
        TSVECTOR => "tsvector",
        3615 => "tsquery",
        REGCONFIG => "regconfig",
        JSONB => "jsonb",
        JSONPATH => "jsonpath",
        REGNAMESPACE => "regnamespace",
        REGROLE => "regrole",
        XID8 => "xid8",
        INT4RANGE => "int4range",
        NUMRANGE => "numrange",
        TSRANGE => "tsrange",
        TSTZRANGE => "tstzrange",
        DATERANGE => "daterange",
        INT8RANGE => "int8range",
        1033 => "aclitem",
        _ => return builtin_array_elem(o).and_then(builtin_name).map(array_name),
    })
}

fn array_name(elem: &'static str) -> &'static str {
    // Leak-free static names for the common array types.
    match elem {
        "bool" => "bool[]",
        "bytea" => "bytea[]",
        "char" => "char[]",
        "name" => "name[]",
        "int8" => "int8[]",
        "int2" => "int2[]",
        "int4" => "int4[]",
        "text" => "text[]",
        "oid" => "oid[]",
        "json" => "json[]",
        "xml" => "xml[]",
        "point" => "point[]",
        "float4" => "float4[]",
        "float8" => "float8[]",
        "money" => "money[]",
        "macaddr" => "macaddr[]",
        "inet" => "inet[]",
        "cidr" => "cidr[]",
        "bpchar" => "bpchar[]",
        "varchar" => "varchar[]",
        "date" => "date[]",
        "time" => "time[]",
        "timestamp" => "timestamp[]",
        "timestamptz" => "timestamptz[]",
        "interval" => "interval[]",
        "timetz" => "timetz[]",
        "bit" => "bit[]",
        "varbit" => "varbit[]",
        "numeric" => "numeric[]",
        "uuid" => "uuid[]",
        "jsonb" => "jsonb[]",
        "aclitem" => "aclitem[]",
        "regtype" => "regtype[]",
        "int4range" => "int4range[]",
        "int8range" => "int8range[]",
        "numrange" => "numrange[]",
        "daterange" => "daterange[]",
        "tsrange" => "tsrange[]",
        "tstzrange" => "tstzrange[]",
        _ => "array",
    }
}

/// Element OID of a builtin array type.
pub(crate) fn builtin_array_elem(o: u32) -> Option<u32> {
    use oid::*;
    Some(match o {
        1000 => BOOL,
        1001 => BYTEA,
        1002 => CHAR,
        1003 => NAME,
        1016 => INT8,
        1005 => INT2,
        1007 => INT4,
        1009 => TEXT,
        1028 => OID,
        199 => JSON,
        143 => XML,
        1017 => POINT,
        1021 => FLOAT4,
        1022 => FLOAT8,
        791 => MONEY,
        1040 => MACADDR,
        1041 => INET,
        651 => CIDR,
        1014 => BPCHAR,
        1015 => VARCHAR,
        1182 => DATE,
        1183 => TIME,
        1115 => TIMESTAMP,
        1185 => TIMESTAMPTZ,
        1187 => INTERVAL,
        1270 => TIMETZ,
        1561 => BIT,
        1563 => VARBIT,
        1231 => NUMERIC,
        2951 => UUID,
        3807 => JSONB,
        1034 => 1033,
        2211 => REGTYPE,
        3905 => INT4RANGE,
        3927 => INT8RANGE,
        3907 => NUMRANGE,
        3913 => DATERANGE,
        3909 => TSRANGE,
        3911 => TSTZRANGE,
        _ => return None,
    })
}

/// Element OID of a builtin range type.
fn builtin_range_elem(o: u32) -> Option<u32> {
    use oid::*;
    Some(match o {
        INT4RANGE => INT4,
        INT8RANGE => INT8,
        NUMRANGE => NUMERIC,
        DATERANGE => DATE,
        TSRANGE => TIMESTAMP,
        TSTZRANGE => TIMESTAMPTZ,
        _ => return None,
    })
}

fn is_text_like(o: u32) -> bool {
    use oid::*;
    matches!(o, TEXT | VARCHAR | BPCHAR | NAME | UNKNOWN | XML | REFCURSOR | CSTRING | PG_NODE_TREE | CHAR)
}

/// Display name for a column type.
pub(crate) fn type_display_name(ti: &PgTypeInfo) -> String {
    if let Some(o) = ti.oid() {
        if let Some(n) = builtin_name(o.0) {
            return n.to_string();
        }
    }
    let n = ti.name();
    if n == "?" || n.is_empty() {
        match ti.oid() {
            Some(o) => format!("oid:{}", o.0),
            None => "unknown".into(),
        }
    } else {
        n.to_string()
    }
}

// ----- top-level decoding ----------------------------------------------------------------------

/// Extra type information for OIDs sqlx did not resolve (simple-protocol results).
#[derive(Debug, Clone)]
pub(crate) struct ExtType {
    pub name: String,
    /// `typtype`: b base, c composite, d domain, e enum, p pseudo, r range, m multirange
    pub typtype: char,
    /// Element type for arrays (0 = not an array).
    pub elem: u32,
    /// Base type for domains.
    pub base: u32,
}

pub(crate) type ExtTypes = std::collections::HashMap<u32, ExtType>;

/// Decode one cell.
pub(crate) fn decode_value(v: PgValueRef<'_>, ext: &ExtTypes) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let ti = v.type_info().into_owned();
    let format = v.format();
    let bytes = match v.as_bytes() {
        Ok(b) => b,
        Err(_) => return Value::Null,
    };
    match format {
        PgValueFormat::Binary => decode_binary_ti(&ti, bytes, ext),
        PgValueFormat::Text => {
            let s = String::from_utf8_lossy(bytes);
            decode_text_ti(&ti, &s, ext)
        }
    }
}

fn decode_binary_ti(ti: &PgTypeInfo, bytes: &[u8], ext: &ExtTypes) -> Value {
    match ti.kind() {
        PgTypeKind::Domain(base) => decode_binary_ti(base, bytes, ext),
        PgTypeKind::Enum(_) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        PgTypeKind::Range(elem) => {
            let elem_oid = elem.oid().map(|o| o.0).unwrap_or(0);
            match decode_range(bytes, elem_oid, ext) {
                Some(text) => Value::Other { type_name: type_display_name(ti), text },
                None => fallback(&type_display_name(ti), bytes),
            }
        }
        _ => {
            let o = ti.oid().map(|o| o.0).unwrap_or(0);
            if builtin_name(o).is_some() || o == 0 {
                decode_binary(o, bytes, ext)
            } else {
                decode_unknown_binary(&type_display_name(ti), o, bytes, ext)
            }
        }
    }
}

fn decode_text_ti(ti: &PgTypeInfo, s: &str, ext: &ExtTypes) -> Value {
    match ti.kind() {
        PgTypeKind::Domain(base) => decode_text_ti(base, s, ext),
        PgTypeKind::Enum(_) => Value::Text(s.to_string()),
        PgTypeKind::Array(elem) => {
            let eo = elem.oid().map(|o| o.0).unwrap_or(0);
            decode_text_array(s, eo, ext).unwrap_or_else(|| Value::Other { type_name: type_display_name(ti), text: s.into() })
        }
        _ => decode_text(ti.oid().map(|o| o.0).unwrap_or(0), s, ext),
    }
}

/// Decode a binary value of an OID that is not builtin, using extension knowledge
/// (`ext`) and type-name heuristics.
fn decode_unknown_binary(name: &str, o: u32, bytes: &[u8], ext: &ExtTypes) -> Value {
    if let Some(e) = ext.get(&o) {
        match e.typtype {
            'e' => return Value::Text(String::from_utf8_lossy(bytes).into_owned()),
            'd' if e.base != 0 => return decode_binary(e.base, bytes, ext),
            _ if e.elem != 0 => return decode_binary_array(bytes, ext).unwrap_or_else(|| fallback(name, bytes)),
            _ => {}
        }
    }
    match name {
        "citext" | "ltree" | "lquery" => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        "hstore" => decode_hstore(bytes).map(|t| Value::Other { type_name: name.into(), text: t }).unwrap_or_else(|| fallback(name, bytes)),
        "geometry" | "geography" => Value::Other { type_name: name.into(), text: hex::encode_upper(bytes) },
        _ if name.starts_with('_') || name.ends_with("[]") => {
            decode_binary_array(bytes, ext).unwrap_or_else(|| fallback(name, bytes))
        }
        _ => fallback(name, bytes),
    }
}

/// UTF-8 text when printable, else `\x` hex.
fn fallback(type_name: &str, bytes: &[u8]) -> Value {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) if !s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) => s.to_string(),
        _ => format!("\\x{}", hex::encode(bytes)),
    };
    Value::Other { type_name: type_name.to_string(), text }
}

// ----- binary format ---------------------------------------------------------------------------

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.b.len() < n {
            return None;
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Some(h)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn i16(&mut self) -> Option<i16> {
        self.take(2).map(|b| i16::from_be_bytes([b[0], b[1]]))
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn i32(&mut self) -> Option<i32> {
        self.take(4).and_then(|b| b.try_into().ok()).map(i32::from_be_bytes)
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes)
    }
    fn i64(&mut self) -> Option<i64> {
        self.take(8).and_then(|b| b.try_into().ok()).map(i64::from_be_bytes)
    }
    fn f64(&mut self) -> Option<f64> {
        self.take(8).and_then(|b| b.try_into().ok()).map(f64::from_be_bytes)
    }
    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.b)
    }
    /// Length-prefixed sub-value (`-1` = NULL).
    fn value(&mut self) -> Option<Option<&'a [u8]>> {
        let len = self.i32()?;
        if len < 0 {
            Some(None)
        } else {
            self.take(len as usize).map(Some)
        }
    }
}

fn pg_epoch_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or_default()
}

fn pg_epoch() -> NaiveDateTime {
    pg_epoch_date().and_hms_opt(0, 0, 0).unwrap_or_default()
}

fn other(type_name: &str, text: impl Into<String>) -> Value {
    Value::Other { type_name: type_name.into(), text: text.into() }
}

/// Decode a binary value of a builtin type (or an unknown OID via heuristics).
pub(crate) fn decode_binary(o: u32, bytes: &[u8], ext: &ExtTypes) -> Value {
    decode_binary_opt(o, bytes, ext).unwrap_or_else(|| {
        let name = builtin_name(o).map(str::to_string).unwrap_or_else(|| format!("oid:{o}"));
        if let Some(e) = ext.get(&o) {
            if o != 0 && !builtin_name(o).is_some() {
                return decode_unknown_binary(&e.name, o, bytes, ext);
            }
        }
        fallback(&name, bytes)
    })
}

fn decode_binary_opt(o: u32, bytes: &[u8], ext: &ExtTypes) -> Option<Value> {
    use oid::*;
    let mut r = Reader::new(bytes);
    let v = match o {
        BOOL => Value::Bool(r.u8()? != 0),
        BYTEA => Value::Bytes(bytes.to_vec()),
        INT2 => Value::Int(r.i16()? as i64),
        INT4 => Value::Int(r.i32()? as i64),
        INT8 => Value::Int(r.i64()?),
        OID | XID | CID | REGPROC | REGPROCEDURE | REGOPER | REGOPERATOR | REGCLASS | REGTYPE | REGCONFIG
        | REGNAMESPACE | REGROLE => Value::Int(r.u32()? as i64),
        XID8 => Value::UInt(r.i64()? as u64),
        FLOAT4 => Value::Float(crate::common::f32_to_f64(f32::from_be_bytes(r.take(4)?.try_into().ok()?))),
        FLOAT8 => Value::Float(r.f64()?),
        NUMERIC => {
            let s = decode_numeric(bytes)?;
            Value::Decimal(s)
        }
        MONEY => {
            let cents = r.i64()?;
            let sign = if cents < 0 { "-" } else { "" };
            let abs = cents.unsigned_abs();
            Value::Decimal(format!("{sign}{}.{:02}", abs / 100, abs % 100))
        }
        TEXT | VARCHAR | BPCHAR | NAME | UNKNOWN | XML | REFCURSOR | CSTRING | PG_NODE_TREE => {
            Value::Text(String::from_utf8_lossy(bytes).into_owned())
        }
        CHAR => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        JSON => json_value(&String::from_utf8_lossy(bytes)),
        JSONB => {
            let ver = r.u8()?;
            if ver != 1 {
                return None;
            }
            json_value(&String::from_utf8_lossy(r.rest()))
        }
        JSONPATH => {
            let _ver = r.u8()?;
            other("jsonpath", String::from_utf8_lossy(r.rest()))
        }
        UUID => Value::Uuid(uuid::Uuid::from_slice(bytes).ok()?),
        DATE => {
            let d = r.i32()?;
            match d {
                i32::MAX => other("date", "infinity"),
                i32::MIN => other("date", "-infinity"),
                _ => match pg_epoch_date().checked_add_signed(Duration::days(d as i64)) {
                    Some(date) => Value::Date(date),
                    None => other("date", format!("{d} days")),
                },
            }
        }
        TIME => Value::Time(time_from_micros(r.i64()?)?),
        TIMETZ => {
            let us = r.i64()?;
            let zone = r.i32()?; // seconds west of UTC
            let t = time_from_micros(us)?;
            other("timetz", format!("{}{}", fmt_time(t), fmt_offset(-zone)))
        }
        TIMESTAMP => {
            let us = r.i64()?;
            match us {
                i64::MAX => other("timestamp", "infinity"),
                i64::MIN => other("timestamp", "-infinity"),
                _ => Value::DateTime(pg_epoch().checked_add_signed(Duration::microseconds(us))?),
            }
        }
        TIMESTAMPTZ => {
            let us = r.i64()?;
            match us {
                i64::MAX => other("timestamptz", "infinity"),
                i64::MIN => other("timestamptz", "-infinity"),
                _ => {
                    let naive = pg_epoch().checked_add_signed(Duration::microseconds(us))?;
                    let utc = FixedOffset::east_opt(0)?;
                    Value::DateTimeTz(DateTime::from_naive_utc_and_offset(naive, utc))
                }
            }
        }
        INTERVAL => {
            let us = r.i64()?;
            let days = r.i32()?;
            let months = r.i32()?;
            other("interval", format_interval(months, days, us))
        }
        INET | CIDR => other(if o == INET { "inet" } else { "cidr" }, decode_inet(bytes)?),
        MACADDR => {
            if bytes.len() != 6 {
                return None;
            }
            other("macaddr", bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"))
        }
        MACADDR8 => {
            if bytes.len() != 8 {
                return None;
            }
            other("macaddr8", bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"))
        }
        BIT | VARBIT => {
            let n = r.i32()?;
            let data = r.rest();
            let mut s = String::with_capacity(n.max(0) as usize);
            for i in 0..n.max(0) as usize {
                let byte = *data.get(i / 8)?;
                s.push(if byte & (0x80 >> (i % 8)) != 0 { '1' } else { '0' });
            }
            other(if o == BIT { "bit" } else { "varbit" }, s)
        }
        POINT => other("point", fmt_point(r.f64()?, r.f64()?)),
        LSEG => {
            let (a, b, c, d) = (r.f64()?, r.f64()?, r.f64()?, r.f64()?);
            other("lseg", format!("[{},{}]", fmt_point(a, b), fmt_point(c, d)))
        }
        BOX => {
            let (a, b, c, d) = (r.f64()?, r.f64()?, r.f64()?, r.f64()?);
            other("box", format!("{},{}", fmt_point(a, b), fmt_point(c, d)))
        }
        LINE => {
            let (a, b, c) = (r.f64()?, r.f64()?, r.f64()?);
            other("line", format!("{{{},{},{}}}", fmt_f(a), fmt_f(b), fmt_f(c)))
        }
        CIRCLE => {
            let (x, y, rad) = (r.f64()?, r.f64()?, r.f64()?);
            other("circle", format!("<{},{}>", fmt_point(x, y), fmt_f(rad)))
        }
        PATH => {
            let closed = r.u8()? != 0;
            let n = r.i32()?;
            let pts = read_points(&mut r, n)?;
            other("path", if closed { format!("({pts})") } else { format!("[{pts}]") })
        }
        POLYGON => {
            let n = r.i32()?;
            other("polygon", format!("({})", read_points(&mut r, n)?))
        }
        PG_LSN => {
            let v = r.i64()? as u64;
            other("pg_lsn", format!("{:X}/{:X}", v >> 32, v & 0xffff_ffff))
        }
        TID => {
            let block = r.u32()?;
            let off = r.u16()?;
            other("tid", format!("({block},{off})"))
        }
        TSVECTOR => other("tsvector", decode_tsvector(bytes)?),
        RECORD => decode_record(bytes, ext)?,
        INT2VECTOR | OIDVECTOR => {
            // binary format of these is the array format
            let arr = decode_binary_array(bytes, ext)?;
            match arr {
                Value::Array(items) => other(
                    builtin_name(o).unwrap_or("vector"),
                    items.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "),
                ),
                v => v,
            }
        }
        INT4RANGE | INT8RANGE | NUMRANGE | DATERANGE | TSRANGE | TSTZRANGE => {
            let elem = builtin_range_elem(o)?;
            other(builtin_name(o).unwrap_or("range"), decode_range(bytes, elem, ext)?)
        }
        _ if builtin_array_elem(o).is_some() => decode_binary_array(bytes, ext)?,
        0 => return None,
        _ => {
            // Unknown OID: arrays carry their element OID, so try that first.
            if let Some(e) = ext.get(&o) {
                if e.elem != 0 {
                    return decode_binary_array(bytes, ext);
                }
            }
            return None;
        }
    };
    Some(v)
}

fn json_value(s: &str) -> Value {
    match serde_json::from_str(s) {
        Ok(j) => Value::Json(j),
        Err(_) => Value::Text(s.to_string()),
    }
}

fn time_from_micros(us: i64) -> Option<NaiveTime> {
    if !(0..86_400_000_000).contains(&us) {
        return None;
    }
    NaiveTime::from_num_seconds_from_midnight_opt((us / 1_000_000) as u32, ((us % 1_000_000) * 1000) as u32)
}

fn fmt_time(t: NaiveTime) -> String {
    if t.nanosecond() == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        t.format("%H:%M:%S%.6f").to_string().trim_end_matches('0').to_string()
    }
}

/// `+HH[:MM[:SS]]` for an offset in seconds east of UTC.
fn fmt_offset(east: i32) -> String {
    let sign = if east < 0 { '-' } else { '+' };
    let a = east.unsigned_abs();
    let (h, m, s) = (a / 3600, (a % 3600) / 60, a % 60);
    match (m, s) {
        (0, 0) => format!("{sign}{h:02}"),
        (_, 0) => format!("{sign}{h:02}:{m:02}"),
        _ => format!("{sign}{h:02}:{m:02}:{s:02}"),
    }
}

/// Interval in PostgreSQL's default `postgres` output style.
pub(crate) fn format_interval(months: i32, days: i32, micros: i64) -> String {
    let mut parts: Vec<String> = vec![];
    let years = months / 12;
    let mons = months % 12;
    let plural = |n: i64, unit: &str, units: &str| format!("{n} {}", if n == 1 { unit } else { units });
    if years != 0 {
        parts.push(plural(years as i64, "year", "years"));
    }
    if mons != 0 {
        parts.push(plural(mons as i64, "mon", "mons"));
    }
    if days != 0 {
        parts.push(plural(days as i64, "day", "days"));
    }
    if micros != 0 || parts.is_empty() {
        let neg = micros < 0;
        let a = micros.unsigned_abs();
        let secs = a / 1_000_000;
        let frac = a % 1_000_000;
        let mut t = format!("{}{:02}:{:02}:{:02}", if neg { "-" } else { "" }, secs / 3600, (secs % 3600) / 60, secs % 60);
        if frac != 0 {
            let f = format!("{frac:06}");
            t.push('.');
            t.push_str(f.trim_end_matches('0'));
        }
        parts.push(t);
    }
    parts.join(" ")
}

fn fmt_f(f: f64) -> String {
    if f.is_finite() {
        format!("{f}")
    } else if f.is_nan() {
        "NaN".into()
    } else if f > 0.0 {
        "Infinity".into()
    } else {
        "-Infinity".into()
    }
}

fn fmt_point(x: f64, y: f64) -> String {
    format!("({},{})", fmt_f(x), fmt_f(y))
}

fn read_points(r: &mut Reader<'_>, n: i32) -> Option<String> {
    let mut pts = vec![];
    for _ in 0..n.max(0) {
        pts.push(fmt_point(r.f64()?, r.f64()?));
    }
    Some(pts.join(","))
}

/// Binary NUMERIC → decimal string.
pub(crate) fn decode_numeric(bytes: &[u8]) -> Option<String> {
    let mut r = Reader::new(bytes);
    let ndigits = r.i16()?;
    let weight = r.i16()? as i32;
    let sign = r.u16()?;
    let dscale = r.u16()? as usize;
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        _ => {}
    }
    let mut digits = Vec::with_capacity(ndigits.max(0) as usize);
    for _ in 0..ndigits.max(0) {
        digits.push(r.i16()?);
    }
    let digit = |i: i32| -> i16 {
        if i >= 0 {
            digits.get(i as usize).copied().unwrap_or(0)
        } else {
            0
        }
    };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            let d = digit(i);
            if i == 0 {
                let _ = write!(s, "{d}");
            } else {
                let _ = write!(s, "{d:04}");
            }
        }
    }
    if dscale > 0 {
        s.push('.');
        let mut frac = String::new();
        let mut i = weight + 1;
        while frac.len() < dscale {
            let _ = write!(frac, "{:04}", digit(i));
            i += 1;
        }
        frac.truncate(dscale);
        s.push_str(&frac);
    }
    if s == "-0" || (s.starts_with("-0") && s[1..].chars().all(|c| c == '0' || c == '.')) {
        s.remove(0);
    }
    Some(s)
}

/// Decimal string → binary NUMERIC.
pub(crate) fn encode_numeric(text: &str) -> Option<Vec<u8>> {
    let t = text.trim();
    let mut out = Vec::new();
    let special = match t {
        "NaN" | "nan" => Some(0xC000u16),
        "Infinity" | "inf" | "+Infinity" => Some(0xD000),
        "-Infinity" | "-inf" => Some(0xF000),
        _ => None,
    };
    if let Some(sign) = special {
        out.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&0i16.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        return Some(out);
    }
    let owned;
    let t = if t.contains(['e', 'E']) {
        let f: f64 = t.parse().ok()?;
        owned = format!("{f}");
        owned.as_str()
    } else {
        t
    };
    let (neg, body) = match t.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = body.split_once('.').unwrap_or((body, ""));
    if !int_part.chars().all(|c| c.is_ascii_digit()) || !frac_part.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    let dscale = frac_part.len() as u16;
    let int_part = int_part.trim_start_matches('0');
    let int_pad = (4 - int_part.len() % 4) % 4;
    let int_s = format!("{}{}", "0".repeat(int_pad), int_part);
    let frac_pad = (4 - frac_part.len() % 4) % 4;
    let frac_s = format!("{}{}", frac_part, "0".repeat(frac_pad));
    let mut groups: Vec<i16> = vec![];
    for chunk in int_s.as_bytes().chunks(4).chain(frac_s.as_bytes().chunks(4)) {
        groups.push(std::str::from_utf8(chunk).ok()?.parse().ok()?);
    }
    let mut weight = (int_s.len() / 4) as i32 - 1;
    // strip leading zero groups
    while groups.first() == Some(&0) {
        groups.remove(0);
        weight -= 1;
    }
    while groups.last() == Some(&0) {
        groups.pop();
    }
    if groups.is_empty() {
        weight = 0;
    }
    out.extend_from_slice(&(groups.len() as i16).to_be_bytes());
    out.extend_from_slice(&(weight as i16).to_be_bytes());
    let sign: u16 = if neg && !groups.is_empty() { 0x4000 } else { 0 };
    out.extend_from_slice(&sign.to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    Some(out)
}

fn decode_inet(bytes: &[u8]) -> Option<String> {
    let mut r = Reader::new(bytes);
    let family = r.u8()?;
    let bits = r.u8()?;
    let is_cidr = r.u8()? != 0;
    let nb = r.u8()? as usize;
    let addr = r.take(nb)?;
    let (s, max) = match family {
        2 => {
            let a: [u8; 4] = addr.try_into().ok()?;
            (Ipv4Addr::from(a).to_string(), 32)
        }
        3 => {
            let a: [u8; 16] = addr.try_into().ok()?;
            (Ipv6Addr::from(a).to_string(), 128)
        }
        _ => return None,
    };
    Some(if is_cidr || bits != max { format!("{s}/{bits}") } else { s })
}

fn encode_inet(text: &str, cidr: bool) -> Option<Vec<u8>> {
    let (addr, bits) = match text.trim().split_once('/') {
        Some((a, b)) => (a, Some(b.parse::<u8>().ok()?)),
        None => (text.trim(), None),
    };
    let ip: std::net::IpAddr = addr.parse().ok()?;
    let mut out = vec![];
    match ip {
        std::net::IpAddr::V4(v4) => {
            out.extend_from_slice(&[2, bits.unwrap_or(32), u8::from(cidr), 4]);
            out.extend_from_slice(&v4.octets());
        }
        std::net::IpAddr::V6(v6) => {
            out.extend_from_slice(&[3, bits.unwrap_or(128), u8::from(cidr), 16]);
            out.extend_from_slice(&v6.octets());
        }
    }
    Some(out)
}

/// Binary array → `Value::Array` (nested for multi-dimensional arrays).
pub(crate) fn decode_binary_array(bytes: &[u8], ext: &ExtTypes) -> Option<Value> {
    let mut r = Reader::new(bytes);
    let ndim = r.i32()?;
    let _has_nulls = r.i32()?;
    let elem = r.u32()?;
    if ndim <= 0 {
        return Some(Value::Array(vec![]));
    }
    let mut dims = vec![];
    for _ in 0..ndim {
        let len = r.i32()?;
        let _lbound = r.i32()?;
        dims.push(len.max(0) as usize);
    }
    let total: usize = dims.iter().product();
    let mut flat = Vec::with_capacity(total.min(1 << 16));
    for _ in 0..total {
        flat.push(match r.value()? {
            None => Value::Null,
            Some(b) => decode_binary(elem, b, ext),
        });
    }
    Some(nest(&dims, &mut flat.into_iter()))
}

fn nest(dims: &[usize], items: &mut impl Iterator<Item = Value>) -> Value {
    match dims {
        [] => Value::Null,
        [n] => Value::Array(items.take(*n).collect()),
        [n, rest @ ..] => Value::Array((0..*n).map(|_| nest(rest, items)).collect()),
    }
}

/// Quote a range/record element when needed.
fn quote_elem(s: &str) -> String {
    if s.is_empty() || s.contains([',', '(', ')', '[', ']', '"', '\\', ' ']) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn elem_text(v: &Value) -> String {
    match v {
        Value::DateTimeTz(dt) => {
            // PostgreSQL style: 2024-01-02 03:04:05+00
            let base = dt.naive_local().format("%Y-%m-%d %H:%M:%S%.f").to_string();
            format!("{base}{}", fmt_offset(dt.offset().local_minus_utc()))
        }
        other => other.to_string(),
    }
}

fn decode_range(bytes: &[u8], elem: u32, ext: &ExtTypes) -> Option<String> {
    let mut r = Reader::new(bytes);
    let flags = r.u8()?;
    const EMPTY: u8 = 0x01;
    const LB_INC: u8 = 0x02;
    const UB_INC: u8 = 0x04;
    const LB_INF: u8 = 0x08;
    const UB_INF: u8 = 0x10;
    if flags & EMPTY != 0 {
        return Some("empty".into());
    }
    let lower = if flags & LB_INF == 0 { r.value()?.map(|b| quote_elem(&elem_text(&decode_binary(elem, b, ext)))) } else { None };
    let upper = if flags & UB_INF == 0 { r.value()?.map(|b| quote_elem(&elem_text(&decode_binary(elem, b, ext)))) } else { None };
    Some(format!(
        "{}{},{}{}",
        if flags & LB_INC != 0 { '[' } else { '(' },
        lower.unwrap_or_default(),
        upper.unwrap_or_default(),
        if flags & UB_INC != 0 { ']' } else { ')' }
    ))
}

fn decode_record(bytes: &[u8], ext: &ExtTypes) -> Option<Value> {
    let mut r = Reader::new(bytes);
    let n = r.i32()?;
    let mut parts = vec![];
    for _ in 0..n.max(0) {
        let field_oid = r.u32()?;
        parts.push(match r.value()? {
            None => String::new(),
            Some(b) => quote_elem(&elem_text(&decode_binary(field_oid, b, ext))),
        });
    }
    Some(other("record", format!("({})", parts.join(","))))
}

fn decode_tsvector(bytes: &[u8]) -> Option<String> {
    let mut r = Reader::new(bytes);
    let n = r.i32()?;
    let mut out = vec![];
    for _ in 0..n.max(0) {
        let end = r.b.iter().position(|&b| b == 0)?;
        let word = String::from_utf8_lossy(r.take(end)?).into_owned();
        r.take(1)?;
        let npos = r.u16()?;
        let mut s = format!("'{}'", word.replace('\'', "''"));
        let mut ps = vec![];
        for _ in 0..npos {
            let p = r.u16()?;
            let weight = match p >> 14 {
                3 => "A",
                2 => "B",
                1 => "C",
                _ => "",
            };
            ps.push(format!("{}{weight}", p & 0x3fff));
        }
        if !ps.is_empty() {
            s.push(':');
            s.push_str(&ps.join(","));
        }
        out.push(s);
    }
    Some(out.join(" "))
}

fn decode_hstore(bytes: &[u8]) -> Option<String> {
    let mut r = Reader::new(bytes);
    let n = r.i32()?;
    let mut parts = vec![];
    for _ in 0..n.max(0) {
        let k = String::from_utf8_lossy(r.value()??).into_owned();
        let v = r.value()?.map(|b| String::from_utf8_lossy(b).into_owned());
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        parts.push(match v {
            Some(v) => format!("{}=>{}", q(&k), q(&v)),
            None => format!("{}=>NULL", q(&k)),
        });
    }
    Some(parts.join(", "))
}

// ----- text format -----------------------------------------------------------------------------

/// Decode a text-format value of the given type OID.
pub(crate) fn decode_text(o: u32, s: &str, ext: &ExtTypes) -> Value {
    use oid::*;
    let name = || {
        builtin_name(o)
            .map(str::to_string)
            .or_else(|| ext.get(&o).map(|e| e.name.clone()))
            .unwrap_or_else(|| format!("oid:{o}"))
    };
    let parsed = match o {
        BOOL => match s {
            "t" | "true" => Some(Value::Bool(true)),
            "f" | "false" => Some(Value::Bool(false)),
            _ => None,
        },
        INT2 | INT4 | INT8 | OID | XID | CID => s.parse::<i64>().ok().map(Value::Int),
        XID8 => s.parse::<u64>().ok().map(Value::UInt),
        FLOAT4 | FLOAT8 => parse_float(s).map(Value::Float),
        NUMERIC => Some(Value::Decimal(s.to_string())),
        _ if is_text_like(o) => Some(Value::Text(s.to_string())),
        JSON | JSONB => Some(json_value(s)),
        UUID => uuid::Uuid::parse_str(s).ok().map(Value::Uuid),
        BYTEA => s.strip_prefix("\\x").and_then(|h| hex::decode(h).ok()).map(Value::Bytes),
        DATE => NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(Value::Date),
        TIME => NaiveTime::parse_from_str(s, "%H:%M:%S%.f").ok().map(Value::Time),
        TIMESTAMP => NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok().map(Value::DateTime),
        TIMESTAMPTZ => parse_timestamptz(s).map(Value::DateTimeTz),
        _ => {
            let elem = builtin_array_elem(o).or_else(|| ext.get(&o).map(|e| e.elem).filter(|e| *e != 0));
            match elem {
                Some(e) => decode_text_array(s, e, ext),
                None => match ext.get(&o) {
                    Some(e) if e.typtype == 'e' => Some(Value::Text(s.to_string())),
                    Some(e) if e.typtype == 'd' && e.base != 0 => Some(decode_text(e.base, s, ext)),
                    Some(e) if e.name == "citext" => Some(Value::Text(s.to_string())),
                    _ => None,
                },
            }
        }
    };
    parsed.unwrap_or_else(|| Value::Other { type_name: name(), text: s.to_string() })
}

pub(crate) fn parse_timestamptz(s: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%#z")
        .ok()
        .or_else(|| DateTime::parse_from_rfc3339(s).ok())
        .or_else(|| {
            // "+05:30:15" style offsets with seconds are rare; drop seconds.
            let cut = s.rfind([':'])?;
            DateTime::parse_from_str(&s[..cut], "%Y-%m-%d %H:%M:%S%.f%#z").ok()
        })
}

/// Parse a text array literal (`{1,2,NULL}`, `{{a,b},{c,d}}`, `[0:1]={x,y}`).
pub(crate) fn decode_text_array(s: &str, elem: u32, ext: &ExtTypes) -> Option<Value> {
    let s = s.trim();
    let body = if s.starts_with('[') { s.split_once('=')?.1 } else { s };
    let delim = if elem == oid::BOX { ';' } else { ',' };
    let mut chars = body.chars().peekable();
    let v = parse_array_level(&mut chars, elem, ext, delim)?;
    Some(v)
}

fn parse_array_level(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    elem: u32,
    ext: &ExtTypes,
    delim: char,
) -> Option<Value> {
    if chars.next()? != '{' {
        return None;
    }
    let mut items = vec![];
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.peek()? {
            '}' => {
                chars.next();
                return Some(Value::Array(items));
            }
            '{' => items.push(parse_array_level(chars, elem, ext, delim)?),
            '"' => {
                chars.next();
                let mut cur = String::new();
                loop {
                    match chars.next()? {
                        '\\' => cur.push(chars.next()?),
                        '"' => break,
                        c => cur.push(c),
                    }
                }
                items.push(decode_text(elem, &cur, ext));
            }
            _ => {
                let mut cur = String::new();
                while let Some(&c) = chars.peek() {
                    if c == delim || c == '}' {
                        break;
                    }
                    cur.push(c);
                    chars.next();
                }
                let t = cur.trim();
                items.push(if t.eq_ignore_ascii_case("NULL") { Value::Null } else { decode_text(elem, t, ext) });
            }
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek() == Some(&delim) {
            chars.next();
        }
    }
}

// ----- parameter encoding ----------------------------------------------------------------------

/// A parameter pre-encoded in the binary format for a specific server type.
pub(crate) struct RawParam {
    pub ty: PgTypeInfo,
    pub bytes: Option<Vec<u8>>,
}

impl sqlx::Type<sqlx::Postgres> for RawParam {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_name("unknown")
    }
    fn compatible(_ty: &PgTypeInfo) -> bool {
        true
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Postgres> for RawParam {
    fn encode_by_ref(
        &self,
        buf: &mut sqlx::postgres::PgArgumentBuffer,
    ) -> std::result::Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        match &self.bytes {
            Some(b) => {
                buf.extend_from_slice(b);
                Ok(sqlx::encode::IsNull::No)
            }
            None => Ok(sqlx::encode::IsNull::Yes),
        }
    }
    fn produces(&self) -> Option<PgTypeInfo> {
        Some(self.ty.clone())
    }
    fn size_hint(&self) -> usize {
        self.bytes.as_ref().map_or(0, Vec::len)
    }
}

fn param_err(v: &Value, ty: &str) -> Error {
    Error::Other(format!("cannot convert parameter {v:?} to type {ty}"))
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Text(s) | Value::Decimal(s) => s.clone(),
        Value::Json(j) => j.to_string(),
        Value::Other { text, .. } => text.clone(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
}

fn value_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        Value::UInt(u) => i64::try_from(*u).ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i64),
        Value::Decimal(s) | Value::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn value_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Text(s) | Value::Decimal(s) => parse_float(s),
        other => other.as_f64(),
    }
}

fn parse_naive_dt(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
        .or_else(|| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)))
}

/// Encode `v` in the binary format of server type `ty` (`None` = SQL NULL).
pub(crate) fn encode_param(v: &Value, ty: &PgTypeInfo) -> Result<Option<Vec<u8>>> {
    if v.is_null() {
        return Ok(None);
    }
    match ty.kind() {
        PgTypeKind::Domain(base) => return encode_param(v, base),
        PgTypeKind::Enum(_) => return Ok(Some(value_text(v).into_bytes())),
        PgTypeKind::Array(elem) => return encode_array(v, elem, ty).map(Some),
        _ => {}
    }
    let o = ty.oid().map(|o| o.0).unwrap_or(0);
    let tname = type_display_name(ty);
    encode_builtin(v, o).map_err(|_| param_err(v, &tname))
}

fn encode_builtin(v: &Value, o: u32) -> std::result::Result<Option<Vec<u8>>, ()> {
    use oid::*;
    let bytes = match o {
        BOOL => {
            let b = match v {
                Value::Bool(b) => *b,
                Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "t" | "true" | "1" | "yes" | "y" | "on" => true,
                    "f" | "false" | "0" | "no" | "n" | "off" => false,
                    _ => return Err(()),
                },
                other => value_i64(other).ok_or(())? != 0,
            };
            vec![u8::from(b)]
        }
        INT2 => i16::try_from(value_i64(v).ok_or(())?).map_err(|_| ())?.to_be_bytes().to_vec(),
        INT4 => i32::try_from(value_i64(v).ok_or(())?).map_err(|_| ())?.to_be_bytes().to_vec(),
        INT8 => value_i64(v).ok_or(())?.to_be_bytes().to_vec(),
        OID | XID | CID | REGPROC | REGCLASS | REGTYPE | REGNAMESPACE | REGROLE => {
            u32::try_from(value_i64(v).ok_or(())?).map_err(|_| ())?.to_be_bytes().to_vec()
        }
        FLOAT4 => (value_f64(v).ok_or(())? as f32).to_be_bytes().to_vec(),
        FLOAT8 => value_f64(v).ok_or(())?.to_be_bytes().to_vec(),
        NUMERIC => encode_numeric(&value_text(v)).ok_or(())?,
        MONEY => {
            let f = value_f64(v).ok_or(())?;
            ((f * 100.0).round() as i64).to_be_bytes().to_vec()
        }
        BYTEA => match v {
            Value::Bytes(b) => b.clone(),
            Value::Text(s) => match s.strip_prefix("\\x").and_then(|h| hex::decode(h).ok()) {
                Some(b) => b,
                None => s.as_bytes().to_vec(),
            },
            other => value_text(other).into_bytes(),
        },
        UUID => match v {
            Value::Uuid(u) => u.as_bytes().to_vec(),
            other => uuid::Uuid::parse_str(value_text(other).trim()).map_err(|_| ())?.as_bytes().to_vec(),
        },
        JSON => value_text(v).into_bytes(),
        JSONB => {
            let mut b = vec![1u8];
            b.extend_from_slice(value_text(v).as_bytes());
            b
        }
        DATE => {
            let d = match v {
                Value::Date(d) => *d,
                Value::DateTime(dt) => dt.date(),
                Value::DateTimeTz(dt) => dt.date_naive(),
                other => NaiveDate::parse_from_str(value_text(other).trim(), "%Y-%m-%d").map_err(|_| ())?,
            };
            let days = (d - pg_epoch_date()).num_days();
            i32::try_from(days).map_err(|_| ())?.to_be_bytes().to_vec()
        }
        TIME => {
            let t = match v {
                Value::Time(t) => *t,
                Value::DateTime(dt) => dt.time(),
                other => NaiveTime::parse_from_str(value_text(other).trim(), "%H:%M:%S%.f")
                    .or_else(|_| NaiveTime::parse_from_str(value_text(other).trim(), "%H:%M"))
                    .map_err(|_| ())?,
            };
            let us = (t - NaiveTime::MIN).num_microseconds().ok_or(())?;
            us.to_be_bytes().to_vec()
        }
        TIMESTAMP | TIMESTAMPTZ => {
            let naive_utc = match v {
                Value::DateTime(dt) => *dt,
                Value::Date(d) => d.and_hms_opt(0, 0, 0).ok_or(())?,
                Value::DateTimeTz(dt) => {
                    if o == TIMESTAMP {
                        dt.naive_local()
                    } else {
                        dt.naive_utc()
                    }
                }
                other => {
                    let s = value_text(other);
                    match parse_timestamptz(s.trim()) {
                        Some(dt) if o == TIMESTAMPTZ => dt.naive_utc(),
                        Some(dt) => dt.naive_local(),
                        None => parse_naive_dt(&s).ok_or(())?,
                    }
                }
            };
            let us = (naive_utc - pg_epoch()).num_microseconds().ok_or(())?;
            us.to_be_bytes().to_vec()
        }
        INET | CIDR => encode_inet(&value_text(v), o == CIDR).ok_or(())?,
        MACADDR | MACADDR8 => {
            let s = value_text(v);
            let parts: std::result::Result<Vec<u8>, _> =
                s.trim().split([':', '-']).map(|p| u8::from_str_radix(p, 16)).collect();
            let parts = parts.map_err(|_| ())?;
            if parts.len() != if o == MACADDR { 6 } else { 8 } {
                return Err(());
            }
            parts
        }
        // Types whose binary representation is their text: all text-likes and anything
        // unknown (enums, citext, ...). For other non-text builtins we cannot encode.
        INTERVAL | POINT | LSEG | BOX | PATH | POLYGON | LINE | CIRCLE | BIT | VARBIT | TIMETZ | TSVECTOR
        | PG_LSN | TID | RECORD | XID8 => return Err(()),
        _ => value_text(v).into_bytes(),
    };
    Ok(Some(bytes))
}

fn encode_array(v: &Value, elem: &PgTypeInfo, ty: &PgTypeInfo) -> Result<Vec<u8>> {
    let items: Vec<Value> = match v {
        Value::Array(items) => items.clone(),
        other => {
            let s = value_text(other);
            match decode_text_array(&s, oid::TEXT, &ExtTypes::new()) {
                Some(Value::Array(items)) => items,
                _ => return Err(param_err(other, &type_display_name(ty))),
            }
        }
    };
    if items.iter().any(|i| matches!(i, Value::Array(_))) {
        return Err(Error::Unsupported("multi-dimensional array parameters".into()));
    }
    let elem_oid = elem.oid().map(|o| o.0).unwrap_or(0);
    let mut out = vec![];
    out.extend_from_slice(&1i32.to_be_bytes());
    out.extend_from_slice(&i32::from(items.iter().any(Value::is_null)).to_be_bytes());
    out.extend_from_slice(&elem_oid.to_be_bytes());
    out.extend_from_slice(&(items.len() as i32).to_be_bytes());
    out.extend_from_slice(&1i32.to_be_bytes());
    for item in &items {
        match encode_param(item, elem)? {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(b) => {
                out.extend_from_slice(&(b.len() as i32).to_be_bytes());
                out.extend_from_slice(&b);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext() -> ExtTypes {
        ExtTypes::new()
    }

    #[test]
    fn numeric_roundtrip() {
        for s in ["0", "1", "-1", "12345.678", "0.0001", "-0.5", "10000", "99999999.00000001", "123456789012345678901234567890.12", "NaN", "0.000"] {
            let enc = encode_numeric(s).unwrap();
            assert_eq!(decode_numeric(&enc).unwrap(), s, "roundtrip {s}");
        }
        assert_eq!(decode_numeric(&encode_numeric("+007.50").unwrap()).unwrap(), "7.50");
        assert!(encode_numeric("abc").is_none());
    }

    #[test]
    fn scalars() {
        let e = ext();
        assert_eq!(decode_binary(oid::INT4, &42i32.to_be_bytes(), &e), Value::Int(42));
        assert_eq!(decode_binary(oid::INT2, &(-3i16).to_be_bytes(), &e), Value::Int(-3));
        assert_eq!(decode_binary(oid::BOOL, &[1], &e), Value::Bool(true));
        assert_eq!(decode_binary(oid::FLOAT4, &1.1f32.to_be_bytes(), &e), Value::Float(1.1));
        assert_eq!(decode_binary(oid::MONEY, &(-12345i64).to_be_bytes(), &e), Value::Decimal("-123.45".into()));
        assert_eq!(decode_binary(oid::JSONB, b"\x01{\"a\":1}", &e), Value::Json(serde_json::json!({"a": 1})));
        assert_eq!(
            decode_binary(oid::DATE, &1i32.to_be_bytes(), &e),
            Value::Date(NaiveDate::from_ymd_opt(2000, 1, 2).unwrap())
        );
        assert_eq!(decode_binary(oid::DATE, &i32::MAX.to_be_bytes(), &e), other("date", "infinity"));
        assert_eq!(
            decode_binary(oid::TIMESTAMP, &1_500_000i64.to_be_bytes(), &e),
            Value::DateTime(NaiveDate::from_ymd_opt(2000, 1, 1).unwrap().and_hms_micro_opt(0, 0, 1, 500_000).unwrap())
        );
        let mut iv = vec![];
        iv.extend_from_slice(&3_723_000_000i64.to_be_bytes());
        iv.extend_from_slice(&3i32.to_be_bytes());
        iv.extend_from_slice(&14i32.to_be_bytes());
        assert_eq!(decode_binary(oid::INTERVAL, &iv, &e), other("interval", "1 year 2 mons 3 days 01:02:03"));
        assert_eq!(format_interval(0, 0, 0), "00:00:00");
        assert_eq!(format_interval(0, -1, -500_000), "-1 days -00:00:00.5");
        assert_eq!(decode_binary(oid::INET, &[2, 32, 0, 4, 10, 0, 0, 1], &e), other("inet", "10.0.0.1"));
        assert_eq!(decode_binary(oid::CIDR, &[2, 24, 1, 4, 10, 0, 0, 0], &e), other("cidr", "10.0.0.0/24"));
        assert_eq!(decode_binary(oid::INET, &encode_inet("::1/64", false).unwrap(), &e), other("inet", "::1/64"));
        let mut bits = 5i32.to_be_bytes().to_vec();
        bits.push(0b1010_1000);
        assert_eq!(decode_binary(oid::VARBIT, &bits, &e), other("varbit", "10101"));
        let mut pt = 1.5f64.to_be_bytes().to_vec();
        pt.extend_from_slice(&(-2.0f64).to_be_bytes());
        assert_eq!(decode_binary(oid::POINT, &pt, &e), other("point", "(1.5,-2)"));
        assert_eq!(decode_binary(oid::MACADDR, &[8, 0, 0x2b, 1, 2, 3], &e), other("macaddr", "08:00:2b:01:02:03"));
    }

    #[test]
    fn arrays_and_ranges() {
        let e = ext();
        // int4[] = {1,NULL,3}
        let mut b = vec![];
        b.extend_from_slice(&1i32.to_be_bytes());
        b.extend_from_slice(&1i32.to_be_bytes());
        b.extend_from_slice(&oid::INT4.to_be_bytes());
        b.extend_from_slice(&3i32.to_be_bytes());
        b.extend_from_slice(&1i32.to_be_bytes());
        for v in [Some(1i32), None, Some(3)] {
            match v {
                Some(i) => {
                    b.extend_from_slice(&4i32.to_be_bytes());
                    b.extend_from_slice(&i.to_be_bytes());
                }
                None => b.extend_from_slice(&(-1i32).to_be_bytes()),
            }
        }
        assert_eq!(decode_binary(1007, &b, &e), Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]));
        // [1,10)
        let mut r = vec![0x02u8];
        r.extend_from_slice(&4i32.to_be_bytes());
        r.extend_from_slice(&1i32.to_be_bytes());
        r.extend_from_slice(&4i32.to_be_bytes());
        r.extend_from_slice(&10i32.to_be_bytes());
        assert_eq!(decode_binary(oid::INT4RANGE, &r, &e), other("int4range", "[1,10)"));
        assert_eq!(decode_binary(oid::INT4RANGE, &[1], &e), other("int4range", "empty"));
    }

    #[test]
    fn text_format() {
        let e = ext();
        assert_eq!(decode_text(oid::BOOL, "t", &e), Value::Bool(true));
        assert_eq!(decode_text(oid::INT8, "-9", &e), Value::Int(-9));
        assert_eq!(decode_text(oid::NUMERIC, "1.50", &e), Value::Decimal("1.50".into()));
        assert_eq!(decode_text(oid::BYTEA, "\\x00ff", &e), Value::Bytes(vec![0, 255]));
        assert_eq!(
            decode_text(1009, "{a,\"b c\",NULL,\"NULL\"}", &e),
            Value::Array(vec![Value::Text("a".into()), Value::Text("b c".into()), Value::Null, Value::Text("NULL".into())])
        );
        assert_eq!(
            decode_text(1007, "{{1,2},{3,4}}", &e),
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Int(4)])
            ])
        );
        assert_eq!(decode_text(1007, "[0:1]={5,6}", &e), Value::Array(vec![Value::Int(5), Value::Int(6)]));
        let tz = decode_text(oid::TIMESTAMPTZ, "2024-01-02 03:04:05.5+05:30", &e);
        assert_eq!(tz.to_string(), "2024-01-02 03:04:05.500 +05:30");
        assert_eq!(decode_text(oid::INTERVAL, "1 day", &e), other("interval", "1 day"));
        assert_eq!(decode_text(99999, "x", &e), other("oid:99999", "x"));
    }

    #[test]
    fn fallback_unknown_binary() {
        assert_eq!(fallback("x", b"hello"), other("x", "hello"));
        assert_eq!(fallback("x", &[0, 1]), other("x", "\\x0001"));
    }
}
