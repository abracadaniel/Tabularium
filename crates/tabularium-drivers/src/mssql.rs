//! Microsoft SQL Server driver (tiberius over tokio).
//!
//! Positional parameters use T-SQL's `@P1`, `@P2`, ... placeholders. Row limits on plain
//! SELECTs are applied with `SET ROWCOUNT` (reset before any other statement, since it
//! also limits DML). Query cancellation is not supported (tiberius exposes no attention
//! signal), so `cancel_handle()` returns `None`.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::TryStreamExt;
use tabularium_core::driver::first_keyword;
use tabularium_core::error::DatabaseError;
use tabularium_core::model::*;
use tabularium_core::{
    ColumnMeta, Connection, ConnectionConfig, Dialect, DriverKind, Error, ExecOptions, Result, SslMode,
    StatementResult, TransactionMode, Value,
};
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, FromSql, Query, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::common::*;
use crate::ssh::SshTunnel;

type TdsClient = Client<Compat<TcpStream>>;

const D: Dialect = Dialect::MsSql;

struct Session {
    client: TdsClient,
    /// Current `SET ROWCOUNT` (`None` = 0, unlimited).
    rowcount: Option<u64>,
}

/// One SQL Server session.
pub struct MsSqlConnection {
    cfg: ConnectionConfig,
    inner: Mutex<Option<Session>>,
    tx: TxState,
    database: RwLock<Option<String>>,
    _tunnel: Option<Arc<SshTunnel>>,
}

fn map_tds_err(e: tiberius::error::Error) -> Error {
    use tiberius::error::Error as E;
    match e {
        E::Server(t) => {
            let mut detail = format!("Line {}", t.line());
            if !t.procedure().is_empty() {
                detail.push_str(&format!(", procedure {}", t.procedure()));
            }
            Error::Database(DatabaseError {
                message: t.message().to_string(),
                code: Some(t.code().to_string()),
                detail: Some(detail),
                hint: None,
                position: None,
            })
        }
        E::Io { kind, message } => Error::Connection(format!("i/o error ({kind:?}): {message}")),
        E::Tls(m) => Error::Connection(format!("TLS error: {m}")),
        E::Routing { host, port } => Error::Connection(format!("server redirected to {host}:{port}")),
        other => Error::Other(other.to_string()),
    }
}

/// Build the tiberius config (before SSH tunnelling) and return (config, host, port).
fn base_config(cfg: &ConnectionConfig) -> Result<(Config, String, u16)> {
    let mut c = cfg.clone();
    if let Some(url) = cfg.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        if url.to_ascii_lowercase().starts_with("jdbc:") {
            let conf = Config::from_jdbc_string(url).map_err(|e| Error::Config(format!("invalid JDBC string: {e}")))?;
            return finish_config(conf, cfg);
        } else if url.contains('=') && url.contains(';') && !url.contains("://") {
            let conf = Config::from_ado_string(url).map_err(|e| Error::Config(format!("invalid ADO string: {e}")))?;
            return finish_config(conf, cfg);
        } else {
            let parsed = ConnectionConfig::from_url(url)?;
            c.host = parsed.host;
            c.port = parsed.port;
            c.user = parsed.user;
            if parsed.password.is_some() {
                c.password = parsed.password;
            }
            c.database = parsed.database;
        }
    }
    let mut conf = Config::new();
    let host = if c.host.trim().is_empty() { "localhost".to_string() } else { c.host.trim().to_string() };
    let port = if c.port == 0 { 1433 } else { c.port };
    conf.host(&host);
    conf.port(port);
    if !c.database.is_empty() {
        conf.database(&c.database);
    }
    let user = if c.user.is_empty() { "sa" } else { c.user.as_str() };
    conf.authentication(AuthMethod::sql_server(user, c.password.clone().unwrap_or_default()));
    finish_config(conf, cfg)
}

fn finish_config(mut conf: Config, cfg: &ConnectionConfig) -> Result<(Config, String, u16)> {
    conf.application_name("Tabularium");
    match cfg.ssl.mode {
        SslMode::Disable => conf.encryption(EncryptionLevel::NotSupported),
        SslMode::Prefer => {
            conf.encryption(EncryptionLevel::Off);
            conf.trust_cert();
        }
        SslMode::Require => {
            conf.encryption(EncryptionLevel::Required);
            conf.trust_cert();
        }
        SslMode::VerifyCa | SslMode::VerifyFull => {
            conf.encryption(EncryptionLevel::Required);
            if let Some(ca) = &cfg.ssl.ca_cert {
                conf.trust_cert_ca(ca.to_string_lossy());
            }
        }
    }
    let addr = conf.get_addr();
    let (host, port) = match addr.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(1433)),
        None => (addr, 1433),
    };
    Ok((conf, host, port))
}

async fn open_client(conf: Config) -> Result<TdsClient> {
    let tcp = TcpStream::connect(conf.get_addr()).await?;
    let _ = tcp.set_nodelay(true);
    match Client::connect(conf.clone(), tcp.compat_write()).await {
        Ok(c) => Ok(c),
        Err(tiberius::error::Error::Routing { host, port }) => {
            // Azure SQL gateway redirect.
            let mut conf = conf;
            conf.host(&host);
            conf.port(port);
            let tcp = TcpStream::connect(conf.get_addr()).await?;
            let _ = tcp.set_nodelay(true);
            Client::connect(conf, tcp.compat_write()).await.map_err(|e| match map_tds_err(e) {
                Error::Database(d) => Error::Connection(d.to_string()),
                other => other,
            })
        }
        Err(e) => Err(match map_tds_err(e) {
            Error::Database(d) => Error::Connection(d.to_string()),
            Error::Other(m) => Error::Connection(m),
            other => other,
        }),
    }
}

/// SQL type name of a result column.
fn column_type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Intn => "int",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money | ColumnType::Money4 => "money",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigBinary => "binary",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

/// Convert a tiberius cell to a core value.
pub(crate) fn decode(data: &ColumnData<'static>) -> Value {
    fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
        v.map(f).unwrap_or(Value::Null)
    }
    match data {
        ColumnData::U8(v) => opt(*v, |x| Value::Int(x as i64)),
        ColumnData::I16(v) => opt(*v, |x| Value::Int(x as i64)),
        ColumnData::I32(v) => opt(*v, |x| Value::Int(x as i64)),
        ColumnData::I64(v) => opt(*v, Value::Int),
        ColumnData::F32(v) => opt(*v, |x| Value::Float(f32_to_f64(x))),
        ColumnData::F64(v) => opt(*v, Value::Float),
        ColumnData::Bit(v) => opt(*v, Value::Bool),
        ColumnData::String(v) => opt(v.as_ref(), |s| Value::Text(s.to_string())),
        ColumnData::Guid(v) => opt(*v, Value::Uuid),
        ColumnData::Binary(v) => opt(v.as_ref(), |b| Value::Bytes(b.to_vec())),
        ColumnData::Numeric(v) => opt(*v, |n| Value::Decimal(n.to_string())),
        ColumnData::Xml(v) => opt(v.as_ref(), |x| Value::Other { type_name: "xml".into(), text: x.to_string() }),
        ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => {
            match chrono::NaiveDateTime::from_sql(data) {
                Ok(Some(dt)) => Value::DateTime(dt),
                Ok(None) => Value::Null,
                Err(e) => Value::Other { type_name: "datetime".into(), text: format!("<{e}>") },
            }
        }
        ColumnData::Date(_) => match chrono::NaiveDate::from_sql(data) {
            Ok(Some(d)) => Value::Date(d),
            Ok(None) => Value::Null,
            Err(e) => Value::Other { type_name: "date".into(), text: format!("<{e}>") },
        },
        ColumnData::Time(_) => match chrono::NaiveTime::from_sql(data) {
            Ok(Some(t)) => Value::Time(t),
            Ok(None) => Value::Null,
            Err(e) => Value::Other { type_name: "time".into(), text: format!("<{e}>") },
        },
        ColumnData::DateTimeOffset(_) => match chrono::DateTime::<chrono::FixedOffset>::from_sql(data) {
            Ok(Some(dt)) => Value::DateTimeTz(dt),
            Ok(None) => Value::Null,
            Err(e) => Value::Other { type_name: "datetimeoffset".into(), text: format!("<{e}>") },
        },
    }
}

fn bind_value<'a>(q: &mut Query<'a>, v: &Value) {
    match v {
        Value::Null => q.bind(Option::<String>::None),
        Value::Bool(b) => q.bind(*b),
        Value::Int(i) => q.bind(*i),
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(i) => q.bind(i),
            Err(_) => q.bind(u.to_string()),
        },
        Value::Float(f) => q.bind(*f),
        Value::Decimal(s) | Value::Text(s) => q.bind(s.clone()),
        Value::Bytes(b) => q.bind(b.clone()),
        Value::Date(d) => q.bind(*d),
        Value::Time(t) => q.bind(*t),
        Value::DateTime(dt) => q.bind(*dt),
        Value::DateTimeTz(dt) => q.bind(*dt),
        Value::Uuid(u) => q.bind(*u),
        Value::Json(j) => q.bind(j.to_string()),
        Value::Array(_) | Value::Other { .. } => q.bind(v.to_string()),
    }
}

/// DML whose affected-row count we want (and which returns no rows).
fn is_plain_dml(sql: &str) -> bool {
    matches!(first_keyword(sql).to_ascii_uppercase().as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE")
        && !contains_word(sql, "OUTPUT")
}

struct Collected {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
    truncated: bool,
}

async fn drain(mut stream: tiberius::QueryStream<'_>, max_rows: Option<usize>) -> Result<Collected> {
    let mut out = Collected { columns: vec![], rows: vec![], truncated: false };
    while let Some(item) = stream.try_next().await.map_err(map_tds_err)? {
        match item {
            QueryItem::Metadata(m) => {
                // a new result set starts; keep the last one
                out.columns = m
                    .columns()
                    .iter()
                    .map(|c| ColumnMeta::new(c.name(), column_type_name(c.column_type())))
                    .collect();
                out.rows.clear();
                out.truncated = false;
            }
            QueryItem::Row(row) => {
                if max_rows.is_some_and(|m| out.rows.len() >= m) {
                    out.truncated = true;
                    break;
                }
                out.rows.push(row.cells().map(|(_, d)| decode(d)).collect());
            }
        }
    }
    Ok(out)
}

fn simple<'a>(client: &'a mut TdsClient, sql: &'a str) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let stream = client.simple_query(sql).await.map_err(map_tds_err)?;
        stream.into_results().await.map_err(map_tds_err)?;
        Ok(())
    })
}

async fn set_rowcount(sess: &mut Session, limit: Option<u64>) -> Result<()> {
    if sess.rowcount != limit {
        let sql = format!("SET ROWCOUNT {}", limit.unwrap_or(0));
        simple(&mut sess.client, &sql).await?;
        sess.rowcount = limit;
    }
    Ok(())
}

fn run<'a>(sess: &'a mut Session, sql: &'a str, opts: &'a ExecOptions) -> BoxFuture<'a, Result<StatementResult>> {
    Box::pin(async move {
        let start = Instant::now();
        let limit = if is_plain_select(sql) { opts.max_rows.map(|m| m as u64 + 1) } else { None };
        set_rowcount(sess, limit).await?;
        let mut result = StatementResult { sql: sql.to_string(), ..Default::default() };
        if is_plain_dml(sql) {
            let mut q = Query::new(sql.to_string());
            for p in &opts.params {
                bind_value(&mut q, p);
            }
            let r = q.execute(&mut sess.client).await.map_err(map_tds_err)?;
            result.rows_affected = Some(r.total());
        } else {
            let stream = if opts.params.is_empty() {
                sess.client.simple_query(sql.to_string()).await.map_err(map_tds_err)?
            } else {
                let mut q = Query::new(sql.to_string());
                for p in &opts.params {
                    bind_value(&mut q, p);
                }
                q.query(&mut sess.client).await.map_err(map_tds_err)?
            };
            let c = drain(stream, opts.max_rows).await?;
            result.columns = c.columns;
            result.rows = c.rows;
            result.truncated = c.truncated;
        }
        result.elapsed = start.elapsed();
        Ok(result)
    })
}

/// Full T-SQL type from sys.columns / sys.parameters metadata.
fn format_type(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length < 0 {
                format!("{n}(max)")
            } else {
                format!("{n}({max_length})")
            }
        }
        "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{n}(max)")
            } else {
                format!("{n}({})", max_length / 2)
            }
        }
        "decimal" | "numeric" => format!("{n}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{n}({scale})"),
        "float" if precision != 53 && precision > 0 => format!("float({precision})"),
        _ => name.to_string(),
    }
}

impl MsSqlConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let (mut conf, host, port) = base_config(cfg)?;
        let endpoint = crate::resolve_endpoint(cfg, &host, port).await?;
        if endpoint.tunnel.is_some() {
            conf.host(&endpoint.host);
            conf.port(endpoint.port);
        }
        let client = with_connect_timeout(cfg, open_client(conf)).await?;
        let mut sess = Session { client, rowcount: None };
        for stmt in init_statements(cfg) {
            simple(&mut sess.client, &stmt).await?;
        }
        let db = run(&mut sess, "SELECT DB_NAME()", &ExecOptions::unlimited()).await?;
        let mode = if cfg.auto_commit { TransactionMode::Auto } else { TransactionMode::Manual };
        Ok(Self {
            cfg: cfg.clone(),
            database: RwLock::new(db.rows.first().and_then(|r| c_str(r, 0))),
            inner: Mutex::new(Some(sess)),
            tx: TxState::new(mode),
            _tunnel: endpoint.tunnel,
        })
    }

    /// Internal query with text parameters (`@P1`...), no transaction bookkeeping.
    async fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<Value>>> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        let opts = ExecOptions { max_rows: None, params: params.iter().map(|p| Value::from(*p)).collect() };
        Ok(run(sess, sql, &opts).await?.rows)
    }

    async fn refresh_database(&self) {
        if let Ok(rows) = self.query("SELECT DB_NAME()", &[]).await {
            if let Ok(mut g) = self.database.write() {
                *g = rows.first().and_then(|r| c_str(r, 0));
            }
        }
    }

    async fn schema_or_default(&self, schema: Option<&str>) -> Result<String> {
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => Ok(s.to_string()),
            None => {
                let rows = self.query("SELECT SCHEMA_NAME()", &[]).await?;
                Ok(rows.first().and_then(|r| c_str(r, 0)).unwrap_or_else(|| "dbo".into()))
            }
        }
    }

    /// object_id of `schema.name` (as text) and its sys.objects type.
    async fn object_id(&self, schema: &str, name: &str) -> Result<(String, String)> {
        let rows = self
            .query(
                "SELECT CAST(o.object_id AS varchar(20)), RTRIM(o.type) FROM sys.objects o \
                 JOIN sys.schemas s ON s.schema_id = o.schema_id WHERE s.name = @P1 AND o.name = @P2",
                &[schema, name],
            )
            .await?;
        rows.first()
            .map(|r| (c_string(r, 0), c_string(r, 1)))
            .ok_or_else(|| not_found("object", &format!("{schema}.{name}")))
    }

    async fn definition(&self, object_id: &str) -> Result<Option<String>> {
        let rows = self.query("SELECT OBJECT_DEFINITION(CAST(@P1 AS int))", &[object_id]).await?;
        Ok(rows.first().and_then(|r| c_str(r, 0)))
    }

    async fn describe(&self, object: &ObjectRef) -> Result<(TableInfo, String, Vec<(String, String, String)>)> {
        let schema = self.schema_or_default(object.schema.as_deref()).await?;
        let (oid, otype) = self.object_id(&schema, &object.name).await?;
        let kind = if otype == "V" { ObjectKind::View } else { ObjectKind::Table };
        let mut info = TableInfo::new(ObjectRef::new(Some(&schema), object.name.clone(), kind));
        let meta = self
            .query(
                "SELECT CAST(ep.value AS nvarchar(4000)), \
                        (SELECT CAST(SUM(p.rows) AS bigint) FROM sys.partitions p WHERE p.object_id = CAST(@P1 AS int) AND p.index_id IN (0, 1)), \
                        CAST(DATABASEPROPERTYEX(DB_NAME(), 'Collation') AS nvarchar(128)) \
                 FROM (SELECT 1 AS x) d LEFT JOIN sys.extended_properties ep ON ep.major_id = CAST(@P1 AS int) \
                  AND ep.minor_id = 0 AND ep.class = 1 AND ep.name = 'MS_Description'",
                &[&oid],
            )
            .await?;
        let db_collation = meta.first().and_then(|r| c_str(r, 2));
        if let Some(r) = meta.first() {
            info.comment = c_nonempty(r, 0);
            info.row_estimate = if kind == ObjectKind::Table { c_i64(r, 1) } else { None };
        }
        let cols = self
            .query(
                "SELECT c.name, TYPE_NAME(c.user_type_id), CAST(c.max_length AS int), CAST(c.precision AS int), \
                        CAST(c.scale AS int), c.is_nullable, dc.definition, c.is_identity, cc.definition, \
                        CAST(ep.value AS nvarchar(4000)), c.collation_name, c.column_id, cc.is_persisted, dc.name, \
                        CAST(ic.seed_value AS bigint), CAST(ic.increment_value AS bigint) \
                 FROM sys.columns c \
                 LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
                 LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                 LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id \
                 LEFT JOIN sys.extended_properties ep ON ep.major_id = c.object_id AND ep.minor_id = c.column_id \
                  AND ep.class = 1 AND ep.name = 'MS_Description' \
                 WHERE c.object_id = CAST(@P1 AS int) ORDER BY c.column_id",
                &[&oid],
            )
            .await?;
        // (column, default constraint name, identity spec) for DDL generation
        let mut extra: Vec<(String, String, String)> = vec![];
        for r in &cols {
            let name = c_string(r, 0);
            let identity = c_bool(r, 7);
            let computed = c_str(r, 8);
            extra.push((
                name.clone(),
                c_string(r, 13),
                if identity {
                    format!("IDENTITY({},{})", c_i64(r, 14).unwrap_or(1), c_i64(r, 15).unwrap_or(1))
                } else if c_bool(r, 12) {
                    "PERSISTED".into()
                } else {
                    String::new()
                },
            ));
            info.columns.push(ColumnInfo {
                name,
                data_type: format_type(&c_string(r, 1), c_i64(r, 2).unwrap_or(0), c_i64(r, 3).unwrap_or(0), c_i64(r, 4).unwrap_or(0)),
                nullable: c_bool(r, 5),
                default: c_str(r, 6),
                auto_increment: identity,
                generated: computed,
                comment: c_nonempty(r, 9),
                collation: c_str(r, 10).filter(|c| Some(c) != db_collation.as_ref()),
                ordinal: c_i64(r, 11).unwrap_or(0) as u32,
            });
        }
        if kind == ObjectKind::Table {
            let idx = self
                .query(
                    "SELECT i.name, i.is_unique, i.is_primary_key, i.type_desc, i.filter_definition, \
                            i.is_unique_constraint, COL_NAME(ic.object_id, ic.column_id), ic.is_descending_key \
                     FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     WHERE i.object_id = CAST(@P1 AS int) AND i.name IS NOT NULL AND ic.is_included_column = 0 \
                     ORDER BY i.index_id, ic.key_ordinal",
                    &[&oid],
                )
                .await?;
            let mut unique_constraints = vec![];
            for r in &idx {
                let name = c_string(r, 0);
                let mut col = c_string(r, 6);
                if c_bool(r, 7) {
                    col.push_str(" DESC");
                }
                if c_bool(r, 5) && !unique_constraints.contains(&name) {
                    unique_constraints.push(name.clone());
                }
                match info.indexes.iter_mut().find(|i| i.name == name) {
                    Some(i) => i.columns.push(col),
                    None => info.indexes.push(IndexInfo {
                        name,
                        columns: vec![col],
                        unique: c_bool(r, 1),
                        primary: c_bool(r, 2),
                        method: c_str(r, 3).map(|m| m.to_ascii_lowercase()),
                        predicate: c_str(r, 4),
                        definition: None,
                    }),
                }
            }
            for i in &mut info.indexes {
                let plain: Vec<String> = i.columns.iter().map(|c| c.trim_end_matches(" DESC").to_string()).collect();
                if i.primary {
                    info.primary_key = Some(KeyInfo { name: Some(i.name.clone()), columns: plain });
                } else if unique_constraints.contains(&i.name) {
                    info.unique_keys.push(KeyInfo { name: Some(i.name.clone()), columns: plain });
                } else {
                    let cols: Vec<String> = i
                        .columns
                        .iter()
                        .map(|c| match c.strip_suffix(" DESC") {
                            Some(n) => format!("{} DESC", D.quote_ident(n)),
                            None => D.quote_ident(c),
                        })
                        .collect();
                    let clustered = if i.method.as_deref() == Some("clustered") { "CLUSTERED " } else { "NONCLUSTERED " };
                    let mut def = format!(
                        "CREATE {}{clustered}INDEX {} ON {} ({})",
                        if i.unique { "UNIQUE " } else { "" },
                        D.quote_ident(&i.name),
                        D.qualified(Some(&schema), &object.name),
                        cols.join(", ")
                    );
                    if let Some(p) = &i.predicate {
                        def.push_str(&format!(" WHERE {p}"));
                    }
                    i.definition = Some(def);
                }
            }
            let fks = self
                .query(
                    "SELECT fk.name, COL_NAME(fkc.parent_object_id, fkc.parent_column_id), SCHEMA_NAME(rt.schema_id), rt.name, \
                            COL_NAME(fkc.referenced_object_id, fkc.referenced_column_id), \
                            fk.update_referential_action_desc, fk.delete_referential_action_desc \
                     FROM sys.foreign_keys fk \
                     JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                     JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
                     WHERE fk.parent_object_id = CAST(@P1 AS int) ORDER BY fk.name, fkc.constraint_column_id",
                    &[&oid],
                )
                .await?;
            for r in &fks {
                let name = c_string(r, 0);
                match info.foreign_keys.iter_mut().find(|f| f.name.as_deref() == Some(name.as_str())) {
                    Some(fk) => {
                        fk.columns.push(c_string(r, 1));
                        fk.ref_columns.push(c_string(r, 4));
                    }
                    None => info.foreign_keys.push(ForeignKeyInfo {
                        name: Some(name),
                        columns: vec![c_string(r, 1)],
                        ref_schema: c_str(r, 2),
                        ref_table: c_string(r, 3),
                        ref_columns: vec![c_string(r, 4)],
                        on_update: FkAction::parse(&c_string(r, 5)),
                        on_delete: FkAction::parse(&c_string(r, 6)),
                    }),
                }
            }
            let checks = self
                .query(
                    "SELECT name, definition FROM sys.check_constraints WHERE parent_object_id = CAST(@P1 AS int) ORDER BY name",
                    &[&oid],
                )
                .await?;
            for r in checks {
                info.checks.push(CheckInfo { name: c_str(&r, 0), expression: c_string(&r, 1) });
            }
        } else {
            info.view_definition = self.definition(&oid).await?;
        }
        let trg = self
            .query(
                "SELECT t.name, CAST(t.object_id AS varchar(20)), t.is_instead_of_trigger FROM sys.triggers t \
                 WHERE t.parent_id = CAST(@P1 AS int) ORDER BY t.name",
                &[&oid],
            )
            .await?;
        for r in trg {
            let tid = c_string(&r, 1);
            let events = self
                .query("SELECT type_desc FROM sys.trigger_events WHERE object_id = CAST(@P1 AS int)", &[&tid])
                .await?
                .iter()
                .map(|e| c_string(e, 0))
                .collect::<Vec<_>>()
                .join(" OR ");
            info.triggers.push(TriggerInfo {
                name: c_string(&r, 0),
                timing: if c_bool(&r, 2) { "INSTEAD OF".into() } else { "AFTER".into() },
                events,
                definition: self.definition(&tid).await?,
            });
        }
        Ok((info, oid, extra))
    }

    async fn routine_signature(&self, oid: &str) -> Result<(String, Option<String>)> {
        let rows = self
            .query(
                "SELECT name, TYPE_NAME(user_type_id), CAST(max_length AS int), CAST(precision AS int), CAST(scale AS int), \
                        is_output, parameter_id FROM sys.parameters WHERE object_id = CAST(@P1 AS int) ORDER BY parameter_id",
                &[oid],
            )
            .await?;
        let mut params = vec![];
        let mut ret = None;
        for r in rows {
            let ty = format_type(&c_string(&r, 1), c_i64(&r, 2).unwrap_or(0), c_i64(&r, 3).unwrap_or(0), c_i64(&r, 4).unwrap_or(0));
            if c_i64(&r, 6) == Some(0) {
                ret = Some(ty);
            } else {
                params.push(format!("{} {ty}{}", c_string(&r, 0), if c_bool(&r, 5) { " OUTPUT" } else { "" }));
            }
        }
        Ok((format!("({})", params.join(", ")), ret))
    }
}

/// Generated `CREATE TABLE` for SQL Server.
fn table_ddl(t: &TableInfo, extra: &[(String, String, String)]) -> String {
    let schema = t.object.schema.as_deref().unwrap_or("dbo");
    let name = D.qualified(Some(schema), &t.object.name);
    let mut lines = vec![];
    for c in &t.columns {
        let (_, df_name, spec) = extra.iter().find(|(n, _, _)| n == &c.name).cloned().unwrap_or_default();
        let mut line = format!("    {}", D.quote_ident(&c.name));
        if let Some(expr) = &c.generated {
            line.push_str(&format!(" AS {expr}"));
            if spec == "PERSISTED" {
                line.push_str(" PERSISTED");
            }
        } else {
            line.push_str(&format!(" {}", c.data_type));
            if spec.starts_with("IDENTITY") {
                line.push_str(&format!(" {spec}"));
            }
            if let Some(coll) = &c.collation {
                line.push_str(&format!(" COLLATE {coll}"));
            }
            line.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            if let Some(def) = &c.default {
                if df_name.is_empty() {
                    line.push_str(&format!(" DEFAULT {def}"));
                } else {
                    line.push_str(&format!(" CONSTRAINT {} DEFAULT {def}", D.quote_ident(&df_name)));
                }
            }
        }
        lines.push(line);
    }
    let cols = |cs: &[String]| cs.iter().map(|c| D.quote_ident(c)).collect::<Vec<_>>().join(", ");
    if let Some(pk) = &t.primary_key {
        let clustered = t.indexes.iter().find(|i| i.primary).and_then(|i| i.method.clone()).unwrap_or_default();
        lines.push(format!(
            "    CONSTRAINT {} PRIMARY KEY {}({})",
            D.quote_ident(pk.name.as_deref().unwrap_or("PK")),
            if clustered == "clustered" { "CLUSTERED " } else if clustered.is_empty() { "" } else { "NONCLUSTERED " },
            cols(&pk.columns)
        ));
    }
    for u in &t.unique_keys {
        lines.push(format!("    CONSTRAINT {} UNIQUE ({})", D.quote_ident(u.name.as_deref().unwrap_or("UQ")), cols(&u.columns)));
    }
    for ch in &t.checks {
        lines.push(format!("    CONSTRAINT {} CHECK {}", D.quote_ident(ch.name.as_deref().unwrap_or("CK")), ch.expression));
    }
    for fk in &t.foreign_keys {
        lines.push(format!(
            "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON UPDATE {} ON DELETE {}",
            D.quote_ident(fk.name.as_deref().unwrap_or("FK")),
            cols(&fk.columns),
            D.qualified(fk.ref_schema.as_deref(), &fk.ref_table),
            cols(&fk.ref_columns),
            fk.on_update.sql(),
            fk.on_delete.sql()
        ));
    }
    let mut s = format!("CREATE TABLE {name} (\n{}\n);\n", lines.join(",\n"));
    for i in &t.indexes {
        if let Some(def) = &i.definition {
            s.push_str(&format!("{def};\n"));
        }
    }
    for tr in &t.triggers {
        if let Some(def) = &tr.definition {
            s.push_str(&format!("GO\n{}\n", def.trim()));
        }
    }
    s
}

#[async_trait]
impl Connection for MsSqlConnection {
    fn driver(&self) -> DriverKind {
        DriverKind::MsSql
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.query("SELECT @@VERSION", &[]).await?;
        Ok(rows.first().map(|r| c_string(r, 0)).unwrap_or_default().lines().next().unwrap_or("").trim().to_string())
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
        check_read_only(self.cfg.read_only, sql)?;
        let ctl = tx_control(sql, true);
        let res = {
            let mut guard = self.inner.lock().await;
            let sess = guard.as_mut().ok_or_else(closed)?;
            if self.tx.needs_begin(ctl) {
                simple(&mut sess.client, "BEGIN TRANSACTION").await?;
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
            simple(&mut sess.client, "IF @@TRANCOUNT > 0 COMMIT TRANSACTION").await?;
            self.tx.ended();
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        if self.tx.is_active() {
            let r = simple(&mut sess.client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
            self.tx.ended();
            r?;
        }
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let sql = if self.cfg.show_system_objects {
            "SELECT name FROM sys.databases ORDER BY name"
        } else {
            "SELECT name FROM sys.databases WHERE database_id > 4 OR name = DB_NAME() ORDER BY name"
        };
        Ok(self.query(sql, &[]).await?.iter().map(|r| c_string(r, 0)).collect())
    }

    fn current_database(&self) -> Option<String> {
        self.database.read().ok().and_then(|g| g.clone())
    }

    async fn use_database(&self, database: &str) -> Result<()> {
        {
            let mut guard = self.inner.lock().await;
            let sess = guard.as_mut().ok_or_else(closed)?;
            simple(&mut sess.client, &format!("USE {}", D.quote_ident(database))).await?;
        }
        self.refresh_database().await;
        Ok(())
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let sql = if self.cfg.show_system_objects {
            "SELECT name FROM sys.schemas ORDER BY name"
        } else {
            "SELECT name FROM sys.schemas WHERE schema_id < 16384 AND name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest') ORDER BY name"
        };
        let mut out: Vec<String> = self.query(sql, &[]).await?.iter().map(|r| c_string(r, 0)).collect();
        if !self.cfg.schema_filter.is_empty() {
            out.retain(|s| self.cfg.schema_filter.iter().any(|f| f == s));
        }
        Ok(out)
    }

    async fn current_schema(&self) -> Result<Option<String>> {
        Ok(Some(self.schema_or_default(None).await?))
    }

    async fn list_objects(&self, schema: Option<&str>, kinds: &[ObjectKind]) -> Result<Vec<ObjectInfo>> {
        let schema = self.schema_or_default(schema).await?;
        let mut types = vec![];
        for k in kinds {
            match k {
                ObjectKind::Table => types.push("'U'"),
                ObjectKind::View => types.push("'V'"),
                ObjectKind::Procedure => types.extend(["'P'", "'PC'"]),
                ObjectKind::Function => types.extend(["'FN'", "'IF'", "'TF'", "'FS'", "'FT'"]),
                ObjectKind::Trigger => types.push("'TR'"),
                ObjectKind::Sequence => types.push("'SO'"),
                ObjectKind::Synonym => types.push("'SN'"),
                _ => {}
            }
        }
        let mut out = vec![];
        if !types.is_empty() {
            let sql = format!(
                "SELECT o.name, RTRIM(o.type), CAST(ep.value AS nvarchar(4000)), \
                        (SELECT CAST(SUM(p.rows) AS bigint) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)), \
                        par.name, CAST(o.object_id AS varchar(20)) \
                 FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
                 LEFT JOIN sys.objects par ON par.object_id = o.parent_object_id \
                 LEFT JOIN sys.extended_properties ep ON ep.major_id = o.object_id AND ep.minor_id = 0 \
                  AND ep.class = 1 AND ep.name = 'MS_Description' \
                 WHERE s.name = @P1 AND o.is_ms_shipped = 0 AND o.type IN ({}) ORDER BY o.name",
                types.join(",")
            );
            let want_sig = kinds.contains(&ObjectKind::Function) || kinds.contains(&ObjectKind::Procedure);
            for r in self.query(&sql, &[&schema]).await? {
                let kind = match c_string(&r, 1).as_str() {
                    "U" => ObjectKind::Table,
                    "V" => ObjectKind::View,
                    "P" | "PC" => ObjectKind::Procedure,
                    "TR" => ObjectKind::Trigger,
                    "SO" => ObjectKind::Sequence,
                    "SN" => ObjectKind::Synonym,
                    _ => ObjectKind::Function,
                };
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), kind));
                info.comment = c_nonempty(&r, 2);
                if kind == ObjectKind::Table {
                    info.row_estimate = c_i64(&r, 3);
                }
                if kind == ObjectKind::Trigger {
                    info.parent = c_str(&r, 4);
                }
                if want_sig && matches!(kind, ObjectKind::Function | ObjectKind::Procedure) {
                    info.signature = self.routine_signature(&c_string(&r, 5)).await.ok().map(|(s, _)| s);
                }
                out.push(info);
            }
        }
        if kinds.contains(&ObjectKind::Index) {
            let rows = self
                .query(
                    "SELECT i.name, t.name FROM sys.indexes i JOIN sys.tables t ON t.object_id = i.object_id \
                     JOIN sys.schemas s ON s.schema_id = t.schema_id \
                     WHERE s.name = @P1 AND i.name IS NOT NULL AND t.is_ms_shipped = 0 ORDER BY t.name, i.name",
                    &[&schema],
                )
                .await?;
            for r in rows {
                let mut info = ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), ObjectKind::Index));
                info.parent = c_str(&r, 1);
                out.push(info);
            }
        }
        if kinds.contains(&ObjectKind::Type) {
            let rows = self
                .query(
                    "SELECT t.name FROM sys.types t JOIN sys.schemas s ON s.schema_id = t.schema_id \
                     WHERE s.name = @P1 AND t.is_user_defined = 1 ORDER BY t.name",
                    &[&schema],
                )
                .await?;
            for r in rows {
                out.push(ObjectInfo::new(ObjectRef::new(Some(&schema), c_string(&r, 0), ObjectKind::Type)));
            }
        }
        if kinds.contains(&ObjectKind::Schema) {
            for s in self.list_schemas().await? {
                out.push(ObjectInfo::new(ObjectRef::new(None, s, ObjectKind::Schema)));
            }
        }
        Ok(out)
    }

    async fn describe_table(&self, object: &ObjectRef) -> Result<TableInfo> {
        Ok(self.describe(object).await?.0)
    }

    async fn describe_routine(&self, object: &ObjectRef, _signature: Option<&str>) -> Result<RoutineInfo> {
        let schema = self.schema_or_default(object.schema.as_deref()).await?;
        let (oid, otype) = self.object_id(&schema, &object.name).await?;
        let (signature, return_type) = self.routine_signature(&oid).await?;
        let is_proc = matches!(otype.as_str(), "P" | "PC");
        Ok(RoutineInfo {
            object: ObjectRef::new(
                Some(&schema),
                object.name.clone(),
                if is_proc { ObjectKind::Procedure } else { ObjectKind::Function },
            ),
            signature: Some(signature),
            return_type: return_type.or_else(|| (otype == "IF" || otype == "TF").then(|| "TABLE".into())),
            language: Some(if otype.ends_with('S') || otype == "PC" { "CLR".into() } else { "T-SQL".into() }),
            definition: self.definition(&oid).await?.unwrap_or_default(),
        })
    }

    async fn object_ddl(&self, object: &ObjectRef) -> Result<String> {
        match object.kind {
            ObjectKind::Table => {
                let (info, _, extra) = self.describe(object).await?;
                Ok(table_ddl(&info, &extra))
            }
            ObjectKind::View | ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Trigger => {
                let schema = self.schema_or_default(object.schema.as_deref()).await?;
                let (oid, _) = self.object_id(&schema, &object.name).await?;
                let def = self
                    .definition(&oid)
                    .await?
                    .ok_or_else(|| Error::Other(format!("definition of {} is not available (encrypted?)", object.display_name())))?;
                Ok(format!("{}\n", def.trim()))
            }
            ObjectKind::Index => {
                let schema = self.schema_or_default(object.schema.as_deref()).await?;
                let rows = self
                    .query(
                        "SELECT t.name FROM sys.indexes i JOIN sys.tables t ON t.object_id = i.object_id \
                         JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE s.name = @P1 AND i.name = @P2",
                        &[&schema, &object.name],
                    )
                    .await?;
                let table = rows.first().map(|r| c_string(r, 0)).ok_or_else(|| not_found("index", &object.display_name()))?;
                let (info, _, _) = self.describe(&ObjectRef::table(Some(&schema), table)).await?;
                let i = info.indexes.iter().find(|i| i.name == object.name).ok_or_else(|| not_found("index", &object.name))?;
                Ok(match &i.definition {
                    Some(d) => format!("{d};\n"),
                    None => format!("-- index {} implements a constraint of {}\n", i.name, info.object.display_name()),
                })
            }
            ObjectKind::Sequence => {
                let schema = self.schema_or_default(object.schema.as_deref()).await?;
                let rows = self
                    .query(
                        "SELECT TYPE_NAME(q.user_type_id), CAST(q.start_value AS nvarchar(40)), CAST(q.increment AS nvarchar(40)), \
                                CAST(q.minimum_value AS nvarchar(40)), CAST(q.maximum_value AS nvarchar(40)), q.is_cycling \
                         FROM sys.sequences q JOIN sys.schemas s ON s.schema_id = q.schema_id WHERE s.name = @P1 AND q.name = @P2",
                        &[&schema, &object.name],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found("sequence", &object.display_name()))?;
                Ok(format!(
                    "CREATE SEQUENCE {} AS {} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {};\n",
                    D.qualified(Some(&schema), &object.name),
                    c_string(r, 0),
                    c_string(r, 1),
                    c_string(r, 2),
                    c_string(r, 3),
                    c_string(r, 4),
                    if c_bool(r, 5) { "CYCLE" } else { "NO CYCLE" }
                ))
            }
            ObjectKind::Synonym => {
                let schema = self.schema_or_default(object.schema.as_deref()).await?;
                let rows = self
                    .query(
                        "SELECT sn.base_object_name FROM sys.synonyms sn JOIN sys.schemas s ON s.schema_id = sn.schema_id \
                         WHERE s.name = @P1 AND sn.name = @P2",
                        &[&schema, &object.name],
                    )
                    .await?;
                let r = rows.first().ok_or_else(|| not_found("synonym", &object.display_name()))?;
                Ok(format!("CREATE SYNONYM {} FOR {};\n", D.qualified(Some(&schema), &object.name), c_string(r, 0)))
            }
            ObjectKind::Schema => Ok(format!("CREATE SCHEMA {};\n", D.quote_ident(&object.name))),
            ObjectKind::Database => Ok(format!("CREATE DATABASE {};\n", D.quote_ident(&object.name))),
            other => Err(Error::Unsupported(format!("DDL for {} objects", other.label()))),
        }
    }

    async fn list_roles(&self) -> Result<Vec<RoleInfo>> {
        let rows = self
            .query(
                "SELECT name, IS_SRVROLEMEMBER('sysadmin', name), type, is_disabled FROM sys.server_principals \
                 WHERE type IN ('S', 'U', 'G', 'R', 'E', 'X') AND name NOT LIKE '##%' ORDER BY name",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| RoleInfo {
                name: c_string(r, 0),
                superuser: c_i64(r, 1) == Some(1),
                can_login: c_string(r, 2) != "R" && !c_bool(r, 3),
                member_of: vec![],
            })
            .collect())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let rows = self
            .query(
                "SELECT CAST(s.session_id AS varchar(10)), s.login_name, DB_NAME(s.database_id), \
                        CONCAT(s.host_name, ' ', s.program_name), COALESCE(r.status, s.status), t.text, \
                        CONVERT(varchar(30), s.login_time, 126) \
                 FROM sys.dm_exec_sessions s LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id \
                 OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t WHERE s.is_user_process = 1 ORDER BY s.session_id",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| SessionInfo {
                id: c_string(r, 0),
                user: c_str(r, 1),
                database: c_str(r, 2),
                client: c_nonempty(r, 3).map(|s| s.trim().to_string()),
                state: c_str(r, 4),
                query: c_str(r, 5),
                started: c_str(r, 6),
            })
            .collect())
    }

    async fn kill_session(&self, id: &str) -> Result<()> {
        let n: u32 = id.trim().parse().map_err(|_| Error::Other(format!("invalid session id: {id}")))?;
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        simple(&mut sess.client, &format!("KILL {n}")).await
    }

    async fn explain(&self, sql: &str, analyze: bool) -> Result<StatementResult> {
        let (on, off) = if analyze {
            ("SET STATISTICS PROFILE ON", "SET STATISTICS PROFILE OFF")
        } else {
            ("SET SHOWPLAN_TEXT ON", "SET SHOWPLAN_TEXT OFF")
        };
        let mut guard = self.inner.lock().await;
        let sess = guard.as_mut().ok_or_else(closed)?;
        set_rowcount(sess, None).await?;
        simple(&mut sess.client, on).await?;
        let res = run(sess, trim_statement(sql), &ExecOptions::unlimited()).await;
        let off_res = simple(&mut sess.client, off).await;
        let r = res?;
        off_res?;
        Ok(r)
    }

    async fn close(&self) -> Result<()> {
        let sess = self.inner.lock().await.take();
        if let Some(s) = sess {
            s.client.close().await.map_err(map_tds_err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_formatting() {
        assert_eq!(format_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(format_type("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(format_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(format_type("datetime2", 8, 27, 7), "datetime2(7)");
        assert_eq!(format_type("int", 4, 10, 0), "int");
    }

    #[test]
    fn values() {
        assert_eq!(decode(&ColumnData::I32(Some(5))), Value::Int(5));
        assert_eq!(decode(&ColumnData::I32(None)), Value::Null);
        assert_eq!(decode(&ColumnData::String(Some("x".into()))), Value::Text("x".into()));
        assert_eq!(decode(&ColumnData::Bit(Some(true))), Value::Bool(true));
        assert_eq!(
            decode(&ColumnData::Numeric(Some(tiberius::numeric::Numeric::new_with_scale(12345, 2)))),
            Value::Decimal("123.45".into())
        );
    }

    #[test]
    fn dml_detection() {
        assert!(is_plain_dml("insert into t values (1)"));
        assert!(!is_plain_dml("insert into t output inserted.id values (1)"));
        assert!(!is_plain_dml("select 1"));
    }

    #[test]
    fn config_from_fields_and_urls() {
        let mut cfg = ConnectionConfig::new(DriverKind::MsSql);
        cfg.host = "sql.local".into();
        cfg.port = 1444;
        let (_, host, port) = base_config(&cfg).unwrap();
        assert_eq!((host.as_str(), port), ("sql.local", 1444));
        cfg.url = Some("mssql://sa:pw@other:1500/db".into());
        let (_, host, port) = base_config(&cfg).unwrap();
        assert_eq!((host.as_str(), port), ("other", 1500));
        cfg.url = Some("jdbc:sqlserver://jhost:1600;database=x;user=sa;password=p".into());
        let (_, host, port) = base_config(&cfg).unwrap();
        assert_eq!((host.as_str(), port), ("jhost", 1600));
    }

    #[test]
    fn ddl_generation() {
        let mut t = TableInfo::new(ObjectRef::table(Some("dbo"), "t"));
        t.columns.push(ColumnInfo { name: "id".into(), data_type: "int".into(), auto_increment: true, ..Default::default() });
        t.columns.push(ColumnInfo {
            name: "n".into(),
            data_type: "nvarchar(10)".into(),
            nullable: true,
            default: Some("(N'x')".into()),
            ..Default::default()
        });
        t.primary_key = Some(KeyInfo { name: Some("PK_t".into()), columns: vec!["id".into()] });
        let extra = vec![("id".into(), String::new(), "IDENTITY(1,1)".into()), ("n".into(), "DF_n".into(), String::new())];
        let ddl = table_ddl(&t, &extra);
        assert!(ddl.contains("[id] int IDENTITY(1,1) NOT NULL"), "{ddl}");
        assert!(ddl.contains("[n] nvarchar(10) NULL CONSTRAINT [DF_n] DEFAULT (N'x')"), "{ddl}");
        assert!(ddl.contains("CONSTRAINT [PK_t] PRIMARY KEY ([id])"), "{ddl}");
    }
}
