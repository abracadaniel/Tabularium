//! Result-set export: CSV/TSV, JSON, JSON Lines, SQL INSERT/UPDATE, Markdown, HTML, XML,
//! XLSX and ASCII text tables.
//!
//! Cell text rules shared by the text formats:
//! * NULL is written as [`ExportOptions::null_text`] (JSON uses `null`, XML an empty element
//!   with `xsi:nil="true"`, SQL `NULL`).
//! * Binary values are written as `0x`-prefixed upper-case hex in CSV/TSV/Markdown/HTML/text
//!   tables, and as base64 in JSON and XML.
//! * [`ExportOptions::date_format`] (a chrono `strftime` pattern) is applied to dates and
//!   timestamps; when it cannot be applied (e.g. time fields on a date) ISO format is used.
//!
//! Duplicate column names are made unique (`_2`, `_3`, ...) for JSON and XML keys.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use tabularium_core::{ColumnMeta, Dialect, ObjectRef, Value};
use unicode_width::UnicodeWidthStr;

use crate::util::{dedupe_names, q, qname};

/// Supported export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // variant names are self-describing
pub enum ExportFormat {
    Csv,
    Tsv,
    /// Array of objects.
    Json,
    /// One object per line.
    JsonLines,
    /// One `INSERT` per row.
    SqlInsert,
    /// Multi-row `INSERT ... VALUES (..), (..)` in batches of `rows_per_insert`.
    SqlInsertMultiRow,
    /// `UPDATE ... SET ... WHERE <key_columns>`.
    SqlUpdate,
    Markdown,
    Html,
    Xml,
    Xlsx,
    /// ASCII box table like the psql/mysql command line clients print.
    TextTable,
}

impl ExportFormat {
    /// Every format, in menu order.
    pub const ALL: &'static [ExportFormat] = &[
        ExportFormat::Csv,
        ExportFormat::Tsv,
        ExportFormat::Json,
        ExportFormat::JsonLines,
        ExportFormat::SqlInsert,
        ExportFormat::SqlInsertMultiRow,
        ExportFormat::SqlUpdate,
        ExportFormat::Markdown,
        ExportFormat::Html,
        ExportFormat::Xml,
        ExportFormat::Xlsx,
        ExportFormat::TextTable,
    ];

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::JsonLines => "JSON Lines",
            ExportFormat::SqlInsert => "SQL INSERT",
            ExportFormat::SqlInsertMultiRow => "SQL INSERT (multi-row)",
            ExportFormat::SqlUpdate => "SQL UPDATE",
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Html => "HTML table",
            ExportFormat::Xml => "XML",
            ExportFormat::Xlsx => "Excel (XLSX)",
            ExportFormat::TextTable => "Text table",
        }
    }

    /// File extension without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::JsonLines => "jsonl",
            ExportFormat::SqlInsert | ExportFormat::SqlInsertMultiRow | ExportFormat::SqlUpdate => "sql",
            ExportFormat::Markdown => "md",
            ExportFormat::Html => "html",
            ExportFormat::Xml => "xml",
            ExportFormat::Xlsx => "xlsx",
            ExportFormat::TextTable => "txt",
        }
    }

    /// `true` for formats producing binary output (cannot be exported to a `String`).
    pub fn is_binary(self) -> bool {
        matches!(self, ExportFormat::Xlsx)
    }
}

/// Export settings.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Output format.
    pub format: ExportFormat,
    /// Write a header row (CSV/TSV/HTML/XLSX/text table; Markdown always has one).
    pub include_header: bool,
    /// Dialect for SQL literals and identifiers.
    pub dialect: Dialect,
    /// Target table for SQL formats (default `my_table`); also used as HTML title / sheet name.
    pub table: Option<ObjectRef>,
    /// Columns identifying a row for [`ExportFormat::SqlUpdate`].
    pub key_columns: Vec<String>,
    /// Text written for NULL in text formats.
    pub null_text: String,
    /// CSV/TSV delimiter override (must be ASCII).
    pub delimiter: Option<char>,
    /// Quote every CSV field, not only those that need it.
    pub quote_all: bool,
    /// Pretty-print JSON/XML.
    pub pretty: bool,
    /// Rows per statement for [`ExportFormat::SqlInsertMultiRow`].
    pub rows_per_insert: usize,
    /// chrono `strftime` pattern for dates and timestamps.
    pub date_format: Option<String>,
}

impl ExportOptions {
    /// Defaults: header on, Generic dialect, empty null text, 100 rows per insert, pretty.
    pub fn new(format: ExportFormat) -> Self {
        Self {
            format,
            include_header: true,
            dialect: Dialect::Generic,
            table: None,
            key_columns: Vec::new(),
            null_text: String::new(),
            delimiter: None,
            quote_all: false,
            pretty: true,
            rows_per_insert: 100,
            date_format: None,
        }
    }

    fn table_name(&self) -> String {
        match &self.table {
            Some(t) => qname(t, self.dialect),
            None => q(self.dialect, "my_table"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cell rendering
// ---------------------------------------------------------------------------------------------

/// Text of a non-null cell for text formats.
fn cell_text(v: &Value, opts: &ExportOptions) -> Option<String> {
    let fmt_with = |f: &dyn Fn(&mut String, &str) -> std::fmt::Result| -> Option<String> {
        let pat = opts.date_format.as_deref()?;
        let mut s = String::new();
        f(&mut s, pat).ok().map(|_| s)
    };
    Some(match v {
        Value::Null => return None,
        Value::Bytes(b) => format!("0x{}", hex::encode_upper(b)),
        Value::Date(d) => fmt_with(&|s, p| write!(s, "{}", d.format(p))).unwrap_or_else(|| v.to_string()),
        Value::DateTime(d) => fmt_with(&|s, p| write!(s, "{}", d.format(p))).unwrap_or_else(|| v.to_string()),
        Value::DateTimeTz(d) => fmt_with(&|s, p| write!(s, "{}", d.format(p))).unwrap_or_else(|| v.to_string()),
        other => other.to_string(),
    })
}

fn cell_or_null(v: &Value, opts: &ExportOptions) -> String {
    cell_text(v, opts).unwrap_or_else(|| opts.null_text.clone())
}

/// JSON value of a cell (dates honour `date_format`).
fn cell_json(v: &Value, opts: &ExportOptions) -> serde_json::Value {
    match v {
        Value::Date(_) | Value::DateTime(_) | Value::DateTimeTz(_) if opts.date_format.is_some() => {
            serde_json::Value::String(cell_text(v, opts).unwrap_or_default())
        }
        _ => v.to_json(),
    }
}

fn row_object(keys: &[String], row: &[Value], opts: &ExportOptions) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::with_capacity(keys.len());
    for (i, k) in keys.iter().enumerate() {
        m.insert(k.clone(), row.get(i).map(|v| cell_json(v, opts)).unwrap_or(serde_json::Value::Null));
    }
    m
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Escape XML text, dropping characters not allowed in XML 1.0.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' => out.push(c),
            '\r' => out.push_str("&#13;"),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            _ => out.push(c),
        }
    }
    out
}

/// Turn a column name into a valid XML element name.
fn xml_element_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.chars().enumerate() {
        let ok = if i == 0 {
            c.is_alphabetic() || c == '_'
        } else {
            c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
        };
        if ok {
            out.push(c);
        } else if i == 0 && (c.is_ascii_digit() || c == '-' || c == '.') {
            out.push('_');
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("column");
    }
    if out.to_ascii_lowercase().starts_with("xml") {
        out.insert(0, '_');
    }
    out
}

fn md_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace("\r\n", "<br>").replace(['\n', '\r'], "<br>")
}

fn text_table_cell(s: &str) -> String {
    s.replace("\r\n", "\\n").replace('\n', "\\n").replace('\r', "\\r").replace('\t', "    ")
}

fn pad(s: &str, width: usize, right: bool) -> String {
    let w = UnicodeWidthStr::width(s);
    let fill = " ".repeat(width.saturating_sub(w));
    if right {
        format!("{fill}{s}")
    } else {
        format!("{s}{fill}")
    }
}

fn sql_literal(v: &Value, d: Dialect) -> String {
    v.to_sql_literal(d)
}

fn where_clause(cols: &[(String, &Value)], d: Dialect) -> String {
    cols.iter()
        .map(|(c, v)| {
            if v.is_null() {
                format!("{} IS NULL", q(d, c))
            } else {
                format!("{} = {}", q(d, c), sql_literal(v, d))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

// ---------------------------------------------------------------------------------------------
// Encoder (shared by one-shot and streaming export)
// ---------------------------------------------------------------------------------------------

struct Encoder {
    columns: Vec<ColumnMeta>,
    keys: Vec<String>,
    opts: ExportOptions,
    rows_written: usize,
    /// Rows held back: multi-row INSERT remainder, or all rows for Markdown/TextTable which
    /// need column widths.
    pending: Vec<Vec<Value>>,
    key_idx: Vec<usize>,
}

impl Encoder {
    fn new(columns: Vec<ColumnMeta>, opts: ExportOptions) -> Result<Self> {
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let keys = match opts.format {
            ExportFormat::Xml => dedupe_names(&names.iter().map(|n| xml_element_name(n)).collect::<Vec<_>>()),
            _ => dedupe_names(&names),
        };
        let mut key_idx = Vec::new();
        if opts.format == ExportFormat::SqlUpdate {
            if opts.key_columns.is_empty() {
                bail!("SQL UPDATE export requires key columns");
            }
            for k in &opts.key_columns {
                let i = names
                    .iter()
                    .position(|n| n == k)
                    .or_else(|| names.iter().position(|n| n.eq_ignore_ascii_case(k)))
                    .ok_or_else(|| anyhow!("key column {k} is not part of the exported columns"))?;
                key_idx.push(i);
            }
        }
        if matches!(opts.format, ExportFormat::Csv | ExportFormat::Tsv) {
            if let Some(c) = opts.delimiter {
                if !c.is_ascii() {
                    bail!("CSV delimiter must be an ASCII character");
                }
            }
        }
        Ok(Self { columns, keys, opts, rows_written: 0, pending: Vec::new(), key_idx })
    }

    fn csv_writer<'a>(&self, buf: &'a mut Vec<u8>) -> csv::Writer<&'a mut Vec<u8>> {
        let default = if self.opts.format == ExportFormat::Tsv { '\t' } else { ',' };
        let delim = self.opts.delimiter.unwrap_or(default) as u8;
        csv::WriterBuilder::new()
            .delimiter(delim)
            .has_headers(false)
            .flexible(true)
            .quote_style(if self.opts.quote_all { csv::QuoteStyle::Always } else { csv::QuoteStyle::Necessary })
            .from_writer(buf)
    }

    fn begin(&mut self, out: &mut dyn Write) -> Result<()> {
        let opts = &self.opts;
        match opts.format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                if opts.include_header {
                    let mut buf = Vec::new();
                    {
                        let mut w = self.csv_writer(&mut buf);
                        w.write_record(self.columns.iter().map(|c| c.name.as_str()))?;
                        w.flush()?;
                    }
                    out.write_all(&buf)?;
                }
            }
            ExportFormat::Json => out.write_all(b"[")?,
            ExportFormat::Html => {
                let title = opts.table.as_ref().map(|t| t.display_name()).unwrap_or_else(|| "Export".into());
                let mut s = String::new();
                s.push_str("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
                s.push_str(&format!("<title>{}</title>\n", html_escape(&title)));
                s.push_str(
                    "<style>\ntable { border-collapse: collapse; font-family: sans-serif; font-size: 13px; }\n\
                     th, td { border: 1px solid #ccc; padding: 4px 8px; vertical-align: top; }\n\
                     th { background: #f0f0f0; text-align: left; }\n\
                     td.num { text-align: right; }\ntd.null { color: #999; font-style: italic; }\n</style>\n",
                );
                s.push_str("</head>\n<body>\n<table>\n");
                if opts.include_header {
                    s.push_str("<thead>\n<tr>");
                    for c in &self.columns {
                        s.push_str(&format!("<th>{}</th>", html_escape(&c.name)));
                    }
                    s.push_str("</tr>\n</thead>\n");
                }
                s.push_str("<tbody>\n");
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::Xml => {
                let nl = if opts.pretty { "\n" } else { "" };
                write!(
                    out,
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>{nl}<data xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">{nl}"
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    fn rows(&mut self, rows: &[Vec<Value>], out: &mut dyn Write) -> Result<()> {
        let d = self.opts.dialect;
        match self.opts.format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                let mut buf = Vec::new();
                {
                    let mut w = self.csv_writer(&mut buf);
                    for row in rows {
                        w.write_record(row.iter().map(|v| cell_or_null(v, &self.opts)))?;
                    }
                    w.flush()?;
                }
                out.write_all(&buf)?;
            }
            ExportFormat::Json => {
                let mut s = String::new();
                for row in rows {
                    if self.rows_written > 0 {
                        s.push(',');
                    }
                    let obj = serde_json::Value::Object(row_object(&self.keys, row, &self.opts));
                    if self.opts.pretty {
                        let pretty = serde_json::to_string_pretty(&obj)?;
                        s.push_str("\n  ");
                        s.push_str(&pretty.replace('\n', "\n  "));
                    } else {
                        s.push_str(&serde_json::to_string(&obj)?);
                    }
                    self.rows_written += 1;
                }
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::JsonLines => {
                let mut s = String::new();
                for row in rows {
                    let obj = serde_json::Value::Object(row_object(&self.keys, row, &self.opts));
                    s.push_str(&serde_json::to_string(&obj)?);
                    s.push('\n');
                }
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::SqlInsert => {
                let prefix = self.insert_prefix();
                let mut s = String::new();
                for row in rows {
                    let vals: Vec<String> = row.iter().map(|v| sql_literal(v, d)).collect();
                    s.push_str(&format!("{prefix} VALUES ({});\n", vals.join(", ")));
                }
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::SqlInsertMultiRow => {
                self.pending.extend(rows.iter().cloned());
                let batch = self.batch_size();
                while self.pending.len() >= batch {
                    let chunk: Vec<Vec<Value>> = self.pending.drain(..batch).collect();
                    self.write_multi_insert(&chunk, out)?;
                }
            }
            ExportFormat::SqlUpdate => {
                let t = self.opts.table_name();
                let mut s = String::new();
                for row in rows {
                    let set: Vec<String> = self
                        .columns
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| !self.key_idx.contains(i))
                        .map(|(i, c)| format!("{} = {}", q(d, &c.name), sql_literal(row.get(i).unwrap_or(&Value::Null), d)))
                        .collect();
                    if set.is_empty() {
                        continue;
                    }
                    let key: Vec<(String, &Value)> = self
                        .key_idx
                        .iter()
                        .map(|&i| (self.columns[i].name.clone(), row.get(i).unwrap_or(&Value::Null)))
                        .collect();
                    s.push_str(&format!("UPDATE {t} SET {} WHERE {};\n", set.join(", "), where_clause(&key, d)));
                }
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::Html => {
                let mut s = String::new();
                for row in rows {
                    s.push_str("<tr>");
                    for (i, v) in row.iter().enumerate() {
                        let num = self.columns.get(i).is_some_and(|c| c.is_numeric_type()) || v.is_numeric();
                        match cell_text(v, &self.opts) {
                            None => s.push_str(&format!("<td class=\"null\">{}</td>", html_escape(&self.opts.null_text))),
                            Some(t) if num => s.push_str(&format!("<td class=\"num\">{}</td>", html_escape(&t))),
                            Some(t) => s.push_str(&format!("<td>{}</td>", html_escape(&t))),
                        }
                    }
                    s.push_str("</tr>\n");
                }
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::Xml => {
                let (nl, i1, i2) = if self.opts.pretty { ("\n", "  ", "    ") } else { ("", "", "") };
                let mut s = String::new();
                for row in rows {
                    s.push_str(&format!("{i1}<row>{nl}"));
                    for (i, k) in self.keys.iter().enumerate() {
                        let v = row.get(i).unwrap_or(&Value::Null);
                        match v {
                            Value::Null => s.push_str(&format!("{i2}<{k} xsi:nil=\"true\"/>{nl}")),
                            Value::Bytes(_) => {
                                let text = v.to_json().as_str().unwrap_or_default().to_string();
                                s.push_str(&format!("{i2}<{k}>{text}</{k}>{nl}"));
                            }
                            _ => {
                                let text = cell_text(v, &self.opts).unwrap_or_default();
                                s.push_str(&format!("{i2}<{k}>{}</{k}>{nl}", xml_escape(&text)));
                            }
                        }
                    }
                    s.push_str(&format!("{i1}</row>{nl}"));
                }
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::Markdown | ExportFormat::TextTable => self.pending.extend(rows.iter().cloned()),
            ExportFormat::Xlsx => bail!("XLSX is not a streaming text format"),
        }
        Ok(())
    }

    fn batch_size(&self) -> usize {
        let n = self.opts.rows_per_insert.max(1);
        // SQL Server allows at most 1000 rows in a VALUES list.
        if self.opts.dialect == Dialect::MsSql {
            n.min(1000)
        } else {
            n
        }
    }

    fn insert_prefix(&self) -> String {
        let d = self.opts.dialect;
        let cols: Vec<String> = self.columns.iter().map(|c| q(d, &c.name)).collect();
        format!("INSERT INTO {} ({})", self.opts.table_name(), cols.join(", "))
    }

    fn write_multi_insert(&self, rows: &[Vec<Value>], out: &mut dyn Write) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let d = self.opts.dialect;
        let mut s = format!("{} VALUES\n", self.insert_prefix());
        for (i, row) in rows.iter().enumerate() {
            let vals: Vec<String> = row.iter().map(|v| sql_literal(v, d)).collect();
            s.push_str(&format!("  ({})", vals.join(", ")));
            s.push_str(if i + 1 == rows.len() { ";\n" } else { ",\n" });
        }
        out.write_all(s.as_bytes())?;
        Ok(())
    }

    fn finish(&mut self, out: &mut dyn Write) -> Result<()> {
        match self.opts.format {
            ExportFormat::Json => {
                if self.opts.pretty && self.rows_written > 0 {
                    out.write_all(b"\n]\n")?;
                } else if self.opts.pretty {
                    out.write_all(b"]\n")?;
                } else {
                    out.write_all(b"]")?;
                }
            }
            ExportFormat::SqlInsertMultiRow => {
                let rest = std::mem::take(&mut self.pending);
                self.write_multi_insert(&rest, out)?;
            }
            ExportFormat::Html => out.write_all(b"</tbody>\n</table>\n</body>\n</html>\n")?,
            ExportFormat::Xml => {
                let nl = if self.opts.pretty { "\n" } else { "" };
                write!(out, "</data>{nl}")?;
            }
            ExportFormat::Markdown => {
                let s = self.render_markdown();
                out.write_all(s.as_bytes())?;
            }
            ExportFormat::TextTable => {
                let s = self.render_text_table();
                out.write_all(s.as_bytes())?;
            }
            _ => {}
        }
        out.flush()?;
        Ok(())
    }

    fn numeric_columns(&self) -> Vec<bool> {
        self.columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                c.is_numeric_type()
                    || (c.type_name.is_empty()
                        && !self.pending.is_empty()
                        && self.pending.iter().all(|r| r.get(i).is_none_or(|v| v.is_null() || v.is_numeric())))
            })
            .collect()
    }

    fn render_markdown(&self) -> String {
        let numeric = self.numeric_columns();
        let header: Vec<String> = self.columns.iter().map(|c| md_escape(&c.name)).collect();
        let body: Vec<Vec<String>> = self
            .pending
            .iter()
            .map(|r| (0..self.columns.len()).map(|i| md_escape(&cell_or_null(r.get(i).unwrap_or(&Value::Null), &self.opts))).collect())
            .collect();
        let widths: Vec<usize> = (0..self.columns.len())
            .map(|i| {
                body.iter()
                    .map(|r| UnicodeWidthStr::width(r[i].as_str()))
                    .chain(std::iter::once(UnicodeWidthStr::width(header[i].as_str())))
                    .max()
                    .unwrap_or(0)
                    .max(3)
            })
            .collect();
        let mut s = String::new();
        let line = |cells: &[String], s: &mut String| {
            s.push('|');
            for (i, c) in cells.iter().enumerate() {
                s.push(' ');
                s.push_str(&pad(c, widths[i], numeric[i]));
                s.push_str(" |");
            }
            s.push('\n');
        };
        line(&header, &mut s);
        s.push('|');
        for (i, w) in widths.iter().enumerate() {
            if numeric[i] {
                s.push_str(&format!(" {}: |", "-".repeat(w - 1)));
            } else {
                s.push_str(&format!(" {} |", "-".repeat(*w)));
            }
        }
        s.push('\n');
        for r in &body {
            line(r, &mut s);
        }
        s
    }

    fn render_text_table(&self) -> String {
        let numeric = self.numeric_columns();
        let header: Vec<String> = self.columns.iter().map(|c| text_table_cell(&c.name)).collect();
        let body: Vec<Vec<String>> = self
            .pending
            .iter()
            .map(|r| {
                (0..self.columns.len())
                    .map(|i| text_table_cell(&cell_or_null(r.get(i).unwrap_or(&Value::Null), &self.opts)))
                    .collect()
            })
            .collect();
        let widths: Vec<usize> = (0..self.columns.len())
            .map(|i| {
                let h = if self.opts.include_header { UnicodeWidthStr::width(header[i].as_str()) } else { 0 };
                body.iter().map(|r| UnicodeWidthStr::width(r[i].as_str())).chain(std::iter::once(h)).max().unwrap_or(0)
            })
            .collect();
        let sep: String = {
            let mut s = String::from("+");
            for w in &widths {
                s.push_str(&"-".repeat(w + 2));
                s.push('+');
            }
            s.push('\n');
            s
        };
        let line = |cells: &[String], align: &dyn Fn(usize) -> bool| {
            let mut s = String::from("|");
            for (i, c) in cells.iter().enumerate() {
                s.push(' ');
                s.push_str(&pad(c, widths[i], align(i)));
                s.push_str(" |");
            }
            s.push('\n');
            s
        };
        let mut s = sep.clone();
        if self.opts.include_header {
            s.push_str(&line(&header, &|_| false));
            s.push_str(&sep);
        }
        for r in &body {
            s.push_str(&line(r, &|i| numeric[i]));
        }
        if !body.is_empty() || !self.opts.include_header {
            s.push_str(&sep);
        }
        s
    }
}

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

/// Export into any writer. Works for every format (XLSX is built in memory first).
pub fn export_to_writer(
    columns: &[ColumnMeta],
    rows: &[Vec<Value>],
    opts: &ExportOptions,
    out: &mut dyn Write,
) -> Result<()> {
    if opts.format == ExportFormat::Xlsx {
        let bytes = export_xlsx_to_buffer(columns, rows, opts)?;
        out.write_all(&bytes)?;
        out.flush()?;
        return Ok(());
    }
    let mut enc = Encoder::new(columns.to_vec(), opts.clone())?;
    enc.begin(out)?;
    enc.rows(rows, out)?;
    enc.finish(out)
}

/// Export a text format into a `String`. Errors for binary formats (XLSX).
pub fn export_to_string(columns: &[ColumnMeta], rows: &[Vec<Value>], opts: &ExportOptions) -> Result<String> {
    if opts.format.is_binary() {
        bail!("{} is a binary format and cannot be exported as text", opts.format.label());
    }
    let mut buf = Vec::new();
    export_to_writer(columns, rows, opts, &mut buf)?;
    Ok(String::from_utf8(buf)?)
}

/// Export into a file (any format, including XLSX).
pub fn export_to_file(columns: &[ColumnMeta], rows: &[Vec<Value>], opts: &ExportOptions, path: &Path) -> Result<()> {
    if opts.format == ExportFormat::Xlsx {
        let mut wb = build_workbook(columns, rows, opts)?;
        wb.save(path).with_context(|| format!("writing {}", path.display()))?;
        return Ok(());
    }
    let file = std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut w = std::io::BufWriter::new(file);
    export_to_writer(columns, rows, opts, &mut w)
}

/// Build an XLSX workbook in memory and return its bytes.
pub fn export_xlsx_to_buffer(columns: &[ColumnMeta], rows: &[Vec<Value>], opts: &ExportOptions) -> Result<Vec<u8>> {
    let mut wb = build_workbook(columns, rows, opts)?;
    Ok(wb.save_to_buffer()?)
}

const XLSX_MAX_ROWS: usize = 1_048_576;
const XLSX_MAX_STR: usize = 32_767;

fn sheet_name(opts: &ExportOptions) -> String {
    let base = opts.table.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| "Sheet1".into());
    let cleaned: String = base
        .chars()
        .map(|c| if matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') { '_' } else { c })
        .take(31)
        .collect();
    let cleaned = cleaned.trim_matches('\'').to_string();
    if cleaned.trim().is_empty() || cleaned.eq_ignore_ascii_case("history") {
        "Sheet1".into()
    } else {
        cleaned
    }
}

fn build_workbook(columns: &[ColumnMeta], rows: &[Vec<Value>], opts: &ExportOptions) -> Result<rust_xlsxwriter::Workbook> {
    use rust_xlsxwriter::{Format, Workbook};
    let header_rows = usize::from(opts.include_header);
    if rows.len() + header_rows > XLSX_MAX_ROWS {
        bail!("XLSX supports at most {XLSX_MAX_ROWS} rows; got {}", rows.len() + header_rows);
    }
    if columns.len() > 16_384 {
        bail!("XLSX supports at most 16384 columns");
    }
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name(sheet_name(opts))?;
    let bold = Format::new().set_bold();
    let date_fmt = Format::new().set_num_format("yyyy-mm-dd");
    let dt_fmt = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    let time_fmt = Format::new().set_num_format("hh:mm:ss");
    let mut widths: Vec<usize> = columns.iter().map(|c| if opts.include_header { c.name.chars().count() } else { 0 }).collect();
    if opts.include_header {
        for (i, c) in columns.iter().enumerate() {
            ws.write_string_with_format(0, i as u16, truncate_str(&c.name), &bold)?;
        }
        ws.set_freeze_panes(1, 0)?;
    }
    for (r, row) in rows.iter().enumerate() {
        let rr = (r + header_rows) as u32;
        for (c, v) in row.iter().enumerate().take(columns.len().max(row.len()).min(16_384)) {
            let cc = c as u16;
            let mut text_len = 0usize;
            match v {
                Value::Null => {
                    if !opts.null_text.is_empty() {
                        ws.write_string(rr, cc, opts.null_text.as_str())?;
                    }
                }
                Value::Bool(b) => {
                    ws.write_boolean(rr, cc, *b)?;
                    text_len = 5;
                }
                Value::Int(i) if i.unsigned_abs() < (1u64 << 53) => {
                    ws.write_number(rr, cc, *i as f64)?;
                    text_len = i.to_string().len();
                }
                Value::UInt(u) if *u < (1u64 << 53) => {
                    ws.write_number(rr, cc, *u as f64)?;
                    text_len = u.to_string().len();
                }
                Value::Float(f) if f.is_finite() => {
                    ws.write_number(rr, cc, *f)?;
                    text_len = f.to_string().len().min(15);
                }
                Value::Decimal(dv) if decimal_fits_f64(dv) => {
                    ws.write_number(rr, cc, dv.parse::<f64>().unwrap_or_default())?;
                    text_len = dv.len();
                }
                Value::Date(d) => {
                    ws.write_datetime_with_format(rr, cc, d, &date_fmt)?;
                    text_len = 10;
                }
                Value::DateTime(dt) => {
                    ws.write_datetime_with_format(rr, cc, dt, &dt_fmt)?;
                    text_len = 19;
                }
                Value::DateTimeTz(dt) => {
                    ws.write_datetime_with_format(rr, cc, dt.naive_local(), &dt_fmt)?;
                    text_len = 19;
                }
                Value::Time(t) => {
                    ws.write_datetime_with_format(rr, cc, t, &time_fmt)?;
                    text_len = 8;
                }
                other => {
                    let s = cell_text(other, opts).unwrap_or_default();
                    text_len = s.lines().map(|l| l.chars().count()).max().unwrap_or(0);
                    ws.write_string(rr, cc, truncate_str(&s))?;
                }
            }
            if let Some(w) = widths.get_mut(c) {
                if r < 1000 {
                    *w = (*w).max(text_len);
                }
            }
        }
    }
    for (i, w) in widths.iter().enumerate() {
        ws.set_column_width(i as u16, ((*w).clamp(6, 60) + 2) as f64)?;
    }
    Ok(wb)
}

fn truncate_str(s: &str) -> String {
    if s.chars().count() <= XLSX_MAX_STR {
        s.to_string()
    } else {
        s.chars().take(XLSX_MAX_STR).collect()
    }
}

fn decimal_fits_f64(d: &str) -> bool {
    let digits = d.chars().filter(|c| c.is_ascii_digit()).count();
    digits <= 15 && d.parse::<f64>().is_ok_and(f64::is_finite)
}

/// Streaming exporter for large result sets: the header is written by [`new`](Self::new),
/// rows by [`write_rows`](Self::write_rows) and the trailer (closing JSON array, HTML/XML
/// tags, final multi-row INSERT) by [`finish`](Self::finish).
///
/// Text formats only. Markdown and text tables need column widths, so their rows are
/// buffered in memory until `finish`.
pub struct StreamingExporter {
    enc: Encoder,
    out: Box<dyn Write + Send>,
}

impl StreamingExporter {
    /// Start an export; errors for XLSX.
    pub fn new(columns: Vec<ColumnMeta>, opts: ExportOptions, mut out: Box<dyn Write + Send>) -> Result<Self> {
        if opts.format.is_binary() {
            bail!("{} cannot be streamed; use export_to_file", opts.format.label());
        }
        let mut enc = Encoder::new(columns, opts)?;
        enc.begin(&mut out)?;
        Ok(Self { enc, out })
    }

    /// Append a batch of rows.
    pub fn write_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        self.enc.rows(rows, &mut self.out)
    }

    /// Write the trailer and flush.
    pub fn finish(mut self) -> Result<()> {
        self.enc.finish(&mut self.out)
    }
}

#[cfg(test)]
mod tests;
