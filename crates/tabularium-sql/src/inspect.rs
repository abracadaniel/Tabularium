//! Script inspections (squiggles in the editor).

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use sqlparser::dialect as sp;
use sqlparser::parser::{Parser, ParserError};
use tabularium_core::Dialect;

use crate::analyze::{self, kw, punct};
use crate::catalog::{self, CatalogTable};
use crate::lexer::{self, unquote, Lexer, Token, TokenKind};
use crate::split::split_statements;

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Style hint (e.g. `SELECT *`).
    Info,
    /// Probable problem (unresolved reference, `DELETE` without `WHERE`, ...).
    Warning,
    /// Definite error (syntax, unterminated literal, unbalanced parentheses).
    Error,
}

/// A problem found in the script; `start..end` is a byte range into the inspected source.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Start byte offset.
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
    /// Severity.
    pub severity: Severity,
    /// Human-readable message.
    pub message: String,
}

fn diag(start: usize, end: usize, severity: Severity, message: impl Into<String>) -> Diagnostic {
    Diagnostic { start, end, severity, message: message.into() }
}

/// First line of a (possibly huge) token, for highlighting.
fn first_line(src: &str, start: usize, end: usize) -> usize {
    src[start..end].find('\n').map_or(end, |k| start + k)
}

/// Inspections over a whole script:
/// - unterminated strings / comments / quoted identifiers and unbalanced parentheses (Error);
/// - syntax errors reported by `sqlparser` for the matching dialect, only for statement kinds
///   it understands well (Error for queries/DML in a specific dialect, Warning otherwise);
/// - `DELETE`/`UPDATE` without `WHERE` (Warning);
/// - unresolved tables/schemas when a non-empty catalog is supplied (Warning);
/// - unknown columns in `alias.column` when resolvable (Warning);
/// - `SELECT *` (Info).
pub fn inspect(src: &str, dialect: Dialect, catalog: Option<&catalog::SchemaCatalog>) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    // Lexical problems over the whole script.
    let mut lx = Lexer::new(src, dialect);
    let mut bad_ranges: Vec<(usize, usize)> = Vec::new();
    while let Some(t) = lx.next() {
        if lx.unterminated {
            let what = match t.kind {
                TokenKind::Comment => "Unterminated comment",
                TokenKind::QuotedIdentifier => "Unterminated quoted identifier",
                _ => "Unterminated string literal",
            };
            out.push(diag(t.start, first_line(src, t.start, t.end), Severity::Error, what));
            bad_ranges.push((t.start, t.end));
        }
    }

    let catalog = catalog.filter(|c| !c.is_empty());
    for stmt in split_statements(src, dialect) {
        let base = stmt.start;
        let text = stmt.text.as_str();
        let toks = lexer::significant(text, dialect);
        if toks.is_empty() {
            continue;
        }
        let lexical_bad = bad_ranges.iter().any(|&(s, e)| s < stmt.end && e > stmt.start);

        // Parentheses balance.
        let mut stack: Vec<&Token> = Vec::new();
        let mut paren_bad = false;
        for t in &toks {
            if punct(t, text, "(") {
                stack.push(t);
            } else if punct(t, text, ")") && stack.pop().is_none() {
                out.push(diag(base + t.start, base + t.end, Severity::Error, "Unmatched closing parenthesis"));
                paren_bad = true;
            }
        }
        for t in stack {
            out.push(diag(base + t.start, base + t.end, Severity::Error, "Unclosed parenthesis"));
            paren_bad = true;
        }

        let first = &toks[0];
        // DELETE / UPDATE without WHERE.
        if (kw(first, text, "DELETE") || kw(first, text, "UPDATE")) && !toks.iter().any(|t| kw(t, text, "WHERE")) {
            let verb = if kw(first, text, "DELETE") { "DELETE" } else { "UPDATE" };
            out.push(diag(
                base + first.start,
                base + first.end,
                Severity::Warning,
                format!("{verb} without WHERE affects all rows"),
            ));
        }

        // SELECT *
        for (i, t) in toks.iter().enumerate() {
            if t.kind == TokenKind::Operator && t.text(text) == "*" && i > 0 {
                let p = &toks[i - 1];
                if kw(p, text, "SELECT") || kw(p, text, "DISTINCT") || kw(p, text, "ALL") {
                    out.push(diag(base + t.start, base + t.end, Severity::Info, "SELECT * may return unnecessary columns"));
                }
            }
        }

        if !paren_bad && !lexical_bad {
            if let Some(sev) = syntax_check_severity(&toks, text, dialect) {
                if let Some(d) = syntax_error(text, &toks, dialect, sev) {
                    out.push(Diagnostic { start: base + d.start, end: base + d.end, ..d });
                }
            }
        }

        if let Some(cat) = catalog {
            resolve_refs(text, base, &toks, cat, &mut out);
        }
    }
    out.sort_by_key(|d| (d.start, d.end));
    out
}

const SKIP_SCHEMAS: &[&str] = &[
    "pg_catalog", "information_schema", "sys", "mysql", "performance_schema", "pg_toast", "temp",
    "main",
];

fn resolve_refs(text: &str, base: usize, toks: &[Token], cat: &catalog::SchemaCatalog, out: &mut Vec<Diagnostic>) {
    let refs = analyze::referenced_spans_toks(toks, text);
    let ctes = analyze::cte_names_toks(toks, text);
    let is_cte = |n: &str| ctes.iter().any(|c| c.eq_ignore_ascii_case(n));
    let creates_table = toks.first().is_some_and(|t| kw(t, text, "CREATE"));
    // alias/name (lower) -> resolved table (None when ambiguous/unknown)
    let mut scope: HashMap<String, Option<&CatalogTable>> = HashMap::new();
    for r in &refs {
        let name = &r.r.name;
        let skip_name = name.starts_with('#')
            || name.starts_with('@')
            || name.to_ascii_lowercase().starts_with("pg_")
            || name.to_ascii_lowercase().starts_with("sqlite_")
            || name.eq_ignore_ascii_case("dual");
        let cte = r.r.schema.is_none() && is_cte(name);
        let resolved = if cte { None } else { cat.find_table(r.r.schema.as_deref(), name) };
        let key = r.r.alias.as_deref().unwrap_or(name).to_lowercase();
        match scope.get(&key) {
            Some(Some(prev)) if resolved.is_some_and(|t| std::ptr::eq(t, *prev)) => {}
            Some(_) => {
                scope.insert(key, None);
            }
            None => {
                scope.insert(key, resolved);
            }
        }
        if cte || skip_name || resolved.is_some() || creates_table {
            continue;
        }
        if let (Some(schema), Some((ss, se))) = (&r.r.schema, r.schema_range) {
            if SKIP_SCHEMAS.iter().any(|s| s.eq_ignore_ascii_case(schema)) {
                continue;
            }
            if cat.schemas.is_empty() {
                continue;
            }
            if cat.find_schema(schema).is_none() {
                out.push(diag(base + ss, base + se, Severity::Warning, format!("Unknown schema '{schema}'")));
                continue;
            }
            // Schema known but possibly not introspected: only warn if we have tables for it.
            if cat.tables_in_schema(schema).next().is_none() {
                continue;
            }
        }
        out.push(diag(
            base + r.name_range.0,
            base + r.name_range.1,
            Severity::Warning,
            format!("Unresolved table '{name}'"),
        ));
    }

    // alias.column checks
    let in_ref = |pos: usize| refs.iter().any(|r| pos >= r.start && pos < r.end);
    let is_name = |t: &Token| matches!(t.kind, TokenKind::Identifier | TokenKind::QuotedIdentifier) || t.is_word();
    for i in 0..toks.len().saturating_sub(2) {
        let (a, dot, c) = (&toks[i], &toks[i + 1], &toks[i + 2]);
        if !(is_name(a) && punct(dot, text, ".") && is_name(c)) {
            continue;
        }
        if i > 0 && punct(&toks[i - 1], text, ".") {
            continue;
        }
        if toks.get(i + 3).is_some_and(|n| punct(n, text, ".") || punct(n, text, "(")) {
            continue;
        }
        if in_ref(a.start) {
            continue;
        }
        let q = unquote(a.text(text)).to_lowercase();
        let Some(Some(table)) = scope.get(&q) else { continue };
        if table.columns.is_empty() {
            continue;
        }
        let col = unquote(c.text(text));
        if table.find_column(&col).is_none() {
            out.push(diag(
                base + c.start,
                base + c.end,
                Severity::Warning,
                format!("Unknown column '{col}' in '{}'", table.name),
            ));
        }
    }
}

/// Decide whether sqlparser should check this statement, and with which severity.
fn syntax_check_severity(toks: &[Token], text: &str, d: Dialect) -> Option<Severity> {
    // Constructs sqlparser does not (fully) understand: dollar quotes, MSSQL batches, etc.
    if toks.iter().any(|t| t.kind == TokenKind::String && t.text(text).starts_with('$')) {
        return None;
    }
    if toks.iter().any(|t| t.kind == TokenKind::Unknown) {
        return None;
    }
    let w: Vec<String> = toks.iter().take(6).filter(|t| t.is_word()).map(|t| t.text(text).to_ascii_uppercase()).collect();
    let first = w.first()?.as_str();
    let strong = if d == Dialect::Generic { Severity::Warning } else { Severity::Error };
    match first {
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH" | "VALUES" => Some(strong),
        "CREATE" => {
            let mut k = 1;
            while k < w.len() && matches!(w[k].as_str(), "OR" | "REPLACE" | "TEMP" | "TEMPORARY" | "UNIQUE" | "GLOBAL" | "LOCAL") {
                k += 1;
            }
            match w.get(k).map(String::as_str) {
                Some("TABLE" | "VIEW" | "INDEX" | "SCHEMA") => Some(Severity::Warning),
                _ => None,
            }
        }
        "DROP" | "TRUNCATE" => Some(Severity::Warning),
        "ALTER" if w.get(1).is_some_and(|s| s == "TABLE") => Some(Severity::Warning),
        _ => None,
    }
}

fn sqlparser_dialect(d: Dialect) -> Box<dyn sp::Dialect> {
    match d {
        Dialect::Postgres => Box::new(sp::PostgreSqlDialect {}),
        Dialect::MySql => Box::new(sp::MySqlDialect {}),
        Dialect::Sqlite => Box::new(sp::SQLiteDialect {}),
        Dialect::MsSql => Box::new(sp::MsSqlDialect {}),
        Dialect::Generic => Box::new(sp::GenericDialect {}),
    }
}

/// Parse `Line: L, Column: C` out of a sqlparser message; returns the message without it.
fn split_location(msg: &str) -> (String, Option<(usize, usize)>) {
    if let Some(k) = msg.rfind(" at Line: ") {
        let loc = &msg[k + " at Line: ".len()..];
        let mut parts = loc.split(", Column: ");
        let line = parts.next().and_then(|s| s.trim().parse().ok());
        let col = parts.next().and_then(|s| {
            let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        });
        if let (Some(l), Some(c)) = (line, col) {
            return (msg[..k].to_string(), Some((l, c)));
        }
    }
    (msg.to_string(), None)
}

/// Convert a 1-based line/char-column to a byte offset in `text`.
fn line_col_to_offset(text: &str, line: usize, col: usize) -> Option<usize> {
    let mut cur_line = 1;
    let mut line_start = 0;
    if line == 0 || col == 0 {
        return None;
    }
    if line > 1 {
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                cur_line += 1;
                if cur_line == line {
                    line_start = i + 1;
                    break;
                }
            }
        }
        if cur_line != line {
            return None;
        }
    }
    let line_text = &text[line_start..];
    let mut off = line_start;
    for (n, (i, ch)) in line_text.char_indices().enumerate() {
        if n + 1 == col {
            return Some(line_start + i);
        }
        if ch == '\n' {
            break;
        }
        off = line_start + i + ch.len_utf8();
    }
    Some(off)
}

fn syntax_error(text: &str, toks: &[Token], d: Dialect, sev: Severity) -> Option<Diagnostic> {
    // Replace parameters by same-length placeholders so sqlparser accepts them and positions stay.
    let mut prepared = String::with_capacity(text.len());
    let mut last = 0;
    for p in analyze::find_parameters(text, d) {
        prepared.push_str(&text[last..p.start]);
        prepared.push('1');
        for _ in 1..(p.end - p.start) {
            prepared.push(' ');
        }
        last = p.end;
    }
    prepared.push_str(&text[last..]);

    let dialect = sqlparser_dialect(d);
    let result = catch_unwind(AssertUnwindSafe(|| Parser::parse_sql(dialect.as_ref(), &prepared)));
    let err = match result {
        Ok(Ok(_)) | Err(_) => return None,
        Ok(Err(e)) => e,
    };
    let msg = match err {
        ParserError::ParserError(m) | ParserError::TokenizerError(m) => m,
        ParserError::RecursionLimitExceeded => return None,
    };
    let (msg, loc) = split_location(&msg);
    let last_tok = toks.last()?;
    let found = msg.rfind("found: ").map(|k| msg[k + "found: ".len()..].trim());
    let (start, end) = match loc.and_then(|(l, c)| line_col_to_offset(text, l, c)) {
        Some(off) if off < text.len() => {
            let at = toks.iter().find(|t| t.start <= off && off < t.end).or_else(|| toks.iter().find(|t| t.start >= off));
            // sqlparser sometimes reports the location of a neighbouring token: prefer the
            // closest token at or before the location whose text is the reported one.
            let by_text = found.and_then(|f| {
                if at.is_some_and(|t| t.text(text).eq_ignore_ascii_case(f)) {
                    return None;
                }
                toks.iter().rev().find(|t| t.start <= off && t.text(text).eq_ignore_ascii_case(f))
            });
            match by_text.or(at) {
                Some(t) => (t.start, t.end),
                None => (last_tok.start, last_tok.end),
            }
        }
        _ => (last_tok.start, last_tok.end),
    };
    Some(diag(start, end, sev, format!("Syntax error: {msg}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::tests::sample;
    use crate::corpus::CORPUS;

    fn msgs(src: &str, d: Dialect) -> Vec<(Severity, String)> {
        inspect(src, d, Some(&sample())).into_iter().map(|d| (d.severity, d.message)).collect()
    }

    #[test]
    fn corpus_has_no_false_errors() {
        for (name, d, sql) in CORPUS {
            let diags = inspect(sql, *d, None);
            let errors: Vec<_> = diags.iter().filter(|x| x.severity >= Severity::Warning && x.message.starts_with("Syntax")).collect();
            assert!(errors.is_empty(), "{name}: {errors:?}");
            assert!(!diags.iter().any(|x| x.severity == Severity::Error), "{name}: {diags:?}");
        }
    }

    #[test]
    fn syntax_errors_are_located() {
        let src = "SELECT 1;\nSELECT * FORM users;";
        let d = inspect(src, Dialect::Postgres, None);
        let e = d.iter().find(|x| x.severity == Severity::Error).expect("error");
        assert!(e.message.starts_with("Syntax error"), "{e:?}");
        assert_eq!(&src[e.start..e.end], "FORM", "{e:?}");
        // Generic dialect only warns.
        let d = inspect("SELECT * FORM users", Dialect::Generic, None);
        assert!(d.iter().any(|x| x.severity == Severity::Warning && x.message.starts_with("Syntax")));
        // Non-ASCII before the error keeps byte offsets right.
        let src = "SELECT 'ü' FROM t ORDER x";
        let d = inspect(src, Dialect::Postgres, None);
        let e = d.iter().find(|x| x.severity == Severity::Error).unwrap_or_else(|| panic!("{d:?}"));
        assert!(src.is_char_boundary(e.start) && src.is_char_boundary(e.end));
        assert_eq!(&src[e.start..e.end], "ORDER");
        // Multi-line statement: the reported token is found by its text.
        let src = "SELECT 1;\nSELECT a,\n  FROM t";
        let d = inspect(src, Dialect::Postgres, None);
        let e = d.iter().find(|x| x.severity == Severity::Error).unwrap_or_else(|| panic!("{d:?}"));
        assert_eq!(&src[e.start..e.end], "FROM");
    }

    #[test]
    fn lexical_errors() {
        let m = msgs("SELECT 'abc", Dialect::Postgres);
        assert!(m.iter().any(|(s, x)| *s == Severity::Error && x == "Unterminated string literal"));
        let m = msgs("SELECT 1 /* abc", Dialect::Postgres);
        assert!(m.iter().any(|(_, x)| x == "Unterminated comment"));
        let m = msgs("SELECT (1", Dialect::Postgres);
        assert!(m.iter().any(|(_, x)| x == "Unclosed parenthesis"));
        let m = msgs("SELECT 1)", Dialect::Postgres);
        assert!(m.iter().any(|(_, x)| x == "Unmatched closing parenthesis"));
        assert!(!m.iter().any(|(_, x)| x.starts_with("Syntax")));
    }

    #[test]
    fn dml_without_where_and_select_star() {
        let m = msgs("DELETE FROM users; UPDATE users SET name = 'x'; DELETE FROM users WHERE id = 1", Dialect::Postgres);
        assert_eq!(m.iter().filter(|(s, _)| *s == Severity::Warning).count(), 2, "{m:?}");
        let m = msgs("SELECT * FROM users; SELECT count(*) FROM users; SELECT u.* FROM users u", Dialect::Postgres);
        assert_eq!(m.iter().filter(|(s, _)| *s == Severity::Info).count(), 1, "{m:?}");
    }

    #[test]
    fn unresolved_tables() {
        let m = msgs("SELECT id FROM nope", Dialect::Postgres);
        assert!(m.contains(&(Severity::Warning, "Unresolved table 'nope'".into())), "{m:?}");
        let m = msgs("SELECT id FROM USERS u JOIN sales.invoices i ON true", Dialect::Postgres);
        assert!(m.is_empty(), "{m:?}");
        let m = msgs("SELECT 1 FROM nosuch.t", Dialect::Postgres);
        assert!(m.contains(&(Severity::Warning, "Unknown schema 'nosuch'".into())), "{m:?}");
        let m = msgs("WITH x AS (SELECT id FROM users) SELECT id FROM x", Dialect::Postgres);
        assert!(m.is_empty(), "{m:?}");
        let m = msgs("SELECT * FROM pg_catalog.pg_class, information_schema.tables", Dialect::Postgres);
        assert!(!m.iter().any(|(s, _)| *s == Severity::Warning), "{m:?}");
        // No catalog: no resolution warnings.
        assert!(inspect("SELECT id FROM nope", Dialect::Postgres, None).is_empty());
    }

    #[test]
    fn unknown_columns() {
        let m = msgs("SELECT u.id, u.nmae, o.total FROM users u JOIN orders o ON o.user_id = u.id", Dialect::Postgres);
        assert_eq!(m, vec![(Severity::Warning, "Unknown column 'nmae' in 'users'".to_string())]);
        let m = msgs("SELECT users.EMAIL, public.users.id FROM public.users", Dialect::Postgres);
        assert!(m.is_empty(), "{m:?}");
        let m = msgs("SELECT s.anything FROM (SELECT 1 AS anything) s", Dialect::Postgres);
        assert!(m.is_empty(), "{m:?}");
    }

    #[test]
    fn helpers() {
        assert_eq!(split_location("Expected: x, found: y at Line: 2, Column: 5"), ("Expected: x, found: y".into(), Some((2, 5))));
        assert_eq!(line_col_to_offset("ab\ncdé f", 2, 5), Some(8));
        assert_eq!(line_col_to_offset("ab", 1, 99), Some(2));
        assert_eq!(line_col_to_offset("ab", 3, 1), None);
    }

    #[test]
    fn never_panics() {
        let inputs = ["", ";", "(((", "SELECT 'ü", "SELECT * FROM ☃.☃ x WHERE x.☃ = 1", "UPDATE", "DELETE", "x.y.z.", "SELECT ${", "SELECT a. FROM t a"];
        for s in inputs {
            for d in Dialect::ALL {
                for dg in inspect(s, d, Some(&sample())) {
                    assert!(dg.start <= dg.end && dg.end <= s.len());
                    assert!(s.is_char_boundary(dg.start) && s.is_char_boundary(dg.end));
                }
            }
        }
        // Deep nesting must not overflow the stack.
        let deep = format!("SELECT {}1{}", "(".repeat(5000), ")".repeat(5000));
        let _ = inspect(&deep, Dialect::Postgres, None);
    }
}
