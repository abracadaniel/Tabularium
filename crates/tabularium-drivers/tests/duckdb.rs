//! DuckDB integration tests (bundled engine, temp files). Run with `--features duckdb`.

#![cfg(feature = "duckdb")]

use std::sync::Arc;
use std::time::Duration;

use tabularium_core::{
    Connection, ConnectionConfig, Error, ExecOptions, ObjectKind, ObjectRef, StatementResult, TransactionMode, Value,
};

async fn open(path: &str) -> Arc<dyn Connection> {
    tabularium_drivers::connect(&ConnectionConfig::duckdb(path)).await.expect("connect")
}

async fn exec(c: &Arc<dyn Connection>, sql: &str) -> StatementResult {
    c.execute(sql, &ExecOptions::unlimited()).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

#[tokio::test]
async fn creates_file_and_reports_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.duckdb");
    let v = tabularium_drivers::test_connection(&ConnectionConfig::duckdb(path.to_str().unwrap())).await.unwrap();
    assert!(v.starts_with("DuckDB 1."), "{v}");
    assert!(path.exists());
    // URL form
    let cfg = ConnectionConfig::from_url(&format!("duckdb://{}", path.display())).unwrap();
    let c = tabularium_drivers::connect(&cfg).await.unwrap();
    exec(&c, "SELECT 1").await;
}

#[tokio::test]
async fn types_and_params() {
    let c = open(":memory:").await;
    exec(&c, "CREATE TYPE mood AS ENUM ('sad', 'happy')").await;
    exec(
        &c,
        "CREATE TABLE t (i INTEGER, ti TINYINT, bi BIGINT, ub UBIGINT, h HUGEINT, f FLOAT, d DOUBLE, \
         dec DECIMAL(10,2), s VARCHAR, b BLOB, bo BOOLEAN, dt DATE, tm TIME, ts TIMESTAMP, tstz TIMESTAMPTZ, \
         iv INTERVAL, u UUID, l INTEGER[], st STRUCT(a INTEGER, b VARCHAR), m MAP(VARCHAR, INTEGER), e mood, j JSON)",
    )
    .await;
    let r = c
        .execute(
            "INSERT INTO t VALUES (?, ?, ?, ?, 170141183460469231731687303715884105727, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
             '2024-01-02 03:04:05+00', INTERVAL '1 year 2 months 3 days 04:05:06', ?, [1, NULL, 3], {'a': 1, 'b': 'x'}, \
             MAP {'k': 5}, 'happy', '{\"k\": [1, 2]}')",
            &ExecOptions {
                max_rows: None,
                params: vec![
                    Value::Int(1),
                    Value::Int(-5),
                    Value::Int(i64::MIN),
                    Value::UInt(u64::MAX),
                    Value::Float(1.5),
                    Value::Float(2.25),
                    Value::Decimal("12.34".into()),
                    Value::from("héllo"),
                    Value::Bytes(vec![0, 1, 255]),
                    Value::Bool(true),
                    Value::from("2024-01-02"),
                    Value::from("03:04:05.5"),
                    Value::from("2024-01-02 03:04:05.678"),
                    Value::from("6af3923e-6a11-4f6a-98a1-1414653cc1c0"),
                ],
            },
        )
        .await
        .unwrap();
    assert_eq!(r.rows_affected, Some(1));
    assert!(r.columns.is_empty());
    let r = exec(&c, "SELECT * FROM t").await;
    let v = |n: &str| r.rows[0][r.column_index(n).unwrap()].clone();
    let ty = |n: &str| r.columns[r.column_index(n).unwrap()].type_name.clone();
    assert_eq!(v("i"), Value::Int(1));
    assert_eq!(ty("i"), "INTEGER");
    assert_eq!(v("ti"), Value::Int(-5));
    assert_eq!(v("bi"), Value::Int(i64::MIN));
    assert_eq!(v("ub"), Value::UInt(u64::MAX));
    assert_eq!(v("h"), Value::Decimal("170141183460469231731687303715884105727".into()));
    assert_eq!(ty("h"), "HUGEINT");
    assert_eq!(v("f"), Value::Float(1.5));
    assert_eq!(v("d"), Value::Float(2.25));
    assert_eq!(v("dec"), Value::Decimal("12.34".into()));
    assert_eq!(ty("dec"), "DECIMAL(10,2)");
    assert_eq!(v("s"), Value::from("héllo"));
    assert_eq!(v("b"), Value::Bytes(vec![0, 1, 255]));
    assert_eq!(v("bo"), Value::Bool(true));
    assert_eq!(v("dt").to_string(), "2024-01-02");
    assert_eq!(v("tm").to_string(), "03:04:05.500");
    assert_eq!(v("ts").to_string(), "2024-01-02 03:04:05.678");
    assert!(matches!(v("tstz"), Value::DateTimeTz(_)), "{:?}", v("tstz"));
    assert_eq!(v("iv").to_string(), "1 year 2 months 3 days 04:05:06");
    assert_eq!(v("u").to_string(), "6af3923e-6a11-4f6a-98a1-1414653cc1c0");
    assert!(matches!(v("u"), Value::Uuid(_)), "{:?}", v("u"));
    assert_eq!(v("l"), Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]));
    assert_eq!(ty("l"), "INTEGER[]");
    assert_eq!(v("st"), Value::Json(serde_json::json!({"a": 1, "b": "x"})));
    assert_eq!(v("m"), Value::Json(serde_json::json!({"k": 5})));
    assert_eq!(v("e"), Value::from("happy"));
    assert_eq!(v("j"), Value::Json(serde_json::json!({"k": [1, 2]})));
    assert_eq!(ty("j"), "JSON");

    // parameters in queries, empty results keep their columns
    let r = c
        .execute("SELECT i, s FROM t WHERE s = ? AND i = ?", &ExecOptions { max_rows: Some(5), params: vec![Value::from("nope"), Value::Int(1)] })
        .await
        .unwrap();
    assert_eq!(r.columns.len(), 2);
    assert!(r.rows.is_empty());
    // RETURNING and FROM-first syntax
    let r = exec(&c, "UPDATE t SET i = 2 RETURNING i").await;
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    let r = exec(&c, "FROM t SELECT i").await;
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    let r = exec(&c, "DELETE FROM t").await;
    assert_eq!(r.rows_affected, Some(1));
    // DDL: no result set
    let r = exec(&c, "CREATE TABLE z (a INT)").await;
    assert!(r.columns.is_empty());
    // PRAGMA / SHOW return rows
    assert!(!exec(&c, "SHOW TABLES").await.rows.is_empty());
    assert!(!exec(&c, "PRAGMA table_info('z')").await.rows.is_empty());
}

#[tokio::test]
async fn truncation_and_errors() {
    let c = open(":memory:").await;
    let r = c.execute("SELECT * FROM range(100000000)", &ExecOptions::with_limit(10)).await.unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.truncated);
    let r = c.execute("SELECT * FROM range(10)", &ExecOptions::with_limit(10)).await.unwrap();
    assert!(!r.truncated);
    assert_eq!(exec(&c, "SELECT count(*) FROM range(1000)").await.rows[0][0], Value::Int(1000));
    match c.execute("SELECT * FROM missing", &ExecOptions::default()).await.unwrap_err() {
        Error::Database(d) => {
            assert!(d.message.contains("missing"), "{d}");
            assert_eq!(d.code.as_deref(), Some("Catalog Error"));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(c.execute("SELEC 1", &ExecOptions::default()).await, Err(Error::Database(_))));
}

#[tokio::test]
async fn manual_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let c = open(dir.path().join("tx.duckdb").to_str().unwrap()).await;
    exec(&c, "CREATE TABLE t (a INT)").await;
    c.set_transaction_mode(TransactionMode::Manual).await.unwrap();
    exec(&c, "INSERT INTO t VALUES (1)").await;
    assert!(c.in_transaction());
    c.rollback().await.unwrap();
    assert!(!c.in_transaction());
    assert_eq!(exec(&c, "SELECT count(*) FROM t").await.rows[0][0], Value::Int(0));
    exec(&c, "INSERT INTO t VALUES (2)").await;
    c.commit().await.unwrap();
    c.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    assert_eq!(exec(&c, "SELECT a FROM t").await.rows, vec![vec![Value::Int(2)]]);

    // user-issued BEGIN / ROLLBACK in auto mode
    exec(&c, "BEGIN").await;
    exec(&c, "DELETE FROM t").await;
    assert!(c.in_transaction());
    exec(&c, "ROLLBACK").await;
    assert!(!c.in_transaction());
    assert_eq!(exec(&c, "SELECT count(*) FROM t").await.rows[0][0], Value::Int(1));

    // a second connection to the same file shares the database instance
    let c2 = open(dir.path().join("tx.duckdb").to_str().unwrap()).await;
    assert_eq!(exec(&c2, "SELECT count(*) FROM t").await.rows[0][0], Value::Int(1));
}

#[tokio::test]
async fn read_only_and_init_sql() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.duckdb");
    {
        let c = open(path.to_str().unwrap()).await;
        exec(&c, "CREATE TABLE t (a INT)").await;
        c.close().await.unwrap();
    }
    let mut cfg = ConnectionConfig::duckdb(path.to_str().unwrap());
    cfg.read_only = true;
    cfg.init_sql = Some("SET threads = 2; CREATE TEMP TABLE boot AS SELECT 42 AS x;".into());
    let c = tabularium_drivers::connect(&cfg).await.unwrap();
    exec(&c, "SELECT * FROM t").await;
    assert_eq!(exec(&c, "SELECT x FROM boot").await.rows[0][0], Value::Int(42));
    let e = c.execute("INSERT INTO t VALUES (1)", &ExecOptions::default()).await.unwrap_err();
    assert!(matches!(e, Error::ReadOnly(_)), "{e:?}");
}

#[tokio::test]
async fn introspection() {
    let c = open(":memory:").await;
    for s in [
        "CREATE SEQUENCE seq_id START 1",
        "CREATE TABLE customers (id INTEGER PRIMARY KEY DEFAULT nextval('seq_id'), email VARCHAR NOT NULL UNIQUE, \
         name VARCHAR DEFAULT 'anon', CHECK (length(email) > 3))",
        "COMMENT ON TABLE customers IS 'All customers'",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id), total DECIMAL(10,2))",
        "CREATE INDEX orders_total ON orders(total)",
        "CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100",
        "CREATE MACRO twice(x) AS x * 2",
        "CREATE SCHEMA other",
        "CREATE TABLE other.t (a INT)",
    ] {
        exec(&c, s).await;
    }
    assert_eq!(c.current_schema().await.unwrap().as_deref(), Some("main"));
    assert_eq!(c.current_database().as_deref(), Some("memory"));
    assert_eq!(c.list_databases().await.unwrap(), vec!["memory"]);
    let schemas = c.list_schemas().await.unwrap();
    assert!(schemas.contains(&"main".to_string()) && schemas.contains(&"other".to_string()), "{schemas:?}");
    assert!(!schemas.contains(&"information_schema".to_string()));

    let objs = c
        .list_objects(
            None,
            &[ObjectKind::Table, ObjectKind::View, ObjectKind::Index, ObjectKind::Sequence, ObjectKind::Function],
        )
        .await
        .unwrap();
    let has = |k: ObjectKind, n: &str| objs.iter().any(|o| o.object.kind == k && o.object.name == n);
    assert!(has(ObjectKind::Table, "customers"));
    assert!(has(ObjectKind::Table, "orders"));
    assert!(!has(ObjectKind::Table, "t"), "other schema leaked: {objs:?}");
    assert!(has(ObjectKind::View, "big_orders"));
    assert!(has(ObjectKind::Index, "orders_total"));
    assert!(has(ObjectKind::Sequence, "seq_id"));
    assert!(has(ObjectKind::Function, "twice"));
    assert_eq!(objs.iter().find(|o| o.object.name == "customers").unwrap().comment.as_deref(), Some("All customers"));
    let other = c.list_objects(Some("other"), &[ObjectKind::Table]).await.unwrap();
    assert_eq!(other.len(), 1);

    let t = c.describe_table(&ObjectRef::table(Some("main"), "customers")).await.unwrap();
    assert_eq!(t.comment.as_deref(), Some("All customers"));
    assert_eq!(t.columns.len(), 3);
    assert!(t.column("id").unwrap().auto_increment);
    assert!(!t.column("email").unwrap().nullable);
    assert_eq!(t.column("name").unwrap().default.as_deref(), Some("'anon'"));
    assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["id"]);
    assert_eq!(t.unique_keys[0].columns, vec!["email"]);
    assert_eq!(t.checks.len(), 1);

    let o = c.describe_table(&ObjectRef::table(None, "orders")).await.unwrap();
    assert_eq!(o.foreign_keys.len(), 1);
    assert_eq!(o.foreign_keys[0].ref_table, "customers");
    assert_eq!(o.foreign_keys[0].ref_columns, vec!["id"]);
    assert_eq!(o.foreign_keys[0].columns, vec!["customer_id"]);
    assert!(o.indexes.iter().any(|i| i.name == "orders_total" && i.columns == vec!["total"]), "{:?}", o.indexes);

    let v = c.describe_table(&ObjectRef::new(None, "big_orders", ObjectKind::View)).await.unwrap();
    assert_eq!(v.object.kind, ObjectKind::View);
    assert_eq!(v.columns.len(), 2);
    assert!(v.view_definition.as_deref().unwrap().to_ascii_uppercase().starts_with("SELECT"), "{:?}", v.view_definition);

    let ddl = c.object_ddl(&ObjectRef::table(None, "orders")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE orders"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX orders_total"), "{ddl}");
    let vddl = c.object_ddl(&ObjectRef::new(None, "big_orders", ObjectKind::View)).await.unwrap();
    assert!(vddl.contains("CREATE VIEW big_orders"), "{vddl}");
    let sddl = c.object_ddl(&ObjectRef::new(None, "seq_id", ObjectKind::Sequence)).await.unwrap();
    assert!(sddl.contains("CREATE SEQUENCE"), "{sddl}");
    let r = c.describe_routine(&ObjectRef::new(None, "twice", ObjectKind::Function), None).await.unwrap();
    assert!(r.definition.starts_with("CREATE MACRO main.twice(x) AS"), "{}", r.definition);

    let snap = c.snapshot_schema(Some("main")).await.unwrap();
    assert!(snap.table("orders").is_some());
    let plan = c.explain("SELECT * FROM orders WHERE id = 1", false).await.unwrap();
    assert!(!plan.rows.is_empty());
}

#[tokio::test]
async fn cancel_long_query() {
    let c = open(":memory:").await;
    let handle = c.cancel_handle().expect("duckdb supports cancel");
    let c2 = c.clone();
    let task = tokio::spawn(async move {
        c2.execute("SELECT count(*) FROM range(10000000000) a, range(1000) b", &ExecOptions::default()).await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    handle.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(10), task).await.expect("cancel took too long").unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    exec(&c, "SELECT 1").await;
}
