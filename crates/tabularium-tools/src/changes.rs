//! Data editor pending changes -> DML (DataGrip "Submit").
//!
//! Rows without a primary key or NOT NULL unique key are identified by **all** their column
//! values. A portable "affect at most one row" limiter does not exist, so such an UPDATE or
//! DELETE affects every identical duplicate row; the editor should warn about this. Floating
//! point keys compare exactly and may not match. Changes with an empty key are skipped
//! rather than generating an unrestricted UPDATE/DELETE.

use tabularium_core::{ColumnMeta, Dialect, ObjectRef, TableInfo, Value};

use crate::util::{q, qname};

/// A pending edit in the data editor.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // fields are described on the variants
pub enum PendingChange {
    /// Set columns `set` on the row identified by `key` (see [`row_key`]).
    Update { key: Vec<(String, Value)>, set: Vec<(String, Value)> },
    /// Insert a row with the given column values.
    Insert { values: Vec<(String, Value)> },
    /// Delete the row identified by `key`.
    Delete { key: Vec<(String, Value)> },
}

/// Types that cannot be compared with `=` on at least one supported database.
fn uncomparable(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or("").trim();
    matches!(
        base,
        "json" | "jsonb" | "xml" | "ntext" | "image" | "bytea" | "blob" | "tinyblob" | "mediumblob"
            | "longblob" | "geometry" | "geography" | "point" | "polygon" | "line" | "box" | "circle"
            | "path" | "lseg" | "tsvector" | "hstore" | "sql_variant"
    ) || t.ends_with("[]")
        || t == "varbinary(max)"
        || t == "nvarchar(max)"
        || t == "varchar(max)"
}

/// Columns and values identifying `row` (whose cells correspond to `columns`):
/// [`TableInfo::row_identity`] when all its columns are present in the result, otherwise
/// every column except types that cannot be compared (JSON, XML, LOBs, geometry, arrays);
/// if that excludes everything, all columns.
pub fn row_key(table: &TableInfo, columns: &[ColumnMeta], row: &[Value]) -> Vec<(String, Value)> {
    let find = |name: &str| {
        columns
            .iter()
            .position(|c| c.name == name)
            .or_else(|| columns.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
    };
    if let Some(ident) = table.row_identity() {
        let idx: Option<Vec<usize>> = ident.iter().map(|c| find(c)).collect();
        if let Some(idx) = idx {
            return ident
                .iter()
                .zip(idx)
                .map(|(name, i)| (name.clone(), row.get(i).cloned().unwrap_or(Value::Null)))
                .collect();
        }
    }
    let all: Vec<(String, Value)> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.clone(), row.get(i).cloned().unwrap_or(Value::Null)))
        .collect();
    let comparable: Vec<(String, Value)> = columns
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            let ty = table.column(&c.name).map(|tc| tc.data_type.as_str()).unwrap_or(c.type_name.as_str());
            !uncomparable(ty)
        })
        .filter(|(i, _)| !matches!(row.get(*i), Some(Value::Json(_) | Value::Bytes(_) | Value::Array(_))))
        .map(|(i, c)| (c.name.clone(), row.get(i).cloned().unwrap_or(Value::Null)))
        .collect();
    if comparable.is_empty() {
        all
    } else {
        comparable
    }
}

fn where_clause(key: &[(String, Value)], d: Dialect) -> String {
    key.iter()
        .map(|(c, v)| {
            if v.is_null() {
                format!("{} IS NULL", q(d, c))
            } else {
                format!("{} = {}", q(d, c), v.to_sql_literal(d))
            }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn render(t: &str, change: &PendingChange, d: Dialect) -> Option<String> {
    match change {
        PendingChange::Update { key, set } => {
            if key.is_empty() || set.is_empty() {
                return None;
            }
            let sets: Vec<String> =
                set.iter().map(|(c, v)| format!("{} = {}", q(d, c), v.to_sql_literal(d))).collect();
            Some(format!("UPDATE {t} SET {} WHERE {}", sets.join(", "), where_clause(key, d)))
        }
        PendingChange::Insert { values } => {
            if values.is_empty() {
                return Some(match d {
                    Dialect::MySql => format!("INSERT INTO {t} () VALUES ()"),
                    _ => format!("INSERT INTO {t} DEFAULT VALUES"),
                });
            }
            let cols: Vec<String> = values.iter().map(|(c, _)| q(d, c)).collect();
            let vals: Vec<String> = values.iter().map(|(_, v)| v.to_sql_literal(d)).collect();
            Some(format!("INSERT INTO {t} ({}) VALUES ({})", cols.join(", "), vals.join(", ")))
        }
        PendingChange::Delete { key } => {
            if key.is_empty() {
                return None;
            }
            Some(format!("DELETE FROM {t} WHERE {}", where_clause(key, d)))
        }
    }
}

/// UPDATE/INSERT/DELETE statements (without terminators), in the order given. NULL key values
/// use `IS NULL`. Inserts include every given value; use [`change_statements_with_table`] to
/// let the database fill auto-increment/default/generated columns.
pub fn change_statements(table: &ObjectRef, changes: &[PendingChange], dialect: Dialect) -> Vec<String> {
    let t = qname(table, dialect);
    changes.iter().filter_map(|c| render(&t, c, dialect)).collect()
}

/// Like [`change_statements`], but using the table definition: inserts omit NULL values of
/// auto-increment columns and columns with a default, generated columns are never written,
/// and updates skip generated columns.
pub fn change_statements_with_table(table: &TableInfo, changes: &[PendingChange], dialect: Dialect) -> Vec<String> {
    let t = qname(&table.object, dialect);
    changes
        .iter()
        .filter_map(|c| {
            let fixed = match c {
                PendingChange::Insert { values } => PendingChange::Insert {
                    values: values
                        .iter()
                        .filter(|(name, v)| match table.column(name) {
                            Some(col) if col.generated.is_some() => false,
                            Some(col) if v.is_null() => !(col.auto_increment || col.default.is_some()),
                            _ => true,
                        })
                        .cloned()
                        .collect(),
                },
                PendingChange::Update { key, set } => PendingChange::Update {
                    key: key.clone(),
                    set: set
                        .iter()
                        .filter(|(name, _)| table.column(name).is_none_or(|col| col.generated.is_none()))
                        .cloned()
                        .collect(),
                },
                other => other.clone(),
            };
            render(&t, &fixed, dialect)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tabularium_core::{ColumnInfo, KeyInfo};

    fn table() -> TableInfo {
        let mut t = TableInfo::new(ObjectRef::table(Some("public"), "users"));
        t.columns = vec![
            ColumnInfo { name: "id".into(), data_type: "integer".into(), auto_increment: true, ..Default::default() },
            ColumnInfo { name: "name".into(), data_type: "text".into(), nullable: true, ..Default::default() },
            ColumnInfo {
                name: "status".into(),
                data_type: "varchar(10)".into(),
                default: Some("'new'".into()),
                ..Default::default()
            },
            ColumnInfo {
                name: "upper_name".into(),
                data_type: "text".into(),
                generated: Some("upper(name)".into()),
                nullable: true,
                ..Default::default()
            },
        ];
        t.primary_key = Some(KeyInfo { name: None, columns: vec!["id".into()] });
        t
    }

    fn metas() -> Vec<ColumnMeta> {
        vec![ColumnMeta::new("id", "int4"), ColumnMeta::new("name", "text"), ColumnMeta::new("status", "varchar")]
    }

    #[test]
    fn key_uses_pk() {
        let k = row_key(&table(), &metas(), &[Value::Int(7), "bob".into(), "x".into()]);
        assert_eq!(k, vec![("id".to_string(), Value::Int(7))]);
    }

    #[test]
    fn key_falls_back_to_comparable_columns() {
        let mut t = table();
        t.primary_key = None;
        let mut metas = metas();
        metas.push(ColumnMeta::new("doc", "jsonb"));
        let k = row_key(&t, &metas, &[Value::Int(7), "bob".into(), Value::Null, Value::Json(serde_json::json!({}))]);
        // jsonb cannot be compared with `=`
        assert_eq!(
            k,
            vec![
                ("id".to_string(), Value::Int(7)),
                ("name".to_string(), Value::Text("bob".into())),
                ("status".to_string(), Value::Null)
            ]
        );
    }

    #[test]
    fn key_missing_pk_column_uses_all() {
        let metas = vec![ColumnMeta::new("name", "varchar")];
        let k = row_key(&table(), &metas, &["bob".into()]);
        assert_eq!(k, vec![("name".to_string(), Value::Text("bob".into()))]);
    }

    #[test]
    fn statements() {
        let obj = ObjectRef::table(Some("public"), "users");
        let changes = vec![
            PendingChange::Update {
                key: vec![("id".into(), Value::Int(1)), ("name".into(), Value::Null)],
                set: vec![("name".into(), "O'Brien".into())],
            },
            PendingChange::Insert { values: vec![("id".into(), Value::Int(2)), ("name".into(), Value::Null)] },
            PendingChange::Delete { key: vec![("id".into(), Value::Int(3))] },
            PendingChange::Delete { key: vec![] },
        ];
        let s = change_statements(&obj, &changes, Dialect::Postgres);
        assert_eq!(
            s,
            vec![
                "UPDATE public.users SET name = 'O''Brien' WHERE id = 1 AND name IS NULL",
                "INSERT INTO public.users (id, name) VALUES (2, NULL)",
                "DELETE FROM public.users WHERE id = 3",
            ]
        );
        let s = change_statements(&ObjectRef::table(None, "Order"), &changes[2..3], Dialect::MySql);
        assert_eq!(s, vec!["DELETE FROM `Order` WHERE id = 3"]);
        let s = change_statements(&ObjectRef::table(Some("dbo"), "t"), &changes[1..2], Dialect::MsSql);
        assert_eq!(s, vec!["INSERT INTO dbo.t (id, name) VALUES (2, NULL)"]);
    }

    #[test]
    fn statements_with_table() {
        let changes = vec![
            PendingChange::Insert {
                values: vec![
                    ("id".into(), Value::Null),
                    ("name".into(), Value::Null),
                    ("status".into(), Value::Null),
                    ("upper_name".into(), "X".into()),
                ],
            },
            PendingChange::Insert {
                values: vec![("id".into(), Value::Int(5)), ("status".into(), "old".into())],
            },
            PendingChange::Update {
                key: vec![("id".into(), Value::Int(1))],
                set: vec![("upper_name".into(), "X".into()), ("name".into(), "x".into())],
            },
            PendingChange::Insert { values: vec![("id".into(), Value::Null)] },
        ];
        let s = change_statements_with_table(&table(), &changes, Dialect::Sqlite);
        assert_eq!(
            s,
            vec![
                "INSERT INTO public.users (name) VALUES (NULL)",
                "INSERT INTO public.users (id, status) VALUES (5, 'old')",
                "UPDATE public.users SET name = 'x' WHERE id = 1",
                "INSERT INTO public.users DEFAULT VALUES",
            ]
        );
    }
}
