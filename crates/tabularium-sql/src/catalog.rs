//! Schema metadata used by completion and inspections.

use tabularium_core::ObjectKind;

/// Schema metadata available for completion and inspections (filled by the app from introspection).
#[derive(Debug, Clone, Default)]
pub struct SchemaCatalog {
    /// Schema used for unqualified names (e.g. `public`).
    pub default_schema: Option<String>,
    /// All known schema names.
    pub schemas: Vec<String>,
    /// Tables, views and other relations.
    pub tables: Vec<CatalogTable>,
    /// User-defined function names.
    pub functions: Vec<String>,
}

/// A table-like relation.
#[derive(Debug, Clone)]
pub struct CatalogTable {
    /// Owning schema (`None` for schema-less databases).
    pub schema: Option<String>,
    /// Relation name.
    pub name: String,
    /// Table, view, materialized view, ...
    pub kind: ObjectKind,
    /// Columns in ordinal order.
    pub columns: Vec<CatalogColumn>,
}

/// A column of a [`CatalogTable`].
#[derive(Debug, Clone)]
pub struct CatalogColumn {
    /// Column name.
    pub name: String,
    /// Database type name as reported by introspection.
    pub data_type: String,
}

fn opt_eq(a: Option<&str>, b: Option<&str>, ci: bool) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            if ci {
                a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
            } else {
                a == b
            }
        }
        (None, None) => true,
        _ => false,
    }
}

fn name_eq(a: &str, b: &str, ci: bool) -> bool {
    if ci {
        a.eq_ignore_ascii_case(b) || a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

impl SchemaCatalog {
    /// True when the catalog holds no relations.
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    /// Find a relation. Exact matches win over case-insensitive ones; with no `schema`
    /// the default schema is preferred, then any schema. A schema-qualified lookup also
    /// matches relations that carry no schema at all (schema-less databases).
    pub fn find_table(&self, schema: Option<&str>, name: &str) -> Option<&CatalogTable> {
        for ci in [false, true] {
            match schema {
                Some(s) => {
                    if let Some(t) = self
                        .tables
                        .iter()
                        .find(|t| name_eq(&t.name, name, ci) && opt_eq(t.schema.as_deref(), Some(s), ci))
                    {
                        return Some(t);
                    }
                }
                None => {
                    let def = self.default_schema.as_deref();
                    if let Some(t) = self.tables.iter().find(|t| {
                        name_eq(&t.name, name, ci) && (def.is_none() || opt_eq(t.schema.as_deref(), def, ci))
                    }) {
                        return Some(t);
                    }
                }
            }
        }
        for ci in [false, true] {
            let found = match schema {
                Some(_) => self.tables.iter().find(|t| t.schema.is_none() && name_eq(&t.name, name, ci)),
                None => self.tables.iter().find(|t| name_eq(&t.name, name, ci)),
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// Case-insensitive schema lookup; returns the canonical spelling.
    pub fn find_schema(&self, name: &str) -> Option<&str> {
        self.schemas
            .iter()
            .find(|s| s.as_str() == name)
            .or_else(|| self.schemas.iter().find(|s| name_eq(s, name, true)))
            .map(String::as_str)
    }

    /// Relations of `schema` (case-insensitive).
    pub fn tables_in_schema<'a>(&'a self, schema: &'a str) -> impl Iterator<Item = &'a CatalogTable> + 'a {
        self.tables
            .iter()
            .filter(move |t| t.schema.as_deref().is_some_and(|s| name_eq(s, schema, true)))
    }
}

impl CatalogTable {
    /// Case-insensitive column lookup (exact match preferred).
    pub fn find_column(&self, name: &str) -> Option<&CatalogColumn> {
        self.columns
            .iter()
            .find(|c| c.name == name)
            .or_else(|| self.columns.iter().find(|c| name_eq(&c.name, name, true)))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn col(name: &str, ty: &str) -> CatalogColumn {
        CatalogColumn { name: name.into(), data_type: ty.into() }
    }

    pub(crate) fn sample() -> SchemaCatalog {
        SchemaCatalog {
            default_schema: Some("public".into()),
            schemas: vec!["public".into(), "sales".into()],
            tables: vec![
                CatalogTable {
                    schema: Some("public".into()),
                    name: "users".into(),
                    kind: ObjectKind::Table,
                    columns: vec![col("id", "int"), col("name", "text"), col("email", "text"), col("createdAt", "timestamp")],
                },
                CatalogTable {
                    schema: Some("public".into()),
                    name: "orders".into(),
                    kind: ObjectKind::Table,
                    columns: vec![col("id", "int"), col("user_id", "int"), col("total", "numeric")],
                },
                CatalogTable {
                    schema: Some("public".into()),
                    name: "active_users".into(),
                    kind: ObjectKind::View,
                    columns: vec![col("id", "int")],
                },
                CatalogTable {
                    schema: Some("sales".into()),
                    name: "Invoices".into(),
                    kind: ObjectKind::Table,
                    columns: vec![col("invoice_id", "int"), col("amount", "numeric")],
                },
            ],
            functions: vec!["my_func".into()],
        }
    }

    #[test]
    fn find_table_lookup() {
        let c = sample();
        assert_eq!(c.find_table(None, "users").unwrap().name, "users");
        assert_eq!(c.find_table(None, "USERS").unwrap().name, "users");
        assert_eq!(c.find_table(Some("SALES"), "invoices").unwrap().name, "Invoices");
        assert_eq!(c.find_table(None, "invoices").unwrap().name, "Invoices");
        assert!(c.find_table(Some("public"), "invoices").is_none());
        assert!(c.find_table(None, "nope").is_none());
        assert_eq!(c.find_schema("Sales"), Some("sales"));
        assert_eq!(c.tables_in_schema("public").count(), 3);
        assert!(c.find_table(None, "users").unwrap().find_column("ID").is_some());
    }

    #[test]
    fn schemaless_catalog() {
        let c = SchemaCatalog {
            tables: vec![CatalogTable { schema: None, name: "t".into(), kind: ObjectKind::Table, columns: vec![] }],
            ..Default::default()
        };
        assert!(c.find_table(Some("main"), "t").is_some());
        assert!(c.find_table(None, "T").is_some());
    }
}
