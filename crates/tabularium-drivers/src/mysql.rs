//! MySQL / MariaDB / TiDB driver (sqlx).
//!
//! Statements run through the prepared-statement protocol (binary results, column
//! metadata even for empty results) and fall back to the text protocol for commands the
//! server cannot prepare (error 1295) and multi-statement strings. Row limits on plain
//! SELECTs are pushed to the server with `SQL_SELECT_LIMIT`, like JDBC's `setMaxRows`.

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::TryStreamExt;
use sqlx::mysql::{MySqlArguments, MySqlConnectOptions, MySqlRow, MySqlSslMode};
use sqlx::{Column, Connection as _, Decode, Either, Executor, MySql, Row, Statement, TypeInfo, ValueRef};
use tabularium_core::driver::first_keyword;
use tabularium_core::model::*;
use tabularium_core::{
    CancelHandle, ColumnMeta, Connection, ConnectionConfig, Dialect, DriverKind, Error, ExecOptions, Result,
    SslMode, StatementResult, TransactionMode, Value,
};
use tokio::sync::Mutex;

use crate::common::*;
use crate::ssh::SshTunnel;

type SqlxConn = sqlx::MySqlConnection;
type MyQuery<'q> = sqlx::query::Query<'q, MySql, MySqlArguments>;

const D: Dialect = Dialect::MySql;

const SYSTEM_SCHEMAS: [&str; 5] = ["information_schema", "mysql", "performance_schema", "sys", "metrics_schema"];

struct Session {
    conn: SqlxConn,
    /// Current `SQL_SELECT_LIMIT` (`None` = DEFAULT).
    select_limit: Option<u64>,
}

/// One MySQL-family session.
pub struct MySqlConnection {
    kind: DriverKind,
    cfg: ConnectionConfig,
    options: MySqlConnectOptions,
    inner: Mutex<Option<Session>>,
    tx: TxState,
    database: RwLock<Option<String>>,
    conn_id: Arc<AtomicU64>,
    version: String,
    tunnel: Option<Arc<SshTunnel>>,
}

struct MySqlCancel {
    options: MySqlConnectOptions,
    conn_id: Arc<AtomicU64>,
    _tunnel: Option<Arc<SshTunnel>>,
}

#[async_trait]
impl CancelHandle for MySqlCancel {
    async fn cancel(&self) -> Result<()> {
        let id = self.conn_id.load(Ordering::SeqCst);
        if id == 0 {
            return Ok(());
        }
        let opts = self.options.clone();
        let fut = async move {
            let mut conn = SqlxConn::connect_with(&opts).await.map_err(map_connect_err)?;
            let r = conn.execute(sqlx::raw_sql(&format!("KILL QUERY {id}"))).await.map_err(map_sqlx_err);
            let _ = conn.close().await;
            r.map(|_| ())
        };
        match tokio::time::timeout(Duration::from_secs(15), fut).await {
            Ok(r) => r,
            Err(_) => Err(Error::Connection("timed out sending KILL QUERY".into())),
        }
    }
}

fn ssl_mode(mode: SslMode) -> MySqlSslMode {
    match mode {
        SslMode::Disable => MySqlSslMode::Disabled,
        SslMode::Prefer => MySqlSslMode::Preferred,
        SslMode::Require => MySqlSslMode::Required,
        SslMode::VerifyCa => MySqlSslMode::VerifyCa,
        SslMode::VerifyFull => MySqlSslMode::VerifyIdentity,
    }
}

fn base_options(cfg: &ConnectionConfig) -> Result<MySqlConnectOptions> {
    let mut o = match cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        Some(url) => {
            let url = url.replacen("mariadb://", "mysql://", 1).replacen("tidb://", "mysql://", 1);
            let mut o = MySqlConnectOptions::from_str(&url).map_err(|e| Error::Config(format!("invalid MySQL URL: {e}")))?;
            if cfg.ssl.mode != SslMode::Prefer {
                o = o.ssl_mode(ssl_mode(cfg.ssl.mode));
            }
            o
        }
        None => {
            let mut o = MySqlConnectOptions::new();
            let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
            o = if host.starts_with('/') { o.socket(host) } else { o.host(host) };
            o = o.port(if cfg.port == 0 { cfg.driver.default_port() } else { cfg.port });
            o = o.username(if cfg.user.is_empty() { cfg.driver.default_user() } else { cfg.user.as_str() });
            if let Some(pw) = cfg.password.as_deref().filter(|p| !p.is_empty()) {
                o = o.password(pw);
            }
            if !cfg.database.is_empty() {
                o = o.database(&cfg.database);
            }
            o.ssl_mode(ssl_mode(cfg.ssl.mode))
        }
    };
    if let Some(ca) = &cfg.ssl.ca_cert {
        o = o.ssl_ca(ca);
    }
    if let Some(cert) = &cfg.ssl.client_cert {
        o = o.ssl_client_cert(cert);
    }
    if let Some(key) = &cfg.ssl.client_key {
        o = o.ssl_client_key(key);
    }
    // Keep the server's own sql_mode and time zone: a console must behave like the server.
    o = o
        .pipes_as_concat(false)
        .no_engine_substitution(false)
        .timezone(None)
        .charset("utf8mb4")
        .statement_cache_capacity(100);
    for (k, v) in &cfg.options {
        match k.as_str() {
            "charset" => o = o.charset(v),
            "collation" => o = o.collation(v),
            "enable_cleartext_plugin" => o = o.enable_cleartext_plugin(v == "true" || v == "1"),
            _ => log::debug!("ignoring unsupported MySQL option {k}"),
        }
    }
    Ok(o)
}

fn raw<'a>(conn: &'a mut SqlxConn, sql: &'a str) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move { conn.execute(sqlx::raw_sql(sql)).await.map(|_| ()).map_err(map_sqlx_err) })
}

fn bind<'q>(q: MyQuery<'q>, v: &Value) -> MyQuery<'q> {
    match v {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Int(i) => q.bind(*i),
        Value::UInt(u) => q.bind(*u),
        Value::Float(f) => q.bind(*f),
        Value::Decimal(s) | Value::Text(s) => q.bind(s.clone()),
        Value::Bytes(b) => q.bind(b.clone()),
        Value::Date(d) => q.bind(*d),
        Value::Time(t) => q.bind(*t),
        Value::DateTime(dt) => q.bind(*dt),
        Value::DateTimeTz(dt) => q.bind(dt.naive_local()),
        Value::Uuid(u) => q.bind(u.to_string()),
        Value::Json(j) => q.bind(j.to_string()),
        Value::Array(_) | Value::Other { .. } => q.bind(v.to_string()),
    }
}

fn is_zero_temporal_type(ty: &str) -> bool {
    matches!(ty, "DATE" | "DATETIME" | "TIMESTAMP")
}

/// Decode one MySQL cell (binary or text protocol).
fn decode_cell(row: &MySqlRow, i: usize) -> Value {
    let v = match row.try_get_raw(i) {
        Ok(v) => v,
        Err(e) => return Value::Other { type_name: "?".into(), text: format!("<error: {e}>") },
    };
    let ty = v.type_info().name().to_string();
    if v.is_null() {
        // sqlx reports zero dates ('0000-00-00') as NULL although the server sent a value.
        if is_zero_temporal_type(&ty) {
            if let Some(Ok(b)) = row.try_get_raw(i).ok().map(<&[u8] as Decode<MySql>>::decode) {
                let text = match std::str::from_utf8(b) {
                    Ok(s) if s.starts_with(|c: char| c.is_ascii_digit()) => s.to_string(),
                    _ if ty == "DATE" => "0000-00-00".into(),
                    _ => "0000-00-00 00:00:00".into(),
                };
                return Value::Other { type_name: ty, text };
            }
        }
        return Value::Null;
    }
    let r: std::result::Result<Value, sqlx::error::BoxDynError> = match ty.as_str() {
        "BOOLEAN" | "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" | "YEAR" => {
            <i64 as Decode<MySql>>::decode(v).map(Value::Int)
        }
        t if t.ends_with("UNSIGNED") => <u64 as Decode<MySql>>::decode(v).map(Value::UInt),
        "FLOAT" => <f32 as Decode<MySql>>::decode(v).map(|f| Value::Float(f32_to_f64(f))),
        "DOUBLE" => <f64 as Decode<MySql>>::decode(v).map(Value::Float),
        "DECIMAL" => <&str as Decode<MySql>>::decode(v).map(|s| Value::Decimal(s.to_string())),
        "DATE" => <chrono::NaiveDate as Decode<MySql>>::decode(v).map(Value::Date),
        "DATETIME" | "TIMESTAMP" => <chrono::NaiveDateTime as Decode<MySql>>::decode(v).map(Value::DateTime),
        // Decode through MySqlTime: chrono's decoder silently drops the sign and rejects
        // values >= 24h, both legal for MySQL TIME.
        "TIME" => <sqlx::mysql::types::MySqlTime as Decode<MySql>>::decode(v).map(|t| {
            let text = t.to_string();
            let hours: i64 = text.trim_start_matches('-').split(':').next().and_then(|h| h.parse().ok()).unwrap_or(99);
            match chrono::NaiveTime::parse_from_str(&text, "%H:%M:%S%.f") {
                Ok(nt) if !text.starts_with('-') && hours < 24 => Value::Time(nt),
                _ => Value::Other { type_name: "TIME".into(), text },
            }
        }),
        "JSON" => <&str as Decode<MySql>>::decode(v).map(|s| match serde_json::from_str(s) {
            Ok(j) => Value::Json(j),
            Err(_) => Value::Text(s.to_string()),
        }),
        "BIT" => <&[u8] as Decode<MySql>>::decode(v).map(|b| {
            let n = b.iter().fold(0u64, |acc, x| (acc << 8) | u64::from(*x));
            Value::UInt(n)
        }),
        // The protocol flags text columns with a `_bin` collation (e.g. MariaDB JSON) as
        // binary and sqlx drops the charset, so printable UTF-8 is shown as text.
        "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB" => {
            <&[u8] as Decode<MySql>>::decode(v).map(|b| match std::str::from_utf8(b) {
                Ok(s) if !b.is_empty() && !s.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) => {
                    if ty.contains("BLOB") && (s.starts_with('{') || s.starts_with('[')) {
                        serde_json::from_str(s).map(Value::Json).unwrap_or_else(|_| Value::Text(s.to_string()))
                    } else {
                        Value::Text(s.to_string())
                    }
                }
                _ => Value::Bytes(b.to_vec()),
            })
        }
        "GEOMETRY" => <&[u8] as Decode<MySql>>::decode(v)
            .map(|b| Value::Other { type_name: "GEOMETRY".into(), text: hex::encode_upper(b) }),
        "NULL" => Ok(Value::Null),
        _ => <&[u8] as Decode<MySql>>::decode(v).map(|b| match std::str::from_utf8(b) {
            Ok(s) => Value::Text(s.to_string()),
            Err(_) => Value::Bytes(b.to_vec()),
        }),
    };
    match r {
        Ok(val) => val,
        Err(_) => {
            let bytes = row.try_get_raw(i).ok().and_then(|v| <&[u8] as Decode<MySql>>::decode(v).ok());
            match bytes {
                Some(b) if b.is_empty() && is_zero_temporal_type(&ty) => Value::Other {
                    text: if ty == "DATE" { "0000-00-00".into() } else { "0000-00-00 00:00:00".into() },
                    type_name: ty,
                },
                Some(b) => bytes_fallback(&ty, b),
                None => Value::Other { type_name: ty, text: "<undecodable>".into() },
            }
        }
    }
}

fn decode_row(row: &MySqlRow) -> Vec<Value> {
    (0..row.len()).map(|i| decode_cell(row, i)).collect()
}

fn column_meta(c: &sqlx::mysql::MySqlColumn) -> ColumnMeta {
    ColumnMeta::new(c.name(), c.type_info().name())
}

/// Errors after which the statement should be retried with the text protocol.
fn needs_text_protocol(e: &Error, sql: &str) -> bool {
    match e {
        Error::Database(d) => {
            let num = d.code.as_deref().unwrap_or("").split_whitespace().next().unwrap_or("").to_string();
            num == "1295" // ER_UNSUPPORTED_PS
                || (num == "1064" && strip_literals_and_comments(trim_statement(sql)).contains(';'))
        }
        _ => false,
    }
}

async fn set_select_limit(sess: &mut Session, limit: Option<u64>) -> Result<()> {
    if sess.select_limit != limit {
        let sql = match limit {
            Some(n) => format!("SET SQL_SELECT_LIMIT = {n}"),
            None => "SET SQL_SELECT_LIMIT = DEFAULT".into(),
        };
        raw(&mut sess.conn, &sql).await?;
        sess.select_limit = limit;
    }
    Ok(())
}

struct Collected {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    affected: u64,
    truncated: bool,
}

async fn collect<'e, E>(conn: &'e mut SqlxConn, query: E, max_rows: Option<usize>) -> Result<Collected>
where
    E: sqlx::Execute<'e, MySql> + 'e,
{
    let mut out = Collected { columns: vec![], rows: vec![], affected: 0, truncated: false };
    let mut stream = conn.fetch_many(query);
    while let Some(item) = stream.try_next().await.map_err(map_sqlx_err)? {
        match item {
            Either::Left(r) => out.affected += r.rows_affected(),
            Either::Right(row) => {
                let cols = row.columns();
                let same = out.columns.len() == cols.len() && out.columns.iter().zip(cols).all(|(a, b)| a.name == b.name());
                if !same {
                    out.columns = cols.iter().map(column_meta).collect();
                    out.rows.clear();
                    out.truncated = false;
                }
                if max_rows.is_some_and(|m| out.rows.len() >= m) {
                    out.truncated = true;
                    break;
                }
                out.rows.push(decode_row(&row));
            }
        }
    }
    Ok(out)
}

fn run<'a>(sess: &'a mut Session, sql: &'a str, opts: &'a ExecOptions) -> BoxFuture<'a, Result<StatementResult>> {
    Box::pin(async move {
        let start = Instant::now();
        let limit = if is_plain_select(sql) { opts.max_rows.map(|m| m as u64 + 1) } else { None };
        set_select_limit(sess, limit).await?;
        let prepared: Result<Collected> = async {
            let stmt = sess.conn.prepare_with(sql, &[]).await.map_err(map_sqlx_err)?;
            let columns: Vec<ColumnMeta> = stmt.columns().iter().map(column_meta).collect();
            let mut q = stmt.query();
            for p in &opts.params {
                q = bind(q, p);
            }
            let mut c = collect(&mut sess.conn, q, opts.max_rows).await?;
            if c.columns.is_empty() {
                c.columns = columns;
            }
            Ok(c)
        }
        .await;
        let c = match prepared {
            Err(e) if opts.params.is_empty() && needs_text_protocol(&e, sql) => {
                collect(&mut sess.conn, sqlx::raw_sql(sql), opts.max_rows).await?
            }
            other => other?,
        };
        let has_rows = !c.columns.is_empty();
        Ok(StatementResult {
            sql: sql.to_string(),
            columns: c.columns,
            rows: c.rows,
            rows_affected: if has_rows { None } else { Some(c.affected) },
            truncated: c.truncated,
            elapsed: start.elapsed(),
            notices: vec![],
        })
    })
}

impl MySqlConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let base = base_options(cfg)?;
        let endpoint = crate::resolve_endpoint(cfg, base.get_host(), base.get_port()).await?;
        let mut options = base;
        if endpoint.tunnel.is_some() {
            options = options.host(&endpoint.host).port(endpoint.port);
        }
        let mut conn = with_connect_timeout(cfg, async { SqlxConn::connect_with(&options).await.map_err(map_connect_err) }).await?;
        if cfg.read_only {
            if let Err(e) = raw(&mut conn, "SET SESSION TRANSACTION READ ONLY").await {
                log::warn!("could not make session read-only: {e}");
            }
        }
        let mut sess = Session { conn, select_limit: None };
        let info = run(&mut sess, "SELECT CONNECTION_ID(), DATABASE(), VERSION()", &ExecOptions::unlimited()).await?;
        let row = info.rows.first().cloned().unwrap_or_default();
        for stmt in init_statements(cfg) {
            raw(&mut sess.conn, &stmt).await?;
        }
        let mode = if cfg.auto_commit { TransactionMode::Auto } else { TransactionMode::Manual };
        Ok(Self {
            kind: cfg.driver,
            cfg: cfg.clone(),
            options,
            inner: Mutex::new(Some(sess)),
            tx: TxState::new(mode),
            database: RwLock::new(c_str(&row, 1)),
            conn_id: Arc::new(AtomicU64::new(c_i64(&row, 0).unwrap_or(0) as u64)),
            version: c_string(&row, 2),
            tunnel: endpoint.tunnel,
        })
    }

    /// `true` for MariaDB servers.
    pub fn is_mariadb(&self) -> bool {
        self.version.contains("MariaDB")
    }

    /// Internal query with text parameters (no transaction bookkeeping).
    async fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Value>>> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        let opts = ExecOptions { max_rows: None, params: params.iter().map(|p| Value::from(*p)).collect() };
        Ok(run(sess, sql, &opts).await?.rows)
    }

    async fn refresh_database(&self) {
        if let Ok(rows) = self.query("SELECT DATABASE()", &[]).await {
            if let Ok(mut g) = self.database.write() {
                *g = rows.first().and_then(|r| c_str(r, 0));
            }
        }
    }

    async fn schema_or_current(&self, schema: Option<&str>) -> Result<Option<String>> {
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => Ok(Some(s.to_string())),
            None => {
                let rows = self.query("SELECT DATABASE()", &[]).await?;
                Ok(rows.first().and_then(|r| c_str(r, 0)))
            }
        }
    }

    async fn require_schema(&self, schema: Option<&str>) -> Result<String> {
        self.schema_or_current(schema)
            .await?
            .ok_or_else(|| Error::Other("no database selected; pass a schema or USE a database".into()))
    }

    /// Run `SHOW CREATE ...` and return the column whose name starts with `col_prefix`.
    async fn show_create(&self, sql: &str, col_prefix: &str) -> Result<Option<String>> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        let res = run(sess, sql, &ExecOptions::unlimited()).await?;
        let idx = res
            .columns
            .iter()
            .position(|c| c.name.to_ascii_lowercase().starts_with(&col_prefix.to_ascii_lowercase()))
            .unwrap_or(1);
        Ok(res.rows.first().and_then(|r| c_str(r, idx)))
    }

    /// Parameter signatures of routines in a schema: specific name -> "(IN a int, ...)".
    async fn routine_signatures(&self, schema: &str) -> Result<Vec<(String, String)>> {
        let rows = self
            .query(
                "SELECT SPECIFIC_NAME, PARAMETER_MODE, PARAMETER_NAME, DTD_IDENTIFIER FROM information_schema.PARAMETERS \
                 WHERE SPECIFIC_SCHEMA = ? AND ORDINAL_POSITION > 0 ORDER BY SPECIFIC_NAME, ORDINAL_POSITION",
                &[schema],
            )
            .await
            .unwrap_or_default();
        let mut out: Vec<(String, Vec<String>)> = vec![];
        for r in rows {
            let name = c_string(&r, 0);
            let part = [c_str(&r, 1), c_str(&r, 2), c_str(&r, 3)].into_iter().flatten().collect::<Vec<_>>().join(" ");
            match out.iter_mut().find(|(n, _)| *n == name) {
                Some((_, parts)) => parts.push(part),
                None => out.push((name, vec![part])),
            }
        }
        Ok(out.into_iter().map(|(n, p)| (n, format!("({})", p.join(", ")))).collect())
    }

    async fn describe(&self, object: &ObjectRef) -> Result<TableInfo> {
        let schema = self.require_schema(object.schema.as_deref()).await?;
        let name = object.name.as_str();
        let rows = self
            .query(
                "SELECT TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS, TABLE_COLLATION FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                &[&schema, name],
            )
            .await?;
        let t = rows.first().ok_or_else(|| not_found("table", &format!("{schema}.{name}")))?;
        let table_type = c_string(t, 0);
        let kind = match table_type.as_str() {
            "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
            "SEQUENCE" => ObjectKind::Sequence,
            _ => ObjectKind::Table,
        };
        let table_collation = c_str(t, 3);
        let mut info = TableInfo::new(ObjectRef::new(Some(&schema), name, kind));
        info.comment = c_nonempty(t, 1).filter(|c| kind != ObjectKind::View || c != "VIEW");
        info.row_estimate = if kind == ObjectKind::Table { c_i64(t, 2) } else { None };

        // Columns
        let cols = match self
            .query(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT, COLLATION_NAME, \
                        ORDINAL_POSITION, GENERATION_EXPRESSION, DATA_TYPE \
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                &[&schema, name],
            )
            .await
        {
            Ok(r) => r,
            Err(_) => {
                // servers without GENERATION_EXPRESSION (MySQL < 5.7)
                self.query(
                    "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT, COLLATION_NAME, \
                            ORDINAL_POSITION, NULL, DATA_TYPE \
                     FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                    &[&schema, name],
                )
                .await?
            }
        };
        let mariadb = self.is_mariadb();
        for r in &cols {
            let extra = c_string(r, 4);
            let extra_l = extra.to_ascii_lowercase();
            let gen_expr = c_nonempty(r, 8);
            let is_generated = extra_l.contains("generated") || extra_l.contains("virtual") || extra_l.contains("persistent")
                || (extra_l.contains("stored") && gen_expr.is_some());
            let data_type = c_string(r, 9).to_ascii_lowercase();
            let default = if is_generated {
                None
            } else {
                normalize_default(c_str(r, 3), &data_type, &extra_l, mariadb)
            };
            info.columns.push(ColumnInfo {
                name: c_string(r, 0),
                data_type: c_string(r, 1),
                nullable: c_string(r, 2).eq_ignore_ascii_case("YES"),
                default,
                auto_increment: extra_l.contains("auto_increment"),
                generated: if is_generated { gen_expr } else { None },
                comment: c_nonempty(r, 5),
                collation: c_str(r, 6).filter(|c| Some(c) != table_collation.as_ref()),
                ordinal: c_i64(r, 7).unwrap_or(0) as u32,
            });
        }

        if kind == ObjectKind::Table {
            // Indexes (also primary / unique keys)
            let idx_rows = self
                .query(
                    "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE, SUB_PART FROM information_schema.STATISTICS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME, SEQ_IN_INDEX",
                    &[&schema, name],
                )
                .await?;
            for r in &idx_rows {
                let idx_name = c_string(r, 0);
                let col = match (c_str(r, 2), c_i64(r, 4)) {
                    (Some(c), Some(len)) => format!("{c}({len})"),
                    (Some(c), None) => c,
                    (None, _) => "<expression>".into(),
                };
                match info.indexes.iter_mut().find(|i| i.name == idx_name) {
                    Some(i) => i.columns.push(col),
                    None => info.indexes.push(IndexInfo {
                        primary: idx_name == "PRIMARY",
                        unique: c_i64(r, 1).unwrap_or(1) == 0,
                        name: idx_name,
                        columns: vec![col],
                        method: c_str(r, 3).map(|m| m.to_ascii_lowercase()),
                        predicate: None,
                        definition: None,
                    }),
                }
            }
            for i in &mut info.indexes {
                i.definition = Some(index_definition(&schema, name, i));
            }
            for i in &info.indexes {
                let cols: Vec<String> =
                    i.columns.iter().map(|c| c.split('(').next().unwrap_or(c).to_string()).collect();
                if i.primary {
                    info.primary_key = Some(KeyInfo { name: Some("PRIMARY".into()), columns: cols });
                } else if i.unique {
                    info.unique_keys.push(KeyInfo { name: Some(i.name.clone()), columns: cols });
                }
            }

            // Foreign keys
            let fk_rows = self
                .query(
                    "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, \
                            k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
                     FROM information_schema.KEY_COLUMN_USAGE k \
                     JOIN information_schema.REFERENTIAL_CONSTRAINTS r \
                       ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
                      AND r.TABLE_NAME = k.TABLE_NAME \
                     WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL \
                     ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                    &[&schema, name],
                )
                .await?;
            for r in &fk_rows {
                let cname = c_string(r, 0);
                match info.foreign_keys.iter_mut().find(|f| f.name.as_deref() == Some(cname.as_str())) {
                    Some(fk) => {
                        fk.columns.push(c_string(r, 1));
                        fk.ref_columns.push(c_string(r, 4));
                    }
                    None => info.foreign_keys.push(ForeignKeyInfo {
                        name: Some(cname),
                        columns: vec![c_string(r, 1)],
                        ref_schema: c_str(r, 2),
                        ref_table: c_string(r, 3),
                        ref_columns: vec![c_string(r, 4)],
                        on_update: FkAction::parse(&c_string(r, 5)),
                        on_delete: FkAction::parse(&c_string(r, 6)),
                    }),
                }
            }

            // Check constraints (MySQL 8.0.16+, MariaDB 10.2+)
            let checks = if mariadb {
                self.query(
                    "SELECT CONSTRAINT_NAME, CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS \
                     WHERE CONSTRAINT_SCHEMA = ? AND TABLE_NAME = ? ORDER BY CONSTRAINT_NAME",
                    &[&schema, name],
                )
                .await
            } else {
                self.query(
                    "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc \
                     JOIN information_schema.TABLE_CONSTRAINTS tc ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA \
                      AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
                     WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK' \
                     ORDER BY cc.CONSTRAINT_NAME",
                    &[&schema, name],
                )
                .await
            };
            for r in checks.unwrap_or_default() {
                info.checks.push(CheckInfo { name: c_str(&r, 0), expression: c_string(&r, 1) });
            }

            // Triggers
            let trg = self
                .query(
                    "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION FROM information_schema.TRIGGERS \
                     WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME",
                    &[&schema, name],
                )
                .await?;
            for r in trg {
                let tname = c_string(&r, 0);
                let definition = self
                    .show_create(&format!("SHOW CREATE TRIGGER {}", D.qualified(Some(&schema), &tname)), "SQL Original")
                    .await
                    .unwrap_or(None);
                info.triggers.push(TriggerInfo { name: tname, timing: c_string(&r, 1), events: c_string(&r, 2), definition });
            }
        } else if kind == ObjectKind::View {
            let v = self
                .query(
                    "SELECT VIEW_DEFINITION FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                    &[&schema, name],
                )
                .await?;
            info.view_definition = v.first().and_then(|r| c_nonempty(r, 0));
        }
        Ok(info)
    }
}

/// Normalise `COLUMN_DEFAULT` into an SQL expression (MariaDB already reports one).
fn normalize_default(raw: Option<String>, data_type: &str, extra: &str, mariadb: bool) -> Option<String> {
    let v = raw?;
    if mariadb {
        return if v == "NULL" { None } else { Some(v) };
    }
    if extra.contains("default_generated") {
        return Some(v);
    }
    let numeric = ["int", "decimal", "numeric", "float", "double", "real", "bit", "year"].iter().any(|t| data_type.contains(t));
    let upper = v.to_ascii_uppercase();
    if numeric || upper.starts_with("CURRENT_TIMESTAMP") || upper == "NULL" || upper.starts_with("NOW(") {
        Some(v)
    } else {
        Some(D.quote_string(&v))
    }
}

fn index_definition(schema: &str, table: &str, i: &IndexInfo) -> String {
    let cols: Vec<String> = i
        .columns
        .iter()
        .map(|c| match c.split_once('(') {
            Some((name, len)) => format!("{}({len}", D.quote_ident(name)),
            None if c == "<expression>" => c.clone(),
            None => D.quote_ident(c),
        })
        .collect();
    let target = D.qualified(Some(schema), table);
    if i.primary {
        return format!("ALTER TABLE {target} ADD PRIMARY KEY ({})", cols.join(", "));
    }
    let kind = match i.method.as_deref() {
        Some("fulltext") => "FULLTEXT ",
        Some("spatial") => "SPATIAL ",
        _ if i.unique => "UNIQUE ",
        _ => "",
    };
    let using = match i.method.as_deref() {
        Some("hash") => " USING HASH",
        _ => "",
    };
    format!("CREATE {kind}INDEX {} ON {target} ({}){using}", D.quote_ident(&i.name), cols.join(", "))
}

#[async_trait]
impl Connection for MySqlConnection {
    fn driver(&self) -> DriverKind {
        self.kind
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.query("SELECT VERSION()", &[]).await?;
        let v = rows.first().map(|r| c_string(r, 0)).unwrap_or_default();
        Ok(if v.contains("MariaDB") {
            format!("MariaDB {}", v.split("-MariaDB").next().unwrap_or(&v))
        } else if v.contains("TiDB") {
            format!("TiDB {v}")
        } else {
            format!("MySQL {v}")
        })
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
        check_read_only(self.cfg.read_only, sql)?;
        let ctl = tx_control(sql, false);
        let res = {
            let mut guard = self.inner.lock().await;
            let sess = guard.as_mut().ok_or_else(closed)?;
            if self.tx.needs_begin(ctl) {
                raw(&mut sess.conn, "START TRANSACTION").await?;
                self.tx.began();
            }
            let res = run(sess, sql, opts).await;
            self.tx.after_statement(ctl, res.is_ok());
            res
        };
        if first_keyword(sql).eq_ignore_ascii_case("USE") && res.is_ok() {
            self.refresh_database().await;
        }
        res
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(MySqlCancel {
            options: self.options.clone(),
            conn_id: self.conn_id.clone(),
            _tunnel: self.tunnel.clone(),
        }))
    }

    async fn ping(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        sess.conn.ping().await.map_err(map_sqlx_err)
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
            raw(&mut sess.conn, "COMMIT").await?;
            self.tx.ended();
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        if self.tx.is_active() {
            let r = raw(&mut sess.conn, "ROLLBACK").await;
            self.tx.ended();
            r?;
        }
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = self.query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME", &[]).await?;
        Ok(rows.iter().map(|r| c_string(r, 0)).collect())
    }

    fn current_database(&self) -> Option<String> {
        self.database.read().ok().and_then(|g| g.clone())
    }

    async fn use_database(&self, database: &str) -> Result<()> {
        {
            let mut guard = self.inner.lock().await;
            let sess = guard.as_mut().ok_or_else(closed)?;
            raw(&mut sess.conn, &format!("USE {}", D.quote_ident(database))).await?;
        }
        self.refresh_database().await;
        Ok(())
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let mut dbs = self.list_databases().await?;
        if !self.cfg.show_system_objects {
            dbs.retain(|d| !SYSTEM_SCHEMAS.contains(&d.to_ascii_lowercase().as_str()));
        }
        if !self.cfg.schema_filter.is_empty() {
            dbs.retain(|d| self.cfg.schema_filter.iter().any(|f| f == d));
        }
        Ok(dbs)
    }

    async fn current_schema(&self) -> Result<Option<String>> {
        self.schema_or_current(None).await
    }

    async fn list_objects(&self, schema: Option<&str>, kinds: &[ObjectKind]) -> Result<Vec<ObjectInfo>> {
        let Some(schema) = self.schema_or_current(schema).await? else { return Ok(vec![]) };
        let s = schema.as_str();
        let mut out = vec![];
        let want = |k: ObjectKind| kinds.contains(&k);
        if want(ObjectKind::Table) || want(ObjectKind::View) || want(ObjectKind::Sequence) {
            let rows = self
                .query(
                    "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                    &[s],
                )
                .await?;
            for r in rows {
                let kind = match c_string(&r, 1).as_str() {
                    "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
                    "SEQUENCE" => ObjectKind::Sequence,
                    _ => ObjectKind::Table,
                };
                if !want(kind) {
                    continue;
                }
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), kind));
                info.comment = c_nonempty(&r, 2).filter(|c| c != "VIEW");
                if kind == ObjectKind::Table {
                    info.row_estimate = c_i64(&r, 3);
                }
                out.push(info);
            }
        }
        if want(ObjectKind::Function) || want(ObjectKind::Procedure) {
            let sigs = self.routine_signatures(s).await?;
            let rows = self
                .query(
                    "SELECT ROUTINE_NAME, ROUTINE_TYPE, ROUTINE_COMMENT, SPECIFIC_NAME FROM information_schema.ROUTINES \
                     WHERE ROUTINE_SCHEMA = ? ORDER BY ROUTINE_NAME",
                    &[s],
                )
                .await?;
            for r in rows {
                let kind = if c_string(&r, 1) == "PROCEDURE" { ObjectKind::Procedure } else { ObjectKind::Function };
                if !want(kind) {
                    continue;
                }
                let specific = c_string(&r, 3);
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), kind));
                info.comment = c_nonempty(&r, 2);
                info.signature =
                    Some(sigs.iter().find(|(n, _)| *n == specific).map(|(_, sig)| sig.clone()).unwrap_or_else(|| "()".into()));
                out.push(info);
            }
        }
        if want(ObjectKind::Trigger) {
            let rows = self
                .query(
                    "SELECT TRIGGER_NAME, EVENT_OBJECT_TABLE FROM information_schema.TRIGGERS \
                     WHERE TRIGGER_SCHEMA = ? ORDER BY EVENT_OBJECT_TABLE, TRIGGER_NAME",
                    &[s],
                )
                .await?;
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), ObjectKind::Trigger));
                info.parent = c_str(&r, 1);
                out.push(info);
            }
        }
        if want(ObjectKind::Event) {
            let rows = self
                .query(
                    "SELECT EVENT_NAME, EVENT_COMMENT FROM information_schema.EVENTS WHERE EVENT_SCHEMA = ? ORDER BY EVENT_NAME",
                    &[s],
                )
                .await
                .unwrap_or_default();
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), ObjectKind::Event));
                info.comment = c_nonempty(&r, 1);
                out.push(info);
            }
        }
        if want(ObjectKind::Index) {
            let rows = self
                .query(
                    "SELECT DISTINCT INDEX_NAME, TABLE_NAME FROM information_schema.STATISTICS \
                     WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, INDEX_NAME",
                    &[s],
                )
                .await?;
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(s), c_string(&r, 0), ObjectKind::Index));
                info.parent = c_str(&r, 1);
                out.push(info);
            }
        }
        if want(ObjectKind::Schema) {
            for d in self.list_schemas().await? {
                out.push(ObjectInfo::new(ObjectRef::new(None, d, ObjectKind::Schema)));
            }
        }
        Ok(out)
    }

    async fn describe_table(&self, object: &ObjectRef) -> Result<TableInfo> {
        self.describe(object).await
    }

    async fn describe_routine(&self, object: &ObjectRef, signature: Option<&str>) -> Result<RoutineInfo> {
        let schema = self.require_schema(object.schema.as_deref()).await?;
        let rows = self
            .query(
                "SELECT ROUTINE_TYPE, DTD_IDENTIFIER, ROUTINE_BODY, SPECIFIC_NAME FROM information_schema.ROUTINES \
                 WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ?",
                &[&schema, &object.name],
            )
            .await?;
        let r = rows.first().ok_or_else(|| not_found("routine", &format!("{schema}.{}", object.name)))?;
        let is_proc = c_string(r, 0) == "PROCEDURE";
        let kw = if is_proc { "PROCEDURE" } else { "FUNCTION" };
        let definition = self
            .show_create(&format!("SHOW CREATE {kw} {}", D.qualified(Some(&schema), &object.name)), &format!("Create {kw}"))
            .await?
            .ok_or_else(|| Error::Other(format!("no privilege to read the definition of {schema}.{}", object.name)))?;
        let specific = c_string(r, 3);
        let sig = self
            .routine_signatures(&schema)
            .await?
            .into_iter()
            .find(|(n, _)| *n == specific)
            .map(|(_, s)| s)
            .or_else(|| signature.map(str::to_string))
            .unwrap_or_else(|| "()".into());
        Ok(RoutineInfo {
            object: ObjectRef::new(Some(&schema), object.name.clone(), if is_proc { ObjectKind::Procedure } else { ObjectKind::Function }),
            signature: Some(sig),
            return_type: c_nonempty(r, 1),
            language: c_str(r, 2),
            definition: format!("{};", definition.trim_end_matches(';')),
        })
    }

    async fn object_ddl(&self, object: &ObjectRef) -> Result<String> {
        let schema = self.schema_or_current(object.schema.as_deref()).await?;
        let qualified = D.qualified(schema.as_deref(), &object.name);
        let (sql, col) = match object.kind {
            ObjectKind::Table => (format!("SHOW CREATE TABLE {qualified}"), "Create Table"),
            ObjectKind::View => (format!("SHOW CREATE VIEW {qualified}"), "Create View"),
            ObjectKind::Procedure => (format!("SHOW CREATE PROCEDURE {qualified}"), "Create Procedure"),
            ObjectKind::Function => (format!("SHOW CREATE FUNCTION {qualified}"), "Create Function"),
            ObjectKind::Trigger => (format!("SHOW CREATE TRIGGER {qualified}"), "SQL Original"),
            ObjectKind::Event => (format!("SHOW CREATE EVENT {qualified}"), "Create Event"),
            ObjectKind::Sequence => (format!("SHOW CREATE SEQUENCE {qualified}"), "Create Table"),
            ObjectKind::Database | ObjectKind::Schema => {
                (format!("SHOW CREATE DATABASE {}", D.quote_ident(&object.name)), "Create Database")
            }
            ObjectKind::Index => {
                let schema = schema.ok_or_else(|| Error::Other("no database selected".into()))?;
                let rows = self
                    .query(
                        "SELECT DISTINCT TABLE_NAME FROM information_schema.STATISTICS \
                         WHERE TABLE_SCHEMA = ? AND INDEX_NAME = ?",
                        &[&schema, &object.name],
                    )
                    .await?;
                let mut out = vec![];
                for r in rows {
                    let t = self.describe(&ObjectRef::table(Some(&schema), c_string(&r, 0))).await?;
                    if let Some(i) = t.indexes.iter().find(|i| i.name == object.name) {
                        out.push(format!("{};", i.definition.clone().unwrap_or_default()));
                    }
                }
                if out.is_empty() {
                    return Err(not_found("index", &object.display_name()));
                }
                return Ok(out.join("\n") + "\n");
            }
            other => return Err(Error::Unsupported(format!("DDL for {} objects", other.label()))),
        };
        let ddl = self
            .show_create(&sql, col)
            .await?
            .ok_or_else(|| Error::Other(format!("no DDL available for {} (insufficient privileges?)", object.display_name())))?;
        let mut out = format!("{};\n", ddl.trim_end().trim_end_matches(';'));
        if object.kind == ObjectKind::Table {
            // Triggers are not part of SHOW CREATE TABLE.
            if let Some(s) = schema.as_deref() {
                let trg = self
                    .query(
                        "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS \
                         WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME",
                        &[s, &object.name],
                    )
                    .await
                    .unwrap_or_default();
                for r in trg {
                    let q = format!("SHOW CREATE TRIGGER {}", D.qualified(Some(s), &c_string(&r, 0)));
                    if let Ok(Some(def)) = self.show_create(&q, "SQL Original").await {
                        out.push_str(&format!("\n{};\n", def.trim_end_matches(';')));
                    }
                }
            }
        }
        Ok(out)
    }

    async fn list_roles(&self) -> Result<Vec<RoleInfo>> {
        let rows = self.query("SELECT User, Host, Super_priv FROM mysql.user ORDER BY User, Host", &[]).await?;
        Ok(rows
            .iter()
            .map(|r| RoleInfo {
                name: format!("{}@{}", c_string(r, 0), c_string(r, 1)),
                superuser: c_string(r, 2).eq_ignore_ascii_case("Y"),
                can_login: true,
                member_of: vec![],
            })
            .collect())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let rows = self
            .query(
                "SELECT ID, USER, DB, HOST, COMMAND, STATE, INFO, TIME FROM information_schema.PROCESSLIST ORDER BY ID",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| SessionInfo {
                id: c_string(r, 0),
                user: c_str(r, 1),
                database: c_str(r, 2),
                client: c_str(r, 3),
                state: Some(
                    [c_str(r, 4), c_nonempty(r, 5)].into_iter().flatten().collect::<Vec<_>>().join(": "),
                ),
                query: c_str(r, 6),
                started: c_i64(r, 7).map(|s| format!("{s} s ago")),
            })
            .collect())
    }

    async fn kill_session(&self, id: &str) -> Result<()> {
        let n: u64 = id.trim().parse().map_err(|_| Error::Other(format!("invalid connection id: {id}")))?;
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        raw(&mut sess.conn, &format!("KILL {n}")).await
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<StatementResult> {
        let body = trim_statement(sql);
        let prefixes: &[&str] = if self.is_mariadb() {
            if analyze { &["ANALYZE FORMAT=JSON ", "ANALYZE "] } else { &["EXPLAIN "] }
        } else if analyze {
            &["EXPLAIN ANALYZE "]
        } else {
            &["EXPLAIN FORMAT=TREE ", "EXPLAIN "]
        };
        let mut last = None;
        for p in prefixes {
            match self.execute(&format!("{p}{body}"), &ExecOptions::unlimited()).await {
                Ok(r) => return Ok(r),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| Error::Unsupported("EXPLAIN".into())))
    }

    async fn close(&self) -> Result<()> {
        let sess = self.inner.lock().await.take();
        if let Some(s) = sess {
            s.conn.close().await.map_err(map_sqlx_err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        assert_eq!(normalize_default(Some("abc".into()), "varchar", "", false).as_deref(), Some("'abc'"));
        assert_eq!(normalize_default(Some("5".into()), "int", "", false).as_deref(), Some("5"));
        assert_eq!(normalize_default(Some("CURRENT_TIMESTAMP".into()), "timestamp", "", false).as_deref(), Some("CURRENT_TIMESTAMP"));
        assert_eq!(normalize_default(Some("NULL".into()), "varchar", "", true), None);
        assert_eq!(normalize_default(Some("'x'".into()), "varchar", "", true).as_deref(), Some("'x'"));
        assert_eq!(normalize_default(None, "int", "", false), None);
    }

    #[test]
    fn index_ddl() {
        let i = IndexInfo {
            name: "ix".into(),
            columns: vec!["a".into(), "b(10)".into()],
            unique: true,
            method: Some("btree".into()),
            ..Default::default()
        };
        assert_eq!(index_definition("s", "t", &i), "CREATE UNIQUE INDEX `ix` ON s.t (`a`, `b`(10))");
    }

    #[test]
    fn text_protocol_detection() {
        let e = Error::Database(tabularium_core::error::DatabaseError {
            code: Some("1295 (HY000)".into()),
            ..Default::default()
        });
        assert!(needs_text_protocol(&e, "LOCK TABLES t READ"));
        let e = Error::Database(tabularium_core::error::DatabaseError { code: Some("1064 (42000)".into()), ..Default::default() });
        assert!(needs_text_protocol(&e, "select 1; select 2"));
        assert!(!needs_text_protocol(&e, "selec 1;"));
    }
}
