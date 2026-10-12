//! PostgreSQL integration tests. Set `TABULARIUM_TEST_PG_URL`, e.g.
//! `postgres://postgres@localhost:54329/postgres`; tests are skipped otherwise.
//! Each test works in its own schema so tests can run in parallel.

#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::time::Duration;

use tabularium_core::{
    Connection, ConnectionConfig, Error, ExecOptions, FkAction, ObjectKind, ObjectRef, StatementResult,
    TransactionMode, Value,
};

fn pg_url() -> Option<String> {
    match std::env::var("TABULARIUM_TEST_PG_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            eprintln!("TABULARIUM_TEST_PG_URL not set; skipping PostgreSQL test");
            None
        }
    }
}

fn config(url: &str) -> ConnectionConfig {
    ConnectionConfig::from_url(url).expect("valid URL")
}

async fn connect_cfg(cfg: &ConnectionConfig) -> Arc<dyn Connection> {
    tabularium_drivers::connect(cfg).await.expect("connect")
}

async fn exec(c: &Arc<dyn Connection>, sql: &str) -> StatementResult {
    c.execute(sql, &ExecOptions::unlimited()).await.unwrap_or_else(|e| panic!("{sql}\n=> {e}"))
}

/// Connect with `search_path` set to a fresh schema.
async fn with_schema(url: &str, schema: &str) -> Arc<dyn Connection> {
    let c = connect_cfg(&config(url)).await;
    exec(&c, &format!("DROP SCHEMA IF EXISTS {schema} CASCADE")).await;
    exec(&c, &format!("CREATE SCHEMA {schema}")).await;
    exec(&c, &format!("SET search_path = {schema}, public")).await;
    c
}

#[tokio::test]
async fn version_and_basics() {
    let Some(url) = pg_url() else { return };
    let v = tabularium_drivers::test_connection(&config(&url)).await.unwrap();
    assert!(v.starts_with("PostgreSQL"), "{v}");
    let c = connect_cfg(&config(&url)).await;
    c.ping().await.unwrap();
    assert!(c.current_database().is_some());
    assert_eq!(c.current_schema().await.unwrap().as_deref(), Some("public"));
}

#[tokio::test]
async fn column_types_binary_and_text() {
    let Some(url) = pg_url() else { return };
    let c = with_schema(&url, "t_types").await;
    exec(&c, "CREATE TYPE mood AS ENUM ('sad', 'happy')").await;
    exec(&c, "CREATE DOMAIN posint AS int CHECK (VALUE > 0)").await;
    let sql = "SELECT 1::int2 AS i2, 2::int4 AS i4, 3::int8 AS i8, 12.50::numeric(10,2) AS num, 'NaN'::numeric AS nan, \
        'txt'::text AS t, 'vc'::varchar(5) AS vc, 'c'::char(3) AS ch, 'nm'::name AS nm, true AS b, \
        '\\x00ff'::bytea AS by, '2024-01-02'::date AS d, '2024-01-02 03:04:05.123456'::timestamp AS ts, \
        '2024-01-02 03:04:05+00'::timestamptz AS tstz, '12:34:56'::time AS tm, \
        'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid AS u, '{\"a\": [1, 2]}'::json AS j, '{\"b\": true}'::jsonb AS jb, \
        ARRAY[1, NULL, 3]::int[] AS ia, ARRAY['x', 'y z']::text[] AS ta, '192.168.1.0/24'::inet AS ip, \
        '10.0.0.0/8'::cidr AS net, '08:00:2b:01:02:03'::macaddr AS mac, '1 year 2 mons 3 days 04:05:06'::interval AS iv, \
        'happy'::mood AS e, 12.34::money AS m, point(1.5, 2) AS p, B'101' AS bits, 5::posint AS dom, \
        int4range(1, 10) AS r, 1.5::float4 AS f4, 2.25::float8 AS f8, 'infinity'::date AS dinf, \
        ROW(1, 'a') AS rec, '{{1,2},{3,4}}'::int[] AS md, to_tsvector('simple', 'a b') AS tsv, NULL::text AS nul";
    let expect = |r: &StatementResult, text_mode: bool| {
        let col = |n: &str| r.column_index(n).unwrap_or_else(|| panic!("column {n}"));
        let v = |n: &str| r.rows[0][col(n)].clone();
        assert_eq!(v("i2"), Value::Int(1));
        assert_eq!(v("i4"), Value::Int(2));
        assert_eq!(v("i8"), Value::Int(3));
        assert_eq!(v("num"), Value::Decimal("12.50".into()));
        assert_eq!(v("nan"), Value::Decimal("NaN".into()));
        assert_eq!(v("t"), Value::Text("txt".into()));
        assert_eq!(v("vc"), Value::Text("vc".into()));
        assert_eq!(v("ch"), Value::Text("c  ".into()));
        assert_eq!(v("nm"), Value::Text("nm".into()));
        assert_eq!(v("b"), Value::Bool(true));
        assert_eq!(v("by"), Value::Bytes(vec![0, 255]));
        assert_eq!(v("d").to_string(), "2024-01-02");
        assert_eq!(v("ts").to_string(), "2024-01-02 03:04:05.123456");
        assert!(matches!(v("tstz"), Value::DateTimeTz(_)), "{:?}", v("tstz"));
        assert_eq!(v("tm").to_string(), "12:34:56");
        assert!(matches!(v("u"), Value::Uuid(_)));
        assert_eq!(v("j"), Value::Json(serde_json::json!({"a": [1, 2]})));
        assert_eq!(v("jb"), Value::Json(serde_json::json!({"b": true})));
        assert_eq!(v("ia"), Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]));
        assert_eq!(v("ta"), Value::Array(vec![Value::Text("x".into()), Value::Text("y z".into())]));
        assert_eq!(v("ip").to_string(), "192.168.1.0/24");
        assert_eq!(v("net").to_string(), "10.0.0.0/8");
        assert_eq!(v("mac").to_string(), "08:00:2b:01:02:03");
        assert_eq!(v("iv").to_string(), "1 year 2 mons 3 days 04:05:06");
        assert_eq!(v("e"), Value::Text("happy".into()));
        if text_mode {
            assert_eq!(v("m").to_string(), "$12.34");
        } else {
            assert_eq!(v("m"), Value::Decimal("12.34".into()));
        }
        assert_eq!(v("p").to_string(), "(1.5,2)");
        assert_eq!(v("bits").to_string(), "101");
        assert_eq!(v("dom"), Value::Int(5));
        assert_eq!(v("r").to_string(), "[1,10)");
        assert_eq!(v("f4"), Value::Float(1.5));
        assert_eq!(v("f8"), Value::Float(2.25));
        assert_eq!(v("dinf").to_string(), "infinity");
        assert_eq!(v("rec").to_string(), "(1,a)");
        assert_eq!(
            v("md"),
            Value::Array(vec![Value::Array(vec![Value::Int(1), Value::Int(2)]), Value::Array(vec![Value::Int(3), Value::Int(4)])])
        );
        assert_eq!(v("tsv").to_string(), "'a':1 'b':2");
        assert_eq!(v("nul"), Value::Null);
    };
    // extended protocol, binary results (cursor path)
    let r = c.execute(sql, &ExecOptions::default()).await.unwrap();
    assert_eq!(r.rows.len(), 1);
    let types: Vec<(&str, &str)> = r.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
    assert!(types.contains(&("i4", "int4")), "{types:?}");
    assert!(types.contains(&("ia", "int4[]")), "{types:?}");
    assert!(types.contains(&("e", "mood")), "{types:?}");
    expect(&r, false);
    // extended protocol without cursor (no row limit)
    let r = exec(&c, sql).await;
    expect(&r, false);
    // simple protocol, text results (multi-statement string)
    let r = exec(&c, &format!("SELECT 1 AS first; {sql}")).await;
    let types: Vec<(&str, &str)> = r.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
    assert!(types.contains(&("e", "mood")), "{types:?}");
    expect(&r, true);
}

#[tokio::test]
async fn params() {
    let Some(url) = pg_url() else { return };
    let c = with_schema(&url, "t_params").await;
    exec(&c, "CREATE TABLE p (id int PRIMARY KEY, name text, amount numeric(12,3), at timestamptz, tags text[], u uuid, ok bool)").await;
    let r = c
        .execute(
            "INSERT INTO p VALUES ($1, $2, $3, $4, $5, $6, $7)",
            &ExecOptions {
                max_rows: None,
                params: vec![
                    Value::Int(1),
                    Value::from("alice"),
                    Value::Decimal("1234.567".into()),
                    Value::from("2024-05-06 07:08:09+02"),
                    Value::Array(vec![Value::from("a"), Value::Null]),
                    Value::from("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
                    Value::Bool(true),
                ],
            },
        )
        .await
        .unwrap();
    assert_eq!(r.rows_affected, Some(1));
    let r = c
        .execute(
            "SELECT id, name, amount, at, tags, ok FROM p WHERE name = $1 AND amount > $2",
            &ExecOptions { max_rows: Some(10), params: vec![Value::from("alice"), Value::Int(1000)] },
        )
        .await
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][2], Value::Decimal("1234.567".into()));
    assert_eq!(r.rows[0][3].to_string(), "2024-05-06 05:08:09 +00:00");
    assert_eq!(r.rows[0][4], Value::Array(vec![Value::from("a"), Value::Null]));
    let r = c
        .execute("SELECT $1::int + 1, $2::text IS NULL", &ExecOptions { max_rows: None, params: vec![Value::from("41"), Value::Null] })
        .await
        .unwrap();
    assert_eq!(r.rows[0], vec![Value::Int(42), Value::Bool(true)]);
    // RETURNING
    let r = c
        .execute("UPDATE p SET name = $1 RETURNING id, name", &ExecOptions { max_rows: None, params: vec![Value::from("bob")] })
        .await
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1), Value::from("bob")]]);
    // zero-row SELECT still reports columns
    let r = c.execute("SELECT id, name FROM p WHERE false", &ExecOptions::default()).await.unwrap();
    assert_eq!(r.columns.len(), 2);
    assert!(r.rows.is_empty());
    let r = c.execute("SELECT id, name FROM p WHERE false", &ExecOptions::unlimited()).await.unwrap();
    assert_eq!(r.columns.len(), 2);
}

#[tokio::test]
async fn errors_carry_position_and_detail() {
    let Some(url) = pg_url() else { return };
    let c = with_schema(&url, "t_errors").await;
    match c.execute("SELECT * FROM no_such_table", &ExecOptions::default()).await.unwrap_err() {
        Error::Database(d) => {
            assert_eq!(d.code.as_deref(), Some("42P01"));
            assert_eq!(d.position, Some(15));
        }
        other => panic!("{other:?}"),
    }
    exec(&c, "CREATE TABLE u (a int UNIQUE)").await;
    exec(&c, "INSERT INTO u VALUES (1)").await;
    match c.execute("INSERT INTO u VALUES (1)", &ExecOptions::default()).await.unwrap_err() {
        Error::Database(d) => {
            assert_eq!(d.code.as_deref(), Some("23505"));
            assert!(d.detail.as_deref().unwrap_or("").contains("already exists"), "{d}");
        }
        other => panic!("{other:?}"),
    }
    match c.execute("SELEKT 1", &ExecOptions::default()).await.unwrap_err() {
        Error::Database(d) => assert_eq!(d.code.as_deref(), Some("42601")),
        other => panic!("{other:?}"),
    }
    // still usable
    exec(&c, "SELECT 1").await;
    // bad connection is a connection error
    let mut bad = config(&url);
    bad.port = 1;
    bad.connect_timeout_secs = 5;
    assert!(matches!(tabularium_drivers::connect(&bad).await.err(), Some(Error::Connection(_))));
}

#[tokio::test]
async fn cancellation() {
    let Some(url) = pg_url() else { return };
    let c = connect_cfg(&config(&url)).await;
    let handle = c.cancel_handle().expect("cancel handle");
    let c2 = c.clone();
    let task = tokio::spawn(async move { c2.execute("SELECT pg_sleep(10)", &ExecOptions::default()).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let start = std::time::Instant::now();
    handle.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel too slow").unwrap();
    assert!(matches!(res, Err(Error::Cancelled)), "{res:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
    exec(&c, "SELECT 1").await;
}

#[tokio::test]
async fn truncation_uses_cursor() {
    let Some(url) = pg_url() else { return };
    let c = connect_cfg(&config(&url)).await;
    let r = c.execute("SELECT g FROM generate_series(1, 1000000) g", &ExecOptions::with_limit(100)).await.unwrap();
    assert_eq!(r.rows.len(), 100);
    assert!(r.truncated);
    assert_eq!(r.rows[99][0], Value::Int(100));
    let r = c.execute("SELECT g FROM generate_series(1, 100) g", &ExecOptions::with_limit(100)).await.unwrap();
    assert_eq!(r.rows.len(), 100);
    assert!(!r.truncated);
    // inside a manual transaction the cursor runs in the user's transaction
    c.set_transaction_mode(TransactionMode::Manual).await.unwrap();
    let r = c.execute("SELECT g FROM generate_series(1, 1000) g;", &ExecOptions::with_limit(5)).await.unwrap();
    assert_eq!(r.rows.len(), 5);
    assert!(r.truncated);
    assert!(c.in_transaction());
    c.rollback().await.unwrap();
}

#[tokio::test]
async fn manual_transactions() {
    let Some(url) = pg_url() else { return };
    let c = with_schema(&url, "t_tx").await;
    exec(&c, "CREATE TABLE t (a int)").await;
    c.set_transaction_mode(TransactionMode::Manual).await.unwrap();
    assert_eq!(c.transaction_mode(), TransactionMode::Manual);
    assert!(!c.in_transaction());
    exec(&c, "INSERT INTO t VALUES (1)").await;
    assert!(c.in_transaction());
    // another session doesn't see it
    let other = connect_cfg(&config(&url)).await;
    assert_eq!(exec(&other, "SELECT count(*) FROM t_tx.t").await.rows[0][0], Value::Int(0));
    c.rollback().await.unwrap();
    assert!(!c.in_transaction());
    exec(&c, "INSERT INTO t VALUES (2)").await;
    c.commit().await.unwrap();
    assert_eq!(exec(&other, "SELECT count(*) FROM t_tx.t").await.rows[0][0], Value::Int(1));
    // error inside a transaction then rollback
    exec(&c, "INSERT INTO t VALUES (3)").await;
    assert!(c.execute("SELECT 1/0", &ExecOptions::default()).await.is_err());
    assert!(c.in_transaction());
    assert!(c.execute("SELECT 1", &ExecOptions::default()).await.is_err(), "aborted transaction");
    c.rollback().await.unwrap();
    exec(&c, "SELECT 1").await;
    c.rollback().await.unwrap();
    // VACUUM is not wrapped in a transaction even in manual mode
    exec(&c, "VACUUM t").await;
    c.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    // explicit BEGIN / COMMIT in auto mode
    exec(&c, "BEGIN").await;
    exec(&c, "INSERT INTO t VALUES (4)").await;
    assert!(c.in_transaction());
    exec(&c, "COMMIT").await;
    assert!(!c.in_transaction());
    assert_eq!(exec(&c, "SELECT count(*) FROM t").await.rows[0][0], Value::Int(2));
    // switching Manual -> Auto commits pending work
    c.set_transaction_mode(TransactionMode::Manual).await.unwrap();
    exec(&c, "INSERT INTO t VALUES (5)").await;
    c.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    assert!(!c.in_transaction());
    assert_eq!(exec(&other, "SELECT count(*) FROM t_tx.t").await.rows[0][0], Value::Int(3));
}

#[tokio::test]
async fn utility_statements() {
    let Some(url) = pg_url() else { return };
    let c = with_schema(&url, "t_util").await;
    exec(&c, "SET statement_timeout = '30s'").await;
    let r = exec(&c, "SHOW statement_timeout").await;
    assert_eq!(r.rows[0][0], Value::from("30s"));
    exec(&c, "CREATE TABLE v (a int)").await;
    exec(&c, "VACUUM ANALYZE v").await;
    exec(&c, "LISTEN tabularium_channel").await;
    let r = exec(&c, "NOTIFY tabularium_channel, 'hi'").await;
    assert!(r.notices.iter().any(|n| n.contains("tabularium_channel") && n.contains("\"hi\"")), "{:?}", r.notices);
    // notification from another session arrives while this one runs queries
    let other = connect_cfg(&config(&url)).await;
    exec(&other, "NOTIFY tabularium_channel, 'from other'").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let r = exec(&c, "SELECT 1").await;
    assert_eq!(r.rows[0][0], Value::Int(1));
    exec(&c, "UNLISTEN *").await;
    exec(&c, "DROP DATABASE IF EXISTS tabularium_tmp_db").await;
    exec(&c, "CREATE DATABASE tabularium_tmp_db").await;
    exec(&c, "DROP DATABASE tabularium_tmp_db").await;
    let r = exec(&c, "DO $$ BEGIN RAISE NOTICE 'hello'; END $$").await;
    assert!(r.columns.is_empty());
    let r = exec(&c, "SELECT relacl FROM pg_class WHERE relacl IS NOT NULL LIMIT 1").await;
    assert_eq!(r.columns.len(), 1, "aclitem[] needs the text protocol fallback");
    // DDL then re-run a statement whose result type changed
    exec(&c, "INSERT INTO v VALUES (1)").await;
    exec(&c, "SELECT * FROM v").await;
    exec(&c, "ALTER TABLE v ADD COLUMN b text DEFAULT 'x'").await;
    let r = exec(&c, "SELECT * FROM v").await;
    assert_eq!(r.columns.len(), 2);
    let plan = c.explain("SELECT * FROM v WHERE a = 1", false).await.unwrap();
    assert!(plan.rows.iter().any(|r| r[0].to_string().contains("Seq Scan")), "{:?}", plan.rows);
}

#[tokio::test]
async fn read_only_connection() {
    let Some(url) = pg_url() else { return };
    let mut cfg = config(&url);
    cfg.read_only = true;
    let c = connect_cfg(&cfg).await;
    assert!(matches!(c.execute("CREATE TABLE x (a int)", &ExecOptions::default()).await, Err(Error::ReadOnly(_))));
    assert_eq!(exec(&c, "SHOW transaction_read_only").await.rows[0][0], Value::from("on"));
    // even a sneaky modification through a function is rejected by the server
    let e = c.execute("SELECT nextval('nonexistent_seq')", &ExecOptions::default()).await.unwrap_err();
    assert!(matches!(e, Error::Database(_)));
}

#[tokio::test]
async fn init_sql_and_use_database() {
    let Some(url) = pg_url() else { return };
    let mut cfg = config(&url);
    cfg.init_sql = Some("SET application_name = 'tab_init'; SET search_path = pg_catalog".into());
    let c = connect_cfg(&cfg).await;
    assert_eq!(exec(&c, "SHOW application_name").await.rows[0][0], Value::from("tab_init"));
    let admin = connect_cfg(&config(&url)).await;
    let exists = exec(&admin, "SELECT 1 FROM pg_database WHERE datname = 'tabularium_test_db2'").await;
    if exists.rows.is_empty() {
        let _ = admin.execute("CREATE DATABASE tabularium_test_db2", &ExecOptions::default()).await;
    }
    let dbs = c.list_databases().await.unwrap();
    assert!(dbs.contains(&"tabularium_test_db2".to_string()), "{dbs:?}");
    assert!(!dbs.contains(&"template0".to_string()));
    c.use_database("tabularium_test_db2").await.unwrap();
    assert_eq!(c.current_database().as_deref(), Some("tabularium_test_db2"));
    assert_eq!(exec(&c, "SELECT current_database()").await.rows[0][0], Value::from("tabularium_test_db2"));
    // session config (init_sql) is re-applied after switching
    assert_eq!(exec(&c, "SHOW application_name").await.rows[0][0], Value::from("tab_init"));
    // cancel still targets the new backend
    let h = c.cancel_handle().unwrap();
    let c2 = c.clone();
    let task = tokio::spawn(async move { c2.execute("SELECT pg_sleep(5)", &ExecOptions::default()).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.cancel().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::Cancelled)));
}

const SCHEMA_DDL: &[&str] = &[
    "CREATE TYPE status AS ENUM ('new', 'paid', 'shipped')",
    "CREATE DOMAIN email AS text CHECK (VALUE LIKE '%@%')",
    "CREATE TYPE addr AS (street text, zip varchar(10))",
    "CREATE TABLE customers (
        id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
        email email NOT NULL,
        name text COLLATE \"C\",
        created timestamptz NOT NULL DEFAULT now(),
        CONSTRAINT customers_email_key UNIQUE (email))",
    "COMMENT ON TABLE customers IS 'All customers'",
    "COMMENT ON COLUMN customers.email IS 'Login e-mail'",
    "CREATE TABLE orders (
        id serial PRIMARY KEY,
        customer_id bigint NOT NULL REFERENCES customers(id) ON DELETE CASCADE ON UPDATE RESTRICT,
        status status NOT NULL DEFAULT 'new',
        total numeric(12,2) CHECK (total >= 0),
        qty int NOT NULL DEFAULT 1,
        doubled int GENERATED ALWAYS AS (qty * 2) STORED,
        ship_to addr)",
    "CREATE INDEX orders_customer_idx ON orders (customer_id)",
    "CREATE UNIQUE INDEX orders_big_idx ON orders USING btree (total DESC) WHERE total > 1000",
    "CREATE INDEX orders_qty_expr ON orders ((qty + 1))",
    "CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    "CREATE TRIGGER orders_touch BEFORE INSERT OR UPDATE ON orders FOR EACH ROW EXECUTE FUNCTION touch()",
    "CREATE VIEW big_orders AS SELECT o.id, c.email FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.total > 100",
    "CREATE MATERIALIZED VIEW order_totals AS SELECT customer_id, sum(total) AS total FROM orders GROUP BY 1",
    "CREATE SEQUENCE invoice_seq START 1000 INCREMENT 5",
    "CREATE FUNCTION add(a int, b int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT a + b'",
    "CREATE FUNCTION add(a numeric, b numeric) RETURNS numeric LANGUAGE sql IMMUTABLE AS 'SELECT a + b'",
    "CREATE PROCEDURE noop() LANGUAGE plpgsql AS $$ BEGIN NULL; END $$",
    "CREATE TABLE events (id int, at date NOT NULL) PARTITION BY RANGE (at)",
    "CREATE TABLE events_2024 PARTITION OF events FOR VALUES FROM ('2024-01-01') TO ('2025-01-01')",
];

#[tokio::test]
async fn introspection_and_ddl() {
    let Some(url) = pg_url() else { return };
    let schema = "t_intro";
    let c = with_schema(&url, schema).await;
    for s in SCHEMA_DDL {
        exec(&c, s).await;
    }
    let s = Some(schema);
    let schemas = c.list_schemas().await.unwrap();
    assert!(schemas.contains(&schema.to_string()));
    assert!(!schemas.contains(&"pg_catalog".to_string()));

    let all = [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Sequence,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Trigger,
        ObjectKind::Index,
        ObjectKind::Type,
    ];
    let objs = c.list_objects(s, &all).await.unwrap();
    let has = |k: ObjectKind, n: &str| objs.iter().any(|o| o.object.kind == k && o.object.name == n);
    assert!(has(ObjectKind::Table, "customers"));
    assert!(has(ObjectKind::Table, "events"));
    assert!(!has(ObjectKind::Table, "events_2024"), "partitions are hidden");
    assert!(has(ObjectKind::View, "big_orders"));
    assert!(has(ObjectKind::MaterializedView, "order_totals"));
    assert!(has(ObjectKind::Sequence, "invoice_seq"));
    assert!(has(ObjectKind::Sequence, "orders_id_seq"));
    assert!(has(ObjectKind::Procedure, "noop"));
    assert!(has(ObjectKind::Trigger, "orders_touch"));
    assert!(has(ObjectKind::Index, "orders_customer_idx"));
    assert!(has(ObjectKind::Type, "status"));
    assert!(has(ObjectKind::Type, "email"));
    assert!(has(ObjectKind::Type, "addr"));
    let adds: Vec<_> = objs.iter().filter(|o| o.object.name == "add").collect();
    assert_eq!(adds.len(), 2);
    assert!(adds.iter().any(|o| o.signature.as_deref() == Some("(a integer, b integer)")));
    let cust = objs.iter().find(|o| o.object.name == "customers").unwrap();
    assert_eq!(cust.comment.as_deref(), Some("All customers"));
    let trg = objs.iter().find(|o| o.object.name == "orders_touch").unwrap();
    assert_eq!(trg.parent.as_deref(), Some("orders"));

    // describe customers
    let t = c.describe_table(&ObjectRef::table(s, "customers")).await.unwrap();
    assert_eq!(t.comment.as_deref(), Some("All customers"));
    let id = t.column("id").unwrap();
    assert_eq!(id.data_type, "bigint");
    assert!(id.auto_increment);
    assert!(!id.nullable);
    // types are rendered relative to the session search_path (which includes the schema)
    assert_eq!(t.column("email").unwrap().data_type, "email");
    assert_eq!(t.column("email").unwrap().comment.as_deref(), Some("Login e-mail"));
    assert_eq!(t.column("name").unwrap().collation.as_deref(), Some("C"));
    assert_eq!(t.column("created").unwrap().default.as_deref(), Some("now()"));
    assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["id"]);
    assert_eq!(t.unique_keys[0].columns, vec!["email"]);
    assert_eq!(t.row_identity(), Some(vec!["id".to_string()]));

    // describe orders
    let o = c.describe_table(&ObjectRef::table(s, "orders")).await.unwrap();
    assert_eq!(o.column("id").unwrap().data_type, "integer");
    assert!(o.column("id").unwrap().auto_increment, "serial");
    assert_eq!(o.column("total").unwrap().data_type, "numeric(12,2)");
    assert_eq!(o.column("doubled").unwrap().generated.as_deref(), Some("qty * 2"));
    assert_eq!(o.column("doubled").unwrap().default, None);
    assert_eq!(o.foreign_keys.len(), 1);
    let fk = &o.foreign_keys[0];
    assert_eq!(fk.columns, vec!["customer_id"]);
    assert_eq!(fk.ref_table, "customers");
    assert_eq!(fk.ref_schema.as_deref(), Some(schema));
    assert_eq!(fk.ref_columns, vec!["id"]);
    assert_eq!(fk.on_delete, FkAction::Cascade);
    assert_eq!(fk.on_update, FkAction::Restrict);
    assert_eq!(o.checks.len(), 1);
    assert_eq!(o.checks[0].expression, "total >= 0::numeric");
    let big = o.indexes.iter().find(|i| i.name == "orders_big_idx").unwrap();
    assert!(big.unique);
    assert_eq!(big.method.as_deref(), Some("btree"));
    assert_eq!(big.predicate.as_deref(), Some("(total > (1000)::numeric)"));
    assert_eq!(big.columns, vec!["total"]);
    let expr = o.indexes.iter().find(|i| i.name == "orders_qty_expr").unwrap();
    assert_eq!(expr.columns, vec!["(qty + 1)"]);
    assert!(o.indexes.iter().any(|i| i.primary));
    assert_eq!(o.triggers.len(), 1);
    assert_eq!(o.triggers[0].timing, "BEFORE");
    assert_eq!(o.triggers[0].events, "INSERT OR UPDATE");
    assert!(o.triggers[0].definition.as_deref().unwrap().starts_with("CREATE TRIGGER orders_touch"));

    let v = c.describe_table(&ObjectRef::new(s, "big_orders", ObjectKind::View)).await.unwrap();
    assert_eq!(v.object.kind, ObjectKind::View);
    assert!(v.view_definition.as_deref().unwrap().contains("SELECT"));
    assert_eq!(v.columns.len(), 2);

    // routines
    let r = c.describe_routine(&ObjectRef::new(s, "add", ObjectKind::Function), Some("(a numeric, b numeric)")).await.unwrap();
    assert_eq!(r.return_type.as_deref(), Some("numeric"));
    assert_eq!(r.language.as_deref(), Some("sql"));
    assert!(r.definition.contains("CREATE OR REPLACE FUNCTION"), "{}", r.definition);
    let p = c.describe_routine(&ObjectRef::new(s, "noop", ObjectKind::Procedure), None).await.unwrap();
    assert_eq!(p.object.kind, ObjectKind::Procedure);

    // DDL
    let ddl = c.object_ddl(&ObjectRef::table(s, "orders")).await.unwrap();
    for needle in [
        "CREATE TABLE t_intro.orders (",
        "id serial NOT NULL",
        "doubled integer GENERATED ALWAYS AS (qty * 2) STORED",
        "CONSTRAINT orders_pkey PRIMARY KEY (id)",
        "CONSTRAINT orders_customer_id_fkey FOREIGN KEY (customer_id) REFERENCES customers(id) ON UPDATE RESTRICT ON DELETE CASCADE",
        "CREATE INDEX orders_customer_idx ON t_intro.orders USING btree (customer_id);",
        "CREATE TRIGGER orders_touch BEFORE INSERT OR UPDATE ON orders",
    ] {
        assert!(ddl.contains(needle), "missing `{needle}` in:\n{ddl}");
    }
    assert!(!ddl.contains("CREATE UNIQUE INDEX orders_pkey"), "constraint index duplicated:\n{ddl}");
    let cddl = c.object_ddl(&ObjectRef::table(s, "customers")).await.unwrap();
    assert!(cddl.contains("id bigint GENERATED ALWAYS AS IDENTITY NOT NULL"), "{cddl}");
    assert!(cddl.contains("COMMENT ON TABLE t_intro.customers IS 'All customers';"), "{cddl}");
    assert!(cddl.contains("name text COLLATE \"C\""), "{cddl}");
    let pddl = c.object_ddl(&ObjectRef::table(s, "events")).await.unwrap();
    assert!(pddl.contains("PARTITION BY RANGE (at)"), "{pddl}");
    let part = c.object_ddl(&ObjectRef::table(s, "events_2024")).await.unwrap();
    assert!(part.contains("PARTITION OF t_intro.events"), "{part}");
    let vddl = c.object_ddl(&ObjectRef::new(s, "big_orders", ObjectKind::View)).await.unwrap();
    assert!(vddl.starts_with("CREATE OR REPLACE VIEW t_intro.big_orders AS"), "{vddl}");
    let mddl = c.object_ddl(&ObjectRef::new(s, "order_totals", ObjectKind::MaterializedView)).await.unwrap();
    assert!(mddl.starts_with("CREATE MATERIALIZED VIEW t_intro.order_totals AS"), "{mddl}");
    let sddl = c.object_ddl(&ObjectRef::new(s, "invoice_seq", ObjectKind::Sequence)).await.unwrap();
    assert!(sddl.contains("START WITH 1000") && sddl.contains("INCREMENT BY 5"), "{sddl}");
    let oddl = c.object_ddl(&ObjectRef::new(s, "orders_id_seq", ObjectKind::Sequence)).await.unwrap();
    assert!(oddl.contains("OWNED BY t_intro.orders.id"), "{oddl}");
    let fddl = c.object_ddl(&ObjectRef::new(s, "add", ObjectKind::Function)).await.unwrap();
    assert_eq!(fddl.matches("CREATE OR REPLACE FUNCTION").count(), 2);
    let eddl = c.object_ddl(&ObjectRef::new(s, "status", ObjectKind::Type)).await.unwrap();
    assert_eq!(eddl.trim(), "CREATE TYPE t_intro.status AS ENUM ('new', 'paid', 'shipped');");
    let dddl = c.object_ddl(&ObjectRef::new(s, "email", ObjectKind::Type)).await.unwrap();
    assert!(dddl.starts_with("CREATE DOMAIN t_intro.email AS text"), "{dddl}");
    let addr = c.object_ddl(&ObjectRef::new(s, "addr", ObjectKind::Type)).await.unwrap();
    assert!(addr.contains("zip character varying(10)"), "{addr}");
    let iddl = c.object_ddl(&ObjectRef::new(s, "orders_big_idx", ObjectKind::Index)).await.unwrap();
    assert!(iddl.contains("WHERE (total > (1000)::numeric);"), "{iddl}");
    let tddl = c.object_ddl(&ObjectRef::new(s, "orders_touch", ObjectKind::Trigger)).await.unwrap();
    assert!(tddl.starts_with("CREATE TRIGGER orders_touch"), "{tddl}");
    let schema_ddl = c.object_ddl(&ObjectRef::new(None, schema, ObjectKind::Schema)).await.unwrap();
    assert!(schema_ddl.starts_with("CREATE SCHEMA t_intro AUTHORIZATION"), "{schema_ddl}");

    // DDL round trip: recreate customers + orders from generated DDL in a clean schema
    exec(&c, "DROP TABLE orders CASCADE").await;
    exec(&c, "DROP TABLE customers CASCADE").await;
    for stmt in [cddl.as_str(), ddl.as_str()] {
        exec(&c, stmt).await; // multi-statement string -> simple protocol
    }
    let o2 = c.describe_table(&ObjectRef::table(s, "orders")).await.unwrap();
    assert_eq!(o2.columns, o.columns);
    assert_eq!(o2.foreign_keys, o.foreign_keys);
    assert_eq!(o2.indexes.len(), o.indexes.len());

    // snapshot
    let snap = c.snapshot_schema(s).await.unwrap();
    assert!(snap.table("orders").is_some());
    assert!(snap.table("events").is_some());
    assert!(snap.sequences.iter().any(|q| q.object.name == "invoice_seq"));
    assert!(snap.routines.iter().any(|r| r.object.name == "add"));
}

#[tokio::test]
async fn roles_and_sessions() {
    let Some(url) = pg_url() else { return };
    let c = connect_cfg(&config(&url)).await;
    let roles = c.list_roles().await.unwrap();
    assert!(roles.iter().any(|r| r.superuser && r.can_login), "{roles:?}");
    let victim = connect_cfg(&config(&url)).await;
    let pid = exec(&victim, "SELECT pg_backend_pid()").await.rows[0][0].to_string();
    let sessions = c.list_sessions().await.unwrap();
    assert!(sessions.iter().any(|s| s.id == pid), "{sessions:?}");
    c.kill_session(&pid).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(victim.execute("SELECT 1", &ExecOptions::default()).await.is_err());
    assert!(c.kill_session("999999").await.is_err());
}
