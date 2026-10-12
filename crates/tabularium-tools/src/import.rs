//! Data import: read CSV/TSV, JSON, JSON Lines and spreadsheets (XLSX/XLS/ODS) into rows,
//! infer column types, and generate `CREATE TABLE` / `INSERT` statements.

use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{NaiveDate, NaiveDateTime};
use tabularium_core::{Dialect, ObjectRef, Value};

use crate::util::{dedupe_names, q, qname};

/// Supported import formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)] // variant names are self-describing
pub enum ImportFormat {
    Csv,
    Tsv,
    /// Array of objects, array of arrays, or `{"key": [...]}` wrapper.
    Json,
    /// One JSON object (or array) per line.
    JsonLines,
    /// Spreadsheets: `.xlsx`, `.xlsm`, `.xls`, `.ods`.
    Xlsx,
}

impl ImportFormat {
    /// Guess the format from a file extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "csv" | "txt" => ImportFormat::Csv,
            "tsv" | "tab" => ImportFormat::Tsv,
            "json" => ImportFormat::Json,
            "jsonl" | "ndjson" => ImportFormat::JsonLines,
            "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" => ImportFormat::Xlsx,
            _ => return None,
        })
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            ImportFormat::Csv => "CSV",
            ImportFormat::Tsv => "TSV",
            ImportFormat::Json => "JSON",
            ImportFormat::JsonLines => "JSON Lines",
            ImportFormat::Xlsx => "Excel",
        }
    }
}

/// Import settings.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Input format.
    pub format: ImportFormat,
    /// First (non-skipped) row holds column names (CSV/TSV, arrays of arrays, spreadsheets).
    pub has_header: bool,
    /// CSV delimiter; `None` sniffs it (TSV defaults to tab).
    pub delimiter: Option<char>,
    /// CSV quote character.
    pub quote: char,
    /// Spreadsheet sheet name (default: first sheet).
    pub sheet: Option<String>,
    /// Cell text treated as NULL (CSV/TSV and spreadsheet text cells).
    pub null_text: Option<String>,
    /// Rows (records) skipped before the header/data.
    pub skip_rows: usize,
    /// Maximum number of data rows read.
    pub max_rows: Option<usize>,
    /// Trim whitespace around CSV cells.
    pub trim: bool,
}

impl ImportOptions {
    /// Defaults: header on, sniffed delimiter, `"` quotes, no NULL text, no limits.
    pub fn new(format: ImportFormat) -> Self {
        Self {
            format,
            has_header: true,
            delimiter: None,
            quote: '"',
            sheet: None,
            null_text: None,
            skip_rows: 0,
            max_rows: None,
            trim: false,
        }
    }
}

/// Imported table: column names and rows (always `columns.len()` wide).
///
/// CSV values are `Value::Text` (or `Null` when matching `null_text`, or for padding of
/// ragged rows); JSON keeps its types (objects/arrays become `Value::Json`); spreadsheets keep
/// numbers, booleans and dates.
#[derive(Debug, Clone, Default)]
pub struct ImportedData {
    /// Column names (unique; blanks become `column_N`).
    pub columns: Vec<String>,
    /// Data rows.
    pub rows: Vec<Vec<Value>>,
}

/// Read a file. Spreadsheets are read with calamine; text files are decoded as UTF-8
/// (invalid sequences replaced).
pub fn read_file(path: &Path, opts: &ImportOptions) -> Result<ImportedData> {
    if opts.format == ImportFormat::Xlsx {
        return read_spreadsheet(path, opts);
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    read_str(&text, opts)
}

/// Parse text in a text format. Errors for spreadsheets.
pub fn read_str(text: &str, opts: &ImportOptions) -> Result<ImportedData> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    match opts.format {
        ImportFormat::Csv | ImportFormat::Tsv => read_csv(text, opts),
        ImportFormat::Json => read_json(text, opts),
        ImportFormat::JsonLines => read_json_lines(text, opts),
        ImportFormat::Xlsx => bail!("spreadsheets must be read from a file"),
    }
}

fn finish(header: Option<Vec<String>>, mut rows: Vec<Vec<Value>>) -> ImportedData {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0).max(header.as_ref().map_or(0, Vec::len));
    let mut names = header.unwrap_or_default();
    for (i, n) in names.iter_mut().enumerate() {
        if n.trim().is_empty() {
            *n = format!("column_{}", i + 1);
        }
    }
    while names.len() < width {
        names.push(format!("column_{}", names.len() + 1));
    }
    for r in &mut rows {
        r.resize(width, Value::Null);
    }
    ImportedData { columns: dedupe_names(&names), rows }
}

fn read_csv(text: &str, opts: &ImportOptions) -> Result<ImportedData> {
    let delim = match (opts.delimiter, opts.format) {
        (Some(c), _) => c,
        (None, ImportFormat::Tsv) => '\t',
        (None, _) => sniff_csv(text).0,
    };
    if !delim.is_ascii() || !opts.quote.is_ascii() {
        bail!("CSV delimiter and quote must be ASCII characters");
    }
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim as u8)
        .quote(opts.quote as u8)
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut header = None;
    let mut rows = Vec::new();
    let mut skipped = 0;
    for rec in rdr.records() {
        let rec = rec.context("parsing CSV")?;
        if skipped < opts.skip_rows {
            skipped += 1;
            continue;
        }
        let cells: Vec<String> =
            rec.iter().map(|c| if opts.trim { c.trim().to_string() } else { c.to_string() }).collect();
        if opts.has_header && header.is_none() {
            header = Some(cells.iter().map(|c| c.trim().to_string()).collect());
            continue;
        }
        if opts.max_rows.is_some_and(|m| rows.len() >= m) {
            break;
        }
        rows.push(
            cells
                .into_iter()
                .map(|c| if opts.null_text.as_deref() == Some(c.as_str()) { Value::Null } else { Value::Text(c) })
                .collect(),
        );
    }
    Ok(finish(header, rows))
}

fn json_to_value(j: serde_json::Value) -> Value {
    use serde_json::Value as J;
    match j {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if let Some(u) = n.as_u64() {
                Value::UInt(u)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        J::String(s) => Value::Text(s),
        other => Value::Json(other),
    }
}

/// Turn a list of JSON records (objects or arrays) into a table.
fn json_records(items: Vec<serde_json::Value>, opts: &ImportOptions) -> Result<ImportedData> {
    let items: Vec<serde_json::Value> = items.into_iter().skip(opts.skip_rows).collect();
    if items.iter().all(|v| v.is_object()) {
        let mut keys: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for it in &items {
            if let serde_json::Value::Object(m) = it {
                for k in m.keys() {
                    if seen.insert(k.clone()) {
                        keys.push(k.clone());
                    }
                }
            }
        }
        let limit = opts.max_rows.unwrap_or(usize::MAX);
        let rows = items
            .into_iter()
            .take(limit)
            .map(|it| {
                let mut m = match it {
                    serde_json::Value::Object(m) => m,
                    _ => unreachable!(),
                };
                keys.iter().map(|k| m.remove(k).map(json_to_value).unwrap_or(Value::Null)).collect()
            })
            .collect();
        return Ok(ImportedData { columns: keys, rows });
    }
    if items.iter().all(|v| v.is_array()) {
        let mut iter = items.into_iter();
        let mut header = None;
        if opts.has_header {
            if let Some(serde_json::Value::Array(first)) = iter.next() {
                header = Some(
                    first
                        .into_iter()
                        .map(|v| match v {
                            serde_json::Value::String(s) => s,
                            other => other.to_string(),
                        })
                        .collect(),
                );
            }
        }
        let rows = iter
            .take(opts.max_rows.unwrap_or(usize::MAX))
            .map(|v| match v {
                serde_json::Value::Array(a) => a.into_iter().map(json_to_value).collect(),
                _ => unreachable!(),
            })
            .collect();
        return Ok(finish(header, rows));
    }
    // Scalars or mixed: one column named "value".
    let rows = items
        .into_iter()
        .take(opts.max_rows.unwrap_or(usize::MAX))
        .map(|v| vec![json_to_value(v)])
        .collect();
    Ok(ImportedData { columns: vec!["value".into()], rows })
}

fn read_json(text: &str, opts: &ImportOptions) -> Result<ImportedData> {
    let v: serde_json::Value = serde_json::from_str(text).context("parsing JSON")?;
    let items = match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(m) => {
            // {"rows": [...]} style wrappers: use the single array member if there is one.
            let arrays: Vec<&serde_json::Value> = m.values().filter(|v| v.is_array()).collect();
            if m.len() == 1 && arrays.len() == 1 {
                match m.into_iter().next() {
                    Some((_, serde_json::Value::Array(a))) => a,
                    _ => unreachable!(),
                }
            } else {
                vec![serde_json::Value::Object(m)]
            }
        }
        other => vec![other],
    };
    json_records(items, opts)
}

fn read_json_lines(text: &str, opts: &ImportOptions) -> Result<ImportedData> {
    let mut items = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        items.push(serde_json::from_str(line).with_context(|| format!("parsing JSON on line {}", n + 1))?);
    }
    json_records(items, opts)
}

fn cell_to_value(d: &calamine::Data, null_text: Option<&str>) -> Value {
    use calamine::Data;
    match d {
        Data::Empty => Value::Null,
        Data::Int(i) => Value::Int(*i),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 9.0e15 {
                Value::Int(*f as i64)
            } else {
                Value::Float(*f)
            }
        }
        Data::String(s) => {
            if null_text == Some(s.as_str()) {
                Value::Null
            } else {
                Value::Text(s.clone())
            }
        }
        Data::Bool(b) => Value::Bool(*b),
        Data::DateTime(dt) => {
            if dt.is_duration() {
                return Value::Other { type_name: "duration".into(), text: dt.as_f64().to_string() };
            }
            match dt.as_datetime() {
                Some(ndt) => {
                    use chrono::Timelike;
                    if ndt.time().num_seconds_from_midnight() == 0 && ndt.time().nanosecond() == 0 && dt.as_f64() >= 1.0 {
                        Value::Date(ndt.date())
                    } else if dt.as_f64() < 1.0 && dt.as_f64() >= 0.0 {
                        Value::Time(ndt.time())
                    } else {
                        Value::DateTime(ndt)
                    }
                }
                None => Value::Float(dt.as_f64()),
            }
        }
        Data::DateTimeIso(s) => {
            if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
                Value::DateTime(dt)
            } else if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                Value::Date(d)
            } else {
                Value::Text(s.clone())
            }
        }
        Data::DurationIso(s) => Value::Other { type_name: "duration".into(), text: s.clone() },
        Data::Error(_) => Value::Null,
    }
}

fn cell_header(d: &calamine::Data) -> String {
    match cell_to_value(d, None) {
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn read_spreadsheet(path: &Path, opts: &ImportOptions) -> Result<ImportedData> {
    use calamine::Reader;
    let mut wb = calamine::open_workbook_auto(path).with_context(|| format!("opening {}", path.display()))?;
    let names = wb.sheet_names().to_vec();
    let sheet = match &opts.sheet {
        Some(s) => names
            .iter()
            .find(|n| *n == s)
            .or_else(|| names.iter().find(|n| n.eq_ignore_ascii_case(s)))
            .cloned()
            .with_context(|| format!("sheet {s} not found (available: {})", names.join(", ")))?,
        None => names.first().cloned().context("workbook has no sheets")?,
    };
    let range = wb.worksheet_range(&sheet).with_context(|| format!("reading sheet {sheet}"))?;
    let mut iter = range.rows().skip(opts.skip_rows);
    let header = if opts.has_header { iter.next().map(|r| r.iter().map(cell_header).collect::<Vec<_>>()) } else { None };
    let null_text = opts.null_text.as_deref();
    let mut rows: Vec<Vec<Value>> = iter
        .take(opts.max_rows.unwrap_or(usize::MAX))
        .map(|r| r.iter().map(|c| cell_to_value(c, null_text)).collect())
        .collect();
    // drop trailing all-empty rows
    while rows.last().is_some_and(|r| r.iter().all(Value::is_null)) {
        rows.pop();
    }
    // trim header to the used width if trailing header cells are empty and unused
    let mut data = finish(header, rows);
    if data.columns.is_empty() && !data.rows.is_empty() {
        data.columns = (1..=data.rows[0].len()).map(|i| format!("column_{i}")).collect();
    }
    Ok(data)
}

/// List the sheet names of a spreadsheet.
pub fn sheet_names(path: &Path) -> Result<Vec<String>> {
    use calamine::Reader;
    let wb = calamine::open_workbook_auto(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(wb.sheet_names().to_vec())
}

// ---------------------------------------------------------------------------------------------
// Sniffing and type inference
// ---------------------------------------------------------------------------------------------

fn looks_numeric(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.parse::<f64>().is_ok() && !s.eq_ignore_ascii_case("nan") && !s.to_ascii_lowercase().contains("inf")
}

fn looks_date(s: &str) -> bool {
    parse_date(s).is_some() || parse_datetime(s).is_some()
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, f) {
            return Some(dt);
        }
    }
    // trailing zone (Z / +02:00)
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.naive_utc())
}

/// Sniff the delimiter (`,`, `;`, tab, `|`) and whether the first row is a header.
pub fn sniff_csv(sample: &str) -> (char, bool) {
    let sample = sample.strip_prefix('\u{FEFF}').unwrap_or(sample);
    // Only look at the first ~50 lines, cut at a line boundary.
    let mut end = sample.len();
    if let Some((i, _)) = sample.match_indices('\n').nth(50) {
        end = i;
    }
    let sample = &sample[..end];
    let mut best = (',', 0usize, 0usize); // delimiter, consistent rows, fields
    for delim in [',', ';', '\t', '|'] {
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(delim as u8)
            .has_headers(false)
            .flexible(true)
            .from_reader(sample.as_bytes());
        let counts: Vec<usize> = rdr.records().filter_map(|r| r.ok()).map(|r| r.len()).collect();
        if counts.is_empty() {
            continue;
        }
        let first = counts[0];
        if first < 2 {
            continue;
        }
        let consistent = counts.iter().filter(|&&c| c == first).count();
        let better = consistent > best.1 || (consistent == best.1 && first > best.2);
        if better {
            best = (delim, consistent, first);
        }
    }
    let delim = best.0;
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim as u8)
        .has_headers(false)
        .flexible(true)
        .from_reader(sample.as_bytes());
    let recs: Vec<Vec<String>> =
        rdr.records().filter_map(|r| r.ok()).map(|r| r.iter().map(str::to_string).collect()).collect();
    let header = match recs.split_first() {
        None => true,
        Some((first, rest)) => {
            let typed = |s: &str| looks_numeric(s) || looks_date(s);
            if first.iter().any(|c| c.trim().is_empty() || typed(c)) {
                false
            } else if rest.is_empty() {
                true
            } else {
                // Header if some column is typed in the data but not in the first row, or if
                // first-row values never reappear and lengths differ from data.
                let typed_col = (0..first.len()).any(|i| {
                    let vals: Vec<&str> = rest.iter().filter_map(|r| r.get(i)).map(|s| s.as_str()).filter(|s| !s.trim().is_empty()).collect();
                    !vals.is_empty() && vals.iter().all(|v| typed(v))
                });
                if typed_col {
                    true
                } else {
                    let unique = {
                        let mut s = std::collections::HashSet::new();
                        first.iter().all(|c| s.insert(c.to_ascii_lowercase()))
                    };
                    let reappears = (0..first.len()).any(|i| rest.iter().any(|r| r.get(i) == Some(&first[i])));
                    unique && !reappears
                }
            }
        }
    };
    (delim, header)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Inferred {
    Unknown,
    Bool,
    Int,
    BigInt,
    Decimal,
    Double,
    Date,
    Timestamp,
    Json,
    Text,
}

struct ColumnStats {
    kind: Inferred,
    max_len: usize,
    int_digits: usize,
    scale: usize,
}

fn classify_text(s: &str) -> (Inferred, usize, usize) {
    let t = s.trim();
    if t.is_empty() {
        return (Inferred::Unknown, 0, 0);
    }
    match t.to_ascii_lowercase().as_str() {
        "true" | "false" => return (Inferred::Bool, 0, 0),
        _ => {}
    }
    let no_lead_zero = !(t.len() > 1 && t.starts_with('0')) && !(t.len() > 2 && t.starts_with("-0"));
    if let Ok(i) = t.parse::<i64>() {
        if no_lead_zero {
            let kind = if i32::try_from(i).is_ok() { Inferred::Int } else { Inferred::BigInt };
            return (kind, t.trim_start_matches('-').len(), 0);
        }
        return (Inferred::Text, 0, 0);
    }
    if looks_numeric(t) && !t.contains(['e', 'E']) && (no_lead_zero || t.starts_with("0.") || t.starts_with("-0.")) {
        let digits = t.trim_start_matches(['-', '+']);
        let (ip, fp) = digits.split_once('.').unwrap_or((digits, ""));
        return (Inferred::Decimal, ip.len().max(1), fp.len());
    }
    if looks_numeric(t) {
        return (Inferred::Double, 0, 0);
    }
    if parse_date(t).is_some() {
        return (Inferred::Date, 0, 0);
    }
    if parse_datetime(t).is_some() {
        return (Inferred::Timestamp, 0, 0);
    }
    (Inferred::Text, 0, 0)
}

fn merge(a: Inferred, b: Inferred) -> Inferred {
    use Inferred::*;
    match (a, b) {
        (Unknown, x) | (x, Unknown) => x,
        (x, y) if x == y => x,
        (Int, BigInt) | (BigInt, Int) => BigInt,
        (Int | BigInt | Decimal, Decimal) | (Decimal, Int | BigInt) => Decimal,
        (Int | BigInt | Decimal | Double, Double) | (Double, Int | BigInt | Decimal) => Double,
        (Date, Timestamp) | (Timestamp, Date) => Timestamp,
        _ => Text,
    }
}

/// Infer a SQL column type per column: integer/bigint, numeric(p,s)/double, boolean, date,
/// timestamp, JSON, else varchar(n) (rounded up) or text for long values.
pub fn infer_column_types(data: &ImportedData, dialect: Dialect) -> Vec<String> {
    (0..data.columns.len())
        .map(|i| {
            let mut st = ColumnStats { kind: Inferred::Unknown, max_len: 0, int_digits: 0, scale: 0 };
            for row in &data.rows {
                let v = row.get(i).unwrap_or(&Value::Null);
                let (kind, idig, scale) = match v {
                    Value::Null => (Inferred::Unknown, 0, 0),
                    Value::Bool(_) => (Inferred::Bool, 0, 0),
                    Value::Int(n) => (if i32::try_from(*n).is_ok() { Inferred::Int } else { Inferred::BigInt }, 0, 0),
                    Value::UInt(n) => (if *n <= i64::MAX as u64 { Inferred::BigInt } else { Inferred::Decimal }, 20, 0),
                    Value::Float(_) => (Inferred::Double, 0, 0),
                    Value::Decimal(d) => classify_text(d),
                    Value::Date(_) => (Inferred::Date, 0, 0),
                    Value::DateTime(_) | Value::DateTimeTz(_) => (Inferred::Timestamp, 0, 0),
                    Value::Json(_) | Value::Array(_) => (Inferred::Json, 0, 0),
                    Value::Text(s) => classify_text(s),
                    _ => (Inferred::Text, 0, 0),
                };
                if !v.is_null() {
                    st.max_len = st.max_len.max(v.edit_text().chars().count());
                }
                st.int_digits = st.int_digits.max(idig);
                st.scale = st.scale.max(scale);
                st.kind = merge(st.kind, kind);
            }
            render_inferred(&st, dialect)
        })
        .collect()
}

fn render_inferred(st: &ColumnStats, d: Dialect) -> String {
    use Dialect::*;
    match st.kind {
        Inferred::Bool => match d {
            MySql => "boolean",
            MsSql => "bit",
            _ => "boolean",
        }
        .into(),
        Inferred::Int => match d {
            MySql | MsSql => "int",
            _ => "integer",
        }
        .into(),
        Inferred::BigInt => match d {
            Sqlite => "integer",
            _ => "bigint",
        }
        .into(),
        Inferred::Decimal => {
            let p = st.int_digits + st.scale;
            if p > 38 {
                return render_inferred(&ColumnStats { kind: Inferred::Double, ..*st }, d);
            }
            let p = p.max(1);
            match d {
                MySql | MsSql => format!("decimal({p},{})", st.scale),
                _ => format!("numeric({p},{})", st.scale),
            }
        }
        Inferred::Double => match d {
            MySql => "double",
            MsSql => "float",
            Sqlite => "real",
            _ => "double precision",
        }
        .into(),
        Inferred::Date => "date".into(),
        Inferred::Timestamp => match d {
            MySql => "datetime",
            MsSql => "datetime2",
            Sqlite => "datetime",
            _ => "timestamp",
        }
        .into(),
        Inferred::Json => match d {
            Postgres => "jsonb",
            MySql | Generic => "json",
            MsSql => "nvarchar(max)",
            Sqlite => "text",
        }
        .into(),
        Inferred::Text | Inferred::Unknown => {
            let n = st.max_len;
            if st.kind == Inferred::Unknown {
                return match d {
                    MySql => "varchar(255)".into(),
                    MsSql => "nvarchar(255)".into(),
                    _ => "text".into(),
                };
            }
            let bucket = [16usize, 32, 64, 128, 255, 512, 1024, 2048, 4000].into_iter().find(|&b| b >= n);
            match (bucket, d) {
                (Some(b), MsSql) => format!("nvarchar({b})"),
                (Some(b), _) => format!("varchar({b})"),
                (None, MsSql) => "nvarchar(max)".into(),
                (None, MySql) => "longtext".into(),
                (None, _) => "text".into(),
            }
        }
    }
}

/// `CREATE TABLE` (without terminator) with the given column names and types.
pub fn create_table_sql(table: &ObjectRef, columns: &[String], types: &[String], dialect: Dialect) -> String {
    let cols: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("    {} {}", q(dialect, c), types.get(i).map(String::as_str).unwrap_or("text")))
        .collect();
    format!("CREATE TABLE {} (\n{}\n)", qname(table, dialect), cols.join(",\n"))
}

fn is_textual(ty: &str) -> bool {
    let t = ty.to_ascii_lowercase();
    ["char", "text", "clob", "string", "enum", "set"].iter().any(|k| t.contains(k))
}

/// Multi-row `INSERT` statements (without terminators) in batches of `batch_size` rows
/// (capped at 1000 for SQL Server). With `types`, text values are converted with
/// [`Value::parse_for_type`] first so numbers/booleans become unquoted literals; empty text
/// in a non-text column becomes NULL.
pub fn insert_statements(
    table: &ObjectRef,
    columns: &[String],
    rows: &[Vec<Value>],
    types: Option<&[String]>,
    dialect: Dialect,
    batch_size: usize,
) -> Vec<String> {
    let mut batch = batch_size.max(1);
    if dialect == Dialect::MsSql {
        batch = batch.min(1000);
    }
    let prefix = format!(
        "INSERT INTO {} ({}) VALUES\n",
        qname(table, dialect),
        columns.iter().map(|c| q(dialect, c)).collect::<Vec<_>>().join(", ")
    );
    rows.chunks(batch)
        .map(|chunk| {
            let tuples: Vec<String> = chunk
                .iter()
                .map(|row| {
                    let vals: Vec<String> = (0..columns.len())
                        .map(|i| {
                            let v = row.get(i).unwrap_or(&Value::Null);
                            let v = match (v, types.and_then(|t| t.get(i))) {
                                (Value::Text(s), Some(ty)) if s.trim().is_empty() && !is_textual(ty) => Value::Null,
                                (Value::Text(s), Some(ty)) => Value::parse_for_type(s, ty),
                                _ => v.clone(),
                            };
                            v.to_sql_literal(dialect)
                        })
                        .collect();
                    format!("  ({})", vals.join(", "))
                })
                .collect();
            format!("{prefix}{}", tuples.join(",\n"))
        })
        .collect()
}

#[cfg(test)]
mod tests;
