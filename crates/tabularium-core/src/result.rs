use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::value::Value;

/// Column metadata of a result set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColumnMeta {
    pub name: String,
    /// Database type name as reported by the driver (`int4`, `VARCHAR`, ...).
    pub type_name: String,
    pub nullable: Option<bool>,
    /// Originating table, when the driver can tell (used to make results editable).
    pub table: Option<String>,
}

impl ColumnMeta {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self { name: name.into(), type_name: type_name.into(), nullable: None, table: None }
    }

    /// Right-align numbers in grids.
    pub fn is_numeric_type(&self) -> bool {
        let t = self.type_name.to_ascii_lowercase();
        ["int", "float", "double", "real", "numeric", "decimal", "serial", "money", "number"]
            .iter()
            .any(|k| t.contains(k))
            && !t.contains("interval")
            && !t.contains("point")
    }
}

/// Outcome of executing one statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StatementResult {
    /// The statement text that was executed.
    pub sql: String,
    /// Empty for statements that return no result set.
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    /// Rows inserted/updated/deleted, for DML.
    pub rows_affected: Option<u64>,
    /// `true` when `ExecOptions::max_rows` cut the result short.
    pub truncated: bool,
    pub elapsed: Duration,
    /// Server notices / warnings emitted while running the statement.
    pub notices: Vec<String>,
}

impl StatementResult {
    pub fn has_result_set(&self) -> bool {
        !self.columns.is_empty()
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// One-line summary, e.g. "42 rows retrieved in 12 ms" / "3 rows affected in 4 ms".
    pub fn summary(&self) -> String {
        let ms = self.elapsed.as_secs_f64() * 1000.0;
        let time = if ms >= 1000.0 { format!("{:.2} s", ms / 1000.0) } else { format!("{ms:.0} ms") };
        if self.has_result_set() {
            let more = if self.truncated { "+" } else { "" };
            let n = self.rows.len();
            format!("{n}{more} row{} retrieved in {time}", if n == 1 { "" } else { "s" })
        } else if let Some(n) = self.rows_affected {
            format!("{n} row{} affected in {time}", if n == 1 { "" } else { "s" })
        } else {
            format!("completed in {time}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary() {
        let r = StatementResult {
            columns: vec![ColumnMeta::new("a", "int")],
            rows: vec![vec![Value::Int(1)]],
            elapsed: Duration::from_millis(5),
            ..Default::default()
        };
        assert_eq!(r.summary(), "1 row retrieved in 5 ms");
        let r = StatementResult { rows_affected: Some(3), ..Default::default() };
        assert_eq!(r.summary(), "3 rows affected in 0 ms");
    }
}
