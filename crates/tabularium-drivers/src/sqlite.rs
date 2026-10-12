//! SQLite driver (sqlx, bundled SQLite).

use std::ptr::NonNull;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::TryStreamExt;
use sqlx::sqlite::{SqliteArguments, SqliteConnectOptions, SqliteRow, SqliteValueRef};
use sqlx::{Column, Connection as _, Decode, Either, Executor, Row, Sqlite, Statement, TypeInfo, ValueRef};
use tabularium_core::model::*;
use tabularium_core::{
    CancelHandle, ColumnMeta, Connection, ConnectionConfig, DriverKind, Error, ExecOptions, Result,
    StatementResult, TransactionMode, Value,
};
use tokio::sync::Mutex;

use crate::common::*;

type SqlxConn = sqlx::SqliteConnection;
type SqliteQuery<'q> = sqlx::query::Query<'q, Sqlite, SqliteArguments<'q>>;

/// Raw `sqlite3*` handle used only for `sqlite3_interrupt`, which SQLite documents as
/// safe to call from any thread while the connection is open.
struct RawHandle(NonNull<libsqlite3_sys::sqlite3>);
// SAFETY: only used with sqlite3_interrupt, which is thread-safe; the pointer is cleared
// (under the same mutex) before the connection is closed or dropped.
unsafe impl Send for RawHandle {}

type InterruptSlot = Arc<std::sync::Mutex<Option<RawHandle>>>;

pub struct SqliteConnection {
    cfg: ConnectionConfig,
    path: String,
    inner: Mutex<Option<SqlxConn>>,
    tx: TxState,
    interrupt: InterruptSlot,
}

struct SqliteCancel {
    slot: InterruptSlot,
}

#[async_trait]
impl CancelHandle for SqliteCancel {
    async fn cancel(&self) -> Result<()> {
        let guard = self.slot.lock().map_err(|_| Error::other("interrupt lock poisoned"))?;
        if let Some(h) = guard.as_ref() {
            // SAFETY: the handle is valid while it is stored in the slot (see RawHandle).
            unsafe { libsqlite3_sys::sqlite3_interrupt(h.0.as_ptr()) };
        }
        Ok(())
    }
}

/// Normalise the configured database path: strip `sqlite:` URL prefixes and expand `~`.
fn normalize_path(db: &str) -> String {
    let mut p = db.trim();
    for prefix in ["sqlite://", "sqlite:", "file://", "file:"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            p = rest;
            break;
        }
    }
    let p = p.split('?').next().unwrap_or(p);
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return std::path::PathBuf::from(home).join(rest).to_string_lossy().into_owned();
        }
    }
    p.to_string()
}

fn is_memory(path: &str) -> bool {
    path.is_empty() || path == ":memory:" || path == "memory"
}

impl SqliteConnection {
    pub async fn connect(cfg: &ConnectionConfig) -> Result<Self> {
        let path = normalize_path(&cfg.database);
        let mut opts = if is_memory(&path) {
            SqliteConnectOptions::from_str("sqlite::memory:").map_err(map_connect_err)?
        } else {
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(!cfg.read_only)
                .read_only(cfg.read_only)
        };
        opts = opts.foreign_keys(true).statement_cache_capacity(64);
        let mut conn =
            with_connect_timeout(cfg, async { SqlxConn::connect_with(&opts).await.map_err(map_connect_err) }).await?;
        // Wrap immediately: a bare NonNull is !Send and must not live across an .await.
        let raw = RawHandle(conn.lock_handle().await.map_err(map_connect_err)?.as_raw_handle());
        for stmt in init_statements(cfg) {
            raw_exec(&mut conn, &stmt).await?;
        }
        let mode = if cfg.auto_commit { TransactionMode::Auto } else { TransactionMode::Manual };
        Ok(Self {
            cfg: cfg.clone(),
            path: if is_memory(&path) { ":memory:".into() } else { path },
            inner: Mutex::new(Some(conn)),
            tx: TxState::new(mode),
            interrupt: Arc::new(std::sync::Mutex::new(Some(raw))),
        })
    }

    /// The database file path (`:memory:` for in-memory databases).
    pub fn path(&self) -> &str {
        &self.path
    }

    fn clear_interrupt(&self) {
        if let Ok(mut g) = self.interrupt.lock() {
            *g = None;
        }
    }

    /// Run a statement without transaction bookkeeping (introspection, BEGIN/COMMIT).
    async fn query_internal(&self, sql: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        let mut guard = self.inner.lock().await;
        let conn = guard.as_mut().ok_or_else(closed)?;
        let res = run(conn, sql, &ExecOptions { max_rows: None, params: params.to_vec() }).await?;
        Ok(res.rows)
    }

    fn schema_or_main<'a>(&self, schema: Option<&'a str>) -> &'a str {
        schema.filter(|s| !s.is_empty()).unwrap_or("main")
    }

    async fn master_rows(&self, schema: &str, where_: &str, params: &[Value]) -> Result<Vec<Vec<Value>>> {
        let sql = format!(
            "SELECT type, name, tbl_name, sql FROM {}.sqlite_master WHERE {where_} ORDER BY type, name",
            q(schema)
        );
        self.query_internal(&sql, params).await
    }

    async fn object_sql(&self, schema: &str, name: &str) -> Result<Option<(String, Option<String>)>> {
        let rows = self.master_rows(schema, "name = ?", &[Value::from(name)]).await?;
        Ok(rows.first().map(|r| (c_string(r, 0), c_str(r, 3))))
    }
}

/// Quote an identifier for SQLite.
fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn bind<'q>(query: SqliteQuery<'q>, v: &Value) -> SqliteQuery<'q> {
    match v {
        Value::Null => query.bind(None::<String>),
        Value::Bool(b) => query.bind(*b),
        Value::Int(i) => query.bind(*i),
        Value::UInt(u) => match i64::try_from(*u) {
            Ok(i) => query.bind(i),
            Err(_) => query.bind(u.to_string()),
        },
        Value::Float(f) => query.bind(*f),
        Value::Decimal(d) => match d.parse::<i64>() {
            Ok(i) => query.bind(i),
            Err(_) => match d.parse::<f64>() {
                Ok(f) => query.bind(f),
                Err(_) => query.bind(d.clone()),
            },
        },
        Value::Text(s) => query.bind(s.clone()),
        Value::Bytes(b) => query.bind(b.clone()),
        Value::Json(j) => query.bind(j.to_string()),
        Value::Uuid(u) => query.bind(u.to_string()),
        Value::Date(_) | Value::Time(_) | Value::DateTime(_) | Value::DateTimeTz(_) => query.bind(v.to_string()),
        Value::Array(_) | Value::Other { .. } => query.bind(v.to_string()),
    }
}

/// Decode a SQLite cell according to its runtime storage class.
fn decode_cell(row: &SqliteRow, i: usize) -> Value {
    let v: SqliteValueRef<'_> = match row.try_get_raw(i) {
        Ok(v) => v,
        Err(e) => return Value::Other { type_name: "?".into(), text: format!("<error: {e}>") },
    };
    if v.is_null() {
        return Value::Null;
    }
    let ty = v.type_info().name().to_ascii_uppercase();
    let r = match ty.as_str() {
        "INTEGER" | "BOOLEAN" | "INT8" | "INT" => <i64 as Decode<Sqlite>>::decode(v).map(Value::Int),
        "REAL" | "FLOAT" | "DOUBLE" => <f64 as Decode<Sqlite>>::decode(v).map(Value::Float),
        "BLOB" => <Vec<u8> as Decode<Sqlite>>::decode(v).map(Value::Bytes),
        _ => <String as Decode<Sqlite>>::decode(v).map(Value::Text),
    };
    match r {
        Ok(val) => val,
        Err(_) => match row.try_get_raw(i).map(<Vec<u8> as Decode<Sqlite>>::decode) {
            Ok(Ok(b)) => bytes_fallback(&ty, &b),
            _ => Value::Other { type_name: ty, text: "<undecodable>".into() },
        },
    }
}

fn decode_row(row: &SqliteRow) -> Vec<Value> {
    (0..row.len()).map(|i| decode_cell(row, i)).collect()
}

/// Run raw SQL (no result) on the connection.
fn raw_exec<'a>(conn: &'a mut SqlxConn, sql: &'a str) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move { conn.execute(sqlx::raw_sql(sql)).await.map(|_| ()).map_err(map_sqlx_err) })
}

/// Execute one statement and collect up to `max_rows` rows.
fn run<'a>(conn: &'a mut SqlxConn, sql: &'a str, opts: &'a ExecOptions) -> BoxFuture<'a, Result<StatementResult>> {
    Box::pin(run_inner(conn, sql, opts))
}

async fn run_inner(conn: &mut SqlxConn, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
    let start = Instant::now();
    let stmt = conn.prepare(sql).await.map_err(map_sqlx_err)?;
    let mut columns: Vec<ColumnMeta> = stmt
        .columns()
        .iter()
        .map(|c| ColumnMeta::new(c.name(), c.type_info().name()))
        .collect();
    let mut query = stmt.query();
    for p in &opts.params {
        query = bind(query, p);
    }
    let mut rows = Vec::new();
    let mut affected = 0u64;
    let mut truncated = false;
    {
        let mut stream = conn.fetch_many(query);
        while let Some(item) = stream.try_next().await.map_err(map_sqlx_err)? {
            match item {
                Either::Left(r) => affected += r.rows_affected(),
                Either::Right(row) => {
                    if columns.is_empty() {
                        columns = row
                            .columns()
                            .iter()
                            .map(|c| ColumnMeta::new(c.name(), c.type_info().name()))
                            .collect();
                    }
                    if opts.max_rows.is_some_and(|m| rows.len() >= m) {
                        truncated = true;
                        break;
                    }
                    rows.push(decode_row(&row));
                }
            }
        }
    }
    // Expression columns have no declared type; report the runtime type of the first value.
    for (i, c) in columns.iter_mut().enumerate() {
        if c.type_name.eq_ignore_ascii_case("NULL") {
            if let Some(v) = rows.iter().map(|r| cell(r, i)).find(|v| !v.is_null()) {
                c.type_name = match v {
                    Value::Int(_) => "INTEGER",
                    Value::Float(_) => "REAL",
                    Value::Bytes(_) => "BLOB",
                    _ => "TEXT",
                }
                .into();
            }
        }
    }
    let has_rows = !columns.is_empty();
    Ok(StatementResult {
        sql: sql.to_string(),
        columns,
        rows,
        rows_affected: if has_rows { None } else { Some(affected) },
        truncated,
        elapsed: start.elapsed(),
        notices: vec![],
    })
}

#[async_trait]
impl Connection for SqliteConnection {
    fn driver(&self) -> DriverKind {
        DriverKind::Sqlite
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.query_internal("SELECT sqlite_version()", &[]).await?;
        Ok(format!("SQLite {}", rows.first().map(|r| c_string(r, 0)).unwrap_or_default()))
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<StatementResult> {
        check_read_only(self.cfg.read_only, sql)?;
        let ctl = tx_control(sql, false);
        let mut guard = self.inner.lock().await;
        let conn = guard.as_mut().ok_or_else(closed)?;
        if self.tx.needs_begin(ctl) {
            raw_exec(conn, "BEGIN").await?;
            self.tx.began();
        }
        let res = run(conn, sql, opts).await;
        self.tx.after_statement(ctl, res.is_ok());
        res
    }

    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        Some(Arc::new(SqliteCancel { slot: self.interrupt.clone() }))
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
        let conn = guard.as_mut().ok_or_else(closed)?;
        if self.tx.is_active() {
            raw_exec(conn, "COMMIT").await?;
            self.tx.ended();
        }
        Ok(())
    }

    async fn rollback(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let conn = guard.as_mut().ok_or_else(closed)?;
        if self.tx.is_active() {
            let r = raw_exec(conn, "ROLLBACK").await;
            self.tx.ended();
            if let Err(e) = r {
                // "no transaction is active" after an automatic rollback is fine.
                log::debug!("rollback: {e}");
            }
        }
        Ok(())
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = self.query_internal("PRAGMA database_list", &[]).await?;
        Ok(rows.iter().map(|r| c_string(r, 1)).collect())
    }

    fn current_database(&self) -> Option<String> {
        Some("main".into())
    }

    async fn use_database(&self, database: &str) -> Result<()> {
        if database == "main" {
            Ok(())
        } else {
            Err(Error::Unsupported(
                "SQLite has a single main database; ATTACH other files and use them as schemas".into(),
            ))
        }
    }

    async fn list_schemas(&self) -> Result<Vec<String>> {
        let mut dbs = self.list_databases().await?;
        if !self.cfg.show_system_objects {
            dbs.retain(|d| d != "temp");
        }
        if !self.cfg.schema_filter.is_empty() {
            dbs.retain(|d| self.cfg.schema_filter.iter().any(|f| f == d));
        }
        Ok(dbs)
    }

    async fn current_schema(&self) -> Result<Option<String>> {
        Ok(Some("main".into()))
    }

    async fn list_objects(&self, schema: Option<&str>, kinds: &[ObjectKind]) -> Result<Vec<ObjectInfo>> {
        let schema = self.schema_or_main(schema);
        let mut types = vec![];
        for k in kinds {
            match k {
                ObjectKind::Table => types.push("'table'"),
                ObjectKind::View => types.push("'view'"),
                ObjectKind::Index => types.push("'index'"),
                ObjectKind::Trigger => types.push("'trigger'"),
                _ => {}
            }
        }
        if types.is_empty() {
            return Ok(vec![]);
        }
        let rows = self.master_rows(schema, &format!("type IN ({})", types.join(",")), &[]).await?;
        let mut out = vec![];
        for r in rows {
            let name = c_string(&r, 1);
            if !self.cfg.show_system_objects && name.starts_with("sqlite_") {
                continue;
            }
            let kind = match c_string(&r, 0).as_str() {
                "table" => ObjectKind::Table,
                "view" => ObjectKind::View,
                "index" => ObjectKind::Index,
                "trigger" => ObjectKind::Trigger,
                _ => continue,
            };
            let mut info = ObjectInfo::new(ObjectRef::new(Some(schema), name, kind));
            if matches!(kind, ObjectKind::Index | ObjectKind::Trigger) {
                info.parent = c_nonempty(&r, 2);
            }
            out.push(info);
        }
        Ok(out)
    }

    async fn describe_table(&self, object: &ObjectRef) -> Result<TableInfo> {
        let schema = self.schema_or_main(object.schema.as_deref()).to_string();
        let name = object.name.as_str();
        let (kind_s, create_sql) =
            self.object_sql(&schema, name).await?.ok_or_else(|| not_found("table", &object.display_name()))?;
        let kind = if kind_s == "view" { ObjectKind::View } else { ObjectKind::Table };
        let mut info = TableInfo::new(ObjectRef::new(Some(&schema), name, kind));
        let create_sql = create_sql.unwrap_or_default();
        let col_defs = column_definitions(&create_sql);
        let has_autoincrement = contains_word(&create_sql, "AUTOINCREMENT");

        let cols = self
            .query_internal(&format!("PRAGMA {}.table_xinfo({})", q(&schema), q(name)), &[])
            .await?;
        let mut pk_cols: Vec<(i64, String)> = vec![];
        for r in &cols {
            let hidden = c_i64(r, 6).unwrap_or(0);
            if hidden == 1 {
                continue; // hidden column of a virtual table
            }
            let col_name = c_string(r, 1);
            let data_type = c_string(r, 2);
            let pk = c_i64(r, 5).unwrap_or(0);
            if pk > 0 {
                pk_cols.push((pk, col_name.clone()));
            }
            let def = col_defs.iter().find(|(n, _)| n.eq_ignore_ascii_case(&col_name)).map(|(_, d)| d.as_str());
            let generated = if hidden == 2 || hidden == 3 {
                Some(def.and_then(generated_expr).unwrap_or_else(|| "?".into()))
            } else {
                None
            };
            info.columns.push(ColumnInfo {
                name: col_name,
                data_type,
                nullable: c_i64(r, 3).unwrap_or(0) == 0 && pk == 0,
                default: c_str(r, 4),
                auto_increment: false,
                generated,
                comment: None,
                collation: def.and_then(collation_of),
                ordinal: (c_i64(r, 0).unwrap_or(0) + 1) as u32,
            });
        }
        // Renumber ordinals after skipping hidden columns.
        for (i, c) in info.columns.iter_mut().enumerate() {
            c.ordinal = i as u32 + 1;
        }
        pk_cols.sort();
        if !pk_cols.is_empty() {
            if pk_cols.len() == 1 {
                let pk_name = &pk_cols[0].1;
                if let Some(c) = info.columns.iter_mut().find(|c| &c.name == pk_name) {
                    // INTEGER PRIMARY KEY is an alias of the rowid and is auto-assigned;
                    // AUTOINCREMENT additionally prevents rowid reuse.
                    if c.data_type.eq_ignore_ascii_case("INTEGER") || has_autoincrement {
                        c.auto_increment = true;
                    }
                }
            }
            info.primary_key = Some(KeyInfo {
                name: constraint_name_for(&create_sql, "PRIMARY"),
                columns: pk_cols.into_iter().map(|(_, n)| n).collect(),
            });
        }

        if kind == ObjectKind::Table {
            // Indexes
            let idx_rows = self
                .query_internal(&format!("PRAGMA {}.index_list({})", q(&schema), q(name)), &[])
                .await?;
            let idx_sql: Vec<Vec<Value>> = self
                .master_rows(&schema, "type = 'index' AND tbl_name = ?", &[Value::from(name)])
                .await?;
            for r in &idx_rows {
                let idx_name = c_string(r, 1);
                let unique = c_bool(r, 2);
                let origin = c_string(r, 3);
                let cols = self
                    .query_internal(&format!("PRAGMA {}.index_xinfo({})", q(&schema), q(&idx_name)), &[])
                    .await?;
                let columns: Vec<String> = cols
                    .iter()
                    .filter(|c| c_bool(c, 5)) // key columns only
                    .map(|c| c_str(c, 2).unwrap_or_else(|| "<expression>".into()))
                    .collect();
                let definition = idx_sql.iter().find(|s| c_string(s, 1) == idx_name).and_then(|s| c_str(s, 3));
                let predicate = definition.as_deref().and_then(where_clause);
                if origin == "u" {
                    info.unique_keys.push(KeyInfo {
                        name: if idx_name.starts_with("sqlite_autoindex_") { None } else { Some(idx_name.clone()) },
                        columns: columns.clone(),
                    });
                }
                info.indexes.push(IndexInfo {
                    name: idx_name,
                    columns,
                    unique,
                    primary: origin == "pk",
                    method: Some("btree".into()),
                    predicate,
                    definition,
                });
            }
            info.indexes.reverse(); // index_list returns most recent first

            // Foreign keys
            let fk_rows = self
                .query_internal(&format!("PRAGMA {}.foreign_key_list({})", q(&schema), q(name)), &[])
                .await?;
            let mut fks: Vec<(i64, ForeignKeyInfo)> = vec![];
            for r in &fk_rows {
                let id = c_i64(r, 0).unwrap_or(0);
                let pos = match fks.iter().position(|(i, _)| *i == id) {
                    Some(p) => p,
                    None => {
                        fks.push((
                            id,
                            ForeignKeyInfo {
                                name: None,
                                columns: vec![],
                                ref_schema: Some(schema.clone()),
                                ref_table: c_string(r, 2),
                                ref_columns: vec![],
                                on_update: FkAction::parse(&c_string(r, 5)),
                                on_delete: FkAction::parse(&c_string(r, 6)),
                            },
                        ));
                        fks.len() - 1
                    }
                };
                fks[pos].1.columns.push(c_string(r, 3));
                if let Some(to) = c_str(r, 4) {
                    fks[pos].1.ref_columns.push(to);
                }
            }
            fks.sort_by_key(|(id, _)| -*id);
            for (_, mut fk) in fks {
                if fk.ref_columns.is_empty() {
                    // REFERENCES t without a column list targets t's primary key.
                    let pk = self
                        .query_internal(&format!("PRAGMA {}.table_info({})", q(&schema), q(&fk.ref_table)), &[])
                        .await
                        .unwrap_or_default();
                    let mut pkc: Vec<(i64, String)> = pk
                        .iter()
                        .filter(|r| c_i64(r, 5).unwrap_or(0) > 0)
                        .map(|r| (c_i64(r, 5).unwrap_or(0), c_string(r, 1)))
                        .collect();
                    pkc.sort();
                    fk.ref_columns = pkc.into_iter().map(|(_, n)| n).collect();
                }
                info.foreign_keys.push(fk);
            }
            info.checks = check_constraints(&create_sql);
        } else {
            info.view_definition = view_body(&create_sql);
        }

        // Triggers
        let trg = self.master_rows(&schema, "type = 'trigger' AND tbl_name = ?", &[Value::from(name)]).await?;
        for r in trg {
            let def = c_string(&r, 3);
            let (timing, events) = trigger_timing_events(&def);
            info.triggers.push(TriggerInfo { name: c_string(&r, 1), timing, events, definition: Some(def) });
        }
        Ok(info)
    }

    async fn describe_routine(&self, _object: &ObjectRef, _signature: Option<&str>) -> Result<RoutineInfo> {
        Err(Error::Unsupported("SQLite has no stored routines".into()))
    }

    async fn object_ddl(&self, object: &ObjectRef) -> Result<String> {
        let schema = self.schema_or_main(object.schema.as_deref()).to_string();
        let (kind, sql) = self
            .object_sql(&schema, &object.name)
            .await?
            .ok_or_else(|| not_found(object.kind.label(), &object.display_name()))?;
        let mut out = match sql {
            Some(s) => format!("{};\n", s.trim_end_matches(';')),
            None => format!("-- {} {} is created implicitly by a constraint\n", kind, object.name),
        };
        if kind == "table" {
            let extra = self
                .master_rows(
                    &schema,
                    "type IN ('index','trigger') AND tbl_name = ? AND sql IS NOT NULL",
                    &[Value::from(object.name.as_str())],
                )
                .await?;
            // Indexes first, then triggers.
            for wanted in ["index", "trigger"] {
                for r in extra.iter().filter(|r| c_string(r, 0) == wanted) {
                    out.push('\n');
                    out.push_str(c_string(r, 3).trim_end_matches(';'));
                    out.push_str(";\n");
                }
            }
        }
        Ok(out)
    }

    async fn close(&self) -> Result<()> {
        self.clear_interrupt();
        let conn = self.inner.lock().await.take();
        if let Some(c) = conn {
            c.close().await.map_err(map_sqlx_err)?;
        }
        Ok(())
    }
}

impl Drop for SqliteConnection {
    fn drop(&mut self) {
        // Invalidate the interrupt handle before the sqlx connection is dropped.
        self.clear_interrupt();
    }
}

// ----- CREATE statement parsing helpers ------------------------------------------------------

/// Split the body of `CREATE TABLE name (...)` into top-level comma-separated items.
fn table_body_items(create_sql: &str) -> Vec<String> {
    let Some(open) = create_sql.find('(') else { return vec![] };
    let mut items = vec![];
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in create_sql[open + 1..].chars() {
        if let Some(qc) = quote {
            cur.push(c);
            if c == qc {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' | '[' => {
                quote = Some(if c == '[' { ']' } else { c });
                cur.push(c);
            }
            '(' => {
                depth += 1;
                cur.push(c);
            }
            ')' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
                cur.push(c);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        items.push(cur.trim().to_string());
    }
    items
}

/// Unquote an SQLite identifier token.
fn unquote(tok: &str) -> String {
    let t = tok.trim();
    if t.len() >= 2 {
        let (f, l) = (t.as_bytes()[0], t.as_bytes()[t.len() - 1]);
        if (f == b'"' && l == b'"') || (f == b'`' && l == b'`') || (f == b'[' && l == b']') {
            return t[1..t.len() - 1].replace("\"\"", "\"");
        }
    }
    t.to_string()
}

/// First token of a column definition (possibly quoted).
fn first_token(def: &str) -> &str {
    let d = def.trim_start();
    let close = match d.chars().next() {
        Some('"') => Some('"'),
        Some('`') => Some('`'),
        Some('[') => Some(']'),
        _ => None,
    };
    match close {
        Some(c) => match d[1..].find(c) {
            Some(end) => &d[..end + 2],
            None => d,
        },
        None => d.split(|ch: char| ch.is_whitespace() || ch == '(').next().unwrap_or(""),
    }
}

const TABLE_CONSTRAINT_WORDS: [&str; 5] = ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

/// `(column name, definition)` for each column definition of a CREATE TABLE.
fn column_definitions(create_sql: &str) -> Vec<(String, String)> {
    table_body_items(create_sql)
        .into_iter()
        .filter(|item| {
            let tok = first_token(item);
            !TABLE_CONSTRAINT_WORDS.iter().any(|w| tok.eq_ignore_ascii_case(w))
        })
        .map(|item| (unquote(first_token(&item)), item))
        .collect()
}

/// Find a word (case-insensitive, outside literals) and return the balanced
/// parenthesised text following it.
fn paren_after(text: &str, word: &str) -> Option<String> {
    let stripped = strip_literals_and_comments(text);
    if stripped.len() != text.len() {
        // Literal stripping changed offsets; fall back to the raw text.
        return paren_after_raw(text, text, word);
    }
    paren_after_raw(text, &stripped, word)
}

fn paren_after_raw(text: &str, search: &str, word: &str) -> Option<String> {
    let upper = search.to_ascii_uppercase();
    let w = word.to_ascii_uppercase();
    let mut from = 0;
    while let Some(pos) = upper[from..].find(&w) {
        let at = from + pos;
        let before_ok = at == 0 || !upper.as_bytes()[at - 1].is_ascii_alphanumeric();
        let end = at + w.len();
        let after_ok = end >= upper.len() || !upper.as_bytes()[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            let rest = &text[end..];
            let open = rest.find('(')?;
            if !rest[..open].trim().is_empty() {
                from = end;
                continue;
            }
            return balanced(&rest[open..]);
        }
        from = end;
    }
    None
}

/// Text inside the leading balanced parentheses of `s` (which starts with `(`).
fn balanced(s: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        if let Some(qc) = quote {
            if c == qc {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[1..i].trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn generated_expr(def: &str) -> Option<String> {
    paren_after(def, "AS")
}

fn collation_of(def: &str) -> Option<String> {
    let words: Vec<&str> = def.split_whitespace().collect();
    words
        .iter()
        .position(|w| w.eq_ignore_ascii_case("COLLATE"))
        .and_then(|i| words.get(i + 1))
        .map(|w| unquote(w.trim_end_matches(',')))
}

/// Name of a `CONSTRAINT name <kind>` table constraint.
fn constraint_name_for(create_sql: &str, kind_word: &str) -> Option<String> {
    for item in table_body_items(create_sql) {
        let words: Vec<&str> = item.split_whitespace().collect();
        if words.len() >= 3 && words[0].eq_ignore_ascii_case("CONSTRAINT") && words[2].eq_ignore_ascii_case(kind_word) {
            return Some(unquote(words[1]));
        }
    }
    None
}

/// CHECK constraints (column- and table-level) of a CREATE TABLE.
fn check_constraints(create_sql: &str) -> Vec<CheckInfo> {
    let mut out = vec![];
    for item in table_body_items(create_sql) {
        let mut rest = item.as_str();
        while let Some(expr) = paren_after(rest, "CHECK") {
            // constraint name: "CONSTRAINT x CHECK"
            let upper = rest.to_ascii_uppercase();
            let check_pos = find_word(&upper, "CHECK").unwrap_or(0);
            let before: Vec<&str> = rest[..check_pos].split_whitespace().collect();
            let name = if before.len() >= 2 && before[before.len() - 2].eq_ignore_ascii_case("CONSTRAINT") {
                Some(unquote(before[before.len() - 1]))
            } else {
                None
            };
            out.push(CheckInfo { name, expression: expr.clone() });
            // continue after this CHECK(...)
            match rest[check_pos..].find(&expr) {
                Some(p) => rest = &rest[check_pos + p + expr.len()..],
                None => break,
            }
        }
    }
    out
}

fn find_word(upper: &str, word: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(pos) = upper[from..].find(word) {
        let at = from + pos;
        let end = at + word.len();
        let before_ok = at == 0 || !upper.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after_ok = end >= upper.len() || !upper.as_bytes()[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return Some(at);
        }
        from = end;
    }
    None
}

/// `WHERE` predicate of a partial index definition.
fn where_clause(index_sql: &str) -> Option<String> {
    let stripped = strip_literals_and_comments(index_sql).to_ascii_uppercase();
    if stripped.len() != index_sql.len() {
        return None;
    }
    find_word(&stripped, "WHERE").map(|p| index_sql[p + 5..].trim().trim_end_matches(';').to_string())
}

/// SELECT body of a CREATE VIEW statement.
fn view_body(create_sql: &str) -> Option<String> {
    let stripped = strip_literals_and_comments(create_sql).to_ascii_uppercase();
    if stripped.len() != create_sql.len() {
        return Some(create_sql.to_string());
    }
    find_word(&stripped, "AS").map(|p| create_sql[p + 2..].trim().trim_end_matches(';').to_string())
}

/// Timing (BEFORE/AFTER/INSTEAD OF) and events of a CREATE TRIGGER statement.
fn trigger_timing_events(def: &str) -> (String, String) {
    let words: Vec<String> = strip_literals_and_comments(def)
        .split_whitespace()
        .map(|w| w.to_ascii_uppercase())
        .collect();
    let mut timing = "BEFORE".to_string(); // SQLite default
    let mut events = vec![];
    let mut i = 0;
    let on = words.iter().position(|w| w == "ON").unwrap_or(words.len());
    while i < on {
        match words[i].as_str() {
            "BEFORE" | "AFTER" => timing = words[i].clone(),
            "INSTEAD" => timing = "INSTEAD OF".into(),
            "INSERT" | "UPDATE" | "DELETE" => events.push(words[i].clone()),
            _ => {}
        }
        i += 1;
    }
    (timing, events.join(" OR "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SQL: &str = "CREATE TABLE \"order items\" (\n  id INTEGER PRIMARY KEY AUTOINCREMENT,\n  name TEXT COLLATE NOCASE NOT NULL CHECK (length(name) > 0),\n  qty INT DEFAULT 1,\n  total REAL GENERATED ALWAYS AS (qty * 2.5) STORED,\n  CONSTRAINT qty_pos CHECK (qty >= 0),\n  FOREIGN KEY (qty) REFERENCES other(id)\n)";

    #[test]
    fn parses_create_table() {
        let cols = column_definitions(SQL);
        assert_eq!(cols.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["id", "name", "qty", "total"]);
        assert_eq!(collation_of(&cols[1].1).as_deref(), Some("NOCASE"));
        assert_eq!(generated_expr(&cols[3].1).as_deref(), Some("qty * 2.5"));
        let checks = check_constraints(SQL);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].expression, "length(name) > 0");
        assert_eq!(checks[1].name.as_deref(), Some("qty_pos"));
        assert_eq!(checks[1].expression, "qty >= 0");
    }

    #[test]
    fn parses_misc() {
        assert_eq!(where_clause("CREATE INDEX i ON t(a) WHERE a > 0").as_deref(), Some("a > 0"));
        assert_eq!(view_body("CREATE VIEW v AS SELECT 1").as_deref(), Some("SELECT 1"));
        assert_eq!(
            trigger_timing_events("CREATE TRIGGER x AFTER UPDATE OF a ON t BEGIN SELECT 1; END"),
            ("AFTER".to_string(), "UPDATE".to_string())
        );
        assert_eq!(normalize_path("sqlite:///tmp/a.db"), "/tmp/a.db");
        assert_eq!(normalize_path("sqlite::memory:"), ":memory:");
        assert!(is_memory(""));
    }
}
