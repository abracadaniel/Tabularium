//! ClickHouse integration tests. Set `TABULARIUM_TEST_CLICKHOUSE_URL`, e.g.
//! `clickhouse://default@127.0.0.1:8123/default`; tests are skipped otherwise.
//! Each test uses its own database.

#![cfg(feature = "clickhouse")]

use std::sync::Arc;
use std::time::Duration;

use tabularium_core::{
    Connection, ConnectionConfig, Error, ExecOptions, ObjectKind, ObjectRef, StatementResult, TransactionMode, Value,
};

fn ch_url() -> Option<String> {
    match std::env::var("TABULARIUM_TEST_CLICKHOUSE_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            eprintln!("TABULARIUM_TEST_CLICKHOUSE_URL not set; skipping ClickHouse test");
            None
        }
    }
}

async fn exec(c: &Arc<dyn Connection>, sql: &str) -> StatementResult {
    c.execute(sql, &ExecOptions::unlimited()).await.unwrap_or_else(|e| panic!("{sql}\n=> {e}"))
}

/// Connect and switch to a fresh database `db`.
async fn with_db(url: &str, db: &str) -> Arc<dyn Connection> {
    let c = tabularium_drivers::connect(&ConnectionConfig::from_url(url).unwrap()).await.expect("connect");
    exec(&c, &format!("DROP DATABASE IF EXISTS {db}")).await;
    exec(&c, &format!("CREATE DATABASE {db}")).await;
    c.use_database(db).await.unwrap();
    c
}

#[tokio::test]
async fn basics_and_types() {
    let Some(url) = ch_url() else { return };
    let v = tabularium_drivers::test_connection(&ConnectionConfig::from_url(&url).unwrap()).await.unwrap();
    assert!(v.starts_with("ClickHouse "), "{v}");
    let c = with_db(&url, "tab_ch_types").await;
    assert_eq!(c.current_database().as_deref(), Some("tab_ch_types"));
    assert_eq!(c.current_schema().await.unwrap().as_deref(), Some("tab_ch_types"));
    exec(
        &c,
        "CREATE TABLE t (id UInt64, i8 Int8, i64 Int64, u64 UInt64, big Int128, f32 Float32, f64 Float64, \
         d Decimal(18, 4), s String, fs FixedString(4), dt Date, d32 Date32, dtm DateTime('UTC'), dtm64 DateTime64(3), \
         id2 UUID, b Bool, n Nullable(Int32), lc LowCardinality(String), arr Array(Nullable(Int32)), \
         m Map(String, UInt8), tup Tuple(a UInt8, b String), ip4 IPv4, ip6 IPv6, e Enum8('x' = 1, 'y' = 2)) \
         ENGINE = MergeTree ORDER BY id",
    )
    .await;
    let r = c
        .execute(
            "INSERT INTO t VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, map('k', 7), (1, 'q'), ?, ?, ?)",
            &ExecOptions {
                max_rows: None,
                params: vec![
                    Value::Int(1),
                    Value::Int(-5),
                    Value::Int(i64::MIN),
                    Value::UInt(u64::MAX),
                    Value::Decimal("170141183460469231731687303715884105727".into()),
                    Value::Float(1.5),
                    Value::Float(2.25),
                    Value::Decimal("12.3400".into()),
                    Value::from("it's a \\ test"),
                    Value::from("ab"),
                    Value::from("2024-01-02"),
                    Value::from("1900-01-01"),
                    Value::from("2024-01-02 03:04:05"),
                    Value::from("2024-01-02 03:04:05.678"),
                    Value::from("6af3923e-6a11-4f6a-98a1-1414653cc1c0"),
                    Value::Bool(true),
                    Value::Null,
                    Value::from("low"),
                    Value::Array(vec![Value::Int(1), Value::Null]),
                    Value::from("1.2.3.4"),
                    Value::from("::1"),
                    Value::from("y"),
                ],
            },
        )
        .await
        .unwrap();
    assert_eq!(r.rows_affected, Some(1));
    let r = exec(&c, "SELECT * FROM t").await;
    let v = |n: &str| r.rows[0][r.column_index(n).unwrap()].clone();
    assert_eq!(v("id"), Value::UInt(1));
    assert_eq!(v("i8"), Value::Int(-5));
    assert_eq!(v("i64"), Value::Int(i64::MIN));
    assert_eq!(v("u64"), Value::UInt(u64::MAX));
    assert_eq!(v("big"), Value::Decimal("170141183460469231731687303715884105727".into()));
    assert_eq!(v("f32"), Value::Float(1.5));
    assert_eq!(v("f64"), Value::Float(2.25));
    assert_eq!(v("d"), Value::Decimal("12.34".into()));
    assert_eq!(v("s"), Value::from("it's a \\ test"));
    assert_eq!(v("fs"), Value::from("ab"));
    assert_eq!(v("dt").to_string(), "2024-01-02");
    assert_eq!(v("d32").to_string(), "1900-01-01");
    assert_eq!(v("dtm").to_string(), "2024-01-02 03:04:05");
    assert_eq!(v("dtm64").to_string(), "2024-01-02 03:04:05.678");
    assert_eq!(v("id2").to_string(), "6af3923e-6a11-4f6a-98a1-1414653cc1c0");
    assert!(matches!(v("id2"), Value::Uuid(_)));
    assert_eq!(v("b"), Value::Bool(true));
    assert_eq!(v("n"), Value::Null);
    assert_eq!(v("lc"), Value::from("low"));
    assert_eq!(v("arr"), Value::Array(vec![Value::Int(1), Value::Null]));
    assert_eq!(v("m"), Value::Json(serde_json::json!({"k": 7})));
    assert_eq!(v("tup"), Value::Json(serde_json::json!({"a": 1, "b": "q"})));
    assert_eq!(v("ip4"), Value::from("1.2.3.4"));
    assert_eq!(v("ip6"), Value::from("::1"));
    assert_eq!(v("e"), Value::from("y"));
    let col = |n: &str| r.columns.iter().find(|c| c.name == n).unwrap().clone();
    assert_eq!(col("n").type_name, "Nullable(Int32)");
    assert_eq!(col("n").nullable, Some(true));
    assert_eq!(col("id").nullable, Some(false));
    // zero rows still have columns
    let r = exec(&c, "SELECT id, s FROM t WHERE 0").await;
    assert_eq!(r.columns.len(), 2);
    assert!(r.rows.is_empty());
    // a user FORMAT clause is shown as text
    let r = exec(&c, "SELECT 1, 'a' FORMAT CSV").await;
    assert_eq!(r.rows, vec![vec![Value::from("1,\"a\"")]]);
    // DDL reports no result set
    let r = exec(&c, "CREATE TABLE x (a Int8) ENGINE = Memory").await;
    assert!(r.columns.is_empty());
}

#[tokio::test]
async fn truncation_errors_and_sessions() {
    let Some(url) = ch_url() else { return };
    let c = with_db(&url, "tab_ch_trunc").await;
    let r = c.execute("SELECT number FROM system.numbers LIMIT 1000000", &ExecOptions::with_limit(10)).await.unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.truncated);
    assert_eq!(r.rows[9][0], Value::UInt(9));
    // the server-side limit does not leak into subqueries or the next query
    let r = c
        .execute("SELECT count() FROM (SELECT number FROM numbers(5000))", &ExecOptions::with_limit(10))
        .await
        .unwrap();
    assert_eq!(r.rows[0][0], Value::UInt(5000));
    assert!(!r.truncated);
    let r = exec(&c, "SELECT number FROM numbers(100)").await;
    assert_eq!(r.rows.len(), 100);
    assert!(!r.truncated);
    match c.execute("SELECT * FROM nope", &ExecOptions::default()).await.unwrap_err() {
        Error::Database(d) => assert!(d.code.as_deref().unwrap().starts_with("60"), "{d}"),
        other => panic!("{other:?}"),
    }
    match c.execute("SELEC 1", &ExecOptions::default()).await.unwrap_err() {
        Error::Database(d) => {
            assert!(d.code.as_deref().unwrap().starts_with("62"), "{d}");
            assert_eq!(d.position, Some(1));
        }
        other => panic!("{other:?}"),
    }
    // errors raised mid-stream
    let e = c.execute("SELECT throwIf(number = 300000) FROM numbers(1000000)", &ExecOptions::unlimited()).await;
    assert!(matches!(e, Err(Error::Database(_))), "{e:?}");
    // session state: SET and temporary tables survive between statements
    exec(&c, "SET max_threads = 3").await;
    assert_eq!(exec(&c, "SELECT getSetting('max_threads')").await.rows[0][0].to_string(), "3");
    exec(&c, "CREATE TEMPORARY TABLE tmp (a UInt8)").await;
    exec(&c, "INSERT INTO tmp VALUES (1), (2)").await;
    assert_eq!(exec(&c, "SELECT count() FROM tmp").await.rows[0][0], Value::UInt(2));
    // USE switches the database
    exec(&c, "USE system").await;
    assert_eq!(c.current_database().as_deref(), Some("system"));
    assert!(c.use_database("does_not_exist").await.is_err());
    let plan = c.explain("SELECT number FROM numbers(10) WHERE number = 1", false).await.unwrap();
    assert!(!plan.rows.is_empty());
    let pipe = c.explain("SELECT number FROM numbers(10)", true).await.unwrap();
    assert!(!pipe.rows.is_empty());
}

#[tokio::test]
async fn transactions_and_cancel() {
    let Some(url) = ch_url() else { return };
    let c = with_db(&url, "tab_ch_tx").await;
    assert!(matches!(c.set_transaction_mode(TransactionMode::Manual).await, Err(Error::Unsupported(_))));
    c.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    assert!(!c.in_transaction());
    c.commit().await.unwrap();
    c.rollback().await.unwrap();

    let h = c.cancel_handle().unwrap();
    let c2 = c.clone();
    let start = std::time::Instant::now();
    let task = tokio::spawn(async move {
        c2.execute("SELECT sleepEachRow(0.5) FROM numbers(40) SETTINGS max_block_size = 1", &ExecOptions::default()).await
    });
    tokio::time::sleep(Duration::from_millis(700)).await;
    h.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(10), task).await.expect("cancel too slow").unwrap();
    assert!(matches!(&res, Err(Error::Cancelled)), "{res:?}");
    assert!(start.elapsed() < Duration::from_secs(10));
    exec(&c, "SELECT 1").await;
}

#[tokio::test]
async fn introspection_and_ddl() {
    let Some(url) = ch_url() else { return };
    let c = with_db(&url, "tab_ch_intro").await;
    for s in [
        "CREATE TABLE events (id UInt64 COMMENT 'event id', ts DateTime DEFAULT now(), kind LowCardinality(String), \
         payload Nullable(String), day Date MATERIALIZED toDate(ts), INDEX kind_idx kind TYPE set(100) GRANULARITY 2) \
         ENGINE = MergeTree PRIMARY KEY (id, ts) ORDER BY (id, ts, kind) COMMENT 'All events'",
        "CREATE VIEW recent AS SELECT id, ts FROM events WHERE ts > now() - INTERVAL 1 DAY",
        "CREATE MATERIALIZED VIEW per_kind ENGINE = SummingMergeTree ORDER BY kind AS SELECT kind, count() AS n FROM events GROUP BY kind",
    ] {
        exec(&c, s).await;
    }
    exec(&c, "INSERT INTO events (id, kind) VALUES (1, 'a'), (2, 'b')").await;
    let schemas = c.list_schemas().await.unwrap();
    assert!(schemas.contains(&"tab_ch_intro".to_string()));
    assert!(!schemas.contains(&"system".to_string()));
    assert!(c.list_databases().await.unwrap().contains(&"system".to_string()));
    let objs = c
        .list_objects(None, &[ObjectKind::Table, ObjectKind::View, ObjectKind::MaterializedView, ObjectKind::Index])
        .await
        .unwrap();
    let has = |k: ObjectKind, n: &str| objs.iter().any(|o| o.object.kind == k && o.object.name == n);
    assert!(has(ObjectKind::Table, "events"), "{objs:?}");
    assert!(has(ObjectKind::View, "recent"));
    assert!(has(ObjectKind::MaterializedView, "per_kind"));
    assert!(has(ObjectKind::Index, "kind_idx"));
    assert!(!objs.iter().any(|o| o.object.name.starts_with(".inner")));
    let ev = objs.iter().find(|o| o.object.name == "events").unwrap();
    assert_eq!(ev.comment.as_deref(), Some("All events"));
    assert_eq!(ev.row_estimate, Some(2));

    let t = c.describe_table(&ObjectRef::table(Some("tab_ch_intro"), "events")).await.unwrap();
    assert_eq!(t.comment.as_deref(), Some("All events"));
    assert_eq!(t.columns.len(), 5);
    assert_eq!(t.column("id").unwrap().comment.as_deref(), Some("event id"));
    assert_eq!(t.column("ts").unwrap().default.as_deref(), Some("now()"));
    assert!(t.column("payload").unwrap().nullable);
    assert!(!t.column("id").unwrap().nullable);
    assert_eq!(t.column("day").unwrap().generated.as_deref(), Some("MATERIALIZED toDate(ts)"));
    assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["id", "ts"]);
    assert_eq!(t.indexes.len(), 1);
    assert_eq!(t.indexes[0].method.as_deref(), Some("set(100)"));

    let v = c.describe_table(&ObjectRef::new(None, "recent", ObjectKind::View)).await.unwrap();
    assert_eq!(v.object.kind, ObjectKind::View);
    assert!(v.view_definition.as_deref().unwrap().contains("FROM"), "{:?}", v.view_definition);
    assert_eq!(v.columns.len(), 2);

    let ddl = c.object_ddl(&ObjectRef::table(None, "events")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE tab_ch_intro.events"), "{ddl}");
    assert!(ddl.contains("MergeTree"), "{ddl}");
    let mv = c.object_ddl(&ObjectRef::new(None, "per_kind", ObjectKind::MaterializedView)).await.unwrap();
    assert!(mv.contains("MATERIALIZED VIEW"), "{mv}");
    let idx = c.object_ddl(&ObjectRef::new(None, "kind_idx", ObjectKind::Index)).await.unwrap();
    assert!(idx.contains("ADD INDEX kind_idx kind TYPE set(100) GRANULARITY 2"), "{idx}");
    let db = c.object_ddl(&ObjectRef::new(None, "tab_ch_intro", ObjectKind::Schema)).await.unwrap();
    assert!(db.starts_with("CREATE DATABASE"), "{db}");

    let snap = c.snapshot_schema(None).await.unwrap();
    assert!(snap.table("events").is_some());
    assert!(!c.list_sessions().await.unwrap().is_empty());
    assert!(c.list_roles().await.unwrap().iter().any(|r| r.name == "default"));
}

#[tokio::test]
async fn read_only_and_init_sql() {
    let Some(url) = ch_url() else { return };
    let mut cfg = ConnectionConfig::from_url(&url).unwrap();
    cfg.read_only = true;
    cfg.init_sql = Some("SET max_threads = 2".into());
    let c = tabularium_drivers::connect(&cfg).await.unwrap();
    assert!(matches!(c.execute("DROP DATABASE x", &ExecOptions::default()).await, Err(Error::ReadOnly(_))));
    // the session is also server-side read-only (readonly = 2)
    assert_eq!(exec(&c, "SELECT getSetting('readonly')").await.rows[0][0].to_string(), "2");
    assert_eq!(exec(&c, "SELECT getSetting('max_threads')").await.rows[0][0].to_string(), "2");
}
