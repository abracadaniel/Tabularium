//! MySQL / MariaDB integration tests. Set `TABULARIUM_TEST_MYSQL_URL`, e.g.
//! `mysql://root@127.0.0.1:33069/tabtest`; tests are skipped otherwise.
//! Each test uses its own database.

#![cfg(feature = "mysql")]

use std::sync::Arc;
use std::time::Duration;

use tabularium_core::{
    Connection, ConnectionConfig, Error, ExecOptions, FkAction, ObjectKind, ObjectRef, StatementResult,
    TransactionMode, Value,
};

fn my_url() -> Option<String> {
    match std::env::var("TABULARIUM_TEST_MYSQL_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            eprintln!("TABULARIUM_TEST_MYSQL_URL not set; skipping MySQL test");
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
    let Some(url) = my_url() else { return };
    let v = tabularium_drivers::test_connection(&ConnectionConfig::from_url(&url).unwrap()).await.unwrap();
    assert!(v.starts_with("MariaDB") || v.starts_with("MySQL"), "{v}");
    let c = with_db(&url, "tab_types").await;
    assert_eq!(c.current_database().as_deref(), Some("tab_types"));
    assert_eq!(c.current_schema().await.unwrap().as_deref(), Some("tab_types"));
    exec(
        &c,
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, ti TINYINT, u BIGINT UNSIGNED, d DECIMAL(10,2), \
         f FLOAT, db DOUBLE, s VARCHAR(20), tx TEXT, b VARBINARY(10), bl BLOB, dt DATE, dtm DATETIME(3), ts TIMESTAMP NULL, \
         tm TIME, y YEAR, e ENUM('a','b'), st SET('x','y'), bit1 BIT(4), j JSON, zero DATETIME NULL)",
    )
    .await;
    let r = c
        .execute(
            "INSERT INTO t (ti, u, d, f, db, s, tx, b, bl, dt, dtm, ts, tm, y, e, st, bit1, j) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, b'1010', ?)",
            &ExecOptions {
                max_rows: None,
                params: vec![
                    Value::Int(-5),
                    Value::UInt(18_000_000_000_000_000_000),
                    Value::Decimal("12.34".into()),
                    Value::Float(1.5),
                    Value::Float(2.25),
                    Value::from("héllo"),
                    Value::from("long text"),
                    Value::Bytes(vec![0, 1, 255]),
                    Value::Bytes(vec![0x89, b'P', b'N', b'G', 0]),
                    Value::from("2024-01-02"),
                    Value::from("2024-01-02 03:04:05.678"),
                    Value::from("2024-01-02 03:04:05"),
                    Value::from("-12:00:01"),
                    Value::Int(2024),
                    Value::from("b"),
                    Value::from("x,y"),
                    Value::from("{\"k\": [1, 2]}"),
                ],
            },
        )
        .await
        .unwrap();
    assert_eq!(r.rows_affected, Some(1));
    let r = exec(&c, "SELECT * FROM t").await;
    let v = |n: &str| r.rows[0][r.column_index(n).unwrap()].clone();
    assert_eq!(v("id"), Value::Int(1));
    assert_eq!(v("ti"), Value::Int(-5));
    assert_eq!(v("u"), Value::UInt(18_000_000_000_000_000_000));
    assert_eq!(v("d"), Value::Decimal("12.34".into()));
    assert_eq!(v("f"), Value::Float(1.5));
    assert_eq!(v("db"), Value::Float(2.25));
    assert_eq!(v("s"), Value::from("héllo"));
    assert_eq!(v("tx"), Value::from("long text"));
    assert_eq!(v("b"), Value::Bytes(vec![0, 1, 255]));
    assert_eq!(v("bl"), Value::Bytes(vec![0x89, b'P', b'N', b'G', 0]));
    assert_eq!(v("dt").to_string(), "2024-01-02");
    assert_eq!(v("dtm").to_string(), "2024-01-02 03:04:05.678");
    assert_eq!(v("ts").to_string(), "2024-01-02 03:04:05");
    assert_eq!(v("tm").to_string(), "-12:00:01");
    assert_eq!(v("y"), Value::Int(2024));
    assert_eq!(v("e"), Value::from("b"));
    assert_eq!(v("st"), Value::from("x,y"));
    assert_eq!(v("bit1"), Value::UInt(10));
    assert_eq!(v("zero"), Value::Null);
    match v("j") {
        Value::Json(j) => assert_eq!(j, serde_json::json!({"k": [1, 2]})),
        Value::Text(t) => assert!(t.contains("\"k\"")), // MariaDB: JSON is LONGTEXT
        other => panic!("{other:?}"),
    }
    // text protocol (multi-statement) decodes the same way
    let r2 = exec(&c, "SELECT 1; SELECT * FROM t").await;
    assert_eq!(r2.rows, r.rows);
    // zero rows still have columns
    let r = c.execute("SELECT id, s FROM t WHERE 1 = 0", &ExecOptions::default()).await.unwrap();
    assert_eq!(r.columns.len(), 2);
    // zero dates
    exec(&c, "SET SESSION sql_mode = ''").await;
    exec(&c, "INSERT INTO t (zero) VALUES ('0000-00-00 00:00:00')").await;
    let r = exec(&c, "SELECT zero FROM t WHERE id = 2").await;
    assert_eq!(r.rows[0][0].to_string(), "0000-00-00 00:00:00");
}

#[tokio::test]
async fn truncation_errors_and_commands() {
    let Some(url) = my_url() else { return };
    let c = with_db(&url, "tab_trunc").await;
    exec(&c, "CREATE TABLE n (x INT)").await;
    // recursive CTE works on both MySQL 8 and MariaDB (MariaDB's seq engine is not portable)
    exec(&c, "INSERT INTO n WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM s WHERE x < 500) SELECT x FROM s").await;
    let r = c.execute("SELECT x FROM n ORDER BY x", &ExecOptions::with_limit(10)).await.unwrap();
    assert_eq!(r.rows.len(), 10);
    assert!(r.truncated);
    let r = c.execute("SELECT count(*) FROM n", &ExecOptions::with_limit(10)).await.unwrap();
    assert_eq!(r.rows[0][0], Value::Int(500), "limit must be reset for other queries");
    match c.execute("SELECT * FROM nope", &ExecOptions::default()).await.unwrap_err() {
        Error::Database(d) => assert!(d.code.as_deref().unwrap().starts_with("1146"), "{d}"),
        other => panic!("{other:?}"),
    }
    // commands that cannot be prepared
    exec(&c, "LOCK TABLES n READ").await;
    exec(&c, "UNLOCK TABLES").await;
    exec(&c, "SHOW TABLES").await;
    exec(&c, "SET @a = 5").await;
    assert_eq!(exec(&c, "SELECT @a").await.rows[0][0], Value::Int(5));
    let plan = c.explain("SELECT * FROM n WHERE x = 1", false).await.unwrap();
    assert!(!plan.rows.is_empty());
}

#[tokio::test]
async fn transactions_and_cancel() {
    let Some(url) = my_url() else { return };
    let c = with_db(&url, "tab_tx").await;
    exec(&c, "CREATE TABLE t (a INT) ENGINE=InnoDB").await;
    c.set_transaction_mode(TransactionMode::Manual).await.unwrap();
    exec(&c, "INSERT INTO t VALUES (1)").await;
    assert!(c.in_transaction());
    c.rollback().await.unwrap();
    assert!(!c.in_transaction());
    exec(&c, "INSERT INTO t VALUES (2)").await;
    c.commit().await.unwrap();
    c.set_transaction_mode(TransactionMode::Auto).await.unwrap();
    assert_eq!(exec(&c, "SELECT a FROM t").await.rows, vec![vec![Value::Int(2)]]);

    let h = c.cancel_handle().unwrap();
    let c2 = c.clone();
    let start = std::time::Instant::now();
    let task = tokio::spawn(async move { c2.execute("SELECT SLEEP(10)", &ExecOptions::default()).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    h.cancel().await.unwrap();
    let res = tokio::time::timeout(Duration::from_secs(5), task).await.expect("cancel too slow").unwrap();
    // SLEEP() returns 1 when interrupted; other statements fail with ER_QUERY_INTERRUPTED.
    assert!(matches!(&res, Ok(_) | Err(Error::Cancelled)), "{res:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
    exec(&c, "SELECT 1").await;
}

#[tokio::test]
async fn introspection_and_ddl() {
    let Some(url) = my_url() else { return };
    let c = with_db(&url, "tab_intro").await;
    for s in [
        "CREATE TABLE customers (id BIGINT AUTO_INCREMENT PRIMARY KEY, email VARCHAR(100) NOT NULL UNIQUE, \
         name VARCHAR(50) DEFAULT 'anon' COMMENT 'display name', created TIMESTAMP DEFAULT CURRENT_TIMESTAMP) \
         COMMENT 'All customers'",
        "CREATE TABLE orders (id INT AUTO_INCREMENT PRIMARY KEY, customer_id BIGINT NOT NULL, total DECIMAL(10,2), \
         qty INT NOT NULL DEFAULT 1, doubled INT AS (qty * 2) VIRTUAL, \
         CONSTRAINT fk_cust FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE CASCADE, \
         CONSTRAINT total_pos CHECK (total >= 0), KEY idx_total (total)) ENGINE=InnoDB",
        "CREATE VIEW big_orders AS SELECT id, total FROM orders WHERE total > 100",
        "CREATE PROCEDURE noop(IN a INT, OUT b INT) BEGIN SET b = a; END",
        "CREATE FUNCTION twice(x INT) RETURNS INT DETERMINISTIC RETURN x * 2",
        "CREATE TRIGGER orders_bi BEFORE INSERT ON orders FOR EACH ROW SET NEW.qty = GREATEST(NEW.qty, 1)",
    ] {
        exec(&c, s).await;
    }
    let schemas = c.list_schemas().await.unwrap();
    assert!(schemas.contains(&"tab_intro".to_string()));
    assert!(!schemas.contains(&"mysql".to_string()));
    let objs = c
        .list_objects(
            None,
            &[ObjectKind::Table, ObjectKind::View, ObjectKind::Procedure, ObjectKind::Function, ObjectKind::Trigger, ObjectKind::Index],
        )
        .await
        .unwrap();
    let has = |k: ObjectKind, n: &str| objs.iter().any(|o| o.object.kind == k && o.object.name == n);
    assert!(has(ObjectKind::Table, "customers"));
    assert!(has(ObjectKind::View, "big_orders"));
    assert!(has(ObjectKind::Procedure, "noop"));
    assert!(has(ObjectKind::Function, "twice"));
    assert!(has(ObjectKind::Trigger, "orders_bi"));
    assert!(has(ObjectKind::Index, "idx_total"));
    let noop = objs.iter().find(|o| o.object.name == "noop").unwrap();
    assert_eq!(noop.signature.as_deref().map(|s| s.replace("(11)", "")), Some("(IN a int, OUT b int)".into()));
    assert_eq!(objs.iter().find(|o| o.object.name == "customers").unwrap().comment.as_deref(), Some("All customers"));

    let t = c.describe_table(&ObjectRef::table(Some("tab_intro"), "customers")).await.unwrap();
    assert_eq!(t.comment.as_deref(), Some("All customers"));
    assert!(t.column("id").unwrap().auto_increment);
    assert!(matches!(t.column("id").unwrap().data_type.as_str(), "bigint" | "bigint(20)"));
    assert_eq!(t.column("name").unwrap().default.as_deref(), Some("'anon'"));
    assert_eq!(t.column("name").unwrap().comment.as_deref(), Some("display name"));
    assert!(t.column("created").unwrap().default.as_deref().unwrap().to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP"));
    assert_eq!(t.primary_key.as_ref().unwrap().columns, vec!["id"]);
    assert_eq!(t.unique_keys[0].columns, vec!["email"]);

    let o = c.describe_table(&ObjectRef::table(None, "orders")).await.unwrap();
    assert_eq!(o.column("doubled").unwrap().generated.as_deref().map(|g| g.replace('`', "")), Some("qty * 2".into()));
    assert_eq!(o.foreign_keys.len(), 1);
    assert_eq!(o.foreign_keys[0].ref_table, "customers");
    assert_eq!(o.foreign_keys[0].ref_columns, vec!["id"]);
    assert_eq!(o.foreign_keys[0].on_delete, FkAction::Cascade);
    assert!(o.checks.iter().any(|ch| ch.name.as_deref() == Some("total_pos")), "{:?}", o.checks);
    assert!(o.indexes.iter().any(|i| i.name == "idx_total" && i.columns == vec!["total"]));
    assert_eq!(o.triggers.len(), 1);
    assert_eq!(o.triggers[0].timing, "BEFORE");
    assert_eq!(o.triggers[0].events, "INSERT");
    assert!(o.triggers[0].definition.as_deref().unwrap().contains("CREATE"));

    let v = c.describe_table(&ObjectRef::new(None, "big_orders", ObjectKind::View)).await.unwrap();
    assert_eq!(v.object.kind, ObjectKind::View);
    assert!(v.view_definition.is_some());

    let ddl = c.object_ddl(&ObjectRef::table(None, "orders")).await.unwrap();
    assert!(ddl.starts_with("CREATE TABLE `orders`"), "{ddl}");
    assert!(ddl.contains("CREATE DEFINER") && ddl.contains("TRIGGER"), "{ddl}");
    let vddl = c.object_ddl(&ObjectRef::new(None, "big_orders", ObjectKind::View)).await.unwrap();
    assert!(vddl.contains("VIEW `big_orders`"), "{vddl}");
    let pddl = c.object_ddl(&ObjectRef::new(None, "noop", ObjectKind::Procedure)).await.unwrap();
    assert!(pddl.contains("PROCEDURE `noop`"), "{pddl}");
    let fddl = c.object_ddl(&ObjectRef::new(None, "twice", ObjectKind::Function)).await.unwrap();
    assert!(fddl.contains("FUNCTION `twice`"), "{fddl}");
    let iddl = c.object_ddl(&ObjectRef::new(None, "idx_total", ObjectKind::Index)).await.unwrap();
    assert!(iddl.contains("CREATE INDEX `idx_total` ON tab_intro.orders (`total`);"), "{iddl}");
    let r = c.describe_routine(&ObjectRef::new(None, "twice", ObjectKind::Function), None).await.unwrap();
    // MariaDB / MySQL 5.x report display widths, MySQL 8 does not
    assert!(matches!(r.return_type.as_deref(), Some("int" | "int(11)")), "{:?}", r.return_type);
    assert!(r.definition.contains("RETURN"));

    let snap = c.snapshot_schema(None).await.unwrap();
    assert!(snap.table("orders").is_some());

    let sessions = c.list_sessions().await.unwrap();
    assert!(!sessions.is_empty());
    let roles = c.list_roles().await.unwrap();
    assert!(roles.iter().any(|r| r.name.starts_with("root@")));
}

#[tokio::test]
async fn read_only() {
    let Some(url) = my_url() else { return };
    let mut cfg = ConnectionConfig::from_url(&url).unwrap();
    cfg.read_only = true;
    let c = tabularium_drivers::connect(&cfg).await.unwrap();
    assert!(matches!(c.execute("DROP DATABASE x", &ExecOptions::default()).await, Err(Error::ReadOnly(_))));
    exec(&c, "SELECT 1").await;
}
