//! Schema comparison ("compare schemas") and migration script generation ("synchronize").
//!
//! [`diff_schemas`] computes the changes needed to make a *target* schema match a *source*
//! schema; [`migration_script`] renders them as ordered statements for a dialect.

use std::collections::HashMap;

use tabularium_core::{
    CheckInfo, ColumnInfo, Dialect, ForeignKeyInfo, IndexInfo, KeyInfo, ObjectKind, ObjectRef,
    RoutineInfo, SchemaSnapshot, TableInfo,
};

use crate::ddl;
use crate::util::{
    canonical_type, is_nextval, is_serial_type, lists_eq, names_eq, normalize_default,
    normalize_expr, parse_type,
};

/// One change to a table.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // variant names are self-describing
pub enum TableChange {
    AddColumn(ColumnInfo),
    /// Column name.
    DropColumn(String),
    /// Type, nullability, default, identity, generation, comment, collation or position changed.
    AlterColumn { old: ColumnInfo, new: ColumnInfo },
    AddIndex(IndexInfo),
    DropIndex(IndexInfo),
    AddForeignKey(ForeignKeyInfo),
    DropForeignKey(ForeignKeyInfo),
    /// Added (`old` None), dropped (`new` None) or changed primary key.
    PrimaryKeyChanged { old: Option<KeyInfo>, new: Option<KeyInfo> },
    AddUnique(KeyInfo),
    DropUnique(KeyInfo),
    AddCheck(CheckInfo),
    DropCheck(CheckInfo),
    /// Table comment changed.
    CommentChanged { old: Option<String>, new: Option<String> },
}

/// One change to a schema.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // variant names are self-describing
pub enum SchemaChange {
    CreateTable(TableInfo),
    DropTable(ObjectRef),
    /// `statements` are rendered for the dialect given to [`diff_schemas_for_dialect`]
    /// (Generic for [`diff_schemas`]); [`migration_script`] re-renders `changes` for its own
    /// dialect and only falls back to `statements` for SQLite table rebuilds.
    AlterTable { name: ObjectRef, changes: Vec<TableChange>, statements: Vec<String> },
    CreateView(TableInfo),
    DropView(ObjectRef),
    ReplaceView(TableInfo),
    CreateRoutine(RoutineInfo),
    DropRoutine(RoutineInfo),
    ReplaceRoutine(RoutineInfo),
}

fn key_desc(k: &KeyInfo) -> String {
    match &k.name {
        Some(n) => format!("{n} ({})", k.columns.join(", ")),
        None => format!("({})", k.columns.join(", ")),
    }
}

fn routine_kind(r: &RoutineInfo) -> &'static str {
    if r.object.kind == ObjectKind::Procedure {
        "procedure"
    } else {
        "function"
    }
}

impl SchemaChange {
    /// Human readable one-line description, e.g. `Alter table users: add column email`.
    pub fn description(&self) -> String {
        match self {
            SchemaChange::CreateTable(t) => format!("Create table {}", t.object.display_name()),
            SchemaChange::DropTable(o) => format!("Drop table {}", o.display_name()),
            SchemaChange::AlterTable { name, changes, .. } => format!(
                "Alter table {}: {}",
                name.display_name(),
                changes.iter().map(TableChange::description).collect::<Vec<_>>().join("; ")
            ),
            SchemaChange::CreateView(t) => format!("Create view {}", t.object.display_name()),
            SchemaChange::DropView(o) => format!("Drop view {}", o.display_name()),
            SchemaChange::ReplaceView(t) => format!("Replace view {}", t.object.display_name()),
            SchemaChange::CreateRoutine(r) => {
                format!("Create {} {}", routine_kind(r), r.object.display_name())
            }
            SchemaChange::DropRoutine(r) => {
                format!("Drop {} {}", routine_kind(r), r.object.display_name())
            }
            SchemaChange::ReplaceRoutine(r) => {
                format!("Replace {} {}", routine_kind(r), r.object.display_name())
            }
        }
    }

    /// Display name (`schema.name`) of the affected object.
    pub fn object_name(&self) -> String {
        match self {
            SchemaChange::CreateTable(t) | SchemaChange::CreateView(t) | SchemaChange::ReplaceView(t) => {
                t.object.display_name()
            }
            SchemaChange::DropTable(o) | SchemaChange::DropView(o) => o.display_name(),
            SchemaChange::AlterTable { name, .. } => name.display_name(),
            SchemaChange::CreateRoutine(r) | SchemaChange::DropRoutine(r) | SchemaChange::ReplaceRoutine(r) => {
                r.object.display_name()
            }
        }
    }
}

impl TableChange {
    /// Human readable description, e.g. `alter column age: type integer -> bigint`.
    pub fn description(&self) -> String {
        match self {
            TableChange::AddColumn(c) => format!("add column {} {}", c.name, c.data_type),
            TableChange::DropColumn(n) => format!("drop column {n}"),
            TableChange::AlterColumn { old, new } => {
                let mut parts = Vec::new();
                if canonical_type(&old.data_type) != canonical_type(&new.data_type) {
                    parts.push(format!("type {} -> {}", old.data_type, new.data_type));
                }
                if old.nullable != new.nullable {
                    parts.push(if new.nullable { "nullable".into() } else { "not null".into() });
                }
                if normalize_default(old.default.as_deref()) != normalize_default(new.default.as_deref()) {
                    parts.push(format!(
                        "default {} -> {}",
                        old.default.as_deref().unwrap_or("none"),
                        new.default.as_deref().unwrap_or("none")
                    ));
                }
                if effective_auto(old) != effective_auto(new) {
                    parts.push(if effective_auto(new) { "auto-increment".into() } else { "no auto-increment".into() });
                }
                if old.generated != new.generated {
                    parts.push("generated expression".into());
                }
                if old.comment != new.comment {
                    parts.push("comment".into());
                }
                if old.collation != new.collation {
                    parts.push("collation".into());
                }
                if parts.is_empty() && old.ordinal != new.ordinal {
                    parts.push("position".into());
                }
                format!("alter column {}: {}", new.name, parts.join(", "))
            }
            TableChange::AddIndex(i) => format!("add index {}", i.name),
            TableChange::DropIndex(i) => format!("drop index {}", i.name),
            TableChange::AddForeignKey(f) => format!(
                "add foreign key {}({}) -> {}",
                f.name.as_deref().map(|n| format!("{n} ")).unwrap_or_default(),
                f.columns.join(", "),
                f.ref_table
            ),
            TableChange::DropForeignKey(f) => format!(
                "drop foreign key {}({}) -> {}",
                f.name.as_deref().map(|n| format!("{n} ")).unwrap_or_default(),
                f.columns.join(", "),
                f.ref_table
            ),
            TableChange::PrimaryKeyChanged { old, new } => match (old, new) {
                (None, Some(n)) => format!("add primary key {}", key_desc(n)),
                (Some(o), None) => format!("drop primary key {}", key_desc(o)),
                (Some(o), Some(n)) => format!("change primary key {} -> {}", key_desc(o), key_desc(n)),
                (None, None) => "primary key unchanged".into(),
            },
            TableChange::AddUnique(k) => format!("add unique {}", key_desc(k)),
            TableChange::DropUnique(k) => format!("drop unique {}", key_desc(k)),
            TableChange::AddCheck(c) => format!("add check {}", c.name.as_deref().unwrap_or(&c.expression)),
            TableChange::DropCheck(c) => format!("drop check {}", c.name.as_deref().unwrap_or(&c.expression)),
            TableChange::CommentChanged { .. } => "change comment".into(),
        }
    }
}

/// Result of [`diff_schemas`].
#[derive(Debug, Clone, Default)]
pub struct SchemaDiff {
    /// Changes in application order.
    pub changes: Vec<SchemaChange>,
}

impl SchemaDiff {
    /// `true` when the schemas match.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Comparison options.
#[derive(Debug, Clone)]
pub struct DiffOptions {
    /// Match object/column names case-insensitively.
    pub ignore_case: bool,
    /// Ignore table and column comments.
    pub ignore_comments: bool,
    /// Do not report columns whose only difference is their position.
    pub ignore_column_order: bool,
    /// Compare functions and procedures.
    pub compare_routines: bool,
    /// Compare (non-constraint) indexes.
    pub compare_indexes: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            ignore_case: false,
            ignore_comments: false,
            ignore_column_order: true,
            compare_routines: true,
            compare_indexes: true,
        }
    }
}

/// Auto-increment either flagged, spelled as serial, or expressed by a `nextval()` default.
fn effective_auto(c: &ColumnInfo) -> bool {
    c.auto_increment || is_serial_type(&c.data_type) || is_nextval(c.default.as_deref()) || parse_type(&c.data_type).auto
}

fn columns_differ(old: &ColumnInfo, new: &ColumnInfo, opts: &DiffOptions) -> bool {
    if canonical_type(&old.data_type) != canonical_type(&new.data_type) || old.nullable != new.nullable {
        return true;
    }
    let (oa, na) = (effective_auto(old), effective_auto(new));
    if oa != na {
        return true;
    }
    // Sequence defaults are implementation details of auto-increment columns.
    let def = |c: &ColumnInfo| if is_nextval(c.default.as_deref()) { None } else { normalize_default(c.default.as_deref()) };
    if def(old) != def(new) {
        return true;
    }
    if old.generated.as_deref().map(normalize_expr) != new.generated.as_deref().map(normalize_expr) {
        return true;
    }
    if !opts.ignore_comments && old.comment.as_deref().filter(|c| !c.is_empty()) != new.comment.as_deref().filter(|c| !c.is_empty()) {
        return true;
    }
    if let (Some(a), Some(b)) = (&old.collation, &new.collation) {
        if !a.eq_ignore_ascii_case(b) {
            return true;
        }
    }
    false
}

fn fk_same(a: &ForeignKeyInfo, b: &ForeignKeyInfo, ic: bool) -> bool {
    lists_eq(&a.columns, &b.columns, ic)
        && names_eq(&a.ref_table, &b.ref_table, ic)
        && lists_eq(&a.ref_columns, &b.ref_columns, ic)
        && a.on_delete == b.on_delete
        && a.on_update == b.on_update
}

fn index_same(a: &IndexInfo, b: &IndexInfo, ic: bool) -> bool {
    let m = |i: &IndexInfo| {
        i.method.as_deref().map(str::to_ascii_lowercase).filter(|m| m != "btree").unwrap_or_default()
    };
    // Columns may be expressions: compare normalised.
    let cols = |i: &IndexInfo| i.columns.iter().map(|c| normalize_expr(c)).collect::<Vec<_>>();
    let _ = ic; // expressions are compared case-insensitively
    cols(a) == cols(b)
        && a.unique == b.unique
        && m(a) == m(b)
        && a.predicate.as_deref().map(normalize_expr) == b.predicate.as_deref().map(normalize_expr)
}

fn key_set(k: &KeyInfo, ic: bool) -> Vec<String> {
    let mut v: Vec<String> =
        k.columns.iter().map(|c| if ic { c.to_ascii_lowercase() } else { c.clone() }).collect();
    v.sort();
    v
}

/// Changes turning table `old` into table `new`. Used by [`ddl::alter_table_ddl`] and
/// [`diff_schemas`]. Constraints are matched by content (FKs by name when both are named),
/// indexes by name and definition.
pub fn table_changes(old: &TableInfo, new: &TableInfo, opts: &DiffOptions) -> Vec<TableChange> {
    let ic = opts.ignore_case;
    let mut out = Vec::new();

    // columns
    let find = |cols: &[ColumnInfo], name: &str| cols.iter().position(|c| names_eq(&c.name, name, ic));
    for oc in &old.columns {
        if find(&new.columns, &oc.name).is_none() {
            out.push(TableChange::DropColumn(oc.name.clone()));
        }
    }
    let common_old: Vec<&str> = old
        .columns
        .iter()
        .filter(|c| find(&new.columns, &c.name).is_some())
        .map(|c| c.name.as_str())
        .collect();
    let common_new: Vec<&str> = new
        .columns
        .iter()
        .filter(|c| find(&old.columns, &c.name).is_some())
        .map(|c| c.name.as_str())
        .collect();
    for nc in &new.columns {
        match find(&old.columns, &nc.name) {
            None => out.push(TableChange::AddColumn(nc.clone())),
            Some(i) => {
                let oc = &old.columns[i];
                let moved = !opts.ignore_column_order
                    && common_old.iter().position(|n| names_eq(n, &oc.name, ic))
                        != common_new.iter().position(|n| names_eq(n, &nc.name, ic));
                if columns_differ(oc, nc, opts) || moved {
                    let mut o = oc.clone();
                    let mut n = nc.clone();
                    if opts.ignore_comments {
                        n.comment = o.comment.clone();
                    }
                    if moved && o.ordinal == n.ordinal {
                        // make the move visible to renderers (MySQL emits AFTER/FIRST)
                        o.ordinal = n.ordinal + 1;
                    }
                    out.push(TableChange::AlterColumn { old: o, new: n });
                }
            }
        }
    }

    // primary key
    let pk_cols = |t: &TableInfo| t.primary_key.as_ref().filter(|k| !k.columns.is_empty()).cloned();
    let (opk, npk) = (pk_cols(old), pk_cols(new));
    let pk_eq = match (&opk, &npk) {
        (None, None) => true,
        (Some(a), Some(b)) => lists_eq(&a.columns, &b.columns, ic),
        _ => false,
    };
    if !pk_eq {
        out.push(TableChange::PrimaryKeyChanged { old: opk, new: npk });
    }

    // unique keys (by column set)
    for uk in &old.unique_keys {
        if !new.unique_keys.iter().any(|n| key_set(n, ic) == key_set(uk, ic)) {
            out.push(TableChange::DropUnique(uk.clone()));
        }
    }
    for uk in &new.unique_keys {
        if !old.unique_keys.iter().any(|o| key_set(o, ic) == key_set(uk, ic)) {
            out.push(TableChange::AddUnique(uk.clone()));
        }
    }

    // checks (by normalised expression)
    for ck in &old.checks {
        if !new.checks.iter().any(|n| normalize_expr(&n.expression) == normalize_expr(&ck.expression)) {
            out.push(TableChange::DropCheck(ck.clone()));
        }
    }
    for ck in &new.checks {
        if !old.checks.iter().any(|o| normalize_expr(&o.expression) == normalize_expr(&ck.expression)) {
            out.push(TableChange::AddCheck(ck.clone()));
        }
    }

    // indexes
    if opts.compare_indexes {
        let oi = ddl::plain_indexes(old);
        let ni = ddl::plain_indexes(new);
        for o in &oi {
            let keep = ni.iter().any(|n| names_eq(&n.name, &o.name, ic) && index_same(o, n, ic));
            if !keep {
                out.push(TableChange::DropIndex((*o).clone()));
            }
        }
        for n in &ni {
            let exists = oi.iter().any(|o| names_eq(&n.name, &o.name, ic) && index_same(o, n, ic));
            if !exists {
                out.push(TableChange::AddIndex((*n).clone()));
            }
        }
    }

    // foreign keys
    let matches_fk = |a: &ForeignKeyInfo, b: &ForeignKeyInfo| match (&a.name, &b.name) {
        (Some(x), Some(y)) => names_eq(x, y, ic) && fk_same(a, b, ic),
        _ => fk_same(a, b, ic),
    };
    for o in &old.foreign_keys {
        if !new.foreign_keys.iter().any(|n| matches_fk(o, n)) {
            out.push(TableChange::DropForeignKey(o.clone()));
        }
    }
    for n in &new.foreign_keys {
        if !old.foreign_keys.iter().any(|o| matches_fk(o, n)) {
            out.push(TableChange::AddForeignKey(n.clone()));
        }
    }

    // comment
    if !opts.ignore_comments {
        let norm = |c: &Option<String>| c.clone().filter(|c| !c.is_empty());
        if norm(&old.comment) != norm(&new.comment) {
            out.push(TableChange::CommentChanged { old: norm(&old.comment), new: norm(&new.comment) });
        }
    }
    out
}

/// Remove `schema.` qualifiers (plain or quoted) from a definition so that definitions from
/// different schemas compare equal.
fn strip_schema(def: &str, schema: Option<&str>) -> String {
    let Some(s) = schema.filter(|s| !s.is_empty()) else { return def.to_string() };
    let mut out = def.to_string();
    for pat in [format!("{s}."), format!("\"{s}\"."), format!("`{s}`."), format!("[{s}].")] {
        out = replace_ci(&out, &pat, "");
    }
    out
}

fn replace_ci(haystack: &str, needle: &str, with: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let lower_h = haystack.to_ascii_lowercase();
    let lower_n = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut last = 0;
    for (i, _) in lower_h.match_indices(&lower_n) {
        if i < last {
            continue;
        }
        // require an identifier boundary before the match
        let before = haystack[..i].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        out.push_str(&haystack[last..i]);
        out.push_str(with);
        last = i + needle.len();
    }
    out.push_str(&haystack[last..]);
    out
}

fn name_key(name: &str, ic: bool) -> String {
    if ic {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

fn routine_key(r: &RoutineInfo, ic: bool) -> String {
    let sig = r.signature.as_deref().map(normalize_expr).unwrap_or_default();
    format!("{}{}", name_key(&r.object.name, ic), sig)
}

/// Changes needed to make `target` match `source`, with `AlterTable::statements` rendered
/// for the Generic dialect. See [`diff_schemas_for_dialect`].
pub fn diff_schemas(source: &SchemaSnapshot, target: &SchemaSnapshot, opts: &DiffOptions) -> SchemaDiff {
    diff_schemas_for_dialect(source, target, opts, Dialect::Generic)
}

/// Changes needed to make `target` match `source`. Objects are matched by name only (schema
/// names are ignored since the compared schemas usually differ). Type spellings, defaults,
/// check expressions and view bodies are normalised before comparing. `AlterTable::statements`
/// are rendered for `dialect` (needed for SQLite rebuilds in [`migration_script`]).
///
/// Change order: table drops (dependents first), table creates (FK targets first), alters,
/// view drops/creates/replacements, routine drops/creates/replacements.
pub fn diff_schemas_for_dialect(
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
    opts: &DiffOptions,
    dialect: Dialect,
) -> SchemaDiff {
    let ic = opts.ignore_case;
    let mut changes = Vec::new();
    let src_tables: Vec<&TableInfo> = source.tables.iter().filter(|t| !ddl::is_view(t)).collect();
    let tgt_tables: Vec<&TableInfo> = target.tables.iter().filter(|t| !ddl::is_view(t)).collect();
    let tgt_map: HashMap<String, &TableInfo> =
        tgt_tables.iter().map(|t| (name_key(&t.object.name, ic), *t)).collect();
    let src_map: HashMap<String, &TableInfo> =
        src_tables.iter().map(|t| (name_key(&t.object.name, ic), *t)).collect();

    // drops: reverse FK order so dependents go first
    let dropped: Vec<&TableInfo> = tgt_tables
        .iter()
        .filter(|t| !src_map.contains_key(&name_key(&t.object.name, ic)))
        .copied()
        .collect();
    for i in ddl::fk_order(&dropped).into_iter().rev() {
        changes.push(SchemaChange::DropTable(dropped[i].object.clone()));
    }
    // creates: FK order
    let created: Vec<&TableInfo> = src_tables
        .iter()
        .filter(|t| !tgt_map.contains_key(&name_key(&t.object.name, ic)))
        .copied()
        .collect();
    let into_target = |t: &TableInfo| -> TableInfo {
        match target.schema.as_deref() {
            Some(schema) => {
                let mut t = retarget_table(t, Some(schema));
                t.object.database = target.tables.first().and_then(|x| x.object.database.clone());
                t
            }
            None => t.clone(),
        }
    };
    for i in ddl::fk_order(&created) {
        changes.push(SchemaChange::CreateTable(into_target(created[i])));
    }
    // alters
    for s in &src_tables {
        if let Some(t) = tgt_map.get(&name_key(&s.object.name, ic)) {
            let mut src = (*s).clone();
            src.object = t.object.clone();
            let tc = table_changes(t, &src, opts);
            if !tc.is_empty() {
                let statements = ddl::table_change_statements(&t.object, &tc, dialect, Some(t), Some(&src));
                changes.push(SchemaChange::AlterTable { name: t.object.clone(), changes: tc, statements });
            }
        }
    }

    // views
    let src_views: Vec<&TableInfo> = source.tables.iter().filter(|t| ddl::is_view(t)).collect();
    let tgt_views: Vec<&TableInfo> = target.tables.iter().filter(|t| ddl::is_view(t)).collect();
    let view_body = |v: &TableInfo, schema: Option<&str>| {
        normalize_expr(&strip_schema(v.view_definition.as_deref().unwrap_or(""), schema))
    };
    for t in &tgt_views {
        if !src_views.iter().any(|s| names_eq(&s.object.name, &t.object.name, ic)) {
            changes.push(SchemaChange::DropView(t.object.clone()));
        }
    }
    for i in ddl::view_order(&src_views) {
        let s = src_views[i];
        match tgt_views.iter().find(|t| names_eq(&s.object.name, &t.object.name, ic)) {
            None => changes.push(SchemaChange::CreateView(into_target(s))),
            Some(t) => {
                if view_body(s, source.schema.as_deref()) != view_body(t, target.schema.as_deref()) {
                    let mut v = s.clone();
                    v.object = t.object.clone();
                    changes.push(SchemaChange::ReplaceView(v));
                }
            }
        }
    }

    // routines
    if opts.compare_routines {
        let body = |r: &RoutineInfo, schema: Option<&str>| normalize_expr(&strip_schema(&r.definition, schema));
        for t in &target.routines {
            if !source.routines.iter().any(|s| routine_key(s, ic) == routine_key(t, ic)) {
                changes.push(SchemaChange::DropRoutine(t.clone()));
            }
        }
        for s in &source.routines {
            match target.routines.iter().find(|t| routine_key(s, ic) == routine_key(t, ic)) {
                None => changes.push(SchemaChange::CreateRoutine(s.clone())),
                Some(t) => {
                    if body(s, source.schema.as_deref()) != body(t, target.schema.as_deref()) {
                        changes.push(SchemaChange::ReplaceRoutine(s.clone()));
                    }
                }
            }
        }
    }
    SchemaDiff { changes }
}

fn retarget(obj: &ObjectRef, schema: Option<&str>) -> ObjectRef {
    match schema {
        Some(s) => ObjectRef { schema: Some(s.to_string()), database: None, ..obj.clone() },
        None => obj.clone(),
    }
}

fn retarget_fk(fk: &ForeignKeyInfo, from_schema: Option<&str>, schema: Option<&str>) -> ForeignKeyInfo {
    let mut fk = fk.clone();
    if let Some(s) = schema {
        if fk.ref_schema.is_none() || fk.ref_schema.as_deref() == from_schema {
            fk.ref_schema = Some(s.to_string());
        }
    }
    fk
}

fn retarget_table(t: &TableInfo, schema: Option<&str>) -> TableInfo {
    let mut t = t.clone();
    let from = t.object.schema.clone();
    t.object = retarget(&t.object, schema);
    t.foreign_keys = t.foreign_keys.iter().map(|fk| retarget_fk(fk, from.as_deref(), schema)).collect();
    t.columns = t.columns.iter().map(portable_column).collect();
    t
}

/// Sequence-backed auto-increment columns (`DEFAULT nextval('src.seq')`) would keep
/// referencing the source database's sequence; express them as identity/auto-increment
/// instead so the target gets its own.
fn portable_column(c: &ColumnInfo) -> ColumnInfo {
    let mut c = c.clone();
    if c.auto_increment && crate::util::is_nextval(c.default.as_deref()) {
        c.default = None;
        c.data_type = match c.data_type.to_ascii_lowercase().as_str() {
            "serial" | "serial4" => "integer".into(),
            "bigserial" | "serial8" => "bigint".into(),
            "smallserial" | "serial2" => "smallint".into(),
            _ => c.data_type,
        };
    }
    c
}

/// Ordered migration statements (bare, without terminators) for `diff`:
/// routine drops, view drops, FK drops, table drops, table creates (FK order; FKs added after
/// all creates and alters, except on SQLite where they stay inline), table alters, FK adds,
/// view creates/replacements, routine creates/replacements.
///
/// When `target_schema` is given, object names (and FK targets that pointed into the source
/// schema) are qualified with it. SQLite rebuilds use the statements stored in the diff, so
/// compute the diff with [`diff_schemas_for_dialect`] and `Dialect::Sqlite` for SQLite.
pub fn migration_script(diff: &SchemaDiff, dialect: Dialect, target_schema: Option<&str>) -> Vec<String> {
    let d = dialect;
    let schema = target_schema.filter(|s| !s.is_empty() && d.supports_schemas());
    let mut routine_drops = Vec::new();
    let mut view_drops = Vec::new();
    let mut fk_drops = Vec::new();
    let mut table_drops = Vec::new();
    let mut creates = Vec::new();
    let mut alters = Vec::new();
    let mut fk_adds = Vec::new();
    let mut views = Vec::new();
    let mut routines = Vec::new();

    let created: Vec<TableInfo> = diff
        .changes
        .iter()
        .filter_map(|c| match c {
            SchemaChange::CreateTable(t) => Some(retarget_table(t, schema)),
            _ => None,
        })
        .collect();
    let created_refs: Vec<&TableInfo> = created.iter().collect();
    for i in ddl::fk_order(&created_refs) {
        let t = created_refs[i];
        if d == Dialect::Sqlite {
            creates.extend(ddl::create_table_statements(t, d));
        } else {
            let mut bare = t.clone();
            bare.foreign_keys.clear();
            creates.extend(ddl::create_table_statements(&bare, d));
            for fk in &t.foreign_keys {
                fk_adds.push(ddl::add_foreign_key_statement(&t.object, fk, d));
            }
        }
    }

    for c in &diff.changes {
        match c {
            SchemaChange::CreateTable(_) => {}
            SchemaChange::DropTable(o) => {
                table_drops.push(ddl::drop_ddl(&retarget(o, schema), d, None, false));
            }
            SchemaChange::AlterTable { name, changes, statements } => {
                let obj = retarget(name, schema);
                let fixed: Vec<TableChange> = changes
                    .iter()
                    .map(|tc| match tc {
                        TableChange::AddForeignKey(fk) => {
                            TableChange::AddForeignKey(retarget_fk(fk, name.schema.as_deref(), schema))
                        }
                        TableChange::AddColumn(c) => TableChange::AddColumn(portable_column(c)),
                        TableChange::AlterColumn { old, new } => {
                            TableChange::AlterColumn { old: old.clone(), new: portable_column(new) }
                        }
                        other => other.clone(),
                    })
                    .collect();
                if d == Dialect::Sqlite {
                    let rendered = ddl::table_change_statements(&obj, &fixed, d, None, None);
                    let needs_rebuild = rendered.iter().any(|s| s.starts_with("--"));
                    if needs_rebuild && statements.iter().any(|s| s.starts_with("PRAGMA foreign_keys")) {
                        alters.extend(statements.iter().cloned());
                    } else {
                        alters.extend(rendered);
                    }
                    continue;
                }
                let (drop_fk, rest): (Vec<_>, Vec<_>) =
                    fixed.into_iter().partition(|tc| matches!(tc, TableChange::DropForeignKey(_)));
                let (add_fk, rest): (Vec<_>, Vec<_>) =
                    rest.into_iter().partition(|tc| matches!(tc, TableChange::AddForeignKey(_)));
                fk_drops.extend(ddl::table_change_statements(&obj, &drop_fk, d, None, None));
                alters.extend(ddl::table_change_statements(&obj, &rest, d, None, None));
                fk_adds.extend(ddl::table_change_statements(&obj, &add_fk, d, None, None));
            }
            SchemaChange::CreateView(v) => {
                let v = retarget_table(v, schema);
                views.extend(ddl::create_table_statements(&v, d));
            }
            SchemaChange::DropView(o) => {
                let mut o = retarget(o, schema);
                if o.kind == ObjectKind::Table {
                    o.kind = ObjectKind::View;
                }
                view_drops.push(ddl::drop_ddl(&o, d, None, false));
            }
            SchemaChange::ReplaceView(v) => {
                let v = retarget_table(v, schema);
                let body = v.view_definition.as_deref().unwrap_or("").trim_start().to_ascii_uppercase();
                let native_replace = matches!(d, Dialect::Postgres | Dialect::MySql | Dialect::MsSql | Dialect::Generic)
                    && v.object.kind != ObjectKind::MaterializedView
                    && !body.starts_with("CREATE");
                if native_replace {
                    views.push(replace_view_statement(&v, d));
                } else {
                    let mut o = v.object.clone();
                    if o.kind == ObjectKind::Table {
                        o.kind = ObjectKind::View;
                    }
                    views.push(ddl::drop_ddl(&o, d, None, false));
                    views.extend(ddl::create_table_statements(&v, d));
                }
            }
            SchemaChange::CreateRoutine(r) => routines.push(r.definition.trim().to_string()),
            SchemaChange::DropRoutine(r) => {
                routine_drops.push(ddl::drop_ddl(&retarget(&r.object, schema), d, r.signature.as_deref(), false));
            }
            SchemaChange::ReplaceRoutine(r) => {
                let upper = r.definition.to_ascii_uppercase();
                if !(upper.contains("OR REPLACE") || upper.contains("OR ALTER")) {
                    routines.push(ddl::drop_ddl(&retarget(&r.object, schema), d, r.signature.as_deref(), false));
                }
                routines.push(r.definition.trim().to_string());
            }
        }
    }
    let mut out = Vec::new();
    for part in [routine_drops, view_drops, fk_drops, table_drops, creates, alters, fk_adds, views, routines] {
        out.extend(part.into_iter().filter(|s| !s.trim().is_empty()));
    }
    out
}

fn replace_view_statement(v: &TableInfo, d: Dialect) -> String {
    let body = v.view_definition.as_deref().unwrap_or("").trim().trim_end_matches(';').trim();
    let kw = match d {
        Dialect::MsSql => "CREATE OR ALTER VIEW",
        _ => "CREATE OR REPLACE VIEW",
    };
    format!("{kw} {} AS\n{body}", crate::util::qname(&v.object, d))
}

#[cfg(test)]
mod tests;
