//! Helpers shared by all drivers: transaction bookkeeping, statement classification,
//! value fallbacks and error mapping.

use std::sync::atomic::{AtomicBool, Ordering};

use tabularium_core::driver::{first_keyword, is_modifying_statement};
use tabularium_core::{ConnectionConfig, Error, Result, TransactionMode, Value};

/// Transaction bookkeeping for one session.
///
/// * `manual`  – the console is in manual-commit mode.
/// * `active`  – a transaction block is open on the server (we or the user issued BEGIN).
/// * `dirty`   – at least one statement ran inside the open transaction.
#[derive(Debug, Default)]
pub(crate) struct TxState {
    manual: AtomicBool,
    active: AtomicBool,
    dirty: AtomicBool,
}

/// What a statement does to the transaction state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TxControl {
    Begin,
    Commit,
    Rollback,
    None,
}

impl TxState {
    pub(crate) fn new(mode: TransactionMode) -> Self {
        let s = Self::default();
        s.set_mode(mode);
        s
    }
    pub(crate) fn set_mode(&self, mode: TransactionMode) {
        self.manual.store(mode == TransactionMode::Manual, Ordering::SeqCst);
    }
    pub(crate) fn mode(&self) -> TransactionMode {
        if self.manual.load(Ordering::SeqCst) {
            TransactionMode::Manual
        } else {
            TransactionMode::Auto
        }
    }
    pub(crate) fn is_manual(&self) -> bool {
        self.manual.load(Ordering::SeqCst)
    }
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
    pub(crate) fn in_transaction(&self) -> bool {
        self.active.load(Ordering::SeqCst) && self.dirty.load(Ordering::SeqCst)
    }
    /// A transaction block was opened (no work done yet).
    pub(crate) fn began(&self) {
        self.active.store(true, Ordering::SeqCst);
        self.dirty.store(false, Ordering::SeqCst);
    }
    /// The transaction block ended (commit or rollback).
    pub(crate) fn ended(&self) {
        self.active.store(false, Ordering::SeqCst);
        self.dirty.store(false, Ordering::SeqCst);
    }
    /// A statement ran; marks the open transaction as having work.
    pub(crate) fn statement_ran(&self) {
        if self.is_active() {
            self.dirty.store(true, Ordering::SeqCst);
        }
    }
    /// Whether a BEGIN must be issued before running a statement with `ctl`.
    pub(crate) fn needs_begin(&self, ctl: TxControl) -> bool {
        ctl == TxControl::None && self.is_manual() && !self.is_active()
    }
    /// Update the state after a statement with `ctl` ran (`ok` = it succeeded).
    pub(crate) fn after_statement(&self, ctl: TxControl, ok: bool) {
        match ctl {
            TxControl::Begin if ok => self.began(),
            TxControl::Commit | TxControl::Rollback if ok => self.ended(),
            TxControl::None => self.statement_ran(),
            _ => {}
        }
    }
}

/// Upper-cased words of a statement after leading comments (at most `n`).
pub(crate) fn leading_words(sql: &str, n: usize) -> Vec<String> {
    let first = first_keyword(sql);
    if first.is_empty() {
        return vec![];
    }
    // `first_keyword` returns a slice of `sql`; continue from there.
    let start = first.as_ptr() as usize - sql.as_ptr() as usize;
    sql[start..]
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(n)
        .map(|w| w.to_ascii_uppercase())
        .collect()
}

/// Classify transaction-control statements. `mssql` switches to T-SQL rules, where a bare
/// `BEGIN` opens a block rather than a transaction.
pub(crate) fn tx_control(sql: &str, mssql: bool) -> TxControl {
    let w = leading_words(sql, 3);
    let w1 = w.first().map(String::as_str).unwrap_or("");
    let w2 = w.get(1).map(String::as_str).unwrap_or("");
    let w3 = w.get(2).map(String::as_str).unwrap_or("");
    if mssql {
        return match (w1, w2) {
            ("BEGIN", "TRAN" | "TRANSACTION") => TxControl::Begin,
            ("COMMIT", _) => TxControl::Commit,
            ("ROLLBACK", "TRAN" | "TRANSACTION" | "") if w3.is_empty() || w2.is_empty() => TxControl::Rollback,
            ("ROLLBACK", "WORK") => TxControl::Rollback,
            _ => TxControl::None,
        };
    }
    match w1 {
        // MariaDB compound statement `BEGIN NOT ATOMIC ... END` is a block, not a transaction.
        "BEGIN" if w2 == "NOT" => TxControl::None,
        "BEGIN" => TxControl::Begin,
        "START" if w2 == "TRANSACTION" => TxControl::Begin,
        "COMMIT" | "END" if w2 != "PREPARED" => TxControl::Commit,
        "ROLLBACK" | "ABORT" => {
            if w2 == "TO" || w2 == "PREPARED" || (w3 == "TO" && matches!(w2, "WORK" | "TRANSACTION")) {
                TxControl::None
            } else {
                TxControl::Rollback
            }
        }
        _ => TxControl::None,
    }
}

/// Reject modifying statements on read-only connections.
pub(crate) fn check_read_only(read_only: bool, sql: &str) -> Result<()> {
    if read_only && is_modifying_statement(sql) {
        let kw = first_keyword(sql).to_ascii_uppercase();
        return Err(Error::ReadOnly(format!("{kw} statements are not allowed on a read-only connection")));
    }
    Ok(())
}

/// Statements that return rows (or may): used to pick the prepared/extended protocol.
pub(crate) fn is_query_like(sql: &str) -> bool {
    matches!(
        first_keyword(sql).to_ascii_uppercase().as_str(),
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "EXPLAIN"
            | "SHOW" | "CALL" | "FETCH" | "DESCRIBE" | "DESC" | "REPLACE"
    )
}

/// Pure row-returning queries (no side effects expected), suitable for cursors or row
/// limits: SELECT / WITH (without data-modifying CTE) / VALUES / TABLE, excluding
/// `SELECT ... INTO`.
pub(crate) fn is_plain_select(sql: &str) -> bool {
    let kw = first_keyword(sql).to_ascii_uppercase();
    if !matches!(kw.as_str(), "SELECT" | "WITH" | "VALUES" | "TABLE") {
        return false;
    }
    if is_modifying_statement(sql) {
        return false;
    }
    !contains_word(sql, "INTO")
}

/// Case-insensitive whole-word search ignoring string literals and comments (roughly).
pub(crate) fn contains_word(sql: &str, word: &str) -> bool {
    let stripped = strip_literals_and_comments(sql).to_ascii_uppercase();
    let word = word.to_ascii_uppercase();
    stripped
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .any(|w| w == word)
}

/// Replace string literals, quoted identifiers and comments with spaces.
pub(crate) fn strip_literals_and_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' | '`' => {
                let q = c;
                while let Some(n) = chars.next() {
                    if n == q {
                        if chars.peek() == Some(&q) {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
                out.push(' ');
            }
            '-' if chars.peek() == Some(&'-') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Remove trailing whitespace and semicolons.
pub(crate) fn trim_statement(sql: &str) -> &str {
    sql.trim().trim_end_matches(|c: char| c == ';' || c.is_whitespace())
}

/// Simple `;` splitter for `init_sql` (no literal awareness needed beyond quotes).
pub(crate) fn split_simple(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in sql.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                ';' => {
                    if !cur.trim().is_empty() {
                        out.push(cur.trim().to_string());
                    }
                    cur.clear();
                }
                _ => cur.push(c),
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Initial statements from `cfg.init_sql`.
pub(crate) fn init_statements(cfg: &ConnectionConfig) -> Vec<String> {
    cfg.init_sql.as_deref().map(split_simple).unwrap_or_default()
}

/// Fallback for values the driver cannot decode natively: UTF-8 text when possible,
/// otherwise a `\x` hex dump.
pub(crate) fn bytes_fallback(type_name: &str, bytes: &[u8]) -> Value {
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => format!("\\x{}", hex::encode(bytes)),
    };
    Value::Other { type_name: type_name.to_string(), text }
}

/// Parse a float text the way databases print them (`NaN`, `Infinity`, `-Infinity`, `inf`).
pub(crate) fn parse_float(s: &str) -> Option<f64> {
    match s.trim() {
        "NaN" | "nan" => Some(f64::NAN),
        "Infinity" | "inf" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" | "-inf" => Some(f64::NEG_INFINITY),
        t => t.parse().ok(),
    }
}

/// `f32` widened to `f64` through its shortest decimal representation, so `1.1f32`
/// shows as `1.1` instead of `1.100000023841858`.
pub(crate) fn f32_to_f64(f: f32) -> f64 {
    if f.is_finite() {
        f.to_string().parse().unwrap_or(f as f64)
    } else {
        f as f64
    }
}

/// Map an sqlx error to the core error type.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub(crate) fn map_sqlx_err(e: sqlx::Error) -> Error {
    use tabularium_core::error::DatabaseError;
    match e {
        sqlx::Error::Database(db) => {
            #[cfg(feature = "postgres")]
            if let Some(pg) = db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>() {
                if pg.code() == "57014" {
                    return Error::Cancelled;
                }
                let position = match pg.position() {
                    Some(sqlx::postgres::PgErrorPosition::Original(p)) => Some(p),
                    _ => None,
                };
                let mut detail = pg.detail().map(str::to_string);
                if let Some(where_) = pg.r#where() {
                    detail = Some(match detail {
                        Some(d) => format!("{d}\n  Where: {where_}"),
                        None => format!("Where: {where_}"),
                    });
                }
                return Error::Database(DatabaseError {
                    message: pg.message().to_string(),
                    code: Some(pg.code().to_string()),
                    detail,
                    hint: pg.hint().map(str::to_string),
                    position,
                });
            }
            #[cfg(feature = "mysql")]
            if let Some(my) = db.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>() {
                if my.number() == 1317 {
                    return Error::Cancelled;
                }
                return Error::Database(DatabaseError {
                    message: my.message().to_string(),
                    code: Some(match my.code() {
                        Some(state) => format!("{} ({state})", my.number()),
                        None => my.number().to_string(),
                    }),
                    ..Default::default()
                });
            }
            let code = db.code().map(|c| c.into_owned());
            #[cfg(feature = "sqlite")]
            if db.try_downcast_ref::<sqlx::sqlite::SqliteError>().is_some()
                && code.as_deref() == Some("9")
            {
                // SQLITE_INTERRUPT
                return Error::Cancelled;
            }
            Error::Database(DatabaseError { message: db.message().to_string(), code, ..Default::default() })
        }
        sqlx::Error::Io(e) => Error::Connection(format!("i/o error: {e}")),
        sqlx::Error::Tls(e) => Error::Connection(format!("TLS error: {e}")),
        sqlx::Error::Protocol(e) => Error::Connection(format!("protocol error: {e}")),
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => {
            Error::Connection(e.to_string())
        }
        sqlx::Error::Configuration(e) => Error::Config(e.to_string()),
        other => Error::Other(other.to_string()),
    }
}

/// Map a connect-time sqlx error: server errors stay structured (bad password,
/// unknown database), everything else becomes `Error::Connection`.
#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
pub(crate) fn map_connect_err(e: sqlx::Error) -> Error {
    match map_sqlx_err(e) {
        Error::Other(m) => Error::Connection(m),
        Error::Database(d) => Error::Connection(d.to_string()),
        other => other,
    }
}

/// Run a future with the configured connect timeout.
pub(crate) async fn with_connect_timeout<T>(
    cfg: &ConnectionConfig,
    fut: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let secs = if cfg.connect_timeout_secs == 0 { 30 } else { cfg.connect_timeout_secs };
    match tokio::time::timeout(std::time::Duration::from_secs(secs), fut).await {
        Ok(r) => r,
        Err(_) => Err(Error::Connection(format!("timed out after {secs} s while connecting"))),
    }
}

// ----- row access helpers for catalog queries ------------------------------------------------

static NULL: Value = Value::Null;

/// Cell `i` of a row (NULL when out of range).
pub(crate) fn cell(row: &[Value], i: usize) -> &Value {
    row.get(i).unwrap_or(&NULL)
}

/// Cell as text (`None` for NULL).
pub(crate) fn c_str(row: &[Value], i: usize) -> Option<String> {
    match cell(row, i) {
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        other => Some(other.to_string()),
    }
}

/// Cell as text, empty string for NULL.
pub(crate) fn c_string(row: &[Value], i: usize) -> String {
    c_str(row, i).unwrap_or_default()
}

/// Cell as non-empty text.
pub(crate) fn c_nonempty(row: &[Value], i: usize) -> Option<String> {
    c_str(row, i).filter(|s| !s.is_empty())
}

pub(crate) fn c_i64(row: &[Value], i: usize) -> Option<i64> {
    match cell(row, i) {
        Value::Int(v) => Some(*v),
        Value::UInt(v) => i64::try_from(*v).ok(),
        Value::Float(f) => Some(*f as i64),
        Value::Bool(b) => Some(i64::from(*b)),
        Value::Decimal(s) | Value::Text(s) => {
            let s = s.trim();
            s.parse().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
        }
        Value::Bytes(b) => std::str::from_utf8(b).ok().and_then(|s| s.trim().parse().ok()),
        Value::Other { text, .. } => text.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn c_bool(row: &[Value], i: usize) -> bool {
    match cell(row, i) {
        Value::Bool(b) => *b,
        Value::Int(v) => *v != 0,
        Value::UInt(v) => *v != 0,
        Value::Text(s) | Value::Decimal(s) => {
            matches!(s.trim().to_ascii_lowercase().as_str(), "t" | "true" | "yes" | "y" | "1" | "on")
        }
        _ => false,
    }
}

/// Parse a Postgres-style text array literal of simple names (`{a,b,"c d"}`) or
/// return the single value; also accepts `Value::Array`.
pub(crate) fn c_list(row: &[Value], i: usize) -> Vec<String> {
    match cell(row, i) {
        Value::Null => vec![],
        Value::Array(items) => items.iter().filter(|v| !v.is_null()).map(|v| v.to_string()).collect(),
        other => {
            let s = other.to_string();
            let t = s.trim();
            if let Some(inner) = t.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
                if inner.is_empty() {
                    return vec![];
                }
                let mut out = vec![];
                let mut cur = String::new();
                let mut in_q = false;
                let mut chars = inner.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '"' => in_q = !in_q,
                        '\\' if in_q => {
                            if let Some(n) = chars.next() {
                                cur.push(n);
                            }
                        }
                        ',' if !in_q => out.push(std::mem::take(&mut cur)),
                        _ => cur.push(c),
                    }
                }
                out.push(cur);
                out
            } else {
                t.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
            }
        }
    }
}

/// Error used after `close()`.
pub(crate) fn closed() -> Error {
    Error::Connection("connection is closed".into())
}

/// Format a list of objects for error messages.
pub(crate) fn not_found(what: &str, name: &str) -> Error {
    Error::Other(format!("{what} not found: {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_control_detection() {
        assert_eq!(tx_control("begin", false), TxControl::Begin);
        assert_eq!(tx_control("  -- x\nSTART TRANSACTION READ ONLY", false), TxControl::Begin);
        assert_eq!(tx_control("commit;", false), TxControl::Commit);
        assert_eq!(tx_control("END", false), TxControl::Commit);
        assert_eq!(tx_control("rollback", false), TxControl::Rollback);
        assert_eq!(tx_control("ROLLBACK TO SAVEPOINT a", false), TxControl::None);
        assert_eq!(tx_control("ROLLBACK WORK TO a", false), TxControl::None);
        assert_eq!(tx_control("COMMIT PREPARED 'x'", false), TxControl::None);
        assert_eq!(tx_control("select 1", false), TxControl::None);
        assert_eq!(tx_control("BEGIN TRAN", true), TxControl::Begin);
        assert_eq!(tx_control("BEGIN SELECT 1 END", true), TxControl::None);
        assert_eq!(tx_control("ROLLBACK TRANSACTION", true), TxControl::Rollback);
        assert_eq!(tx_control("ROLLBACK", true), TxControl::Rollback);
        assert_eq!(tx_control("ROLLBACK TRAN sp1", true), TxControl::None);
    }

    #[test]
    fn tx_state_flow() {
        let t = TxState::new(TransactionMode::Manual);
        assert!(t.needs_begin(TxControl::None));
        t.began();
        assert!(!t.in_transaction());
        t.after_statement(TxControl::None, true);
        assert!(t.in_transaction());
        t.after_statement(TxControl::Commit, true);
        assert!(!t.in_transaction());
        assert!(!t.is_active());
        let a = TxState::new(TransactionMode::Auto);
        assert!(!a.needs_begin(TxControl::None));
        a.after_statement(TxControl::Begin, true);
        a.after_statement(TxControl::None, false);
        assert!(a.in_transaction());
    }

    #[test]
    fn select_classification() {
        assert!(is_plain_select("select * from t"));
        assert!(is_plain_select("(select 1)"));
        assert!(!is_plain_select("select * into x from t"));
        assert!(is_plain_select("select 'into' from t"));
        assert!(!is_plain_select("with x as (delete from t returning *) select * from x"));
        assert!(!is_plain_select("insert into t values (1)"));
        assert!(is_query_like("insert into t values (1) returning id"));
        assert!(!is_query_like("vacuum"));
    }

    #[test]
    fn splitting() {
        assert_eq!(split_simple("set a = 1; select ';' ;; "), vec!["set a = 1", "select ';'"]);
        assert_eq!(trim_statement(" select 1 ; ;\n"), "select 1");
        assert_eq!(leading_words("/* c */ rollback to x", 3), vec!["ROLLBACK", "TO", "X"]);
    }

    #[test]
    fn fallback() {
        assert_eq!(bytes_fallback("t", b"abc"), Value::Other { type_name: "t".into(), text: "abc".into() });
        assert_eq!(bytes_fallback("t", &[0xff, 0x00]), Value::Other { type_name: "t".into(), text: "\\xff00".into() });
        assert_eq!(f32_to_f64(1.1), 1.1);
        assert!(parse_float("NaN").is_some_and(f64::is_nan));
    }

    #[test]
    fn read_only() {
        assert!(check_read_only(true, "select 1").is_ok());
        assert!(matches!(check_read_only(true, "delete from t"), Err(Error::ReadOnly(_))));
        assert!(check_read_only(false, "delete from t").is_ok());
    }
}
