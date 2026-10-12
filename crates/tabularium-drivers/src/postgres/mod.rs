//! PostgreSQL driver (also used for CockroachDB and Amazon Redshift).
//!
//! Execution strategy:
//! * plain `SELECT`/`WITH`/`VALUES`/`TABLE` with a row limit run through a server-side
//!   cursor (`DECLARE ... ; FETCH n+1`), so a huge table is never streamed to the client
//!   just to show the first page;
//! * other row-returning statements and anything with parameters use the extended
//!   protocol (unnamed statements, binary results);
//! * everything else (`SET`, `BEGIN`, `VACUUM`, `CREATE DATABASE`, multi-statement
//!   strings, ...) uses the simple query protocol.

mod catalog;
pub(crate) mod types;

use std::str::FromStr;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::TryStreamExt;
use sqlx::postgres::{
    PgArguments, PgConnectOptions, PgConnection, PgListener, PgPool, PgPoolOptions, PgRow, PgSslMode, PgTypeInfo,
};
use sqlx::{Column, Connection as _, Either, Executor, Row, Statement, TypeInfo};
use tabularium_core::driver::first_keyword;
use tabularium_core::{
    CancelHandle, ColumnMeta, Connection, ConnectionConfig, DriverKind, Error, ExecOptions, Result, SslMode,
    StatementResult, TransactionMode, Value,
};
use tokio::sync::Mutex;

use self::types::{decode_value, encode_param, type_display_name, ExtType, ExtTypes, RawParam};
use crate::common::*;
use crate::ssh::SshTunnel;

type PgQuery<'q> = sqlx::query::Query<'q, sqlx::Postgres, PgArguments>;

/// The physical session. A `PgListener` (on a single-connection pool) is used instead of
/// a bare `PgConnection` because only listeners accept asynchronous notifications: after
/// `LISTEN`, a `NOTIFY` arriving mid-query would otherwise break the protocol stream.
pub(crate) struct Session {
    conn: PgListener,
    pool: PgPool,
    /// Type info for OIDs sqlx does not resolve in simple-protocol results.
    ext: ExtTypes,
}

/// One PostgreSQL session.
pub struct PostgresConnection {
    kind: DriverKind,
    cfg: ConnectionConfig,
    /// Options of the current session (database changes with `use_database`).
    options: Arc<RwLock<PgConnectOptions>>,
    inner: Mutex<Option<Session>>,
    tx: TxState,
    database: RwLock<String>,
    pid: Arc<AtomicI32>,
    version_num: AtomicU32,
    cursor_seq: AtomicU64,
    tunnel: Option<Arc<SshTunnel>>,
}

struct PgCancel {
    options: Arc<RwLock<PgConnectOptions>>,
    pid: Arc<AtomicI32>,
    _tunnel: Option<Arc<SshTunnel>>,
}

#[async_trait]
impl CancelHandle for PgCancel {
    async fn cancel(&self) -> Result<()> {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid == 0 {
            return Ok(());
        }
        let opts = self.options.read().map_err(|_| Error::other("lock poisoned"))?.clone();
        let fut = async move {
            let mut conn = PgConnection::connect_with(&opts).await.map_err(map_connect_err)?;
            let r = conn.execute(sqlx::query("SELECT pg_cancel_backend($1)").bind(pid)).await.map_err(map_sqlx_err);
            let _ = conn.close().await;
            r.map(|_| ())
        };
        match tokio::time::timeout(Duration::from_secs(15), fut).await {
            Ok(r) => r,
            Err(_) => Err(Error::Connection("timed out sending cancel request".into())),
        }
    }
}

fn ssl_mode(mode: SslMode) -> PgSslMode {
    match mode {
        SslMode::Disable => PgSslMode::Disable,
        SslMode::Prefer => PgSslMode::Prefer,
        SslMode::Require => PgSslMode::Require,
        SslMode::VerifyCa => PgSslMode::VerifyCa,
        SslMode::VerifyFull => PgSslMode::VerifyFull,
    }
}

/// Statements PostgreSQL refuses to run inside a transaction block; manual-commit mode
/// does not open a transaction for them.
fn cannot_run_in_transaction(sql: &str) -> bool {
    let w = leading_words(sql, 4);
    let s = |i: usize| w.get(i).map(String::as_str).unwrap_or("");
    match s(0) {
        "VACUUM" | "CHECKPOINT" => true,
        "CREATE" | "DROP" => {
            matches!(s(1), "DATABASE" | "TABLESPACE" | "SUBSCRIPTION")
                || (matches!(s(1), "INDEX") && s(2) == "CONCURRENTLY")
                || (s(1) == "UNIQUE" && s(2) == "INDEX" && s(3) == "CONCURRENTLY")
        }
        "ALTER" => s(1) == "SYSTEM" || (s(1) == "DATABASE" && contains_word(sql, "TABLESPACE")),
        "REINDEX" => contains_word(sql, "CONCURRENTLY") || matches!(s(1), "DATABASE" | "SYSTEM"),
        "CLUSTER" => w.len() == 1,
        _ => false,
    }
}

/// Statements that may return rows even when they produced none (to describe columns).
fn may_return_rows(sql: &str) -> bool {
    matches!(
        first_keyword(sql).to_ascii_uppercase().as_str(),
        "SELECT" | "WITH" | "VALUES" | "TABLE" | "SHOW" | "EXPLAIN" | "FETCH"
    ) || contains_word(sql, "RETURNING")
}

fn db_error_matches(e: &Error, code: &str, needle: &str) -> bool {
    matches!(e, Error::Database(d) if d.code.as_deref() == Some(code) && d.message.contains(needle))
}

/// Errors that disappear when the statement is sent with the simple protocol.
fn needs_simple_protocol(e: &Error) -> bool {
    db_error_matches(e, "42601", "cannot insert multiple commands")
        || db_error_matches(e, "42883", "no binary output function")
        || db_error_matches(e, "42P05", "") // prepared statement already exists (pgbouncer)
}

impl PostgresConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let base = base_options(cfg)?;
        let endpoint = crate::resolve_endpoint(cfg, base.get_host(), base.get_port()).await?;
        let mut options = base;
        if endpoint.tunnel.is_some() {
            options = options.host(&endpoint.host).port(endpoint.port);
        }
        let database = options.get_database().map(str::to_string).unwrap_or_else(|| cfg.user.clone());
        let mode = if cfg.auto_commit { TransactionMode::Auto } else { TransactionMode::Manual };
        let this = Self {
            kind: cfg.driver,
            cfg: cfg.clone(),
            options: Arc::new(RwLock::new(options.clone())),
            inner: Mutex::new(None),
            tx: TxState::new(mode),
            database: RwLock::new(database),
            pid: Arc::new(AtomicI32::new(0)),
            version_num: AtomicU32::new(0),
            cursor_seq: AtomicU64::new(0),
            tunnel: endpoint.tunnel,
        };
        let session = this.open_session(&options).await?;
        *this.inner.lock().await = Some(session);
        Ok(this)
    }

    /// Open and initialise a physical session.
    async fn open_session(&self, options: &PgConnectOptions) -> Result<Session> {
        let timeout = Duration::from_secs(if self.cfg.connect_timeout_secs == 0 { 30 } else { self.cfg.connect_timeout_secs });
        let (pool, conn) = with_connect_timeout(&self.cfg, async {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .min_connections(0)
                .max_lifetime(None)
                .idle_timeout(None)
                .acquire_timeout(timeout)
                .test_before_acquire(false)
                .connect_with(options.clone())
                .await
                .map_err(map_connect_err)?;
            let listener = PgListener::connect_with(&pool).await.map_err(map_connect_err)?;
            Ok((pool, listener))
        })
        .await?;
        let mut sess = Session { conn, pool, ext: ExtTypes::new() };
        if self.cfg.read_only {
            if let Err(e) = simple(&mut sess, "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY").await {
                log::warn!("could not make session read-only: {e}");
            }
        }
        match query_rows(&mut sess, "SELECT pg_backend_pid()", &[]).await {
            Ok(rows) => self.pid.store(rows.first().and_then(|r| c_i64(r, 0)).unwrap_or(0) as i32, Ordering::SeqCst),
            Err(e) => log::debug!("pg_backend_pid() failed: {e}"),
        }
        if let Ok(rows) = query_rows(&mut sess, "SELECT current_setting('server_version_num')", &[]).await {
            let v = rows.first().and_then(|r| c_i64(r, 0)).unwrap_or(0);
            self.version_num.store(v as u32, Ordering::SeqCst);
        }
        if let Ok(rows) = query_rows(&mut sess, "SELECT current_database()", &[]).await {
            if let Some(db) = rows.first().and_then(|r| c_str(r, 0)) {
                if let Ok(mut g) = self.database.write() {
                    *g = db;
                }
            }
        }
        for stmt in init_statements(&self.cfg) {
            simple(&mut sess, &stmt).await?;
        }
        Ok(sess)
    }

    /// Server version as `server_version_num` (e.g. 160002).
    pub fn server_version_num(&self) -> u32 {
        self.version_num.load(Ordering::SeqCst)
    }

    /// Backend process id of this session.
    pub fn backend_pid(&self) -> i32 {
        self.pid.load(Ordering::SeqCst)
    }

    /// Run an internal (catalog) query with text parameters, bypassing transaction
    /// bookkeeping and read-only checks.
    pub(crate) async fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Value>>> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        let params: Vec<String> = params.iter().map(|s| s.to_string()).collect();
        query_rows(sess, sql, &params).await
    }

    async fn run(&self, sess: &mut Session, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
        let start = Instant::now();
        let mut res = if opts.max_rows.is_some() && is_plain_select(sql) {
            let seq = self.cursor_seq.fetch_add(1, Ordering::Relaxed);
            let own_tx = !self.tx.is_active();
            match run_cursor(sess, sql, opts, seq, own_tx).await {
                Err(e) if needs_simple_protocol(&e) && opts.params.is_empty() && own_tx => {
                    run_simple(sess, sql, opts.max_rows).await
                }
                r => r,
            }
        } else if !opts.params.is_empty() || is_query_like(sql) {
            match run_extended(sess, sql, opts).await {
                Err(e) if needs_simple_protocol(&e) && opts.params.is_empty() => run_simple(sess, sql, opts.max_rows).await,
                r => r,
            }
        } else {
            run_simple(sess, sql, opts.max_rows).await
        };
        let mut notices = vec![];
        while let Some(n) = sess.conn.next_buffered() {
            notices.push(format!(
                "Asynchronous notification \"{}\" with payload \"{}\" received from server process with PID {}.",
                n.channel(),
                n.payload(),
                n.process_id()
            ));
        }
        if let Ok(r) = &mut res {
            r.sql = sql.to_string();
            r.elapsed = start.elapsed();
            r.notices.extend(notices);
        }
        res
    }

    fn current_db(&self) -> String {
        self.database.read().map(|g| g.clone()).unwrap_or_default()
    }

    fn show_system(&self) -> bool {
        self.cfg.show_system_objects
    }
}

/// Connection options from the config (before SSH tunnelling).
fn base_options(cfg: &ConnectionConfig) -> Result<PgConnectOptions> {
    let mut o = match cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => {
            let url = url.replacen("postgresql://", "postgres://", 1);
            PgConnectOptions::from_str(&url).map_err(|e| Error::Config(format!("invalid PostgreSQL URL: {e}")))?
        }
        None => {
            let mut o = PgConnectOptions::new_without_pgpass();
            let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
            o = if host.starts_with('/') { o.socket(host) } else { o.host(host) };
            let port = if cfg.port == 0 { cfg.driver.default_port() } else { cfg.port };
            o = o.port(port);
            let user = if cfg.user.is_empty() { cfg.driver.default_user() } else { cfg.user.as_str() };
            o = o.username(user);
            if let Some(pw) = cfg.password.as_deref().filter(|p| !p.is_empty()) {
                o = o.password(pw);
            }
            if !cfg.database.is_empty() {
                o = o.database(&cfg.database);
            }
            o.ssl_mode(ssl_mode(cfg.ssl.mode))
        }
    };
    if cfg.url.as_deref().is_some_and(|u| !u.trim().is_empty()) && cfg.ssl.mode != SslMode::Prefer {
        o = o.ssl_mode(ssl_mode(cfg.ssl.mode));
    }
    if let Some(ca) = &cfg.ssl.ca_cert {
        o = o.ssl_root_cert(ca);
    }
    if let Some(cert) = &cfg.ssl.client_cert {
        o = o.ssl_client_cert(cert);
    }
    if let Some(key) = &cfg.ssl.client_key {
        o = o.ssl_client_key(key);
    }
    o = o.application_name("Tabularium").statement_cache_capacity(100);
    let mut runtime: Vec<(String, String)> = vec![];
    for (k, v) in &cfg.options {
        match k.as_str() {
            "application_name" => o = o.application_name(v),
            "options" => o = o.options(parse_pg_options(v)),
            "connect_timeout" | "sslmode" | "sslrootcert" | "sslcert" | "sslkey" | "target_session_attrs" => {}
            _ => runtime.push((k.clone(), v.clone())),
        }
    }
    if !runtime.is_empty() {
        o = o.options(runtime);
    }
    Ok(o)
}

/// Parse libpq `options=-c a=b -c c=d`.
fn parse_pg_options(s: &str) -> Vec<(String, String)> {
    s.split("-c")
        .filter_map(|p| p.trim().split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

// ----- execution helpers (free functions so futures can be boxed) ------------------------------

async fn close_session(s: Session) {
    let Session { conn, pool, .. } = s;
    drop(conn); // returns the connection to the pool
    pool.close().await;
}

/// Map a server error position from a wrapped statement back to the user's text:
/// `prefix` chars were prepended, `lead` chars of leading whitespace were removed.
fn shift_position(e: Error, prefix: usize, lead: usize) -> Error {
    match e {
        Error::Database(mut d) => {
            d.position = d.position.and_then(|p| (p > prefix).then(|| p - prefix + lead));
            Error::Database(d)
        }
        other => other,
    }
}

fn simple<'a>(sess: &'a mut Session, sql: &'a str) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move { sess.conn.execute(sqlx::raw_sql(sql)).await.map(|_| ()).map_err(map_sqlx_err) })
}

/// Internal query with text parameters through the (cached) extended protocol.
fn query_rows<'a>(sess: &'a mut Session, sql: &'a str, params: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<Value>>>> {
    Box::pin(async move {
        let mut q = sqlx::query(sql);
        for p in params {
            q = q.bind(p.as_str());
        }
        let rows: Vec<PgRow> = sess.conn.fetch_all(q).await.map_err(map_sqlx_err)?;
        Ok(rows.iter().map(|r| decode_row(r, &sess.ext)).collect())
    })
}

fn decode_row(row: &PgRow, ext: &ExtTypes) -> Vec<Value> {
    (0..row.len())
        .map(|i| match row.try_get_raw(i) {
            Ok(v) => decode_value(v, ext),
            Err(e) => Value::Other { type_name: "?".into(), text: format!("<error: {e}>") },
        })
        .collect()
}

static DESCRIBE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Statement text made unique so sqlx's statement cache never serves metadata that went
/// stale after DDL (the cache is keyed by SQL text and we cannot clear it on a listener).
fn uncached(sql: &str) -> String {
    format!("{sql}\n/* tabularium describe {} */", DESCRIBE_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Server-inferred parameter types of a statement.
async fn param_types(sess: &mut Session, sql: &str) -> Result<Vec<PgTypeInfo>> {
    let text = uncached(sql);
    let stmt = sess.conn.prepare_with(&text, &[]).await.map_err(map_sqlx_err)?;
    Ok(match stmt.parameters() {
        Some(Either::Left(types)) => types.to_vec(),
        _ => vec![],
    })
}

/// Column metadata of a statement without executing it.
async fn describe_columns(sess: &mut Session, sql: &str) -> Result<Vec<ColumnMeta>> {
    let text = uncached(sql);
    let stmt = sess.conn.prepare_with(&text, &[]).await.map_err(map_sqlx_err)?;
    Ok(stmt.columns().iter().map(|c| ColumnMeta::new(c.name(), type_display_name(c.type_info()))).collect())
}

fn bind_params<'q>(mut q: PgQuery<'q>, params: &[Value], types: &[PgTypeInfo]) -> Result<PgQuery<'q>> {
    for (i, v) in params.iter().enumerate() {
        let ty = types.get(i).cloned().unwrap_or_else(|| PgTypeInfo::with_name("text"));
        let bytes = encode_param(v, &ty).map_err(|e| Error::Other(format!("parameter ${}: {e}", i + 1)))?;
        q = q.bind(RawParam { ty, bytes });
    }
    Ok(q)
}

struct Collected {
    columns: Vec<ColumnMeta>,
    rows: Vec<PgRow>,
    affected: u64,
    truncated: bool,
}

/// Drain a query stream, keeping at most `max_rows` rows of the last result set.
async fn collect<'e, E>(sess: &'e mut Session, query: E, max_rows: Option<usize>) -> Result<Collected>
where
    E: sqlx::Execute<'e, sqlx::Postgres> + 'e,
{
    let mut out = Collected { columns: vec![], rows: vec![], affected: 0, truncated: false };
    let mut stream = sess.conn.fetch_many(query);
    while let Some(item) = stream.try_next().await.map_err(map_sqlx_err)? {
        match item {
            Either::Left(r) => out.affected += r.rows_affected(),
            Either::Right(row) => {
                let cols = row.columns();
                let same = out.columns.len() == cols.len()
                    && out.columns.iter().zip(cols).all(|(a, b)| a.name == b.name());
                if !same {
                    // first row, or a new result set of a multi-statement string
                    out.columns = cols.iter().map(|c| ColumnMeta::new(c.name(), type_display_name(c.type_info()))).collect();
                    out.rows.clear();
                    out.truncated = false;
                }
                if max_rows.is_some_and(|m| out.rows.len() >= m) {
                    out.truncated = true;
                    break;
                }
                out.rows.push(row);
            }
        }
    }
    Ok(out)
}

/// Resolve OIDs sqlx left unresolved (simple protocol) so values decode properly.
async fn resolve_ext_types(sess: &mut Session, rows: &[PgRow]) -> Result<()> {
    let Some(first) = rows.first() else { return Ok(()) };
    let missing: Vec<u32> = first
        .columns()
        .iter()
        .filter_map(|c| {
            let ti = c.type_info();
            let o = ti.oid()?.0;
            (ti.name() == "?" && types::builtin_name(o).is_none() && !sess.ext.contains_key(&o)).then_some(o)
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let list = missing.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT oid::int8, format_type(oid, NULL), typtype::text, typelem::int8, typbasetype::int8 \
         FROM pg_type WHERE oid IN ({list})"
    );
    let rows = query_rows(sess, &sql, &[]).await?;
    for r in rows {
        let Some(o) = c_i64(&r, 0) else { continue };
        sess.ext.insert(
            o as u32,
            ExtType {
                name: c_string(&r, 1),
                typtype: c_string(&r, 2).chars().next().unwrap_or('b'),
                elem: c_i64(&r, 3).unwrap_or(0) as u32,
                base: c_i64(&r, 4).unwrap_or(0) as u32,
            },
        );
    }
    Ok(())
}

fn finish(sess: &Session, c: Collected) -> StatementResult {
    let mut columns = c.columns;
    // Column type names for OIDs resolved through the extension cache.
    for col in &mut columns {
        if let Some(o) = col.type_name.strip_prefix("oid:").and_then(|o| o.parse::<u32>().ok()) {
            if let Some(e) = sess.ext.get(&o) {
                col.type_name = e.name.clone();
            }
        }
    }
    let rows: Vec<Vec<Value>> = c.rows.iter().map(|r| decode_row(r, &sess.ext)).collect();
    let has_rows = !columns.is_empty();
    StatementResult {
        sql: String::new(),
        columns,
        rows,
        rows_affected: if has_rows { None } else { Some(c.affected) },
        truncated: c.truncated,
        elapsed: Duration::ZERO,
        notices: vec![],
    }
}

fn run_extended<'a>(sess: &'a mut Session, sql: &'a str, opts: &'a ExecOptions) -> BoxFuture<'a, Result<StatementResult>> {
    Box::pin(async move {
        let types = if opts.params.is_empty() { vec![] } else { param_types(sess, sql).await? };
        let q = bind_params(sqlx::query(sql).persistent(false), &opts.params, &types)?;
        let mut c = collect(sess, q, opts.max_rows).await?;
        if c.columns.is_empty() && may_return_rows(sql) {
            if let Ok(cols) = describe_columns(sess, sql).await {
                c.columns = cols;
            }
        }
        Ok(finish(sess, c))
    })
}

fn run_simple<'a>(sess: &'a mut Session, sql: &'a str, max_rows: Option<usize>) -> BoxFuture<'a, Result<StatementResult>> {
    Box::pin(async move {
        let c = collect(sess, sqlx::raw_sql(sql), max_rows).await?;
        resolve_ext_types(sess, &c.rows).await?;
        Ok(finish(sess, c))
    })
}

fn run_cursor<'a>(
    sess: &'a mut Session,
    sql: &'a str,
    opts: &'a ExecOptions,
    seq: u64,
    own_tx: bool,
) -> BoxFuture<'a, Result<StatementResult>> {
    Box::pin(async move {
        let max = opts.max_rows.unwrap_or(usize::MAX);
        let body = trim_statement(sql);
        let lead = sql[..sql.len() - sql.trim_start().len()].chars().count();
        let name = format!("tabularium_cursor_{seq}");
        if own_tx {
            simple(sess, "BEGIN").await?;
        }
        let result: Result<StatementResult> = async {
            let types = if opts.params.is_empty() {
                vec![]
            } else {
                param_types(sess, body).await.map_err(|e| shift_position(e, 0, lead))?
            };
            let prefix = format!("DECLARE {name} NO SCROLL CURSOR FOR ");
            let declare = format!("{prefix}{body}");
            let q = bind_params(sqlx::query(&declare).persistent(false), &opts.params, &types)?;
            sess.conn
                .execute(q)
                .await
                .map_err(|e| shift_position(map_sqlx_err(e), prefix.chars().count(), lead))?;
            let fetch = format!("FETCH FORWARD {} FROM {name}", max.saturating_add(1));
            let mut c = collect(sess, sqlx::query(&fetch).persistent(false), Some(max)).await?;
            simple(sess, &format!("CLOSE {name}")).await?;
            if c.columns.is_empty() {
                if let Ok(cols) = describe_columns(sess, body).await {
                    c.columns = cols;
                }
            }
            c.affected = 0;
            Ok(finish(sess, c))
        }
        .await;
        if own_tx {
            let end = if result.is_ok() { "COMMIT" } else { "ROLLBACK" };
            if let Err(e) = simple(sess, end).await {
                log::debug!("{end} after cursor query failed: {e}");
                if result.is_ok() {
                    return Err(e);
                }
            }
        }
        result
    })
}

#[async_trait]
impl Connection for PostgresConnection {
    fn driver(&self) -> DriverKind {
        self.kind
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.query("SELECT version()", &[]).await?;
        Ok(rows.first().map(|r| c_string(r, 0)).unwrap_or_default())
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
        check_read_only(self.cfg.read_only, sql)?;
        let ctl = tx_control(sql, false);
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        if self.tx.needs_begin(ctl) && !cannot_run_in_transaction(sql) {
            simple(sess, "BEGIN").await?;
            self.tx.began();
        }
        let res = self.run(sess, sql, opts).await;
        self.tx.after_statement(ctl, res.is_ok());
        res
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(PgCancel { options: self.options.clone(), pid: self.pid.clone(), _tunnel: self.tunnel.clone() }))
    }

    async fn ping(&self) -> Result<()> {
        self.query("SELECT 1", &[]).await.map(|_| ())
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
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        if self.tx.is_active() {
            // COMMIT of a failed transaction performs a rollback; either way it ends.
            let r = simple(sess, "COMMIT").await;
            self.tx.ended();
            r?;
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        if self.tx.is_active() {
            let r = simple(sess, "ROLLBACK").await;
            self.tx.ended();
            r?;
        }
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.catalog_list_databases().await
    }

    fn current_database(&self) -> Option<String> {
        Some(self.current_db())
    }

    async fn use_database(&self, database: &str) -> Result<()> {
        let new_opts = self.options.read().map_err(|_| Error::other("lock poisoned"))?.clone().database(database);
        let session = self.open_session(&new_opts).await?;
        let mut guard = self.inner.lock().await;
        if let Some(old) = guard.replace(session) {
            close_session(old).await;
        }
        self.tx.ended();
        if let Ok(mut o) = self.options.write() {
            *o = new_opts;
        }
        Ok(())
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        self.catalog_list_schemas().await
    }

    async fn current_schema(&self) -> Result<Option<String>> {
        let rows = self.query("SELECT current_schema()", &[]).await?;
        Ok(rows.first().and_then(|r| c_str(r, 0)))
    }

    async fn list_objects(
        &self,
        schema: Option<&str>,
        kinds: &[tabularium_core::ObjectKind],
    ) -> Result<Vec<tabularium_core::ObjectInfo>> {
        self.catalog_list_objects(schema, kinds).await
    }

    async fn describe_table(&self, object: &tabularium_core::ObjectRef) -> Result<tabularium_core::TableInfo> {
        Ok(self.catalog_describe_table(object).await?.info)
    }

    async fn describe_routine(
        &self,
        object: &tabularium_core::ObjectRef,
        signature: Option<&str>,
    ) -> Result<tabularium_core::RoutineInfo> {
        self.catalog_describe_routine(object, signature).await
    }

    async fn object_ddl(&self, object: &tabularium_core::ObjectRef) -> Result<String> {
        self.catalog_object_ddl(object).await
    }

    async fn list_roles(&self) -> Result<Vec<tabularium_core::RoleInfo>> {
        self.catalog_list_roles().await
    }

    async fn list_sessions(&self) -> Result<Vec<tabularium_core::SessionInfo>> {
        self.catalog_list_sessions().await
    }

    async fn kill_session(&self, id: &str) -> Result<()> {
        let pid: i32 = id.trim().parse().map_err(|_| Error::Other(format!("invalid backend pid: {id}")))?;
        let rows = self.query("SELECT pg_terminate_backend($1::text::int4)", &[&pid.to_string()]).await?;
        if rows.first().is_some_and(|r| c_bool(r, 0)) {
            Ok(())
        } else {
            Err(Error::Other(format!("could not terminate backend {pid} (no such process or insufficient privileges)")))
        }
    }

    async fn close(&self) -> Result<()> {
        let sess = self.inner.lock().await.take();
        if let Some(s) = sess {
            close_session(s).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tx_statements() {
        assert!(cannot_run_in_transaction("VACUUM ANALYZE t"));
        assert!(cannot_run_in_transaction("create database x"));
        assert!(cannot_run_in_transaction("CREATE INDEX CONCURRENTLY i ON t(a)"));
        assert!(!cannot_run_in_transaction("CREATE INDEX i ON t(a)"));
        assert!(!cannot_run_in_transaction("CLUSTER t USING i"));
        assert!(cannot_run_in_transaction("CLUSTER"));
        assert!(!cannot_run_in_transaction("select 1"));
    }

    #[test]
    fn options_parsing() {
        assert_eq!(
            parse_pg_options("-c search_path=a,b -c statement_timeout=5s"),
            vec![("search_path".to_string(), "a,b".to_string()), ("statement_timeout".to_string(), "5s".to_string())]
        );
        let mut cfg = ConnectionConfig::new(DriverKind::Postgres);
        cfg.host = "db".into();
        cfg.port = 6000;
        cfg.database = "shop".into();
        let o = base_options(&cfg).unwrap();
        assert_eq!(o.get_host(), "db");
        assert_eq!(o.get_port(), 6000);
        assert_eq!(o.get_database(), Some("shop"));
        cfg.url = Some("postgresql://u@h2:7000/x".into());
        let o = base_options(&cfg).unwrap();
        assert_eq!(o.get_host(), "h2");
        assert_eq!(o.get_port(), 7000);
    }

    #[test]
    fn row_statement_detection() {
        assert!(may_return_rows("select 1"));
        assert!(may_return_rows("insert into t values (1) returning id"));
        assert!(!may_return_rows("update t set a = 1"));
    }
}
