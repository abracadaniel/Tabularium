//! Token-based statement analysis: referenced tables, editable single-table selects,
//! query parameters and statement classification. Tolerant of incomplete SQL.

use std::collections::HashMap;

use tabularium_core::Dialect;

use crate::lexer::{self, unquote, Token, TokenKind};
use crate::words;

/// A table referenced by a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// Schema qualifier if written (unquoted).
    pub schema: Option<String>,
    /// Table name (unquoted).
    pub name: String,
    /// Alias if given (unquoted).
    pub alias: Option<String>,
}

/// A table reference with source positions (crate internal).
#[derive(Debug, Clone)]
pub(crate) struct RefSpan {
    pub r: TableRef,
    /// Byte range of the whole (qualified) name.
    pub start: usize,
    pub end: usize,
    /// Byte range of the schema part, if any.
    pub schema_range: Option<(usize, usize)>,
    /// Byte range of the name part.
    pub name_range: (usize, usize),
}

/// Words that terminate a table reference / cannot be aliases.
const STOP_WORDS: &[&str] = &[
    "AS", "CROSS", "DEFAULT", "DO", "EXCEPT", "FETCH", "FOR", "FROM", "FULL", "GROUP", "HAVING",
    "INNER", "INTERSECT", "INTO", "JOIN", "LATERAL", "LEFT", "LIMIT", "MINUS", "NATURAL", "OFFSET",
    "ON", "ONLY", "ORDER", "OUTER", "OUTPUT", "QUALIFY", "RETURNING", "RIGHT", "SELECT", "SET",
    "STRAIGHT_JOIN", "TABLESAMPLE", "UNION", "USING", "VALUES", "WHERE", "WINDOW", "WITH", "APPLY",
    "PIVOT", "UNPIVOT", "USE", "IGNORE", "FORCE", "PARTITION", "WHEN", "THEN", "ELSE", "END",
    "IN", "IS", "NOT", "AND", "OR", "TOP", "LOCK", "OVERRIDING", "ORDINALITY", "OPTION",
    "CONNECT", "START", "MATCH", "BY",
];

fn is_stop(t: &Token, src: &str) -> bool {
    t.is_word() && STOP_WORDS.iter().any(|w| t.text(src).eq_ignore_ascii_case(w))
}

pub(crate) fn kw(t: &Token, src: &str, w: &str) -> bool {
    t.is_word() && t.text(src).eq_ignore_ascii_case(w)
}

pub(crate) fn punct(t: &Token, src: &str, p: &str) -> bool {
    t.kind == TokenKind::Punctuation && t.text(src) == p
}

/// Can this token name a table / schema?
fn is_name_tok(t: &Token, src: &str) -> bool {
    match t.kind {
        TokenKind::Identifier | TokenKind::QuotedIdentifier => true,
        TokenKind::Keyword | TokenKind::Type => {
            !is_stop(t, src) && !tabularium_core::dialect::is_reserved_word(t.text(src))
        }
        _ => false,
    }
}

/// Can this token be an alias?
fn is_alias_tok(t: &Token, src: &str) -> bool {
    match t.kind {
        TokenKind::Identifier | TokenKind::QuotedIdentifier => true,
        TokenKind::Keyword => !is_stop(t, src) && !tabularium_core::dialect::is_reserved_word(t.text(src)),
        _ => false,
    }
}

/// Matching close paren index for each `(` in `toks` (significant tokens).
pub(crate) fn match_parens(toks: &[Token], src: &str) -> Vec<Option<usize>> {
    let mut out = vec![None; toks.len()];
    let mut stack = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if punct(t, src, "(") {
            stack.push(i);
        } else if punct(t, src, ")") {
            if let Some(o) = stack.pop() {
                out[o] = Some(i);
                out[i] = Some(o);
            }
        }
    }
    out
}

/// Parse one table reference starting at `j`. Returns the ref (or None for a derived table /
/// table function) and the index after it, or None when no reference starts there.
fn parse_one(
    toks: &[Token],
    src: &str,
    j: usize,
    matches: &[Option<usize>],
    allow_alias: bool,
    stop_at_paren: bool,
) -> Option<(Option<RefSpan>, usize)> {
    let mut i = j;
    while i < toks.len() && kw(&toks[i], src, "ONLY") {
        i += 1;
    }
    let t = toks.get(i)?;
    if punct(t, src, "(") {
        // Derived table: skip it and its alias.
        let close = matches[i]?;
        let mut k = close + 1;
        k = skip_alias(toks, src, k, matches).1;
        return Some((None, k));
    }
    if !is_name_tok(t, src) && t.kind != TokenKind::Function {
        return None;
    }
    let mut parts: Vec<usize> = vec![i];
    let mut k = i + 1;
    while k + 1 < toks.len() && punct(&toks[k], src, ".") && (is_name_tok(&toks[k + 1], src) || toks[k + 1].is_word()) {
        parts.push(k + 1);
        k += 2;
    }
    // Table function call: `generate_series(...)`.
    if !stop_at_paren && k < toks.len() && punct(&toks[k], src, "(") {
        let close = matches[k]?;
        let k2 = skip_alias(toks, src, close + 1, matches).1;
        return Some((None, k2));
    }
    if toks[i].kind == TokenKind::Function && parts.len() == 1 {
        return None;
    }
    let name_idx = *parts.last()?;
    let schema_idx = if parts.len() >= 2 { Some(parts[parts.len() - 2]) } else { None };
    let mut r = RefSpan {
        r: TableRef {
            schema: schema_idx.map(|s| unquote(toks[s].text(src))),
            name: unquote(toks[name_idx].text(src)),
            alias: None,
        },
        start: toks[i].start,
        end: toks[name_idx].end,
        schema_range: schema_idx.map(|s| (toks[s].start, toks[s].end)),
        name_range: (toks[name_idx].start, toks[name_idx].end),
    };
    if allow_alias {
        let (alias, k2) = skip_alias(toks, src, k, matches);
        r.r.alias = alias;
        k = k2;
    }
    Some((Some(r), k))
}

/// Parse `[AS] alias [(col, ...)]` at `k`.
fn skip_alias(toks: &[Token], src: &str, mut k: usize, matches: &[Option<usize>]) -> (Option<String>, usize) {
    let mut alias = None;
    let had_as = k < toks.len() && kw(&toks[k], src, "AS");
    if had_as {
        k += 1;
    }
    if k < toks.len() && (is_alias_tok(&toks[k], src) || (had_as && toks[k].is_word())) {
        alias = Some(unquote(toks[k].text(src)));
        k += 1;
        if k < toks.len() && punct(&toks[k], src, "(") {
            if let Some(c) = matches[k] {
                k = c + 1;
            }
        }
    }
    (alias, k)
}

const INSERT_INTO_PREV: &[&str] =
    &["INSERT", "REPLACE", "MERGE", "IGNORE", "LOW_PRIORITY", "DELAYED", "HIGH_PRIORITY"];

/// Table references with positions.
pub(crate) fn referenced_spans(sql: &str, d: Dialect) -> Vec<RefSpan> {
    let toks = lexer::significant(sql, d);
    referenced_spans_toks(&toks, sql)
}

pub(crate) fn referenced_spans_toks(toks: &[Token], sql: &str) -> Vec<RefSpan> {
    let matches = match_parens(toks, sql);
    let mut out: Vec<RefSpan> = Vec::new();
    let mut parens: Vec<bool> = Vec::new();
    for i in 0..toks.len() {
        let t = &toks[i];
        if punct(t, sql, "(") {
            let sub = toks.get(i + 1).is_some_and(|n| {
                kw(n, sql, "SELECT") || kw(n, sql, "WITH") || kw(n, sql, "VALUES")
            });
            parens.push(sub);
            continue;
        }
        if punct(t, sql, ")") {
            parens.pop();
            continue;
        }
        if !parens.last().copied().unwrap_or(true) || !t.is_word() {
            continue;
        }
        let prev = if i > 0 { Some(&toks[i - 1]) } else { None };
        let text = t.text(sql);
        let (list, alias, stop_paren) = if text.eq_ignore_ascii_case("FROM") {
            if prev.is_some_and(|p| kw(p, sql, "DISTINCT")) {
                continue;
            }
            (true, true, false)
        } else if text.eq_ignore_ascii_case("JOIN") || text.eq_ignore_ascii_case("STRAIGHT_JOIN") {
            (false, true, false)
        } else if text.eq_ignore_ascii_case("UPDATE") {
            if prev.is_some_and(|p| kw(p, sql, "KEY") || kw(p, sql, "DO") || kw(p, sql, "FOR")) {
                continue;
            }
            (true, true, true)
        } else if text.eq_ignore_ascii_case("INTO") {
            if !prev.is_some_and(|p| INSERT_INTO_PREV.iter().any(|w| kw(p, sql, w))) {
                continue;
            }
            (false, true, true)
        } else if text.eq_ignore_ascii_case("TABLE") {
            if !prev.is_some_and(|p| kw(p, sql, "TRUNCATE") || kw(p, sql, "LOCK")) {
                continue;
            }
            (false, false, true)
        } else {
            continue;
        };
        let mut j = i + 1;
        while let Some((r, next)) = parse_one(toks, sql, j, &matches, alias, stop_paren) {
            if let Some(r) = r {
                out.push(r);
            }
            j = next;
            if list && j < toks.len() && punct(&toks[j], sql, ",") {
                j += 1;
            } else {
                break;
            }
        }
    }
    out
}

/// Tables referenced in FROM/JOIN/UPDATE/INSERT INTO/DELETE FROM of one statement
/// (tolerant of incomplete SQL; token based, not requiring a full parse).
/// Duplicate references are reported once.
pub fn referenced_tables(sql: &str, dialect: Dialect) -> Vec<TableRef> {
    let mut out: Vec<TableRef> = Vec::new();
    for r in referenced_spans(sql, dialect) {
        if !out.contains(&r.r) {
            out.push(r.r);
        }
    }
    out
}

/// Names defined by `WITH` clauses (common table expressions), unquoted.
pub(crate) fn cte_names_toks(toks: &[Token], src: &str) -> Vec<String> {
    let matches = match_parens(toks, src);
    let mut names = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if kw(&toks[i], src, "WITH") && !toks.get(i + 1).is_some_and(|n| punct(n, src, "(")) {
            let mut j = i + 1;
            if toks.get(j).is_some_and(|t| kw(t, src, "RECURSIVE")) {
                j += 1;
            }
            while let Some(name) = toks.get(j) {
                if !(name.is_word() || name.kind == TokenKind::QuotedIdentifier) {
                    break;
                }
                let mut k = j + 1;
                if toks.get(k).is_some_and(|t| punct(t, src, "(")) {
                    match matches[k] {
                        Some(c) => k = c + 1,
                        None => break,
                    }
                }
                if !toks.get(k).is_some_and(|t| kw(t, src, "AS")) {
                    break;
                }
                names.push(unquote(name.text(src)));
                k += 1;
                while toks.get(k).is_some_and(|t| kw(t, src, "NOT") || kw(t, src, "MATERIALIZED")) {
                    k += 1;
                }
                if !toks.get(k).is_some_and(|t| punct(t, src, "(")) {
                    break;
                }
                match matches[k] {
                    Some(c) => k = c + 1,
                    None => break,
                }
                if toks.get(k).is_some_and(|t| punct(t, src, ",")) {
                    j = k + 1;
                } else {
                    break;
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    names
}

/// If the statement is a plain `SELECT ... FROM one_table [WHERE/ORDER/LIMIT...]` with no
/// joins/group by/union/distinct/aggregates, return that table (used to make query results editable).
pub fn single_table_select(sql: &str, dialect: Dialect) -> Option<TableRef> {
    let toks = lexer::significant(sql, dialect);
    let toks: &[Token] = match toks.last() {
        Some(t) if punct(t, sql, ";") => &toks[..toks.len() - 1],
        _ => &toks,
    };
    if !kw(toks.first()?, sql, "SELECT") {
        return None;
    }
    let matches = match_parens(toks, sql);
    const REJECT: &[&str] = &[
        "JOIN", "GROUP", "HAVING", "UNION", "INTERSECT", "EXCEPT", "MINUS", "DISTINCT", "INTO",
        "WINDOW", "LATERAL", "APPLY", "PIVOT", "UNPIVOT", "OVER", "DISTINCTROW", "CONNECT",
    ];
    let mut parens: Vec<bool> = Vec::new();
    let mut from_idx = None;
    for (i, t) in toks.iter().enumerate() {
        if punct(t, sql, "(") {
            parens.push(toks.get(i + 1).is_some_and(|n| kw(n, sql, "SELECT") || kw(n, sql, "WITH")));
            continue;
        }
        if punct(t, sql, ")") {
            parens.pop();
            continue;
        }
        if parens.iter().any(|s| *s) {
            continue; // inside a subquery
        }
        if t.kind == TokenKind::Function || (t.is_word() && toks.get(i + 1).is_some_and(|n| punct(n, sql, "("))) {
            let mut buf = [0u8; 48];
            if words::lower_in(t.text(sql), &mut buf).is_some_and(|l| words::AGGREGATES.contains(&l)) {
                return None;
            }
        }
        if !parens.is_empty() {
            continue;
        }
        if t.is_word() && REJECT.iter().any(|w| kw(t, sql, w)) {
            return None;
        }
        if kw(t, sql, "FROM") {
            if from_idx.is_some() {
                return None;
            }
            from_idx = Some(i);
        }
    }
    let f = from_idx?;
    let (r, next) = parse_one(toks, sql, f + 1, &matches, true, false)?;
    let r = r?;
    match toks.get(next) {
        None => Some(r.r),
        Some(t) if ["WHERE", "ORDER", "LIMIT", "OFFSET", "FETCH", "FOR"].iter().any(|w| kw(t, sql, w)) => {
            Some(r.r)
        }
        _ => None,
    }
}

/// A query parameter occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    /// Parameter name: `name` for `:name` and `${name}`, `?1`, `?2`... for positional `?`
    /// (or `?N` as written for SQLite-style `?N`), and `$1` for `$1`.
    pub name: String,
    /// Start byte offset of the whole parameter token.
    pub start: usize,
    /// End byte offset (exclusive) of the parameter token.
    pub end: usize,
}

/// User parameters: `:name`, `${name}`, `?` (named "?1","?2"...) and `$1`. Strings, comments,
/// `::` casts and Postgres dollar-quoted bodies are skipped. `@var` variables are not parameters.
pub fn find_parameters(sql: &str, dialect: Dialect) -> Vec<Parameter> {
    let mut out = Vec::new();
    let mut positional = 0usize;
    for t in lexer::Lexer::new(sql, dialect) {
        if t.kind != TokenKind::Parameter {
            continue;
        }
        let text = t.text(sql);
        let name = if text == "?" {
            positional += 1;
            format!("?{positional}")
        } else if text.starts_with('?') {
            text.to_string()
        } else if let Some(inner) = text.strip_prefix("${") {
            inner.trim_end_matches('}').trim().to_string()
        } else if let Some(n) = text.strip_prefix(':') {
            n.to_string()
        } else if text.starts_with('$') {
            text.to_string()
        } else {
            continue; // @var
        };
        out.push(Parameter { name, start: t.start, end: t.end });
    }
    out
}

/// Replace parameters by the given literal SQL text (values are already SQL literals).
/// Unknown names are left as-is.
pub fn substitute_parameters(sql: &str, dialect: Dialect, values: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for p in find_parameters(sql, dialect) {
        if let Some(v) = values.get(&p.name) {
            out.push_str(&sql[last..p.start]);
            out.push_str(v);
            last = p.end;
        }
    }
    out.push_str(&sql[last..]);
    out
}

/// Broad statement category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    /// Returns rows (`SELECT`, `WITH ... SELECT`, `SHOW`, `EXPLAIN`, DML with `RETURNING`...).
    Query,
    /// Modifies data (`INSERT`, `UPDATE`, `DELETE`, `MERGE`...).
    Dml,
    /// Schema change (`CREATE`, `ALTER`, `DROP`, `TRUNCATE`, `GRANT`...).
    Ddl,
    /// Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`...).
    Transaction,
    /// Anything else (`SET`, `USE`, `CALL`, ...).
    Other,
}

fn depth0_has(toks: &[Token], src: &str, list: &[&str]) -> bool {
    let mut depth = 0i32;
    for t in toks {
        if punct(t, src, "(") {
            depth += 1;
        } else if punct(t, src, ")") {
            depth -= 1;
        } else if depth <= 0 && list.iter().any(|w| kw(t, src, w)) {
            return true;
        }
    }
    false
}

/// Classify a statement: Query (returns rows), Dml, Ddl, Transaction, Other.
/// DML with a top-level `RETURNING`/`OUTPUT` clause is classified as `Query`.
pub fn statement_kind(sql: &str) -> StatementKind {
    let toks = lexer::significant(sql, Dialect::Generic);
    let Some(first_idx) = toks.iter().position(|t| !punct(t, sql, "(")) else {
        return StatementKind::Other;
    };
    let first = &toks[first_idx];
    let rest = &toks[first_idx + 1..];
    let w = first.text(sql).to_ascii_uppercase();
    let next_is = |list: &[&str]| rest.first().is_some_and(|n| list.iter().any(|x| kw(n, sql, x)));
    let dml = |toks: &[Token]| {
        if depth0_has(toks, sql, &["RETURNING", "OUTPUT"]) {
            StatementKind::Query
        } else {
            StatementKind::Dml
        }
    };
    match w.as_str() {
        "SELECT" | "VALUES" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "PRAGMA" | "TABLE" => {
            StatementKind::Query
        }
        "WITH" => {
            if depth0_has(rest, sql, &["INSERT", "UPDATE", "DELETE", "MERGE"]) {
                dml(rest)
            } else {
                StatementKind::Query
            }
        }
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" | "UPSERT" => dml(rest),
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "COMMENT" | "GRANT" | "REVOKE" => {
            StatementKind::Ddl
        }
        "BEGIN" => {
            if rest.is_empty()
                || punct(&rest[0], sql, ";")
                || next_is(&["TRANSACTION", "WORK", "TRAN", "DEFERRED", "IMMEDIATE", "EXCLUSIVE", "ISOLATION", "READ", "DISTRIBUTED"])
            {
                StatementKind::Transaction
            } else {
                StatementKind::Other
            }
        }
        "START" if next_is(&["TRANSACTION"]) => StatementKind::Transaction,
        "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "ABORT" => StatementKind::Transaction,
        "END" if rest.is_empty() || punct(&rest[0], sql, ";") || next_is(&["TRANSACTION", "WORK"]) => {
            StatementKind::Transaction
        }
        _ => StatementKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(schema: Option<&str>, name: &str, alias: Option<&str>) -> TableRef {
        TableRef { schema: schema.map(Into::into), name: name.into(), alias: alias.map(Into::into) }
    }

    #[test]
    fn referenced_tables_basic() {
        let r = referenced_tables("SELECT * FROM public.users u JOIN orders AS o ON o.user_id = u.id", Dialect::Postgres);
        assert_eq!(r, vec![tr(Some("public"), "users", Some("u")), tr(None, "orders", Some("o"))]);
        let r = referenced_tables("select * from a, b x, \"Weird T\" where 1=1", Dialect::Postgres);
        assert_eq!(r, vec![tr(None, "a", None), tr(None, "b", Some("x")), tr(None, "Weird T", None)]);
        let r = referenced_tables("UPDATE t SET a = 1", Dialect::Generic);
        assert_eq!(r, vec![tr(None, "t", None)]);
        let r = referenced_tables("INSERT INTO s.t (a, b) SELECT a, b FROM src", Dialect::Generic);
        assert_eq!(r, vec![tr(Some("s"), "t", None), tr(None, "src", None)]);
        let r = referenced_tables("DELETE FROM `db`.`t` WHERE id = 1", Dialect::MySql);
        assert_eq!(r, vec![tr(Some("db"), "t", None)]);
        let r = referenced_tables("SELECT * FROM [dbo].[T] t", Dialect::MsSql);
        assert_eq!(r, vec![tr(Some("dbo"), "T", Some("t"))]);
    }

    #[test]
    fn referenced_tables_tolerant() {
        assert_eq!(referenced_tables("SELECT * FROM", Dialect::Generic), vec![]);
        assert_eq!(referenced_tables("SELECT * FROM users WHERE", Dialect::Generic), vec![tr(None, "users", None)]);
        assert_eq!(referenced_tables("SELECT a FROM t1 LEFT JOIN", Dialect::Generic), vec![tr(None, "t1", None)]);
        let r = referenced_tables("SELECT extract(year FROM d), substring(x FROM 2) FROM t", Dialect::Postgres);
        assert_eq!(r, vec![tr(None, "t", None)]);
        let r = referenced_tables("SELECT * FROM (SELECT * FROM inner_t) s JOIN other o ON true", Dialect::Postgres);
        assert_eq!(r, vec![tr(None, "inner_t", None), tr(None, "other", Some("o"))]);
        let r = referenced_tables("SELECT * FROM generate_series(1, 3) g, t", Dialect::Postgres);
        assert_eq!(r, vec![tr(None, "t", None)]);
        let r = referenced_tables("INSERT INTO t VALUES (1) ON DUPLICATE KEY UPDATE a = 1", Dialect::MySql);
        assert_eq!(r, vec![tr(None, "t", None)]);
        let r = referenced_tables("SELECT a IS DISTINCT FROM b FROM t", Dialect::Postgres);
        assert_eq!(r, vec![tr(None, "t", None)]);
    }

    #[test]
    fn ctes() {
        let sql = "WITH RECURSIVE a(x) AS (SELECT 1), \"B\" AS MATERIALIZED (SELECT 2) SELECT * FROM a, \"B\"";
        let toks = lexer::significant(sql, Dialect::Postgres);
        assert_eq!(cte_names_toks(&toks, sql), vec!["a", "B"]);
    }

    #[test]
    fn single_table() {
        let d = Dialect::Postgres;
        assert_eq!(single_table_select("SELECT * FROM users", d), Some(tr(None, "users", None)));
        assert_eq!(
            single_table_select("select id, name from public.users u where id > 1 order by id limit 10;", d),
            Some(tr(Some("public"), "users", Some("u")))
        );
        assert_eq!(
            single_table_select("SELECT * FROM t WHERE id IN (SELECT max(id) FROM t2)", d),
            Some(tr(None, "t", None))
        );
        for sql in [
            "SELECT * FROM a JOIN b ON true",
            "SELECT * FROM a, b",
            "SELECT count(*) FROM a",
            "SELECT DISTINCT x FROM a",
            "SELECT x FROM a GROUP BY x",
            "SELECT x FROM a UNION SELECT x FROM b",
            "SELECT * FROM (SELECT 1) s",
            "WITH c AS (SELECT 1) SELECT * FROM c",
            "SELECT row_number() OVER () FROM a",
            "UPDATE a SET x = 1",
            "SELECT 1",
            "SELECT * FROM generate_series(1,2)",
        ] {
            assert_eq!(single_table_select(sql, d), None, "{sql}");
        }
    }

    #[test]
    fn parameters() {
        let sql = "SELECT :a, ?, ${b}, $1, x::int, ':no', \"?\" -- :c ?\n, /* ${d} */ $$ :e ? $$, ?, @v, :=";
        let p = find_parameters(sql, Dialect::Postgres);
        let names: Vec<_> = p.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["a", "?1", "b", "$1", "?2"]);
        assert_eq!(&sql[p[0].start..p[0].end], ":a");
        let mut m = HashMap::new();
        m.insert("a".to_string(), "42".to_string());
        m.insert("?1".to_string(), "'x'".to_string());
        m.insert("b".to_string(), "NULL".to_string());
        let s = substitute_parameters("SELECT :a, ?, ${b}, :unknown", Dialect::Postgres, &m);
        assert_eq!(s, "SELECT 42, 'x', NULL, :unknown");
        assert_eq!(find_parameters("SELECT @x, @@version", Dialect::MsSql), vec![]);
        let p = find_parameters("select ?1, ?2", Dialect::Sqlite);
        assert_eq!(p[1].name, "?2");
    }

    #[test]
    fn kinds() {
        use StatementKind::*;
        let cases = [
            ("SELECT 1", Query),
            ("  (SELECT 1) UNION (SELECT 2)", Query),
            ("with x as (select 1) select * from x", Query),
            ("WITH x AS (SELECT 1) DELETE FROM t", Dml),
            ("insert into t values (1)", Dml),
            ("INSERT INTO t VALUES (1) RETURNING id", Query),
            ("UPDATE t SET a = 1", Dml),
            ("delete from t", Dml),
            ("CREATE TABLE t (id int)", Ddl),
            ("drop table t", Ddl),
            ("BEGIN", Transaction),
            ("begin transaction", Transaction),
            ("COMMIT", Transaction),
            ("START TRANSACTION", Transaction),
            ("ROLLBACK TO SAVEPOINT a", Transaction),
            ("SHOW TABLES", Query),
            ("EXPLAIN SELECT 1", Query),
            ("SET search_path = x", Other),
            ("-- comment only", Other),
            ("", Other),
            ("BEGIN SELECT 1; END", Other),
        ];
        for (sql, k) in cases {
            assert_eq!(statement_kind(sql), k, "{sql}");
        }
    }
}
