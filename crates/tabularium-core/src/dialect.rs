use serde::{Deserialize, Serialize};

/// SQL dialect: controls identifier quoting, literal syntax and generated SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
    MsSql,
    #[default]
    Generic,
}

impl Dialect {
    pub const ALL: [Dialect; 5] =
        [Dialect::Postgres, Dialect::MySql, Dialect::Sqlite, Dialect::MsSql, Dialect::Generic];

    pub fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "PostgreSQL",
            Dialect::MySql => "MySQL",
            Dialect::Sqlite => "SQLite",
            Dialect::MsSql => "SQL Server",
            Dialect::Generic => "Generic SQL",
        }
    }

    /// Quote an identifier unconditionally.
    pub fn quote_ident(self, ident: &str) -> String {
        match self {
            Dialect::MySql => format!("`{}`", ident.replace('`', "``")),
            Dialect::MsSql => format!("[{}]", ident.replace(']', "]]")),
            _ => format!("\"{}\"", ident.replace('"', "\"\"")),
        }
    }

    /// Quote an identifier only if it is not a plain lower/upper-case word
    /// (matching how the database would fold it) or is a reserved word.
    pub fn quote_ident_if_needed(self, ident: &str) -> String {
        if self.ident_is_plain(ident) {
            ident.to_string()
        } else {
            self.quote_ident(ident)
        }
    }

    fn ident_is_plain(self, ident: &str) -> bool {
        let mut chars = ident.chars();
        let Some(first) = chars.next() else { return false };
        if !(first.is_ascii_alphabetic() || first == '_') {
            return false;
        }
        let rest_ok = ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
        if !rest_ok || is_reserved_word(ident) {
            return false;
        }
        match self {
            // Postgres folds unquoted identifiers to lower case.
            Dialect::Postgres => !ident.chars().any(|c| c.is_ascii_uppercase()),
            _ => true,
        }
    }

    /// Qualified name `schema.name` with per-part quoting as needed.
    pub fn qualified(self, schema: Option<&str>, name: &str) -> String {
        match schema {
            Some(s) if !s.is_empty() => {
                format!("{}.{}", self.quote_ident_if_needed(s), self.quote_ident_if_needed(name))
            }
            _ => self.quote_ident_if_needed(name),
        }
    }

    /// Quote a string literal.
    pub fn quote_string(self, s: &str) -> String {
        match self {
            Dialect::MySql => {
                let escaped = s.replace('\\', "\\\\").replace('\'', "''");
                format!("'{escaped}'")
            }
            Dialect::MsSql => format!("N'{}'", s.replace('\'', "''")),
            _ => format!("'{}'", s.replace('\'', "''")),
        }
    }

    /// Hex blob literal.
    pub fn quote_bytes(self, b: &[u8]) -> String {
        let h = hex::encode(b);
        match self {
            Dialect::Postgres => format!("'\\x{h}'::bytea"),
            Dialect::MsSql => format!("0x{h}"),
            Dialect::MySql => format!("X'{h}'"),
            _ => format!("X'{h}'"),
        }
    }

    pub fn bool_literal(self, b: bool) -> &'static str {
        match (self, b) {
            (Dialect::Postgres | Dialect::Generic, true) => "TRUE",
            (Dialect::Postgres | Dialect::Generic, false) => "FALSE",
            (_, true) => "1",
            (_, false) => "0",
        }
    }

    /// Wrap a query so it returns at most `limit` rows starting at `offset`.
    pub fn paginate(self, select: &str, limit: u64, offset: u64) -> String {
        let select = select.trim().trim_end_matches(';');
        match self {
            Dialect::MsSql => {
                format!(
                    "SELECT * FROM ({select}) AS t ORDER BY (SELECT NULL) OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
                )
            }
            _ => format!("{select} LIMIT {limit} OFFSET {offset}"),
        }
    }

    /// `SELECT * FROM table [WHERE ..] [ORDER BY ..]` with paging applied.
    pub fn select_page(
        self,
        schema: Option<&str>,
        table: &str,
        filter: Option<&str>,
        order_by: Option<&str>,
        limit: u64,
        offset: u64,
    ) -> String {
        let mut sql = format!("SELECT * FROM {}", self.qualified(schema, table));
        if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
            sql.push_str(" WHERE ");
            sql.push_str(f);
        }
        let order = order_by.map(str::trim).filter(|o| !o.is_empty());
        match self {
            Dialect::MsSql => {
                sql.push_str(" ORDER BY ");
                sql.push_str(order.unwrap_or("(SELECT NULL)"));
                sql.push_str(&format!(" OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"));
            }
            _ => {
                if let Some(o) = order {
                    sql.push_str(" ORDER BY ");
                    sql.push_str(o);
                }
                sql.push_str(&format!(" LIMIT {limit}"));
                if offset > 0 {
                    sql.push_str(&format!(" OFFSET {offset}"));
                }
            }
        }
        sql
    }

    /// Statement prefix used to obtain an execution plan.
    pub fn explain_prefix(self, analyze: bool) -> Option<&'static str> {
        match (self, analyze) {
            (Dialect::Postgres, false) => Some("EXPLAIN (FORMAT TEXT, VERBOSE) "),
            (Dialect::Postgres, true) => Some("EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT, VERBOSE) "),
            (Dialect::MySql, false) => Some("EXPLAIN FORMAT=TREE "),
            (Dialect::MySql, true) => Some("EXPLAIN ANALYZE "),
            (Dialect::Sqlite, _) => Some("EXPLAIN QUERY PLAN "),
            (Dialect::MsSql, _) => None,
            (Dialect::Generic, _) => Some("EXPLAIN "),
        }
    }

    /// Statement separator used when generating scripts.
    pub fn batch_separator(self) -> &'static str {
        match self {
            Dialect::MsSql => "\nGO\n",
            _ => ";\n",
        }
    }

    pub fn supports_schemas(self) -> bool {
        !matches!(self, Dialect::Sqlite)
    }

    pub fn supports_transactional_ddl(self) -> bool {
        matches!(self, Dialect::Postgres | Dialect::Sqlite | Dialect::MsSql)
    }
}

/// Words that must be quoted when used as identifiers (union of common dialects, upper case).
pub const RESERVED_WORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "BETWEEN", "BY", "CASE", "CAST", "CHECK",
    "COLLATE", "COLUMN", "CONSTRAINT", "CREATE", "CROSS", "CURRENT_DATE", "CURRENT_TIME",
    "CURRENT_TIMESTAMP", "CURRENT_USER", "DEFAULT", "DELETE", "DESC", "DISTINCT", "DROP", "ELSE",
    "END", "EXCEPT", "EXISTS", "FALSE", "FETCH", "FOR", "FOREIGN", "FROM", "FULL", "GRANT",
    "GROUP", "HAVING", "IN", "INDEX", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "KEY",
    "LEFT", "LIKE", "LIMIT", "NATURAL", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER",
    "PRIMARY", "REFERENCES", "RIGHT", "SELECT", "SET", "TABLE", "THEN", "TO", "TRUE", "UNION",
    "UNIQUE", "UPDATE", "USER", "USING", "VALUES", "WHEN", "WHERE", "WINDOW", "WITH",
];

pub fn is_reserved_word(word: &str) -> bool {
    let upper = word.to_ascii_uppercase();
    RESERVED_WORDS.binary_search(&upper.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_sorted() {
        let mut sorted = RESERVED_WORDS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, RESERVED_WORDS);
    }

    #[test]
    fn quoting() {
        assert_eq!(Dialect::Postgres.quote_ident_if_needed("users"), "users");
        assert_eq!(Dialect::Postgres.quote_ident_if_needed("Users"), "\"Users\"");
        assert_eq!(Dialect::Postgres.quote_ident_if_needed("order"), "\"order\"");
        assert_eq!(Dialect::MySql.quote_ident("a`b"), "`a``b`");
        assert_eq!(Dialect::MsSql.quote_ident("a]b"), "[a]]b]");
        assert_eq!(Dialect::Postgres.quote_string("it's"), "'it''s'");
        assert_eq!(Dialect::MySql.quote_string("a\\b"), "'a\\\\b'");
        assert_eq!(Dialect::Postgres.qualified(Some("public"), "t"), "public.t");
    }

    #[test]
    fn paging() {
        assert_eq!(
            Dialect::Postgres.select_page(Some("s"), "t", Some("id > 1"), Some("id"), 10, 20),
            "SELECT * FROM s.t WHERE id > 1 ORDER BY id LIMIT 10 OFFSET 20"
        );
        assert!(Dialect::MsSql.select_page(None, "t", None, None, 10, 0).contains("FETCH NEXT 10"));
    }
}
