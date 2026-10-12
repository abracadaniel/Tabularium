//! DDL generation: `CREATE TABLE`/`VIEW` scripts, `DROP`/`TRUNCATE`, whole-schema scripts,
//! `ALTER TABLE` scripts transforming one table definition into another, and type mapping
//! between dialects.
//!
//! All generated statements quote identifiers with [`Dialect::quote_ident_if_needed`].
//! Scripts ([`create_table_ddl`], [`snapshot_ddl`]) terminate every statement with the
//! dialect's [`Dialect::batch_separator`] (`";\n"`, or `"\nGO\n"` for SQL Server), while
//! functions returning `Vec<String>` return bare statements without terminators.

use std::collections::{HashMap, HashSet};

use tabularium_core::{
    CheckInfo, ColumnInfo, Dialect, FkAction, ForeignKeyInfo, IndexInfo, KeyInfo, ObjectKind,
    ObjectRef, SchemaSnapshot, TableInfo,
};

use crate::diff::{table_changes, DiffOptions, TableChange};
use crate::util::{
    ident_list, is_integer_base, is_nextval, is_serial_type, paren, parse_type, q, qname,
    strip_outer_parens,
};

// ---------------------------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------------------------

/// Join bare statements into a script, terminating each with the dialect's batch separator.
pub fn script(statements: &[String], dialect: Dialect) -> String {
    let sep = dialect.batch_separator();
    let mut out = String::new();
    for s in statements {
        let s = s.trim_end();
        let s = if dialect == Dialect::MsSql { s } else { s.trim_end_matches(';').trim_end() };
        if s.is_empty() {
            continue;
        }
        out.push_str(s);
        out.push_str(sep);
    }
    out
}

/// `CREATE TABLE` (or `CREATE VIEW` when `table.view_definition` is set) followed by
/// `CREATE INDEX` for indexes that do not back a constraint, comments and triggers.
pub fn create_table_ddl(table: &TableInfo, dialect: Dialect) -> String {
    script(&create_table_statements(table, dialect), dialect)
}

/// The bare statements behind [`create_table_ddl`].
pub fn create_table_statements(table: &TableInfo, dialect: Dialect) -> Vec<String> {
    if is_view(table) {
        let mut out = vec![create_view_statement(table, dialect, false)];
        out.extend(comment_statements(table, dialect, false));
        return out;
    }
    let mut out = vec![create_table_statement(table, dialect, true)];
    out.extend(table_extras(table, dialect));
    out
}

/// Indexes, comments and triggers following a `CREATE TABLE`.
fn table_extras(table: &TableInfo, dialect: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    for idx in plain_indexes(table) {
        out.push(index_ddl(idx, &table.object, dialect));
    }
    out.extend(comment_statements(table, dialect, true));
    for trg in &table.triggers {
        if let Some(def) = trg.definition.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            out.push(def.to_string());
        }
    }
    out
}

pub(crate) fn is_view(table: &TableInfo) -> bool {
    table.view_definition.is_some()
        || matches!(table.object.kind, ObjectKind::View | ObjectKind::MaterializedView)
}

fn create_view_statement(table: &TableInfo, d: Dialect, replace: bool) -> String {
    let body = table.view_definition.as_deref().unwrap_or("").trim().trim_end_matches(';').trim();
    let materialized = table.object.kind == ObjectKind::MaterializedView;
    let upper = body.to_ascii_uppercase();
    if upper.starts_with("CREATE ") {
        // Full statement supplied by the server (SQL Server, SQLite, MySQL SHOW CREATE VIEW).
        return body.to_string();
    }
    let kw = match (replace, d, materialized) {
        (_, Dialect::Postgres, true) => "CREATE MATERIALIZED VIEW",
        (true, Dialect::MsSql, _) => "CREATE OR ALTER VIEW",
        (true, Dialect::Postgres | Dialect::MySql | Dialect::Generic, _) => "CREATE OR REPLACE VIEW",
        _ => "CREATE VIEW",
    };
    format!("{kw} {} AS\n{body}", qname(&table.object, d))
}

/// The `CREATE TABLE` statement only. `with_fks` controls whether foreign keys are inlined.
fn create_table_statement(table: &TableInfo, d: Dialect, with_fks: bool) -> String {
    create_table_statement_filtered(table, d, &|_| with_fks)
}

fn create_table_statement_filtered(
    table: &TableInfo,
    d: Dialect,
    include_fk: &dyn Fn(&ForeignKeyInfo) -> bool,
) -> String {
    let sqlite_inline = sqlite_inline_pk(table, d);
    let mut parts: Vec<String> = table
        .columns
        .iter()
        .map(|c| column_definition(c, d, sqlite_inline.as_deref() == Some(c.name.as_str())))
        .collect();
    if let Some(pk) = table.primary_key.as_ref().filter(|pk| !pk.columns.is_empty()) {
        if sqlite_inline.is_none() {
            parts.push(primary_key_clause(pk, d));
        }
    }
    for uk in &table.unique_keys {
        if !uk.columns.is_empty() {
            parts.push(unique_clause(uk, d));
        }
    }
    for ck in &table.checks {
        parts.push(check_clause(ck, d));
    }
    for fk in &table.foreign_keys {
        if include_fk(fk) {
            parts.push(foreign_key_constraint(fk, d));
        }
    }
    let mut s = format!("CREATE TABLE {} (\n", qname(&table.object, d));
    s.push_str(&parts.iter().map(|p| format!("    {p}")).collect::<Vec<_>>().join(",\n"));
    s.push_str("\n)");
    if d == Dialect::MySql {
        if let Some(c) = table.comment.as_deref().filter(|c| !c.is_empty()) {
            s.push_str(&format!(" COMMENT = {}", d.quote_string(c)));
        }
    }
    s
}

/// Name of the column that becomes SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT`.
fn sqlite_inline_pk(table: &TableInfo, d: Dialect) -> Option<String> {
    if d != Dialect::Sqlite {
        return None;
    }
    let pk = table.primary_key.as_ref()?;
    if pk.columns.len() != 1 {
        return None;
    }
    let col = table.column(&pk.columns[0])?;
    let p = parse_type(&col.data_type);
    (col.auto_increment && is_integer_base(&p.base)).then(|| col.name.clone())
}

/// Postgres: identity syntax is needed (not already expressed by a serial type or nextval default).
fn pg_identity(col: &ColumnInfo) -> bool {
    col.auto_increment && !is_serial_type(&col.data_type) && !is_nextval(col.default.as_deref())
}

/// A single column definition as used inside `CREATE TABLE` / `ADD COLUMN`.
///
/// `sqlite_inline_pk` renders SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT`.
pub fn column_definition(col: &ColumnInfo, d: Dialect, sqlite_inline_pk: bool) -> String {
    let mut s = q(d, &col.name);
    s.push(' ');
    if sqlite_inline_pk {
        s.push_str("INTEGER PRIMARY KEY AUTOINCREMENT");
        return s;
    }
    let collate = col
        .collation
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| format!(" COLLATE {}", q(d, c)))
        .unwrap_or_default();
    if let Some(expr) = col.generated.as_deref().filter(|e| !e.trim().is_empty()) {
        if d == Dialect::MsSql {
            s.push_str(&format!("AS {} PERSISTED", paren(expr)));
            return s;
        }
        s.push_str(col.data_type.trim());
        s.push_str(&collate);
        s.push_str(&format!(" GENERATED ALWAYS AS {} STORED", paren(expr)));
        if !col.nullable {
            s.push_str(" NOT NULL");
        }
        push_mysql_comment(&mut s, col, d);
        return s;
    }
    s.push_str(col.data_type.trim());
    s.push_str(&collate);
    let mut skip_default = false;
    match d {
        Dialect::Postgres | Dialect::Generic => {
            if pg_identity(col) {
                s.push_str(" GENERATED BY DEFAULT AS IDENTITY");
                skip_default = true;
            } else if is_serial_type(&col.data_type) && is_nextval(col.default.as_deref()) {
                skip_default = true;
            }
        }
        Dialect::MsSql if col.auto_increment && !parse_type(&col.data_type).auto => {
            s.push_str(" IDENTITY(1,1)");
            skip_default = true;
        }
        Dialect::MySql if col.auto_increment => skip_default = true,
        _ => {}
    }
    if !col.nullable {
        s.push_str(" NOT NULL");
    }
    if !skip_default {
        if let Some(def) = col.default.as_deref().filter(|d| !d.trim().is_empty()) {
            s.push_str(" DEFAULT ");
            s.push_str(def.trim());
        }
    }
    if d == Dialect::MySql && col.auto_increment && !parse_type(&col.data_type).auto {
        s.push_str(" AUTO_INCREMENT");
    }
    push_mysql_comment(&mut s, col, d);
    s
}

fn push_mysql_comment(s: &mut String, col: &ColumnInfo, d: Dialect) {
    if d == Dialect::MySql {
        if let Some(c) = col.comment.as_deref().filter(|c| !c.is_empty()) {
            s.push_str(&format!(" COMMENT {}", d.quote_string(c)));
        }
    }
}

fn constraint_prefix(name: Option<&str>, d: Dialect) -> String {
    match name.filter(|n| !n.is_empty()) {
        Some(n) => format!("CONSTRAINT {} ", q(d, n)),
        None => String::new(),
    }
}

/// `CONSTRAINT pk PRIMARY KEY (a, b)`.
pub fn primary_key_clause(pk: &KeyInfo, d: Dialect) -> String {
    // MySQL always names the primary key PRIMARY and rejects other names silently.
    let name = if d == Dialect::MySql { None } else { pk.name.as_deref() };
    format!("{}PRIMARY KEY ({})", constraint_prefix(name, d), ident_list(d, &pk.columns))
}

/// `CONSTRAINT uk UNIQUE (a, b)`.
pub fn unique_clause(uk: &KeyInfo, d: Dialect) -> String {
    format!("{}UNIQUE ({})", constraint_prefix(uk.name.as_deref(), d), ident_list(d, &uk.columns))
}

/// `CONSTRAINT ck CHECK (expr)`. A leading `CHECK` keyword in the stored expression is tolerated.
pub fn check_clause(ck: &CheckInfo, d: Dialect) -> String {
    let mut e = ck.expression.trim();
    if e.len() >= 5 && e[..5].eq_ignore_ascii_case("check") {
        e = e[5..].trim_start();
    }
    let e = e.trim_end_matches(" NOT VALID").trim();
    format!("{}CHECK {}", constraint_prefix(ck.name.as_deref(), d), paren(e))
}

/// `FOREIGN KEY (a) REFERENCES s.t (b) ON DELETE CASCADE` (no constraint name).
pub fn foreign_key_clause(fk: &ForeignKeyInfo, d: Dialect) -> String {
    let target = match (d, fk.ref_schema.as_deref()) {
        // SQLite does not allow schema-qualified FK targets.
        (Dialect::Sqlite, _) | (_, None) => q(d, &fk.ref_table),
        (_, Some("")) => q(d, &fk.ref_table),
        (_, Some(s)) => format!("{}.{}", q(d, s), q(d, &fk.ref_table)),
    };
    let mut s = format!(
        "FOREIGN KEY ({}) REFERENCES {target} ({})",
        ident_list(d, &fk.columns),
        ident_list(d, &fk.ref_columns)
    );
    let action = |a: FkAction| -> Option<&'static str> {
        match (a, d) {
            (FkAction::NoAction, _) => None,
            // SQL Server has no RESTRICT; NO ACTION is equivalent.
            (FkAction::Restrict, Dialect::MsSql) => None,
            (a, _) => Some(a.sql()),
        }
    };
    if let Some(a) = action(fk.on_delete) {
        s.push_str(&format!(" ON DELETE {a}"));
    }
    if let Some(a) = action(fk.on_update) {
        s.push_str(&format!(" ON UPDATE {a}"));
    }
    s
}

fn foreign_key_constraint(fk: &ForeignKeyInfo, d: Dialect) -> String {
    format!("{}{}", constraint_prefix(fk.name.as_deref(), d), foreign_key_clause(fk, d))
}

/// Indexes that are not the primary key or the backing index of a unique constraint.
pub(crate) fn plain_indexes(table: &TableInfo) -> Vec<&IndexInfo> {
    let same = |a: &[String], b: &[String]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
    };
    table
        .indexes
        .iter()
        .filter(|idx| !idx.primary)
        .filter(|idx| {
            if let Some(pk) = &table.primary_key {
                if pk.name.as_deref().is_some_and(|n| n == idx.name) {
                    return false;
                }
                if idx.unique && same(&pk.columns, &idx.columns) {
                    return false;
                }
            }
            !table.unique_keys.iter().any(|uk| {
                uk.name.as_deref().is_some_and(|n| n == idx.name)
                    || (idx.unique && same(&uk.columns, &idx.columns))
            })
        })
        .collect()
}

/// `CREATE INDEX` statement for `idx` on `table`. On Postgres a native `definition` is used
/// verbatim (with the table name rewritten when it refers to a different qualification).
pub fn index_ddl(idx: &IndexInfo, table: &ObjectRef, d: Dialect) -> String {
    if d == Dialect::Postgres {
        if let Some(def) = idx.definition.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            let target = qname(table, d);
            if def.contains(&format!(" ON {target} ")) || def.contains(&format!(" ON ONLY {target} ")) {
                return def.trim_end_matches(';').to_string();
            }
            // Re-target the definition (e.g. when migrating into another schema).
            for on in [" ON ONLY ", " ON "] {
                if let Some(pos) = def.find(on) {
                    let after = &def[pos + on.len()..];
                    if let Some(end) = after.find(' ') {
                        return format!("{}{on}{target}{}", &def[..pos], &after[end..])
                            .trim_end_matches(';')
                            .to_string();
                    }
                }
            }
        }
    }
    let method = idx.method.as_deref().map(|m| m.trim().to_ascii_lowercase()).unwrap_or_default();
    let mut kw = String::from("CREATE ");
    if idx.unique {
        kw.push_str("UNIQUE ");
    }
    match d {
        Dialect::MySql if method == "fulltext" || method == "spatial" => {
            kw.push_str(&method.to_ascii_uppercase());
            kw.push(' ');
        }
        Dialect::MsSql if method == "clustered" || method == "nonclustered" => {
            kw.push_str(&method.to_ascii_uppercase());
            kw.push(' ');
        }
        _ => {}
    }
    let cols = idx
        .columns
        .iter()
        .map(|c| index_column(c, d))
        .collect::<Vec<_>>()
        .join(", ");
    let table_name = qname(table, d);
    // Index names are never schema-qualified in CREATE INDEX (Postgres puts the index in the
    // table's schema).
    let name = q(d, &idx.name);
    let mut s = format!("{kw}INDEX {name} ON {table_name}");
    if matches!(d, Dialect::Postgres | Dialect::Generic) && !method.is_empty() && method != "btree" {
        s.push_str(&format!(" USING {method}"));
    }
    s.push_str(&format!(" ({cols})"));
    if let Some(pred) = idx.predicate.as_deref().filter(|p| !p.trim().is_empty()) {
        if d != Dialect::MySql {
            s.push_str(&format!(" WHERE {}", strip_outer_parens(pred)));
        }
    }
    s
}

/// Quote plain identifiers (optionally followed by ASC/DESC); leave expressions verbatim.
fn index_column(c: &str, d: Dialect) -> String {
    let c = c.trim();
    let (name, suffix) = match c.rsplit_once(' ') {
        Some((n, s)) if s.eq_ignore_ascii_case("asc") || s.eq_ignore_ascii_case("desc") => {
            (n.trim(), format!(" {}", s.to_ascii_uppercase()))
        }
        _ => (c, String::new()),
    };
    let plain = !name.is_empty()
        && name.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '$')
        && !name.chars().next().is_some_and(|ch| ch.is_ascii_digit());
    if plain {
        format!("{}{suffix}", q(d, name))
    } else {
        c.to_string()
    }
}

fn mssql_ext_property(
    proc_name: &str,
    table: &ObjectRef,
    column: Option<&str>,
    value: Option<&str>,
    kind: &str,
) -> String {
    let lit = |s: &str| format!("N'{}'", s.replace('\'', "''"));
    let schema = table.schema.as_deref().unwrap_or("dbo");
    let mut s = format!("EXEC {proc_name} @name = N'MS_Description'");
    if let Some(v) = value {
        s.push_str(&format!(", @value = {}", lit(v)));
    }
    s.push_str(&format!(
        ", @level0type = N'SCHEMA', @level0name = {}, @level1type = N'{kind}', @level1name = {}",
        lit(schema),
        lit(&table.name)
    ));
    if let Some(c) = column {
        s.push_str(&format!(", @level2type = N'COLUMN', @level2name = {}", lit(c)));
    }
    s
}

/// Comment statements for a table and (optionally) its columns. MySQL comments are inline.
fn comment_statements(table: &TableInfo, d: Dialect, columns: bool) -> Vec<String> {
    let mut out = Vec::new();
    let view = is_view(table);
    // MySQL table comments are part of CREATE TABLE.
    if let Some(c) = table.comment.as_deref().filter(|c| !c.is_empty() && d != Dialect::MySql) {
        if let Some(s) = table_comment_statement(&table.object, view, Some(c), None, d) {
            out.push(s);
        }
    }
    if columns {
        for col in &table.columns {
            if let Some(c) = col.comment.as_deref().filter(|c| !c.is_empty()) {
                if let Some(s) = column_comment_statement(&table.object, &col.name, Some(c), None, d) {
                    out.push(s);
                }
            }
        }
    }
    out
}

fn table_comment_statement(
    obj: &ObjectRef,
    view: bool,
    new: Option<&str>,
    old: Option<&str>,
    d: Dialect,
) -> Option<String> {
    let kind = if view { "VIEW" } else { "TABLE" };
    match d {
        Dialect::Postgres | Dialect::Generic => Some(format!(
            "COMMENT ON {kind} {} IS {}",
            qname(obj, d),
            new.map(|c| d.quote_string(c)).unwrap_or_else(|| "NULL".into())
        )),
        Dialect::MySql if !view => Some(format!(
            "ALTER TABLE {} COMMENT = {}",
            qname(obj, d),
            d.quote_string(new.unwrap_or(""))
        )),
        Dialect::MsSql => Some(match (old, new) {
            (_, None) => mssql_ext_property("sp_dropextendedproperty", obj, None, None, kind),
            (Some(_), Some(c)) => mssql_ext_property("sp_updateextendedproperty", obj, None, Some(c), kind),
            (None, Some(c)) => mssql_ext_property("sp_addextendedproperty", obj, None, Some(c), kind),
        }),
        _ => None,
    }
}

fn column_comment_statement(
    obj: &ObjectRef,
    column: &str,
    new: Option<&str>,
    old: Option<&str>,
    d: Dialect,
) -> Option<String> {
    match d {
        Dialect::Postgres | Dialect::Generic => Some(format!(
            "COMMENT ON COLUMN {}.{} IS {}",
            qname(obj, d),
            q(d, column),
            new.map(|c| d.quote_string(c)).unwrap_or_else(|| "NULL".into())
        )),
        Dialect::MsSql => Some(match (old, new) {
            (_, None) => mssql_ext_property("sp_dropextendedproperty", obj, Some(column), None, "TABLE"),
            (Some(_), Some(c)) => {
                mssql_ext_property("sp_updateextendedproperty", obj, Some(column), Some(c), "TABLE")
            }
            (None, Some(c)) => {
                mssql_ext_property("sp_addextendedproperty", obj, Some(column), Some(c), "TABLE")
            }
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// DROP / TRUNCATE
// ---------------------------------------------------------------------------------------------

fn kind_keyword(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Database => "DATABASE",
        ObjectKind::Schema => "SCHEMA",
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::ForeignTable => "FOREIGN TABLE",
        ObjectKind::Sequence => "SEQUENCE",
        ObjectKind::Function => "FUNCTION",
        ObjectKind::Procedure => "PROCEDURE",
        ObjectKind::Trigger => "TRIGGER",
        ObjectKind::Index => "INDEX",
        ObjectKind::Type => "TYPE",
        ObjectKind::Event => "EVENT",
        ObjectKind::Synonym => "SYNONYM",
    }
}

/// `DROP <kind> <name>` for any object.
///
/// * Functions/procedures: on Postgres (and Generic) `signature` (e.g. `(a integer)`) is
///   appended to select the overload.
/// * Indexes and triggers: `signature` is interpreted as the **owning table name**, which
///   MySQL/SQL Server (`DROP INDEX i ON t`) and Postgres triggers (`DROP TRIGGER t ON tbl`)
///   require. It is resolved in the object's schema.
/// * `cascade` appends `CASCADE` where supported (Postgres, Generic).
///
/// The result has no statement terminator.
pub fn drop_ddl(object: &ObjectRef, dialect: Dialect, signature: Option<&str>, cascade: bool) -> String {
    let d = dialect;
    let kw = kind_keyword(object.kind);
    let name = match object.kind {
        ObjectKind::Database | ObjectKind::Schema => q(d, &object.name),
        _ => qname(object, d),
    };
    let sig = signature.map(str::trim).filter(|s| !s.is_empty());
    let parent = |t: &str| qname(&ObjectRef::table(object.schema.as_deref(), t), d);
    let mut s = match (object.kind, d) {
        (ObjectKind::Function | ObjectKind::Procedure, Dialect::Postgres | Dialect::Generic) => {
            let sig = match sig {
                Some(s) if s.starts_with('(') => s.to_string(),
                Some(s) => format!("({s})"),
                None => String::new(),
            };
            format!("DROP {kw} {name}{sig}")
        }
        (ObjectKind::Index, Dialect::MySql | Dialect::MsSql) => match sig {
            Some(t) => format!("DROP INDEX {} ON {}", q(d, &object.name), parent(t)),
            None => format!("DROP INDEX {}", q(d, &object.name)),
        },
        (ObjectKind::Trigger, Dialect::Postgres) => match sig {
            Some(t) => format!("DROP TRIGGER {} ON {}", q(d, &object.name), parent(t)),
            None => format!("DROP TRIGGER {name}"),
        },
        (ObjectKind::Trigger, Dialect::Sqlite) => format!("DROP TRIGGER {}", q(d, &object.name)),
        _ => format!("DROP {kw} {name}"),
    };
    if cascade && matches!(d, Dialect::Postgres | Dialect::Generic) {
        s.push_str(" CASCADE");
    }
    s
}

/// `TRUNCATE TABLE t`; SQLite has no TRUNCATE, so `DELETE FROM t` is generated.
pub fn truncate_ddl(object: &ObjectRef, dialect: Dialect) -> String {
    match dialect {
        Dialect::Sqlite => format!("DELETE FROM {}", qname(object, dialect)),
        _ => format!("TRUNCATE TABLE {}", qname(object, dialect)),
    }
}

// ---------------------------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------------------------

/// Order tables so that FK targets come first (Kahn's algorithm, stable w.r.t. input order).
/// Cycles are broken by taking the first remaining table in input order. Returns indexes.
pub(crate) fn fk_order(tables: &[&TableInfo]) -> Vec<usize> {
    let key = |s: &str| s.to_ascii_lowercase();
    let pos: HashMap<String, usize> =
        tables.iter().enumerate().map(|(i, t)| (key(&t.object.name), i)).collect();
    let deps: Vec<HashSet<usize>> = tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            t.foreign_keys
                .iter()
                .filter_map(|fk| pos.get(&key(&fk.ref_table)).copied())
                .filter(|&j| j != i)
                .collect()
        })
        .collect();
    let mut done = vec![false; tables.len()];
    let mut order = Vec::with_capacity(tables.len());
    while order.len() < tables.len() {
        let next = (0..tables.len())
            .find(|&i| !done[i] && deps[i].iter().all(|&j| done[j]))
            .or_else(|| (0..tables.len()).find(|&i| !done[i]))
            .expect("remaining table");
        done[next] = true;
        order.push(next);
    }
    order
}

/// Order views so that views referenced (by name) in another view's definition come first.
pub(crate) fn view_order(views: &[&TableInfo]) -> Vec<usize> {
    let mentions = |def: &str, name: &str| {
        let def = def.to_ascii_lowercase();
        let name = name.to_ascii_lowercase();
        def.match_indices(&name).any(|(i, _)| {
            let before = def[..i].chars().next_back();
            let after = def[i + name.len()..].chars().next();
            let boundary = |c: Option<char>| c.is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            boundary(before) && boundary(after)
        })
    };
    let deps: Vec<HashSet<usize>> = views
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let def = v.view_definition.as_deref().unwrap_or("");
            views
                .iter()
                .enumerate()
                .filter(|(j, w)| *j != i && mentions(def, &w.object.name))
                .map(|(j, _)| j)
                .collect()
        })
        .collect();
    let mut done = vec![false; views.len()];
    let mut order = Vec::new();
    while order.len() < views.len() {
        let next = (0..views.len())
            .find(|&i| !done[i] && deps[i].iter().all(|&j| done[j]))
            .or_else(|| (0..views.len()).find(|&i| !done[i]))
            .expect("remaining view");
        done[next] = true;
        order.push(next);
    }
    order
}

/// Statements for tables in FK order; FKs pointing at tables created later (cycles) are
/// returned separately as `ALTER TABLE .. ADD CONSTRAINT` (SQLite keeps them inline since it
/// does not validate FK targets at creation time and cannot add constraints later).
pub(crate) fn ordered_table_statements(tables: &[&TableInfo], d: Dialect) -> (Vec<String>, Vec<String>) {
    let order = fk_order(tables);
    let position: HashMap<String, usize> = order
        .iter()
        .enumerate()
        .map(|(p, &i)| (tables[i].object.name.to_ascii_lowercase(), p))
        .collect();
    let mut creates = Vec::new();
    let mut deferred = Vec::new();
    for (p, &i) in order.iter().enumerate() {
        let t = tables[i];
        let later = |fk: &ForeignKeyInfo| {
            position.get(&fk.ref_table.to_ascii_lowercase()).is_some_and(|&rp| rp > p)
        };
        if d == Dialect::Sqlite {
            creates.push(create_table_statement(t, d, true));
        } else {
            creates.push(create_table_statement_filtered(t, d, &|fk| !later(fk)));
            for fk in t.foreign_keys.iter().filter(|fk| later(fk)) {
                deferred.push(add_foreign_key_statement(&t.object, fk, d));
            }
        }
        creates.extend(table_extras(t, d));
    }
    (creates, deferred)
}

/// Full DDL for a schema: sequences referenced by `nextval()` defaults (Postgres), tables in FK
/// order (cyclic FKs added afterwards via `ALTER TABLE`), views in dependency order, then
/// routines (their `definition` verbatim).
pub fn snapshot_ddl(snapshot: &SchemaSnapshot, dialect: Dialect) -> String {
    script(&snapshot_statements(snapshot, dialect), dialect)
}

/// The bare statements behind [`snapshot_ddl`].
pub fn snapshot_statements(snapshot: &SchemaSnapshot, d: Dialect) -> Vec<String> {
    let mut out = Vec::new();
    if matches!(d, Dialect::Postgres | Dialect::Generic) {
        for seq in &snapshot.sequences {
            let name = &seq.object.name;
            let used = snapshot.tables.iter().flat_map(|t| &t.columns).any(|c| {
                is_nextval(c.default.as_deref())
                    && !is_serial_type(&c.data_type)
                    && c.default.as_deref().is_some_and(|def| def.contains(name.as_str()))
            });
            if used {
                out.push(format!("CREATE SEQUENCE {}", qname(&seq.object, d)));
            }
        }
    }
    let tables: Vec<&TableInfo> = snapshot.tables.iter().filter(|t| !is_view(t)).collect();
    let (creates, deferred) = ordered_table_statements(&tables, d);
    out.extend(creates);
    out.extend(deferred);
    let views: Vec<&TableInfo> = snapshot.tables.iter().filter(|t| is_view(t)).collect();
    for i in view_order(&views) {
        out.extend(create_table_statements(views[i], d));
    }
    for r in &snapshot.routines {
        let def = r.definition.trim();
        if !def.is_empty() {
            out.push(def.to_string());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// ALTER TABLE
// ---------------------------------------------------------------------------------------------

/// Statements transforming table `old` into `new` (bare statements, no terminators).
///
/// See [`table_change_statements`] for the per-dialect strategy.
pub fn alter_table_ddl(old: &TableInfo, new: &TableInfo, dialect: Dialect) -> Vec<String> {
    let changes = table_changes(old, new, &DiffOptions::default());
    table_change_statements(&new.object, &changes, dialect, Some(old), Some(new))
}

/// Render a list of [`TableChange`]s against table `table` as `ALTER TABLE` statements.
///
/// Order: drop FKs, checks, uniques, indexes and the old primary key; drop columns; add and
/// alter columns; add primary key, uniques, checks, indexes, FKs; comments.
///
/// * Postgres/Generic: `ALTER COLUMN .. TYPE / SET|DROP NOT NULL / SET|DROP DEFAULT`, identity
///   via `ADD GENERATED BY DEFAULT AS IDENTITY` / `DROP IDENTITY IF EXISTS`, `COMMENT ON`.
/// * MySQL: `MODIFY COLUMN` with the full new definition (incl. `AFTER`/`FIRST` when the
///   change is an ordinal move).
/// * SQL Server: `ALTER COLUMN type [NOT] NULL`; defaults are dropped through a lookup of the
///   default constraint in `sys.default_constraints`; comments via extended properties.
/// * SQLite: `ADD COLUMN`, `DROP COLUMN`, `CREATE/DROP INDEX` natively; any other change needs
///   the table rebuild (create new table, copy data, drop old, rename), which requires `old`
///   and `new`. Without them a SQL comment explaining the limitation is emitted.
///
/// Constraints without a name are dropped using the database's default naming convention
/// (Postgres `t_pkey`, `t_a_key`, `t_a_fkey`, `t_check`; MySQL `t_ibfk_1`, `t_chk_1`;
/// SQL Server `PK_t`), which may not match the actual name.
pub fn table_change_statements(
    table: &ObjectRef,
    changes: &[TableChange],
    dialect: Dialect,
    old: Option<&TableInfo>,
    new: Option<&TableInfo>,
) -> Vec<String> {
    if dialect == Dialect::Sqlite {
        return sqlite_changes(table, changes, old, new);
    }
    let d = dialect;
    let t = qname(table, d);
    let mut out: Vec<String> = Vec::new();
    let alter = |out: &mut Vec<String>, s: String| out.push(format!("ALTER TABLE {t} {s}"));

    // 1. drops
    for c in changes {
        match c {
            TableChange::DropForeignKey(fk) => {
                let name = fk.name.clone().unwrap_or_else(|| default_fk_name(table, &fk.columns, d));
                match d {
                    Dialect::MySql => alter(&mut out, format!("DROP FOREIGN KEY {}", q(d, &name))),
                    _ => alter(&mut out, format!("DROP CONSTRAINT {}", q(d, &name))),
                }
            }
            TableChange::DropCheck(ck) => {
                let name = ck.name.clone().unwrap_or_else(|| default_check_name(table, d));
                match d {
                    Dialect::MySql => alter(&mut out, format!("DROP CHECK {}", q(d, &name))),
                    _ => alter(&mut out, format!("DROP CONSTRAINT {}", q(d, &name))),
                }
            }
            TableChange::DropUnique(uk) => {
                let name = uk.name.clone().unwrap_or_else(|| default_unique_name(table, &uk.columns, d));
                match d {
                    Dialect::MySql => alter(&mut out, format!("DROP INDEX {}", q(d, &name))),
                    _ => alter(&mut out, format!("DROP CONSTRAINT {}", q(d, &name))),
                }
            }
            _ => {}
        }
    }
    for c in changes {
        if let TableChange::DropIndex(idx) = c {
            out.push(drop_index_statement(idx, table, d));
        }
    }
    for c in changes {
        if let TableChange::PrimaryKeyChanged { old: Some(pk), .. } = c {
            out.push(drop_primary_key_statement(table, pk, d));
        }
    }
    // 2. columns
    for c in changes {
        if let TableChange::DropColumn(name) = c {
            if d == Dialect::MsSql {
                out.push(mssql_drop_default(table, name));
            }
            alter(&mut out, format!("DROP COLUMN {}", q(d, name)));
        }
    }
    for c in changes {
        if let TableChange::AddColumn(col) = c {
            let kw = if d == Dialect::MsSql { "ADD" } else { "ADD COLUMN" };
            alter(&mut out, format!("{kw} {}", column_definition(col, d, false)));
        }
    }
    let new_cols: Vec<String> =
        new.map(|n| n.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
    for c in changes {
        if let TableChange::AlterColumn { old: oc, new: nc } = c {
            out.extend(alter_column_statements(table, oc, nc, d, &new_cols));
        }
    }
    // 3. adds
    for c in changes {
        if let TableChange::PrimaryKeyChanged { new: Some(pk), .. } = c {
            alter(&mut out, format!("ADD {}", primary_key_clause(pk, d)));
        }
    }
    for c in changes {
        match c {
            TableChange::AddUnique(uk) => alter(&mut out, format!("ADD {}", unique_clause(uk, d))),
            TableChange::AddCheck(ck) => alter(&mut out, format!("ADD {}", check_clause(ck, d))),
            _ => {}
        }
    }
    for c in changes {
        if let TableChange::AddIndex(idx) = c {
            out.push(index_ddl(idx, table, d));
        }
    }
    for c in changes {
        if let TableChange::AddForeignKey(fk) = c {
            out.push(add_foreign_key_statement(table, fk, d));
        }
    }
    let view = new.is_some_and(is_view);
    for c in changes {
        if let TableChange::CommentChanged { old: o, new: n } = c {
            if let Some(s) = table_comment_statement(table, view, n.as_deref(), o.as_deref(), d) {
                out.push(s);
            }
        }
    }
    out
}

/// T-SQL batch dropping the (system-named) default constraint of a column, if any.
fn mssql_drop_default(table: &ObjectRef, column: &str) -> String {
    let t = qname(table, Dialect::MsSql);
    let lit = |s: &str| format!("N'{}'", s.replace('\'', "''"));
    format!(
        "DECLARE @df sysname; SELECT @df = d.name FROM sys.default_constraints d \
         JOIN sys.columns c ON d.parent_object_id = c.object_id AND d.parent_column_id = c.column_id \
         WHERE d.parent_object_id = OBJECT_ID({}) AND c.name = {}; \
         IF @df IS NOT NULL EXEC(N'ALTER TABLE {} DROP CONSTRAINT ' + QUOTENAME(@df))",
        lit(&t),
        lit(column),
        t.replace('\'', "''")
    )
}

fn default_fk_name(table: &ObjectRef, cols: &[String], d: Dialect) -> String {
    match d {
        Dialect::MySql => format!("{}_ibfk_1", table.name),
        Dialect::MsSql => format!("FK_{}_{}", table.name, cols.join("_")),
        _ => format!("{}_{}_fkey", table.name, cols.join("_")),
    }
}

fn default_unique_name(table: &ObjectRef, cols: &[String], d: Dialect) -> String {
    match d {
        Dialect::MySql => cols.first().cloned().unwrap_or_default(),
        Dialect::MsSql => format!("UQ_{}_{}", table.name, cols.join("_")),
        _ => format!("{}_{}_key", table.name, cols.join("_")),
    }
}

fn default_check_name(table: &ObjectRef, d: Dialect) -> String {
    match d {
        Dialect::MySql => format!("{}_chk_1", table.name),
        Dialect::MsSql => format!("CK_{}", table.name),
        _ => format!("{}_check", table.name),
    }
}

fn drop_primary_key_statement(table: &ObjectRef, pk: &KeyInfo, d: Dialect) -> String {
    let t = qname(table, d);
    match d {
        Dialect::MySql => format!("ALTER TABLE {t} DROP PRIMARY KEY"),
        Dialect::MsSql => {
            let name = pk.name.clone().unwrap_or_else(|| format!("PK_{}", table.name));
            format!("ALTER TABLE {t} DROP CONSTRAINT {}", q(d, &name))
        }
        _ => {
            let name = pk.name.clone().unwrap_or_else(|| format!("{}_pkey", table.name));
            format!("ALTER TABLE {t} DROP CONSTRAINT {}", q(d, &name))
        }
    }
}

/// `ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY ...`.
pub fn add_foreign_key_statement(table: &ObjectRef, fk: &ForeignKeyInfo, d: Dialect) -> String {
    format!("ALTER TABLE {} ADD {}", qname(table, d), foreign_key_constraint(fk, d))
}

fn drop_index_statement(idx: &IndexInfo, table: &ObjectRef, d: Dialect) -> String {
    match d {
        Dialect::MySql | Dialect::MsSql => {
            format!("DROP INDEX {} ON {}", q(d, &idx.name), qname(table, d))
        }
        _ => {
            let obj = ObjectRef::new(table.schema.as_deref(), idx.name.clone(), ObjectKind::Index);
            format!("DROP INDEX {}", qname(&obj, d))
        }
    }
}

fn alter_column_statements(
    table: &ObjectRef,
    oc: &ColumnInfo,
    nc: &ColumnInfo,
    d: Dialect,
    new_cols: &[String],
) -> Vec<String> {
    use crate::util::{canonical_type, normalize_default, normalize_expr};
    let t = qname(table, d);
    let c = q(d, &nc.name);
    let mut out = Vec::new();
    let gen_changed = oc.generated.as_deref().map(normalize_expr) != nc.generated.as_deref().map(normalize_expr);
    if gen_changed {
        // Generated expressions cannot be altered portably: re-create the column.
        if d == Dialect::MsSql {
            out.push(mssql_drop_default(table, &oc.name));
        }
        out.push(format!("ALTER TABLE {t} DROP COLUMN {}", q(d, &oc.name)));
        let kw = if d == Dialect::MsSql { "ADD" } else { "ADD COLUMN" };
        out.push(format!("ALTER TABLE {t} {kw} {}", column_definition(nc, d, false)));
        return out;
    }
    let type_changed = canonical_type(&oc.data_type) != canonical_type(&nc.data_type);
    let coll_changed = nc.collation.is_some()
        && !oc.collation.as_deref().unwrap_or("").eq_ignore_ascii_case(nc.collation.as_deref().unwrap_or(""));
    let null_changed = oc.nullable != nc.nullable;
    let def_changed = normalize_default(oc.default.as_deref()) != normalize_default(nc.default.as_deref());
    let comment_changed = oc.comment != nc.comment;
    match d {
        Dialect::MySql => {
            let mut s = format!("ALTER TABLE {t} MODIFY COLUMN {}", column_definition(nc, d, false));
            if oc.ordinal != nc.ordinal && nc.ordinal > 0 {
                let pos = new_cols.iter().position(|n| n == &nc.name);
                match pos {
                    Some(0) => s.push_str(" FIRST"),
                    Some(p) => s.push_str(&format!(" AFTER {}", q(d, &new_cols[p - 1]))),
                    None => {}
                }
            }
            out.push(s);
        }
        Dialect::MsSql => {
            if type_changed || null_changed || coll_changed {
                let collate = nc
                    .collation
                    .as_deref()
                    .map(|cl| format!(" COLLATE {}", q(d, cl)))
                    .unwrap_or_default();
                out.push(format!(
                    "ALTER TABLE {t} ALTER COLUMN {c} {}{collate} {}",
                    nc.data_type.trim(),
                    if nc.nullable { "NULL" } else { "NOT NULL" }
                ));
            }
            if def_changed {
                if oc.default.is_some() {
                    out.push(mssql_drop_default(table, &nc.name));
                }
                if let Some(def) = nc.default.as_deref().filter(|s| !s.trim().is_empty()) {
                    out.push(format!("ALTER TABLE {t} ADD DEFAULT {} FOR {c}", def.trim()));
                }
            }
            if comment_changed {
                if let Some(s) = column_comment_statement(table, &nc.name, nc.comment.as_deref(), oc.comment.as_deref(), d) {
                    out.push(s);
                }
            }
        }
        _ => {
            let a = format!("ALTER TABLE {t} ALTER COLUMN {c}");
            if type_changed || coll_changed {
                // serial pseudo-types are not valid in ALTER TYPE
                let ty = match parse_type(&nc.data_type).base.as_str() {
                    "serial" | "serial4" => "integer".to_string(),
                    "bigserial" | "serial8" => "bigint".to_string(),
                    "smallserial" | "serial2" => "smallint".to_string(),
                    _ => nc.data_type.trim().to_string(),
                };
                let collate = nc
                    .collation
                    .as_deref()
                    .map(|cl| format!(" COLLATE {}", q(d, cl)))
                    .unwrap_or_default();
                out.push(format!("{a} TYPE {ty}{collate}"));
            }
            if null_changed {
                out.push(format!("{a} {} NOT NULL", if nc.nullable { "DROP" } else { "SET" }));
            }
            let (oi, ni) = (pg_identity(oc), pg_identity(nc));
            if oi && !ni {
                out.push(format!("{a} DROP IDENTITY IF EXISTS"));
            }
            if def_changed && !ni {
                match nc.default.as_deref().filter(|s| !s.trim().is_empty()) {
                    Some(def) => out.push(format!("{a} SET DEFAULT {}", def.trim())),
                    None => out.push(format!("{a} DROP DEFAULT")),
                }
            }
            if ni && !oi {
                if oc.default.is_some() {
                    out.push(format!("{a} DROP DEFAULT"));
                }
                out.push(format!("{a} ADD GENERATED BY DEFAULT AS IDENTITY"));
            }
            if comment_changed {
                if let Some(s) = column_comment_statement(table, &nc.name, nc.comment.as_deref(), oc.comment.as_deref(), d) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// SQLite: native statements when possible, otherwise the documented table rebuild.
fn sqlite_changes(
    table: &ObjectRef,
    changes: &[TableChange],
    old: Option<&TableInfo>,
    new: Option<&TableInfo>,
) -> Vec<String> {
    let d = Dialect::Sqlite;
    let t = qname(table, d);
    let dropped_indexes: Vec<&str> = changes
        .iter()
        .filter_map(|c| match c {
            TableChange::DropIndex(i) => Some(i.name.as_str()),
            _ => None,
        })
        .collect();
    let in_constraint = |name: &str| -> bool {
        let Some(old) = old else { return false };
        let eq = |c: &String| c.eq_ignore_ascii_case(name);
        old.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(eq))
            || old.unique_keys.iter().any(|k| k.columns.iter().any(eq))
            || old
                .indexes
                .iter()
                .filter(|i| !dropped_indexes.contains(&i.name.as_str()))
                .any(|i| i.columns.iter().any(eq))
            || old.foreign_keys.iter().any(|f| f.columns.iter().any(eq))
    };
    let native = changes.iter().all(|c| match c {
        TableChange::AddColumn(col) => {
            col.generated.is_none()
                && (col.nullable || col.default.is_some())
                && !new.is_some_and(|n| {
                    n.primary_key.as_ref().is_some_and(|k| k.columns.contains(&col.name))
                        || n.unique_keys.iter().any(|k| k.columns.contains(&col.name))
                        || n.foreign_keys.iter().any(|f| f.columns.contains(&col.name))
                })
        }
        TableChange::DropColumn(name) => !in_constraint(name),
        TableChange::AddIndex(_) | TableChange::DropIndex(_) | TableChange::CommentChanged { .. } => true,
        TableChange::AlterColumn { old: o, new: n } => {
            // comment-only / ordinal-only changes are no-ops in SQLite
            let mut a = o.clone();
            a.comment = n.comment.clone();
            a.ordinal = n.ordinal;
            &a == n
        }
        _ => false,
    });
    if native {
        let mut out = Vec::new();
        for c in changes {
            if let TableChange::DropIndex(idx) = c {
                out.push(drop_index_statement(idx, table, d));
            }
        }
        for c in changes {
            if let TableChange::DropColumn(name) = c {
                out.push(format!("ALTER TABLE {t} DROP COLUMN {}", q(d, name)));
            }
        }
        for c in changes {
            if let TableChange::AddColumn(col) = c {
                out.push(format!("ALTER TABLE {t} ADD COLUMN {}", column_definition(col, d, false)));
            }
        }
        for c in changes {
            if let TableChange::AddIndex(idx) = c {
                out.push(index_ddl(idx, table, d));
            }
        }
        return out;
    }
    let (Some(old), Some(new)) = (old, new) else {
        return vec![format!(
            "-- SQLite cannot apply these changes to {t} with ALTER TABLE; a table rebuild is required"
        )];
    };
    sqlite_rebuild(table, old, new)
}

/// The SQLite 12-step table rebuild (<https://sqlite.org/lang_altertable.html#otheralter>),
/// without the surrounding transaction so it can run inside the caller's transaction.
pub fn sqlite_rebuild(table: &ObjectRef, old: &TableInfo, new: &TableInfo) -> Vec<String> {
    let d = Dialect::Sqlite;
    let t = qname(table, d);
    let tmp_name = format!("_new_{}", table.name);
    let mut tmp = new.clone();
    tmp.object = ObjectRef { name: tmp_name.clone(), ..table.clone() };
    let tmp_q = qname(&tmp.object, d);
    let copy_cols: Vec<String> = new
        .columns
        .iter()
        .filter(|c| c.generated.is_none())
        .filter(|c| old.column(&c.name).is_some_and(|oc| oc.generated.is_none()))
        .map(|c| c.name.clone())
        .collect();
    let mut out = vec!["PRAGMA foreign_keys = OFF".to_string(), create_table_statement(&tmp, d, true)];
    if !copy_cols.is_empty() {
        let new_list = ident_list(d, &copy_cols);
        let old_list = copy_cols
            .iter()
            .map(|c| q(d, &old.column(c).map(|oc| oc.name.clone()).unwrap_or_else(|| c.clone())))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!("INSERT INTO {tmp_q} ({new_list}) SELECT {old_list} FROM {t}"));
    }
    out.push(format!("DROP TABLE {t}"));
    out.push(format!("ALTER TABLE {tmp_q} RENAME TO {}", q(d, &table.name)));
    let mut final_table = new.clone();
    final_table.object = table.clone();
    out.extend(table_extras(&final_table, d));
    out.push("PRAGMA foreign_key_check".to_string());
    out.push("PRAGMA foreign_keys = ON".to_string());
    out
}

// ---------------------------------------------------------------------------------------------
// Type conversion
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Logical {
    TinyInt,
    SmallInt,
    Int,
    BigInt,
    SmallSerial,
    Serial,
    BigSerial,
    Decimal(Option<(String, Option<String>)>),
    Real,
    Double,
    Bool,
    Char(Option<String>),
    Varchar(Option<String>),
    Text,
    Bytes { len: Option<String>, fixed: bool },
    Date,
    Time,
    TimeTz,
    Timestamp(Option<String>),
    TimestampTz(Option<String>),
    Interval,
    Json,
    Jsonb,
    Uuid,
    Xml,
    Array(Box<Logical>),
    Unknown(String),
}

fn to_logical(data_type: &str, from: Dialect) -> Logical {
    let raw = data_type.trim();
    let p = parse_type(raw);
    if p.array || (from == Dialect::Postgres && p.base.starts_with('_') && p.base.len() > 1) {
        let inner = if p.array { raw.trim_end_matches("[]").trim().to_string() } else { p.base[1..].to_string() };
        return Logical::Array(Box::new(to_logical(&inner, from)));
    }
    let arg0 = p.args.first().cloned();
    let num_arg = |a: &Option<String>| a.as_ref().and_then(|s| s.parse::<u32>().ok());
    let base = p.base.as_str();
    let auto = p.auto;
    let int = |l: Logical| -> Logical {
        if auto {
            match l {
                Logical::BigInt => Logical::BigSerial,
                Logical::SmallInt | Logical::TinyInt => Logical::SmallSerial,
                _ => Logical::Serial,
            }
        } else {
            l
        }
    };
    match base {
        "tinyint" if from == Dialect::MySql && arg0.as_deref() == Some("1") => Logical::Bool,
        "tinyint" if p.unsigned || from == Dialect::MsSql => int(Logical::SmallInt),
        "tinyint" => int(Logical::TinyInt),
        "int2" | "smallint" if p.unsigned => int(Logical::Int),
        "int2" | "smallint" => int(Logical::SmallInt),
        "mediumint" => int(Logical::Int),
        "int" | "integer" | "int4" if p.unsigned => int(Logical::BigInt),
        "int" | "integer" | "int4" => int(Logical::Int),
        "bigint" | "int8" if p.unsigned => Logical::Decimal(Some(("20".into(), Some("0".into())))),
        "bigint" | "int8" => int(Logical::BigInt),
        "serial" | "serial4" => Logical::Serial,
        "bigserial" | "serial8" => Logical::BigSerial,
        "smallserial" | "serial2" => Logical::SmallSerial,
        "year" => Logical::SmallInt,
        "numeric" | "decimal" | "dec" | "number" => {
            Logical::Decimal(arg0.map(|pr| (pr, p.args.get(1).cloned())))
        }
        "money" => Logical::Decimal(Some(("19".into(), Some("4".into())))),
        "smallmoney" => Logical::Decimal(Some(("10".into(), Some("4".into())))),
        "real" | "float4" => Logical::Real,
        "float" => match (from, num_arg(&arg0)) {
            (_, Some(n)) if n <= 24 => Logical::Real,
            (_, Some(_)) => Logical::Double,
            (Dialect::MySql, None) => Logical::Real,
            _ => Logical::Double,
        },
        "double" | "double precision" | "float8" => Logical::Double,
        "bool" | "boolean" => Logical::Bool,
        "bit" if num_arg(&arg0).unwrap_or(1) <= 1 => Logical::Bool,
        "bit" | "bit varying" | "varbit" => Logical::Bytes { len: None, fixed: false },
        "char" | "character" | "nchar" | "bpchar" | "national char" | "national character" => {
            Logical::Char(arg0)
        }
        "varchar" | "character varying" | "nvarchar" | "varchar2" | "nvarchar2"
        | "national character varying" | "national varchar" => match arg0.as_deref() {
            Some("max") | Some("-1") => Logical::Text,
            _ => Logical::Varchar(arg0),
        },
        "text" | "tinytext" | "mediumtext" | "longtext" | "ntext" | "clob" | "citext" | "string" => {
            Logical::Text
        }
        "enum" | "set" => {
            let max = p.args.iter().map(|a| a.trim_matches('\'').chars().count()).max().unwrap_or(0);
            Logical::Varchar(Some(max.max(1).to_string()))
        }
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "image" => {
            Logical::Bytes { len: None, fixed: false }
        }
        "binary" => Logical::Bytes { len: arg0, fixed: true },
        "varbinary" => match arg0.as_deref() {
            Some("max") | None => Logical::Bytes { len: None, fixed: false },
            _ => Logical::Bytes { len: arg0, fixed: false },
        },
        "rowversion" => Logical::Bytes { len: Some("8".into()), fixed: true },
        "timestamp" if from == Dialect::MsSql => Logical::Bytes { len: Some("8".into()), fixed: true },
        "date" => Logical::Date,
        "time" if p.tz == Some(true) => Logical::TimeTz,
        "time" => Logical::Time,
        "timetz" => Logical::TimeTz,
        "timestamp" if p.tz == Some(true) => Logical::TimestampTz(arg0),
        "timestamp" | "datetime" | "datetime2" => Logical::Timestamp(arg0),
        "smalldatetime" => Logical::Timestamp(None),
        "timestamptz" | "datetimeoffset" => Logical::TimestampTz(arg0),
        "interval" => Logical::Interval,
        "json" => Logical::Json,
        "jsonb" => Logical::Jsonb,
        "uuid" | "uniqueidentifier" => Logical::Uuid,
        "xml" => Logical::Xml,
        _ => Logical::Unknown(raw.to_string()),
    }
}

fn from_logical(l: &Logical, to: Dialect) -> String {
    use Dialect::*;
    let with_args = |name: &str, a: &Option<String>| match a {
        Some(a) => format!("{name}({a})"),
        None => name.to_string(),
    };
    match (l, to) {
        (Logical::TinyInt, Postgres | Generic) => "smallint".into(),
        (Logical::TinyInt, Sqlite) => "integer".into(),
        (Logical::TinyInt, _) => "tinyint".into(),
        (Logical::SmallInt, Sqlite) => "integer".into(),
        (Logical::SmallInt, _) => "smallint".into(),
        (Logical::Int, MySql | MsSql) => "int".into(),
        (Logical::Int, _) => "integer".into(),
        (Logical::BigInt, Sqlite) => "integer".into(),
        (Logical::BigInt, _) => "bigint".into(),
        (Logical::SmallSerial, Postgres) => "smallserial".into(),
        (Logical::Serial, Postgres) => "serial".into(),
        (Logical::BigSerial, Postgres) => "bigserial".into(),
        (Logical::SmallSerial, MySql) => "smallint auto_increment".into(),
        (Logical::Serial, MySql) => "int auto_increment".into(),
        (Logical::BigSerial, MySql) => "bigint auto_increment".into(),
        (Logical::SmallSerial, MsSql) => "smallint identity(1,1)".into(),
        (Logical::Serial, MsSql) => "int identity(1,1)".into(),
        (Logical::BigSerial, MsSql) => "bigint identity(1,1)".into(),
        (Logical::SmallSerial | Logical::Serial | Logical::BigSerial, Sqlite) => "integer".into(),
        (Logical::SmallSerial, Generic) => "smallint".into(),
        (Logical::Serial, Generic) => "integer".into(),
        (Logical::BigSerial, Generic) => "bigint".into(),
        (Logical::Decimal(None), MySql | MsSql) => "decimal(38,10)".into(),
        (Logical::Decimal(None), _) => "numeric".into(),
        (Logical::Decimal(Some((p, s))), _) => {
            let name = if matches!(to, MySql | MsSql) { "decimal" } else { "numeric" };
            // MySQL/MSSQL max precision is 65/38
            let p = match (to, p.parse::<u32>()) {
                (MsSql, Ok(n)) if n > 38 => "38".to_string(),
                (MySql, Ok(n)) if n > 65 => "65".to_string(),
                _ => p.clone(),
            };
            match s {
                Some(s) => format!("{name}({p},{s})"),
                None => format!("{name}({p})"),
            }
        }
        (Logical::Real, MySql) => "float".into(),
        (Logical::Real, _) => "real".into(),
        (Logical::Double, MySql) => "double".into(),
        (Logical::Double, MsSql) => "float".into(),
        (Logical::Double, Sqlite) => "real".into(),
        (Logical::Double, _) => "double precision".into(),
        (Logical::Bool, MySql) => "tinyint(1)".into(),
        (Logical::Bool, MsSql) => "bit".into(),
        (Logical::Bool, _) => "boolean".into(),
        (Logical::Char(n), MsSql) => with_args("nchar", n),
        (Logical::Char(n), _) => with_args("char", n),
        (Logical::Varchar(n), MsSql) => match n.as_deref().and_then(|s| s.parse::<u32>().ok()) {
            Some(len) if len > 4000 => "nvarchar(max)".into(),
            Some(len) => format!("nvarchar({len})"),
            None => "nvarchar(max)".into(),
        },
        (Logical::Varchar(n), MySql) => match n.as_deref().and_then(|s| s.parse::<u32>().ok()) {
            Some(len) if len > 16383 => "longtext".into(),
            Some(len) => format!("varchar({len})"),
            None => "longtext".into(),
        },
        (Logical::Varchar(n), _) => with_args("varchar", n),
        (Logical::Text, MsSql) => "nvarchar(max)".into(),
        (Logical::Text, MySql) => "longtext".into(),
        (Logical::Text, _) => "text".into(),
        (Logical::Bytes { .. }, Postgres) => "bytea".into(),
        (Logical::Bytes { len: Some(n), fixed: true }, MySql | MsSql) => format!("binary({n})"),
        (Logical::Bytes { len: Some(n), fixed: false }, MySql | MsSql) => format!("varbinary({n})"),
        (Logical::Bytes { .. }, MySql) => "longblob".into(),
        (Logical::Bytes { .. }, MsSql) => "varbinary(max)".into(),
        (Logical::Bytes { .. }, _) => "blob".into(),
        (Logical::Date, _) => "date".into(),
        (Logical::Time, _) => "time".into(),
        (Logical::TimeTz, Postgres) => "time with time zone".into(),
        (Logical::TimeTz, _) => "time".into(),
        (Logical::Timestamp(p), MySql) => with_args("datetime", p),
        (Logical::Timestamp(p), MsSql) => with_args("datetime2", p),
        (Logical::Timestamp(_), Sqlite) => "datetime".into(),
        (Logical::Timestamp(p), _) => with_args("timestamp", p),
        (Logical::TimestampTz(p), Postgres | Generic) => match p {
            Some(p) => format!("timestamp({p}) with time zone"),
            None => "timestamp with time zone".into(),
        },
        (Logical::TimestampTz(p), MySql) => with_args("datetime", p),
        (Logical::TimestampTz(p), MsSql) => with_args("datetimeoffset", p),
        (Logical::TimestampTz(_), _) => "datetime".into(),
        (Logical::Interval, Postgres | Generic) => "interval".into(),
        (Logical::Interval, MsSql) => "nvarchar(64)".into(),
        (Logical::Interval, MySql) => "varchar(64)".into(),
        (Logical::Interval, _) => "text".into(),
        (Logical::Json, Postgres | MySql | Generic) => "json".into(),
        (Logical::Jsonb, Postgres) => "jsonb".into(),
        (Logical::Jsonb, MySql | Generic) => "json".into(),
        (Logical::Json | Logical::Jsonb, MsSql) => "nvarchar(max)".into(),
        (Logical::Json | Logical::Jsonb, _) => "text".into(),
        (Logical::Uuid, Postgres | Generic) => "uuid".into(),
        (Logical::Uuid, MySql) => "char(36)".into(),
        (Logical::Uuid, MsSql) => "uniqueidentifier".into(),
        (Logical::Uuid, _) => "text".into(),
        (Logical::Xml, Postgres | MsSql | Generic) => "xml".into(),
        (Logical::Xml, MySql) => "longtext".into(),
        (Logical::Xml, _) => "text".into(),
        (Logical::Array(inner), Postgres) => format!("{}[]", from_logical(inner, to)),
        (Logical::Array(_), MySql | Generic) => "json".into(),
        (Logical::Array(_), MsSql) => "nvarchar(max)".into(),
        (Logical::Array(_), _) => "text".into(),
        (Logical::Unknown(raw), _) => raw.clone(),
    }
}

/// Map a column type from one dialect to another, e.g. Postgres `serial` -> MySQL
/// `int auto_increment`, `text` -> SQL Server `nvarchar(max)`, `bytea` -> MySQL `longblob` /
/// SQL Server `varbinary(max)`, `timestamptz` -> `datetime` / `datetimeoffset`, `jsonb` ->
/// `json` / `nvarchar(max)`, `boolean` -> `tinyint(1)` / `bit`, `uuid` -> `char(36)` /
/// `uniqueidentifier`. Unknown types are returned unchanged; identical dialects return the
/// input unchanged.
pub fn convert_type(data_type: &str, from: Dialect, to: Dialect) -> String {
    if from == to {
        return data_type.to_string();
    }
    from_logical(&to_logical(data_type, from), to)
}

/// Copy a table definition to another dialect: column types converted with
/// [`convert_type`], dialect-specific defaults that cannot be translated (e.g. `nextval()`)
/// dropped, and native index definitions discarded so they are regenerated.
pub fn convert_table(table: &TableInfo, from: Dialect, to: Dialect) -> TableInfo {
    let mut t = table.clone();
    if from == to {
        return t;
    }
    for c in &mut t.columns {
        let p = parse_type(&c.data_type);
        if p.auto || is_serial_type(&c.data_type) {
            c.auto_increment = true;
        }
        if is_nextval(c.default.as_deref()) {
            c.auto_increment = true;
            c.default = None;
        }
        let mut converted = convert_type(&c.data_type, from, to);
        // identity is expressed by the column flag, not the type spelling
        for suffix in [" auto_increment", " identity(1,1)"] {
            if let Some(s) = converted.strip_suffix(suffix) {
                converted = s.to_string();
            }
        }
        if to == Dialect::Postgres && c.auto_increment && is_serial_type(&converted) {
            converted = match converted.as_str() {
                "bigserial" => "bigint",
                "smallserial" => "smallint",
                _ => "integer",
            }
            .to_string();
        }
        c.data_type = converted;
        c.collation = None;
        if let Some(def) = &c.default {
            let n = def.trim().to_ascii_lowercase();
            if n.contains("::") || n.starts_with("(newid") || n.starts_with("newid") {
                c.default = crate::util::normalize_default(Some(def));
            }
        }
    }
    for idx in &mut t.indexes {
        idx.definition = None;
        if from != to {
            idx.method = idx.method.take().filter(|m| m.eq_ignore_ascii_case("btree"));
        }
    }
    t.triggers.clear();
    t
}

#[cfg(test)]
pub(crate) mod tests;
