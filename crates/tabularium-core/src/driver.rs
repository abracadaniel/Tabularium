//! The driver API: one [`Connection`] is one server session.
//!
//! The app opens a dedicated `Connection` for introspection and one per query console,
//! so transactions in a console never interfere with the explorer. Implementations must
//! serialize access internally (methods take `&self`) so a connection can be shared via
//! `Arc<dyn Connection>` between the UI and a background runtime.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::DriverKind;
use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::model::{
    ObjectInfo, ObjectKind, ObjectRef, RoleInfo, RoutineInfo, SchemaSnapshot, SessionInfo, TableInfo,
};
use crate::result::StatementResult;
use crate::value::Value;

/// Options for executing a statement.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Stop fetching after this many rows and set `truncated`. `None` fetches everything.
    pub max_rows: Option<usize>,
    /// Positional parameters bound to `$1`/`?` placeholders.
    pub params: Vec<Value>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self { max_rows: Some(500), params: vec![] }
    }
}

impl ExecOptions {
    pub fn unlimited() -> Self {
        Self { max_rows: None, params: vec![] }
    }
    pub fn with_limit(max_rows: usize) -> Self {
        Self { max_rows: Some(max_rows), params: vec![] }
    }
}

/// Transaction mode of a session, mirroring DataGrip's Auto/Manual switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransactionMode {
    #[default]
    Auto,
    Manual,
}

/// Cancels the statement currently running on a connection, from another task/thread.
#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> Result<()>;
}

#[async_trait]
pub trait Connection: Send + Sync {
    fn driver(&self) -> DriverKind;

    fn dialect(&self) -> Dialect {
        self.driver().dialect()
    }

    /// Server product + version string, e.g. "PostgreSQL 16.2".
    async fn server_version(&self) -> Result<String>;

    /// Execute a single statement. Statements returning rows fill `columns`/`rows`.
    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<StatementResult>;

    /// Execute a script that may contain several statements; returns one result each.
    /// The default implementation relies on the caller having split the script; drivers
    /// may override to use a native multi-statement protocol.
    async fn execute_batch(&self, statements: &[String], opts: &ExecOptions) -> Result<Vec<StatementResult>> {
        let mut out = Vec::with_capacity(statements.len());
        for s in statements {
            out.push(self.execute(s, opts).await?);
        }
        Ok(out)
    }

    /// Handle that can cancel a running statement on this connection.
    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>> {
        None
    }

    /// Check the connection is alive (used before reusing an idle session).
    async fn ping(&self) -> Result<()> {
        self.execute("SELECT 1", &ExecOptions::with_limit(1)).await.map(|_| ())
    }

    // ----- transactions -------------------------------------------------------------------

    async fn set_transaction_mode(&self, mode: TransactionMode) -> Result<()>;
    fn transaction_mode(&self) -> TransactionMode;
    /// `true` while a manual-mode transaction has uncommitted work.
    fn in_transaction(&self) -> bool;
    async fn commit(&self) -> Result<()>;
    async fn rollback(&self) -> Result<()>;

    // ----- navigation ---------------------------------------------------------------------

    /// Databases/catalogs on the server (single entry for SQLite: "main" and attached dbs).
    async fn list_databases(&self) -> Result<Vec<String>>;
    fn current_database(&self) -> Option<String>;
    /// Switch the session's current database (MySQL `USE`, MSSQL `USE`); Postgres drivers
    /// reconnect. Returns `Unsupported` when impossible.
    async fn use_database(&self, database: &str) -> Result<()>;

    /// Schemas in the current database. Databases without schemas return an empty list
    /// (MySQL: databases act as schemas and are returned here too).
    async fn list_schemas(&self) -> Result<Vec<String>>;
    /// Default schema used for unqualified names (`public`, `dbo`, current db, `main`).
    async fn current_schema(&self) -> Result<Option<String>>;

    /// All objects of the given kinds in a schema (`None` = default/only schema).
    async fn list_objects(&self, schema: Option<&str>, kinds: &[ObjectKind]) -> Result<Vec<ObjectInfo>>;

    /// Columns, keys, indexes, FKs, checks and triggers of a table or view.
    async fn describe_table(&self, object: &ObjectRef) -> Result<TableInfo>;

    /// Function/procedure source.
    async fn describe_routine(&self, object: &ObjectRef, signature: Option<&str>) -> Result<RoutineInfo>;

    /// `CREATE ...` DDL for any object, as the server would produce it when possible.
    async fn object_ddl(&self, object: &ObjectRef) -> Result<String>;

    /// Describe every table/view in a schema (used by schema compare, ER diagrams and
    /// completion). The default implementation calls `describe_table` per object.
    async fn snapshot_schema(&self, schema: Option<&str>) -> Result<SchemaSnapshot> {
        let objects = self
            .list_objects(schema, &[ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView])
            .await?;
        let mut snap = SchemaSnapshot { schema: schema.map(str::to_string), ..Default::default() };
        for o in objects {
            snap.tables.push(self.describe_table(&o.object).await?);
        }
        if let Ok(seqs) = self.list_objects(schema, &[ObjectKind::Sequence]).await {
            snap.sequences = seqs;
        }
        if let Ok(routines) = self.list_objects(schema, &[ObjectKind::Function, ObjectKind::Procedure]).await {
            for r in routines {
                if let Ok(info) = self.describe_routine(&r.object, r.signature.as_deref()).await {
                    snap.routines.push(info);
                }
            }
        }
        Ok(snap)
    }

    // ----- administration -----------------------------------------------------------------

    async fn list_roles(&self) -> Result<Vec<RoleInfo>> {
        Err(Error::Unsupported("roles".into()))
    }

    /// Active server sessions (process list).
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        Err(Error::Unsupported("session list".into()))
    }

    /// Terminate a server session by id.
    async fn kill_session(&self, _id: &str) -> Result<()> {
        Err(Error::Unsupported("kill session".into()))
    }

    /// Execution plan as text. Default implementation uses the dialect's EXPLAIN prefix.
    async fn explain(&self, sql: &str, analyze: bool) -> Result<StatementResult> {
        let prefix = self
            .dialect()
            .explain_prefix(analyze)
            .ok_or_else(|| Error::Unsupported("EXPLAIN".into()))?;
        self.execute(&format!("{prefix}{}", sql.trim().trim_end_matches(';')), &ExecOptions::unlimited())
            .await
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

/// Heuristic: does this statement modify data or schema? Used to enforce read-only mode
/// and to warn about uncommitted changes. Leading comments are skipped.
pub fn is_modifying_statement(sql: &str) -> bool {
    let first = first_keyword(sql).to_ascii_uppercase();
    match first.as_str() {
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "VALUES" | "TABLE" | "PRAGMA" | "SET"
        | "USE" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "END" | "SAVEPOINT" | "RELEASE"
        | "FETCH" | "DECLARE" | "CLOSE" | "LISTEN" | "UNLISTEN" | "ANALYZE" | "HELP" => false,
        // WITH ... could be a data-modifying CTE.
        "WITH" => {
            let upper = sql.to_ascii_uppercase();
            ["INSERT ", "UPDATE ", "DELETE ", "MERGE "].iter().any(|k| upper.contains(k))
        }
        "" => false,
        _ => true,
    }
}

/// First keyword of a statement, skipping whitespace, `--` and `/* */` comments, and `(`.
pub fn first_keyword(sql: &str) -> &str {
    let mut s = sql;
    loop {
        s = s.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        } else if let Some(rest) = s.strip_prefix('#') {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("");
        } else {
            break;
        }
    }
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(s.len());
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifying() {
        assert!(!is_modifying_statement("  -- hi\n select 1"));
        assert!(!is_modifying_statement("/* x */ (SELECT 1)"));
        assert!(is_modifying_statement("update t set a=1"));
        assert!(is_modifying_statement("WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x"));
        assert!(!is_modifying_statement("with x as (select 1) select * from x"));
        assert!(is_modifying_statement("CREATE TABLE t(a int)"));
    }
}
