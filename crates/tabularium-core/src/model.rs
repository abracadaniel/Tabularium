//! Database object model produced by driver introspection and consumed by the explorer,
//! DDL generator, schema comparison, ER diagrams and code completion.

use serde::{Deserialize, Serialize};

/// Kind of a database object shown in the explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectKind {
    Database,
    Schema,
    Table,
    View,
    MaterializedView,
    ForeignTable,
    Sequence,
    Function,
    Procedure,
    Trigger,
    Index,
    Type,
    Event,
    Synonym,
}

impl ObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Database => "database",
            ObjectKind::Schema => "schema",
            ObjectKind::Table => "table",
            ObjectKind::View => "view",
            ObjectKind::MaterializedView => "materialized view",
            ObjectKind::ForeignTable => "foreign table",
            ObjectKind::Sequence => "sequence",
            ObjectKind::Function => "function",
            ObjectKind::Procedure => "procedure",
            ObjectKind::Trigger => "trigger",
            ObjectKind::Index => "index",
            ObjectKind::Type => "type",
            ObjectKind::Event => "event",
            ObjectKind::Synonym => "synonym",
        }
    }

    /// Plural label for explorer folders ("tables", "views", ...).
    pub fn folder_label(self) -> &'static str {
        match self {
            ObjectKind::Database => "databases",
            ObjectKind::Schema => "schemas",
            ObjectKind::Table => "tables",
            ObjectKind::View => "views",
            ObjectKind::MaterializedView => "materialized views",
            ObjectKind::ForeignTable => "foreign tables",
            ObjectKind::Sequence => "sequences",
            ObjectKind::Function => "functions",
            ObjectKind::Procedure => "procedures",
            ObjectKind::Trigger => "triggers",
            ObjectKind::Index => "indexes",
            ObjectKind::Type => "types",
            ObjectKind::Event => "events",
            ObjectKind::Synonym => "synonyms",
        }
    }

    /// Objects whose rows can be browsed in the data editor.
    pub fn has_data(self) -> bool {
        matches!(
            self,
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::ForeignTable
        )
    }

    /// Objects with a column list.
    pub fn has_columns(self) -> bool {
        self.has_data()
    }
}

/// Fully-qualified reference to a database object.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    /// Catalog/database. `None` means "the connection's current database".
    pub database: Option<String>,
    /// Schema/namespace. `None` for databases without schemas (SQLite uses `Some("main")`).
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
}

impl ObjectRef {
    pub fn new(schema: Option<&str>, name: impl Into<String>, kind: ObjectKind) -> Self {
        Self { database: None, schema: schema.map(str::to_string), name: name.into(), kind }
    }

    pub fn table(schema: Option<&str>, name: impl Into<String>) -> Self {
        Self::new(schema, name, ObjectKind::Table)
    }

    /// `schema.name` without quoting, for display.
    pub fn display_name(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }

    pub fn qualified(&self, dialect: crate::Dialect) -> String {
        dialect.qualified(self.schema.as_deref(), &self.name)
    }
}

/// Lightweight object entry used to populate the explorer tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub object: ObjectRef,
    pub comment: Option<String>,
    /// For functions/procedures: argument signature `(a integer, b text)`, used to
    /// distinguish overloads and to build DDL/DROP statements.
    pub signature: Option<String>,
    /// For triggers/indexes: owning table name.
    pub parent: Option<String>,
    /// Approximate row count when cheaply available.
    pub row_estimate: Option<i64>,
}

impl ObjectInfo {
    pub fn new(object: ObjectRef) -> Self {
        Self { object, comment: None, signature: None, parent: None, row_estimate: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColumnInfo {
    pub name: String,
    /// Full type as the database spells it, e.g. `character varying(255)`, `int unsigned`.
    pub data_type: String,
    pub nullable: bool,
    /// Default expression, verbatim.
    pub default: Option<String>,
    /// Identity / auto-increment / serial column.
    pub auto_increment: bool,
    /// Generation expression for computed columns.
    pub generated: Option<String>,
    pub comment: Option<String>,
    pub collation: Option<String>,
    /// 1-based ordinal position.
    pub ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KeyInfo {
    pub name: Option<String>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IndexInfo {
    pub name: String,
    /// Column names or expressions, in index order.
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    /// Access method: btree, hash, gin, gist, fulltext...
    pub method: Option<String>,
    /// Partial index predicate.
    pub predicate: Option<String>,
    /// Native definition when the server can provide it (Postgres `pg_get_indexdef`).
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FkAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl FkAction {
    pub fn sql(self) -> &'static str {
        match self {
            FkAction::NoAction => "NO ACTION",
            FkAction::Restrict => "RESTRICT",
            FkAction::Cascade => "CASCADE",
            FkAction::SetNull => "SET NULL",
            FkAction::SetDefault => "SET DEFAULT",
        }
    }

    pub fn parse(s: &str) -> FkAction {
        match s.trim().to_ascii_uppercase().replace('_', " ").as_str() {
            "RESTRICT" | "R" => FkAction::Restrict,
            "CASCADE" | "C" => FkAction::Cascade,
            "SET NULL" | "N" => FkAction::SetNull,
            "SET DEFAULT" | "D" => FkAction::SetDefault,
            _ => FkAction::NoAction,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ForeignKeyInfo {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: FkAction,
    pub on_delete: FkAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CheckInfo {
    pub name: Option<String>,
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TriggerInfo {
    pub name: String,
    /// BEFORE / AFTER / INSTEAD OF
    pub timing: String,
    /// INSERT / UPDATE / DELETE (possibly several, joined by " OR ")
    pub events: String,
    /// Native definition (full CREATE TRIGGER) when available.
    pub definition: Option<String>,
}

/// Full description of a table or view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableInfo {
    pub object: ObjectRef,
    pub columns: Vec<ColumnInfo>,
    pub primary_key: Option<KeyInfo>,
    pub unique_keys: Vec<KeyInfo>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub checks: Vec<CheckInfo>,
    pub triggers: Vec<TriggerInfo>,
    pub comment: Option<String>,
    /// For views: the SELECT body.
    pub view_definition: Option<String>,
    pub row_estimate: Option<i64>,
}

impl TableInfo {
    pub fn new(object: ObjectRef) -> Self {
        Self {
            object,
            columns: vec![],
            primary_key: None,
            unique_keys: vec![],
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            comment: None,
            view_definition: None,
            row_estimate: None,
        }
    }

    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
            .or_else(|| self.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
    }

    /// Columns that uniquely identify a row, for generating UPDATE/DELETE from the
    /// data editor: the primary key, else the first unique key whose columns are all
    /// NOT NULL. `None` means rows must be identified by all column values.
    pub fn row_identity(&self) -> Option<Vec<String>> {
        if let Some(pk) = &self.primary_key {
            if !pk.columns.is_empty() {
                return Some(pk.columns.clone());
            }
        }
        self.unique_keys
            .iter()
            .find(|k| {
                !k.columns.is_empty()
                    && k.columns.iter().all(|c| self.column(c).is_some_and(|col| !col.nullable))
            })
            .map(|k| k.columns.clone())
    }
}

/// Function / procedure description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutineInfo {
    pub object: ObjectRef,
    pub signature: Option<String>,
    pub return_type: Option<String>,
    pub language: Option<String>,
    /// Full CREATE statement.
    pub definition: String,
}

/// Complete snapshot of a schema: used for schema comparison, ER diagrams and completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SchemaSnapshot {
    pub schema: Option<String>,
    pub tables: Vec<TableInfo>,
    pub routines: Vec<RoutineInfo>,
    pub sequences: Vec<ObjectInfo>,
}

impl SchemaSnapshot {
    pub fn table(&self, name: &str) -> Option<&TableInfo> {
        self.tables.iter().find(|t| t.object.name == name)
    }
}

/// Database server user/role, for the "users & roles" explorer node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleInfo {
    pub name: String,
    pub superuser: bool,
    pub can_login: bool,
    pub member_of: Vec<String>,
}

/// A live server session, for the session manager / process list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub user: Option<String>,
    pub database: Option<String>,
    pub client: Option<String>,
    pub state: Option<String>,
    pub query: Option<String>,
    pub started: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_identity_prefers_pk_then_not_null_unique() {
        let mut t = TableInfo::new(ObjectRef::table(None, "t"));
        t.columns = vec![
            ColumnInfo { name: "a".into(), nullable: true, ..Default::default() },
            ColumnInfo { name: "b".into(), nullable: false, ..Default::default() },
        ];
        t.unique_keys = vec![
            KeyInfo { name: None, columns: vec!["a".into()] },
            KeyInfo { name: None, columns: vec!["b".into()] },
        ];
        assert_eq!(t.row_identity(), Some(vec!["b".to_string()]));
        t.primary_key = Some(KeyInfo { name: None, columns: vec!["a".into()] });
        assert_eq!(t.row_identity(), Some(vec!["a".to_string()]));
    }
}
