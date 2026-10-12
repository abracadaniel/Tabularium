use super::*;
use crate::ddl::tests::{col, orders, users};
use tabularium_core::FkAction;

fn snap(schema: &str, tables: Vec<TableInfo>) -> SchemaSnapshot {
    SchemaSnapshot { schema: Some(schema.into()), tables, routines: vec![], sequences: vec![] }
}

fn routine(schema: &str, name: &str, body: &str) -> RoutineInfo {
    RoutineInfo {
        object: ObjectRef::new(Some(schema), name, ObjectKind::Function),
        signature: Some("(a integer)".into()),
        return_type: Some("integer".into()),
        language: Some("sql".into()),
        definition: format!("CREATE OR REPLACE FUNCTION {schema}.{name}(a integer) RETURNS integer AS $$ {body} $$ LANGUAGE sql"),
    }
}

#[test]
fn identical_schemas_in_different_schemas_have_no_diff() {
    let mut s = snap("dev", vec![users(Some("dev")), orders(Some("dev"))]);
    s.routines.push(routine("dev", "f", "SELECT a + 1"));
    let mut t = snap("prod", vec![orders(Some("prod")), users(Some("prod"))]);
    t.routines.push(routine("prod", "f", "SELECT a + 1"));
    let d = diff_schemas(&s, &t, &DiffOptions::default());
    assert!(d.is_empty(), "{:#?}", d.changes);
}

#[test]
fn type_spelling_normalisation_has_no_false_positives() {
    let mut a = TableInfo::new(ObjectRef::table(Some("a"), "t"));
    a.columns = vec![
        col("i", "int4", false),
        col("s", "character varying(20)", true),
        ColumnInfo { default: Some("'new'::text".into()), ..col("st", "TEXT", true) },
        col("ts", "timestamp with time zone", true),
        col("b", "bool", true),
        ColumnInfo { default: Some("nextval('t_id_seq'::regclass)".into()), ..col("id", "integer", false) },
        col("d", "double precision", true),
        col("n", "numeric(10, 2)", true),
    ];
    let mut b = TableInfo::new(ObjectRef::table(Some("b"), "t"));
    b.columns = vec![
        col("i", "INTEGER", false),
        col("s", "varchar(20)", true),
        ColumnInfo { default: Some("('new')".into()), ..col("st", "text", true) },
        col("ts", "timestamptz", true),
        col("b", "boolean", true),
        ColumnInfo { auto_increment: true, ..col("id", "serial", false) },
        col("d", "float8", true),
        col("n", "decimal(10,2)", true),
    ];
    a.checks = vec![CheckInfo { name: Some("c".into()), expression: "CHECK ((i > 0))".into() }];
    b.checks = vec![CheckInfo { name: Some("c2".into()), expression: "i > 0".into() }];
    assert!(table_changes(&a, &b, &DiffOptions::default()).is_empty());
}

#[test]
fn detects_column_changes() {
    let src = users(Some("dev"));
    let mut tgt = users(Some("prod"));
    tgt.columns.retain(|c| c.name != "created_at"); // missing in target -> add
    tgt.columns.push(col("legacy", "text", true)); // extra in target -> drop
    tgt.columns[1].data_type = "varchar(100)".into(); // altered
    let changes = table_changes(&tgt, &src, &DiffOptions::default());
    assert_eq!(changes.len(), 3, "{changes:#?}");
    assert!(matches!(&changes[0], TableChange::DropColumn(n) if n == "legacy"));
    assert!(matches!(&changes[1], TableChange::AlterColumn { old, new } if old.data_type == "varchar(100)" && new.data_type == "varchar(255)"));
    assert!(matches!(&changes[2], TableChange::AddColumn(c) if c.name == "created_at"));
    assert_eq!(changes[1].description(), "alter column email: type varchar(100) -> varchar(255)");
}

#[test]
fn detects_index_fk_pk_and_comment_changes() {
    let src = orders(Some("dev"));
    let mut tgt = orders(Some("prod"));
    tgt.foreign_keys[0].on_delete = FkAction::NoAction;
    tgt.indexes.push(IndexInfo { name: "idx_total".into(), columns: vec!["total".into()], ..Default::default() });
    tgt.primary_key = None;
    tgt.comment = Some("old".into());
    let changes = table_changes(&tgt, &src, &DiffOptions::default());
    let kinds: Vec<String> = changes.iter().map(TableChange::description).collect();
    assert_eq!(
        kinds,
        vec![
            "add primary key orders_pkey (id)",
            "drop index idx_total",
            "drop foreign key orders_user_id_fkey (user_id) -> users",
            "add foreign key orders_user_id_fkey (user_id) -> users",
            "change comment",
        ]
    );
    let opts = DiffOptions { compare_indexes: false, ignore_comments: true, ..Default::default() };
    let changes = table_changes(&tgt, &src, &opts);
    assert_eq!(changes.len(), 3);
}

#[test]
fn ignore_case_option() {
    let src = users(Some("dev"));
    let mut tgt = users(Some("prod"));
    tgt.object.name = "USERS".into();
    for c in &mut tgt.columns {
        c.name = c.name.to_uppercase();
    }
    let strict = diff_schemas(&snap("dev", vec![src.clone()]), &snap("prod", vec![tgt.clone()]), &DiffOptions::default());
    assert_eq!(strict.changes.len(), 2); // drop USERS, create users
    let opts = DiffOptions { ignore_case: true, ..Default::default() };
    let lax = diff_schemas(&snap("dev", vec![src]), &snap("prod", vec![tgt]), &opts);
    assert!(lax.changes.iter().all(|c| !matches!(c, SchemaChange::CreateTable(_) | SchemaChange::DropTable(_))), "{:#?}", lax.changes);
}

#[test]
fn schema_level_changes_and_migration_order() {
    let mut src = snap("dev", vec![users(Some("dev")), orders(Some("dev"))]);
    let mut v = TableInfo::new(ObjectRef::new(Some("dev"), "big_orders", ObjectKind::View));
    v.view_definition = Some("SELECT * FROM dev.orders WHERE total > 100".into());
    src.tables.push(v);
    src.routines.push(routine("dev", "f", "SELECT a + 2"));
    src.routines.push(routine("dev", "g", "SELECT a"));

    let mut legacy = TableInfo::new(ObjectRef::table(Some("prod"), "legacy"));
    legacy.columns = vec![col("id", "int", false), col("user_id", "int", true)];
    legacy.foreign_keys = vec![ForeignKeyInfo {
        name: Some("legacy_user".into()),
        columns: vec!["user_id".into()],
        ref_schema: Some("prod".into()),
        ref_table: "users".into(),
        ref_columns: vec!["id".into()],
        ..Default::default()
    }];
    let mut legacy_child = TableInfo::new(ObjectRef::table(Some("prod"), "legacy_child"));
    legacy_child.columns = vec![col("legacy_id", "int", false)];
    legacy_child.foreign_keys = vec![ForeignKeyInfo {
        name: Some("lc_legacy".into()),
        columns: vec!["legacy_id".into()],
        ref_table: "legacy".into(),
        ref_columns: vec!["id".into()],
        ..Default::default()
    }];
    let mut tgt_users = users(Some("prod"));
    tgt_users.columns.retain(|c| c.name != "created_at");
    let mut old_view = TableInfo::new(ObjectRef::new(Some("prod"), "old_view", ObjectKind::View));
    old_view.view_definition = Some("SELECT 1".into());
    let mut tgt = snap("prod", vec![legacy, tgt_users, legacy_child, old_view]);
    tgt.routines.push(routine("prod", "f", "SELECT a + 1"));
    tgt.routines.push(routine("prod", "h", "SELECT 0"));

    let d = diff_schemas(&src, &tgt, &DiffOptions::default());
    let desc: Vec<String> = d.changes.iter().map(SchemaChange::description).collect();
    assert_eq!(
        desc,
        vec![
            "Drop table prod.legacy_child",
            "Drop table prod.legacy",
            "Create table prod.orders",
            "Alter table prod.users: add column created_at timestamp",
            "Drop view prod.old_view",
            "Create view prod.big_orders",
            "Drop function prod.h",
            "Replace function dev.f",
            "Create function dev.g",
        ]
    );
    assert_eq!(d.changes[0].object_name(), "prod.legacy_child");

    let script = migration_script(&d, Dialect::Postgres, Some("prod"));
    let pos = |needle: &str| script.iter().position(|s| s.starts_with(needle)).unwrap_or_else(|| panic!("{needle}: {script:#?}"));
    let exact = |needle: &str| script.iter().position(|s| s == needle).unwrap_or_else(|| panic!("{needle}: {script:#?}"));
    assert!(pos("DROP FUNCTION prod.h(a integer)") < pos("DROP VIEW prod.old_view"));
    assert!(pos("DROP VIEW prod.old_view") < exact("DROP TABLE prod.legacy_child"));
    assert!(exact("DROP TABLE prod.legacy_child") < exact("DROP TABLE prod.legacy"));
    assert!(exact("DROP TABLE prod.legacy") < pos("CREATE TABLE prod.orders"));
    // FK of the new table is added after creates/alters, retargeted to prod
    let create = &script[pos("CREATE TABLE prod.orders")];
    assert!(!create.contains("FOREIGN KEY"));
    assert!(pos("ALTER TABLE prod.users ADD COLUMN created_at") < pos("ALTER TABLE prod.orders ADD CONSTRAINT orders_user_id_fkey"));
    assert!(script[pos("ALTER TABLE prod.orders ADD CONSTRAINT")].contains("REFERENCES prod.users (id)"));
    assert!(pos("ALTER TABLE prod.orders ADD CONSTRAINT") < pos("CREATE VIEW prod.big_orders"));
    // CREATE OR REPLACE definitions are emitted without a drop
    assert!(script.iter().all(|s| !s.starts_with("DROP FUNCTION prod.f")));
    assert!(script.last().unwrap().contains("dev.g"));
}

#[test]
fn migration_fk_drops_come_first_and_views_replace() {
    let src = snap("s", vec![users(Some("s")), {
        let mut o = orders(Some("s"));
        o.foreign_keys.clear();
        o
    }]);
    let mut tv = TableInfo::new(ObjectRef::new(Some("t"), "v", ObjectKind::View));
    tv.view_definition = Some("SELECT 1".into());
    let mut sv = tv.clone();
    sv.object.schema = Some("s".into());
    sv.view_definition = Some("SELECT 2".into());
    let mut src = src;
    src.tables.push(sv);
    let mut tgt_users = users(Some("t"));
    tgt_users.columns.retain(|c| c.name != "name");
    let tgt = snap("t", vec![tgt_users, orders(Some("t")), tv]);
    let d = diff_schemas(&src, &tgt, &DiffOptions::default());
    let script = migration_script(&d, Dialect::MySql, None);
    assert_eq!(
        script,
        vec![
            "ALTER TABLE t.orders DROP FOREIGN KEY orders_user_id_fkey",
            "ALTER TABLE t.users ADD COLUMN name text DEFAULT 'anon'",
            "CREATE OR REPLACE VIEW t.v AS\nSELECT 2",
        ]
    );
    let script = migration_script(&d, Dialect::Sqlite, None);
    assert!(script.contains(&"DROP VIEW t.v".to_string()), "{script:#?}");
}

#[test]
fn sqlite_migration_uses_rebuild_statements() {
    let src = snap("main", vec![users(Some("main"))]);
    let mut tu = users(Some("main"));
    tu.columns[2].data_type = "varchar(5)".into();
    let tgt = snap("main", vec![tu]);
    let d = diff_schemas_for_dialect(&src, &tgt, &DiffOptions::default(), Dialect::Sqlite);
    let script = migration_script(&d, Dialect::Sqlite, None);
    assert_eq!(script[0], "PRAGMA foreign_keys = OFF");
    assert!(script.iter().any(|s| s == "ALTER TABLE _new_users RENAME TO users"));
}

#[test]
fn migration_does_not_reuse_source_sequences() {
    let mut src = SchemaSnapshot { schema: Some("a".into()), ..Default::default() };
    let mut t = TableInfo::new(ObjectRef::table(Some("a"), "items"));
    t.columns = vec![ColumnInfo {
        name: "id".into(),
        data_type: "bigint".into(),
        nullable: false,
        default: Some("nextval('a.items_id_seq'::regclass)".into()),
        auto_increment: true,
        ordinal: 1,
        ..Default::default()
    }];
    src.tables.push(t);
    let dst = SchemaSnapshot { schema: Some("b".into()), ..Default::default() };
    let script = migration_script(&diff_schemas(&src, &dst, &DiffOptions::default()), Dialect::Postgres, Some("b"));
    let joined = script.join("\n");
    assert!(!joined.contains("nextval"), "{joined}");
    assert!(joined.contains("GENERATED BY DEFAULT AS IDENTITY"), "{joined}");
}
