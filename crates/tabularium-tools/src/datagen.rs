//! Test data generator: plausible random rows for a table, reproducible with a seed.
//!
//! Values are chosen from column-name heuristics (email, name, city, phone, url, ...) and the
//! column type (integer ranges, numeric precision/scale, string length, dates, uuids, JSON).
//! Auto-increment and generated columns are skipped. Foreign-key columns cannot be satisfied
//! generically: pass candidate values in `fk_values` (column name -> values), otherwise they
//! get type-based values. Primary key and single-column unique key columns get unique values.

use std::collections::{HashMap, HashSet};

use chrono::{Duration, NaiveDate, NaiveTime};
use tabularium_core::{ColumnInfo, TableInfo, Value};

use crate::util::parse_type;

/// SplitMix64: tiny, fast, seedable PRNG with good statistical quality for test data.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Generator with the given seed.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = (hi as i128 - lo as i128 + 1) as u128;
        (lo as i128 + (self.next_u64() as u128 % span) as i128) as i64
    }

    /// Uniform float in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// `true` with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    /// Uniformly chosen element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(0, items.len() as i64 - 1) as usize]
    }
}

const FIRST: &[&str] = &[
    "James", "Mary", "John", "Patricia", "Robert", "Jennifer", "Michael", "Linda", "William", "Elizabeth",
    "David", "Barbara", "Richard", "Susan", "Joseph", "Jessica", "Thomas", "Sarah", "Charles", "Karen",
    "Olivia", "Liam", "Emma", "Noah", "Ava", "Lucas", "Mia", "Hugo", "Sofia", "Mateo",
];
const LAST: &[&str] = &[
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller", "Davis", "Rodriguez", "Martinez",
    "Hernandez", "Lopez", "Wilson", "Anderson", "Thomas", "Taylor", "Moore", "Jackson", "Martin", "Lee",
    "Schmidt", "Rossi", "Dubois", "Novak", "Kowalski", "Nielsen", "Tanaka", "Silva", "Costa", "Weber",
];
const CITIES: &[&str] = &[
    "New York", "London", "Paris", "Berlin", "Madrid", "Rome", "Tokyo", "Sydney", "Toronto", "Chicago",
    "Amsterdam", "Prague", "Vienna", "Lisbon", "Dublin", "Oslo", "Stockholm", "Copenhagen", "Warsaw", "Zurich",
];
const COUNTRIES: &[&str] = &[
    "United States", "United Kingdom", "France", "Germany", "Spain", "Italy", "Japan", "Australia", "Canada",
    "Netherlands", "Czechia", "Austria", "Portugal", "Ireland", "Norway", "Sweden", "Denmark", "Poland",
];
const STREETS: &[&str] = &["Main St", "Oak Ave", "Maple Rd", "Park Lane", "High St", "Elm St", "Cedar Blvd", "Lake View"];
const COMPANIES: &[&str] = &["Acme", "Globex", "Initech", "Umbrella", "Hooli", "Stark", "Wayne", "Wonka", "Tyrell", "Cyberdyne"];
const DOMAINS: &[&str] = &["example.com", "example.org", "example.net", "test.io", "mail.test"];
const WORDS: &[&str] = &[
    "lorem", "ipsum", "dolor", "sit", "amet", "consectetur", "adipiscing", "elit", "sed", "do", "eiusmod",
    "tempor", "incididunt", "ut", "labore", "et", "dolore", "magna", "aliqua", "enim", "minim", "veniam",
];
const COLORS: &[&str] = &["red", "green", "blue", "yellow", "purple", "orange", "black", "white", "gray"];
const STATUSES: &[&str] = &["active", "inactive", "pending", "archived"];

fn sentence(rng: &mut Rng, words: usize) -> String {
    let mut s: Vec<String> = (0..words).map(|_| rng.pick(WORDS).to_string()).collect();
    if let Some(first) = s.first_mut() {
        let mut c = first.chars();
        if let Some(f) = c.next() {
            *first = f.to_uppercase().collect::<String>() + c.as_str();
        }
    }
    format!("{}.", s.join(" "))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Int { lo: i64, hi: i64 },
    Decimal { precision: u32, scale: u32 },
    Float,
    Bool,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    Bytes,
    Text { max: Option<usize> },
}

fn kind_of(col: &ColumnInfo) -> Kind {
    let p = parse_type(&col.data_type);
    let n = |i: usize| p.args.get(i).and_then(|a| a.trim().parse::<u32>().ok());
    match p.base.as_str() {
        "tinyint" if n(0) == Some(1) => Kind::Bool,
        "tinyint" => Kind::Int { lo: 0, hi: if p.unsigned { 255 } else { 127 } },
        "smallint" | "int2" | "smallserial" | "serial2" | "year" => Kind::Int { lo: 0, hi: 32_767 },
        "mediumint" | "int" | "integer" | "int4" | "serial" | "serial4" => Kind::Int { lo: 1, hi: 100_000 },
        "bigint" | "int8" | "bigserial" | "serial8" => Kind::Int { lo: 1, hi: 10_000_000 },
        "numeric" | "decimal" | "dec" | "number" | "money" | "smallmoney" => {
            let precision = n(0).unwrap_or(10).clamp(1, 18);
            let scale = n(1).unwrap_or(if n(0).is_some() { 0 } else { 2 }).min(precision);
            Kind::Decimal { precision, scale }
        }
        "real" | "float" | "float4" | "float8" | "double" | "double precision" => Kind::Float,
        "bool" | "boolean" => Kind::Bool,
        "bit" if n(0).unwrap_or(1) == 1 => Kind::Bool,
        "date" => Kind::Date,
        "time" | "timetz" => Kind::Time,
        "timestamp" if p.tz == Some(true) => Kind::TimestampTz,
        "timestamptz" | "datetimeoffset" => Kind::TimestampTz,
        "timestamp" | "datetime" | "datetime2" | "smalldatetime" => Kind::Timestamp,
        "uuid" | "uniqueidentifier" => Kind::Uuid,
        "json" | "jsonb" => Kind::Json,
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "image" => Kind::Bytes,
        _ => {
            let max = match p.args.first().map(String::as_str) {
                Some("max") | None => None,
                Some(a) => a.trim().parse::<usize>().ok(),
            };
            Kind::Text { max }
        }
    }
}

fn truncate(s: String, max: Option<usize>) -> String {
    match max {
        Some(m) if s.chars().count() > m => s.chars().take(m).collect(),
        _ => s,
    }
}

fn base_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2015, 1, 1).expect("valid date")
}

/// Text value from name heuristics, if the name suggests one.
fn text_by_name(name: &str, rng: &mut Rng, row: usize, unique: bool) -> Option<String> {
    let n = name.to_ascii_lowercase();
    let has = |k: &str| n.contains(k);
    let first = rng.pick(FIRST).to_string();
    let last = rng.pick(LAST).to_string();
    let suffix = if unique { format!("{}", row + 1) } else { String::new() };
    Some(if has("email") || has("e_mail") || n == "mail" {
        format!("{}.{}{}@{}", first.to_lowercase(), last.to_lowercase(), suffix, rng.pick(DOMAINS))
    } else if has("first") && has("name") || n == "given_name" || n == "forename" {
        first
    } else if (has("last") && has("name")) || has("surname") || n == "family_name" {
        last
    } else if has("username") || has("login") || n == "user" || n == "nick" || n == "nickname" {
        format!("{}{}{}", first.to_lowercase(), last.chars().next().unwrap_or('x').to_ascii_lowercase(), if unique { suffix } else { rng.range(1, 999).to_string() })
    } else if has("company") || has("organization") || has("organisation") || has("employer") {
        format!("{} {}", rng.pick(COMPANIES), rng.pick(&["Inc", "Ltd", "GmbH", "LLC", "Corp"]))
    } else if has("city") || has("town") {
        rng.pick(CITIES).to_string()
    } else if has("country") {
        rng.pick(COUNTRIES).to_string()
    } else if has("address") || has("street") {
        format!("{} {}", rng.range(1, 9999), rng.pick(STREETS))
    } else if has("zip") || has("postal") || has("postcode") {
        format!("{:05}", rng.range(1000, 99999))
    } else if has("phone") || has("mobile") || has("fax") || has("tel") {
        format!("+1-{:03}-{:03}-{:04}", rng.range(200, 999), rng.range(100, 999), rng.range(0, 9999))
    } else if has("url") || has("website") || has("homepage") || has("link") {
        format!("https://www.{}/{}", rng.pick(DOMAINS), rng.pick(WORDS))
    } else if has("ip") && (n == "ip" || has("ip_") || has("_ip") || has("ipaddr")) {
        format!("{}.{}.{}.{}", rng.range(1, 223), rng.range(0, 255), rng.range(0, 255), rng.range(1, 254))
    } else if has("color") || has("colour") {
        rng.pick(COLORS).to_string()
    } else if has("status") || has("state") {
        rng.pick(STATUSES).to_string()
    } else if has("password") || has("hash") || has("token") || has("secret") {
        (0..32).map(|_| format!("{:x}", rng.range(0, 15))).collect()
    } else if has("uuid") || has("guid") {
        uuid_from(rng).to_string()
    } else if has("sku") || has("code") {
        format!("SKU-{:06}", if unique { row as i64 + 1 } else { rng.range(1, 999_999) })
    } else if has("description") || has("comment") || has("note") || has("summary") || has("bio") || has("body") || has("text") || has("content") {
        {
        let n = rng.range(6, 14) as usize;
        sentence(rng, n)
    }
    } else if has("title") || has("subject") || has("headline") {
        {
        let n = rng.range(2, 5) as usize;
        sentence(rng, n).trim_end_matches('.').to_string()
    }
    } else if has("name") {
        format!("{first} {last}")
    } else {
        return None;
    })
}

fn uuid_from(rng: &mut Rng) -> uuid::Uuid {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&rng.next_u64().to_le_bytes());
    bytes[8..].copy_from_slice(&rng.next_u64().to_le_bytes());
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

fn int_by_name(name: &str, rng: &mut Rng) -> Option<i64> {
    let n = name.to_ascii_lowercase();
    if n == "age" || n.ends_with("_age") {
        Some(rng.range(18, 90))
    } else if n.contains("quantity") || n.contains("qty") || n.ends_with("count") {
        Some(rng.range(1, 100))
    } else if n.contains("year") {
        Some(rng.range(1990, 2025))
    } else if n.contains("rating") || n.contains("stars") {
        Some(rng.range(1, 5))
    } else if n.contains("percent") || n.contains("pct") {
        Some(rng.range(0, 100))
    } else {
        None
    }
}

fn generate_value(col: &ColumnInfo, kind: Kind, rng: &mut Rng, row: usize, unique: bool) -> Value {
    match kind {
        Kind::Int { lo, hi } => {
            if unique {
                return Value::Int(row as i64 + 1);
            }
            let v = int_by_name(&col.name, rng).unwrap_or_else(|| rng.range(lo, hi));
            Value::Int(v.clamp(lo.min(v), hi))
        }
        Kind::Decimal { precision, scale } => {
            let int_digits = precision - scale;
            let n = col.name.to_ascii_lowercase();
            let money = ["price", "amount", "cost", "total", "salary", "balance", "fee"].iter().any(|k| n.contains(k));
            let max_int = 10i64.pow(int_digits.min(15)) - 1;
            let hi = if money { max_int.min(1000) } else { max_int.min(100_000) };
            let ip = if unique { row as i64 + 1 } else { rng.range(if money { 1.min(hi) } else { 0 }, hi.max(0)) };
            if scale == 0 {
                Value::Decimal(ip.to_string())
            } else {
                let frac = rng.range(0, 10i64.pow(scale.min(9)) - 1);
                Value::Decimal(format!("{ip}.{frac:0width$}", width = scale.min(9) as usize) + &"0".repeat(scale.saturating_sub(9) as usize))
            }
        }
        Kind::Float => {
            if unique {
                return Value::Float(row as f64 + 1.0);
            }
            let n = col.name.to_ascii_lowercase();
            let v = if n.contains("lat") {
                rng.unit() * 180.0 - 90.0
            } else if n.contains("lon") || n.contains("lng") {
                rng.unit() * 360.0 - 180.0
            } else {
                rng.unit() * 1000.0
            };
            Value::Float((v * 10000.0).round() / 10000.0)
        }
        Kind::Bool => Value::Bool(rng.chance(0.5)),
        Kind::Date => Value::Date(base_date() + Duration::days(rng.range(0, 3652))),
        Kind::Time => Value::Time(
            NaiveTime::from_num_seconds_from_midnight_opt(rng.range(0, 86_399) as u32, 0).expect("valid time"),
        ),
        Kind::Timestamp | Kind::TimestampTz => {
            let dt = base_date().and_hms_opt(0, 0, 0).expect("valid") + Duration::seconds(rng.range(0, 3652 * 86_400));
            if kind == Kind::TimestampTz {
                Value::DateTimeTz(dt.and_utc().fixed_offset())
            } else {
                Value::DateTime(dt)
            }
        }
        Kind::Uuid => Value::Uuid(uuid_from(rng)),
        Kind::Json => Value::Json(serde_json::json!({
            "id": rng.range(1, 1000),
            "tag": rng.pick(WORDS),
            "active": rng.chance(0.5),
        })),
        Kind::Bytes => Value::Bytes((0..16).map(|_| rng.range(0, 255) as u8).collect()),
        Kind::Text { max } => {
            let s = text_by_name(&col.name, rng, row, unique).unwrap_or_else(|| {
                let words = match max {
                    Some(m) if m < 12 => 1,
                    _ => rng.range(1, 4) as usize,
                };
                let mut s = (0..words).map(|_| *rng.pick(WORDS)).collect::<Vec<_>>().join(" ");
                if unique {
                    s = format!("{s}_{}", row + 1);
                }
                s
            });
            if unique && max.is_some_and(|m| s.chars().count() > m) {
                // keep uniqueness when truncating: use the row number
                return Value::Text(truncate(format!("{}", row + 1), max));
            }
            Value::Text(truncate(s, max))
        }
    }
}

/// Generate `count` rows for `table`. Returns the generated column names (auto-increment and
/// generated columns excluded) and the rows. The same `seed` always yields the same data.
pub fn generate_rows(
    table: &TableInfo,
    count: usize,
    seed: u64,
    fk_values: &HashMap<String, Vec<Value>>,
) -> (Vec<String>, Vec<Vec<Value>>) {
    let mut rng = Rng::new(seed);
    let cols: Vec<&ColumnInfo> = table
        .columns
        .iter()
        .filter(|c| !c.auto_increment && c.generated.is_none())
        .filter(|c| !crate::util::is_serial_type(&c.data_type) && !crate::util::is_nextval(c.default.as_deref()))
        .collect();
    let unique_cols: HashSet<String> = table
        .primary_key
        .iter()
        .chain(table.unique_keys.iter())
        .filter(|k| k.columns.len() == 1)
        .map(|k| k.columns[0].to_ascii_lowercase())
        .collect();
    let pk_cols: HashSet<String> = table
        .primary_key
        .iter()
        .flat_map(|k| k.columns.iter().map(|c| c.to_ascii_lowercase()))
        .collect();
    let kinds: Vec<Kind> = cols.iter().map(|c| kind_of(c)).collect();
    let fk_lookup = |name: &str| {
        fk_values
            .get(name)
            .or_else(|| fk_values.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
            .filter(|v| !v.is_empty())
    };
    let rows = (0..count)
        .map(|row| {
            cols.iter()
                .zip(&kinds)
                .map(|(col, kind)| {
                    let lname = col.name.to_ascii_lowercase();
                    let unique = unique_cols.contains(&lname);
                    if let Some(vals) = fk_lookup(&col.name) {
                        if col.nullable && !pk_cols.contains(&lname) && rng.chance(0.05) {
                            return Value::Null;
                        }
                        if unique && vals.len() >= count {
                            return vals[row].clone();
                        }
                        return rng.pick(vals).clone();
                    }
                    if col.nullable && !unique && !pk_cols.contains(&lname) && rng.chance(0.05) {
                        return Value::Null;
                    }
                    generate_value(col, *kind, &mut rng, row, unique)
                })
                .collect()
        })
        .collect();
    (cols.iter().map(|c| c.name.clone()).collect(), rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabularium_core::{KeyInfo, ObjectRef};

    fn col(name: &str, ty: &str) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: false, ..Default::default() }
    }

    fn table() -> TableInfo {
        let mut t = TableInfo::new(ObjectRef::table(None, "people"));
        t.columns = vec![
            ColumnInfo { auto_increment: true, ..col("id", "integer") },
            col("email", "varchar(100)"),
            col("first_name", "varchar(5)"),
            col("age", "smallint"),
            col("price", "numeric(6,2)"),
            col("active", "boolean"),
            col("born", "date"),
            col("created_at", "timestamp"),
            col("token", "uuid"),
            col("meta", "jsonb"),
            col("country_id", "integer"),
            col("code", "char(3)"),
            ColumnInfo { generated: Some("upper(email)".into()), ..col("email_upper", "text") },
            ColumnInfo { nullable: true, ..col("notes", "text") },
        ];
        t.primary_key = Some(KeyInfo { name: None, columns: vec!["id".into()] });
        t.unique_keys = vec![KeyInfo { name: None, columns: vec!["email".into()] }];
        t
    }

    #[test]
    fn deterministic_and_typed() {
        let mut fk = HashMap::new();
        fk.insert("country_id".to_string(), vec![Value::Int(10), Value::Int(20)]);
        let (cols, rows) = generate_rows(&table(), 50, 42, &fk);
        let (cols2, rows2) = generate_rows(&table(), 50, 42, &fk);
        assert_eq!(cols, cols2);
        assert_eq!(rows, rows2);
        let (_, rows3) = generate_rows(&table(), 50, 43, &fk);
        assert_ne!(rows, rows3);

        assert!(!cols.contains(&"id".to_string()));
        assert!(!cols.contains(&"email_upper".to_string()));
        assert_eq!(rows.len(), 50);
        let idx = |n: &str| cols.iter().position(|c| c == n).unwrap();
        let mut emails = HashSet::new();
        for r in &rows {
            match &r[idx("email")] {
                Value::Text(e) => {
                    assert!(e.contains('@') && e.len() <= 100);
                    assert!(emails.insert(e.clone()), "duplicate email {e}");
                }
                other => panic!("email {other:?}"),
            }
            match &r[idx("first_name")] {
                Value::Text(s) => assert!(s.chars().count() <= 5),
                other => panic!("{other:?}"),
            }
            match &r[idx("age")] {
                Value::Int(a) => assert!((18..=90).contains(a)),
                other => panic!("{other:?}"),
            }
            match &r[idx("price")] {
                Value::Decimal(p) => {
                    let (ip, fp) = p.split_once('.').unwrap();
                    assert!(ip.len() <= 4 && fp.len() == 2, "{p}");
                }
                other => panic!("{other:?}"),
            }
            assert!(matches!(r[idx("active")], Value::Bool(_)));
            assert!(matches!(r[idx("born")], Value::Date(_)));
            assert!(matches!(r[idx("created_at")], Value::DateTime(_)));
            assert!(matches!(r[idx("token")], Value::Uuid(_)));
            assert!(matches!(r[idx("meta")], Value::Json(_)));
            assert!(matches!(r[idx("country_id")], Value::Int(10) | Value::Int(20)));
            match &r[idx("code")] {
                Value::Text(s) => assert!(s.chars().count() <= 3),
                other => panic!("{other:?}"),
            }
        }
        // nullable column gets some NULLs over many rows
        let (_, many) = generate_rows(&table(), 400, 1, &fk);
        assert!(many.iter().any(|r| r[idx("notes")].is_null()));
    }

    #[test]
    fn rng_range() {
        let mut r = Rng::new(7);
        for _ in 0..1000 {
            let v = r.range(-3, 3);
            assert!((-3..=3).contains(&v));
            let u = r.unit();
            assert!((0.0..1.0).contains(&u));
        }
        assert_eq!(r.range(5, 5), 5);
    }
}
