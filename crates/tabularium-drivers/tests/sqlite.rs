//! SQLite integration tests (always runnable: bundled SQLite, temp files).

#![cfg(feature = "sqlite")]

use std::sync::Arc;
use std::time::Duration;

use tabularium_core::{
    Connection, ConnectionConfig, Error, ExecOptions, FkAction, ObjectKind, ObjectRef, TransactionMode, Value,
};

async fn open(path: &str) -> Arc<dyn Connection> {
    tabularium_drivers::connect(&ConnectionConfig::sqlite(path)).await.expect("connect")
}

async fn exec(c: &Arc<dyn Connection>, sql: &str) -> tabularium_core::StatementResult {
    c.execute(sql, &ExecOptions::unlimited()).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE customers (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT NOT NULL UNIQUE, name TEXT COLLATE NOCASE, created DATETIME DEFAULT CURRENT_TIMESTAMP)",
    "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE, total REAL CHECK (total >= 0), note BLOB, CONSTRAINT note_len CHECK (length(note) < 100))",
    "CREATE INDEX orders_customer ON orders(customer_id)",
    "CREATE UNIQUE INDEX orders_big ON orders(total) WHERE total > 1000",
    "CREATE VIEW big_orders AS SELECT o.id, c.email FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.total > 100",
    "CREATE TRIGGER orders_ai AFTER INSERT ON orders BEGIN UPDATE customers SET name = name WHERE id = NEW.customer_id; END",
];

async fn setup() -> (tempfile::TempDir, Arc<dyn Connection>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sub").with_extension("db");
    let c = open(path.to_str().unwrap()).await;
    for s in SCHEMA {
        exec(&c, s).await;
    }
    (dir, c)
}

#[tokio::test]
async fn creates_file_and_reports_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.db");
    let v = tabularium_drivers::test_connection(&ConnectionConfig::sqlite(path.to_str().unwrap())).await.unwrap();
    assert!(v.starts_with("SQLite 3."), "{v}");
    assert!(path.exists());
}

#[tokio::test]
async fn in_memory_execute_types_and_params() {
    let c = open(":memory:").await;
    exec(&c, "CREATE TABLE t (i INTEGER, r REAL, s TEXT, b BLOB, n NUMERIC)").await;
    let r = exec(&c, "INSERT INTO t VALUES (1, 1.5, 'a', x'00ff', NULL), (2, 2.5, 'b', NULL, 3)").await;
    assert_eq!(r.rows_affected, Some(2));
    assert!(r.columns.is_empty());

    let r = c
        .execute(
            "SELECT i, r, s, b, n, i * 2 AS doubled FROM t WHERE s = ? OR i = ? ORDER BY i",
            &ExecOptions { max_rows: Some(10), params: vec![Value::from("a"), Value::Int(2)] },
        )
        .await
        .unwrap();
    assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["i", "r", "s", "b", "n", "doubled"]);
    assert_eq!(r.columns[0].type_name, "INTEGER");
    assert_eq!(r.columns[5].type_name, "INTEGER", "expression column typed from data");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0], vec![
        Value::Int(1),
        Value::Float(1.5),
        Value::Text("a".into()),
        Value::Bytes(vec![0, 255]),
        Value::Null,
        Value::Int(2)
    ]);
    assert_eq!(r.rows_affected, None);
    assert!(!r.truncated);

    // empty result still has columns
    let r = exec(&c, "SELECT i, s FROM t WHERE 1 = 0").await;
    assert_eq!(r.columns.len(), 2);
    assert!(r.rows.is_empty());
}

#[tokio::test]
async fn truncation() {
    let c = open(":memory:").await;
    exec(&c, "CREATE TABLE n (x INTEGER)").await;
    exec(&c, "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 1000) INSERT INTO n SELECT x FROM s").await;
    let r = c.execute("SELECT x FROM n ORDER BY x", &ExecOptions::with_limit(10)).await.unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.truncated);
    let r = c.execute("SELECT x FROM n LIMIT 10", &ExecOptions::with_limit(10)).await.unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(!r.truncated);
    // the connection is still usable after a truncated read
    let r = exec(&c, "SELECT count(*) FROM n").await;
    assert_eq!(r.rows[0][0], Value::Int(1000));
}

#[tokio::test]
async fn errors_are_database_errors() {
    let c = open(":memory:").await;
    let e = c.execute("SELEC 1", &ExecOptions::default()).await.unwrap_err();
    assert!(matches!(e, Error::Database(_)), "{e:?}");
    let e = c.execute("SELECT * FROM missing", &ExecOptions::default()).await.unwrap_err();
    match e {
        Error::Database(d) => assert!(d.message.contains("missing"), "{d}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn manual_transactions() {
    let (_dir, c) = setup().await;
    assert_eq!(c.transaction_mode(), TransactionMode::Auto);
    c.set_transaction_mode(TransactionMode::Manual).await.unwrap();
    assert!(!c.in_transaction());
    exec(&c, "INSERT INTO customers (email) VALUES ('a@x')").await;
    assert!(c.in_transaction());
    c.rollback().await.unwrap();
    assert!(!c.in_transaction());
    let r = exec(&c, "SELECT count(*) FROM customers").await;
    assert_eq!(r.rows[0][0], Value::Int(0));
    // SELECT starts a new transaction lazily; commit it
    c.commit().await.unwrap();
    exec(&c, "INSERT INTO customers (email) VALUES ('b@x')").await;
    c.commit().await.unwrap();
    assert!(!c.in_transaction());
    c.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    let r = exec(&c, "SELECT email FROM customers").await;
    assert_eq!(r.rows, vec![vec![Value::Text("b@x".into())]]);

    // user-issued BEGIN/ROLLBACK in auto mode
    exec(&c, "BEGIN").await;
    exec(&c, "DELETE FROM customers").await;
    assert!(c.in_transaction());
    exec(&c, "ROLLBACK").await;
    assert!(!c.in_transaction());
    let r = exec(&c, "SELECT count(*) FROM customers").await;
    assert_eq!(r.rows[0][0], Value::Int(1));
}

#[tokio::test]
async fn read_only_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.db");
    {
        let c = open(path.to_str().unwrap()).await;
        exec(&c, "CREATE TABLE t (a INT)").await;
        c.close().await.unwrap();
    }
    let mut cfg = ConnectionConfig::sqlite(path.to_str().unwrap());
    cfg.read_only = true;
    let c = tabularium_drivers::connect(&cfg).await.unwrap();
    exec(&c, "SELECT * FROM t").await;
    let e = c.execute("INSERT INTO t VALUES (1)", &ExecOptions::default()).await.unwrap_err();
    assert!(matches!(e, Error::ReadOnly(_)), "{e:?}");
}

#[tokio::test]
async fn init_sql_runs() {
    let mut cfg = ConnectionConfig::sqlite(":memory:");
    cfg.init_sql = Some("CREATE TABLE boot (x INT); INSERT INTO boot VALUES (42);".into());
    let c = tabularium_drivers::connect(&cfg).await.unwrap();
    let r = exec(&c, "SELECT x FROM boot").await;
    assert_eq!(r.rows[0][0], Value::Int(42));
}

#[tokio::test]
async fn introspection() {
    let (_dir, c) = setup().await;
    assert_eq!(c.list_databases().await.unwrap(), vec!["main"]);
    assert_eq!(c.list_schemas().await.unwrap(), vec!["main"]);
    assert_eq!(c.current_schema().await.unwrap().as_deref(), Some("main"));

    let objs = c
        .list_objects(None, &[ObjectKind::Table, ObjectKind::View, ObjectKind::Index, ObjectKind::Trigger])
        .await
        .unwrap();
    let names: Vec<(ObjectKind, &str)> = objs.iter().map(|o| (o.object.kind, o.object.name.as_str())).collect();
    assert!(names.contains(&(ObjectKind::Table, "customers")));
    assert!(names.contains(&(ObjectKind::Table, "orders")));
    assert!(names.contains(&(ObjectKind::View, "big_orders")));
    assert!(names.contains(&(ObjectKind::Index, "orders_customer")));
    assert!(names.contains(&(ObjectKind::Trigger, "orders_ai")));
    assert!(!names.iter().any(|(_, n)| n.starts_with("sqlite_")), "{names:?}");
    let idx = objs.iter().find(|o| o.object.name == "orders_customer").unwrap();
    assert_eq!(idx.parent.as_deref(), Some("orders"));

    let t = c.describe_table(&ObjectRef::table(Some("main"), "customers")).await.unwrap();
    assert_eq!(t.columns.len(), 4);
    let id = t.column("id").unwrap();
    assert!(id.auto_increment);
    assert!(!id.nullable);
    assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["id"]);
    assert!(!t.column("email").unwrap().nullable);
    assert!(t.column("name").unwrap().nullable);
    assert_eq!(t.column("name").unwrap().collation.as_deref(), Some("NOCASE"));
    assert_eq!(t.column("created").unwrap().default.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(t.unique_keys.len(), 1);
    assert_eq!(t.unique_keys[0].columns, vec!["email"]);

    let o = c.describe_table(&ObjectRef::table(None, "orders")).await.unwrap();
    assert_eq!(o.foreign_keys.len(), 1);
    let fk = &o.foreign_keys[0];
    assert_eq!(fk.columns, vec!["customer_id"]);
    assert_eq!(fk.ref_table, "customers");
    assert_eq!(fk.ref_columns, vec!["id"]);
    assert_eq!(fk.on_delete, FkAction::Cascade);
    assert_eq!(o.checks.len(), 2, "{:?}", o.checks);
    assert_eq!(o.checks[1].name.as_deref(), Some("note_len"));
    let big = o.indexes.iter().find(|i| i.name == "orders_big").unwrap();
    assert!(big.unique);
    assert_eq!(big.predicate.as_deref(), Some("total > 1000"));
    assert!(o.indexes.iter().any(|i| i.name == "orders_customer" && i.columns == vec!["customer_id"]));
    assert_eq!(o.triggers.len(), 1);
    assert_eq!(o.triggers[0].timing, "AFTER");
    assert_eq!(o.triggers[0].events, "INSERT");

    let v = c.describe_table(&ObjectRef::new(None, "big_orders", ObjectKind::View)).await.unwrap();
    assert_eq!(v.object.kind, ObjectKind::View);
    assert_eq!(v.columns.len(), 2);
    assert!(v.view_definition.as_deref().unwrap().starts_with("SELECT"));

    let ddl = c.object_ddl(&ObjectRef::table(None, "orders")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE orders"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX orders_customer"), "{ddl}");
    assert!(ddl.contains("CREATE TRIGGER orders_ai"), "{ddl}");

    let snap = c.snapshot_schema(Some("main")).await.unwrap();
    assert_eq!(snap.tables.len(), 3);
    assert!(snap.table("orders").is_some());

    let plan = c.explain("SELECT * FROM orders WHERE customer_id = 1", false).await.unwrap();
    assert!(!plan.rows.is_empty());
    let text: String = plan.rows.iter().map(|r| r.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ")).collect();
    assert!(text.contains("orders_customer"), "{text}");
}

#[tokio::test]
async fn cancel_long_query() {
    let c = open(":memory:").await;
    let handle = c.cancel_handle().expect("sqlite supports cancel");
    let c2 = c.clone();
    let task = tokio::spawn(async move {
        c2.execute(
            "WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s) SELECT count(*) FROM s",
            &ExecOptions::default(),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    handle.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(10), task).await.expect("cancel took too long").unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    // still usable
    exec(&c, "SELECT 1").await;
}
