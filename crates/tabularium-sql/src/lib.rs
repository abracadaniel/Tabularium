//! SQL tooling for the Tabularium editor: lossless lexer (highlighting), statement splitter
//! ("run statement under caret"), formatter, code completion, inspections, live templates
//! and query parameters.
//!
//! Everything is token based, fast enough to run on every keystroke over scripts of
//! thousands of lines, and never panics on any input. All offsets are byte offsets into the
//! source `&str` and always lie on char boundaries.

#![warn(missing_docs)]

pub mod analyze;
pub mod catalog;
pub mod complete;
pub mod format;
pub mod inspect;
pub mod lexer;
pub mod split;
pub mod templates;
mod words;

#[cfg(test)]
mod corpus;

pub use analyze::{
    find_parameters, referenced_tables, single_table_select, statement_kind, substitute_parameters, Parameter,
    StatementKind, TableRef,
};
pub use catalog::{CatalogColumn, CatalogTable, SchemaCatalog};
pub use complete::{complete, Completion, CompletionItem, CompletionKind};
pub use format::{compact_sql, format_sql, FormatOptions, KeywordCase};
pub use inspect::{inspect, Diagnostic, Severity};
pub use lexer::{tokenize, Token, TokenKind};
pub use split::{split_statements, statement_at, StatementSpan};
pub use templates::{templates, Template};

#[cfg(test)]
mod bench {
    use std::time::Instant;

    use tabularium_core::Dialect;

    #[test]
    fn tokenize_and_split_large_script_quickly() {
        let unit = "SELECT u.id, u.name, count(o.id) AS n -- comment\nFROM users u\nLEFT JOIN orders o ON o.user_id = u.id\nWHERE u.name LIKE 'a%' AND o.total > 10.5\nGROUP BY u.id, u.name;\n\n";
        let src = unit.repeat(5000 / 6 + 1);
        assert!(src.lines().count() >= 5000);
        let t = Instant::now();
        let toks = crate::lexer::tokenize(&src, Dialect::Postgres);
        let stmts = crate::split::split_statements(&src, Dialect::Postgres);
        let elapsed = t.elapsed();
        assert!(toks.len() > 50_000);
        assert_eq!(stmts.len(), 5000 / 6 + 1);
        let limit_ms = if cfg!(debug_assertions) { 2000 } else { 100 };
        assert!(elapsed.as_millis() < limit_ms, "took {elapsed:?}");
        // statement_at / completion / inspection stay usable too.
        let t = Instant::now();
        let _ = crate::split::statement_at(&src, src.len() / 2, Dialect::Postgres);
        let _ = crate::complete::complete(&src, src.len() / 2, Dialect::Postgres, &Default::default());
        assert!(t.elapsed().as_millis() < limit_ms * 2, "took {:?}", t.elapsed());
    }

    /// Deterministic pseudo-random "fuzz" over token soup: nothing may panic, the lexer must be
    /// lossless, offsets must be on char boundaries and formatting must preserve tokens and be
    /// idempotent.
    #[test]
    fn fuzz_token_soup() {
        const FRAGS: &[&str] = &[
            "SELECT", "select", "FROM", "WHERE", "AND", "OR", "JOIN", "LEFT", "ON", "GROUP", "BY",
            "ORDER", "CASE", "WHEN", "THEN", "ELSE", "END", "BEGIN", "INSERT", "INTO", "VALUES",
            "UPDATE", "SET", "DELETE", "WITH", "AS", "UNION", "ALL", "CREATE", "TABLE", "TRIGGER",
            "PROCEDURE", "BETWEEN", "DISTINCT", "TOP", "LIMIT", "DELIMITER", "GO", "key", "value",
            "t", "u.id", "\"Q\"", "`b`", "[m]", "'s'", "'it''s'", "E'x\\'y'", "$$ body; $$", "$t$ x $t$",
            "1", "2.5", "-", "+", "*", "/", "=", "<>", "::", "->>", "(", ")", ",", ";", ".", "[", "]",
            "-- c\n", "/* b */", "# h\n", ":p", "?", "$1", "${v}", "@x", "ü", "表", "😀", "\n", " ",
            "\t", "count(", "coalesce(", "int", "varchar(10)", "'", "\"", "/*", "$$", "GO\n", "\nGO\n",
            "\nDELIMITER $$\n", "\nDELIMITER ;\n", "$$",
        ];
        let mut state: u64 = std::env::var("SQL_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x9E37_79B9_7F4A_7C15);
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let cat = crate::catalog::SchemaCatalog {
            default_schema: Some("public".into()),
            schemas: vec!["public".into()],
            tables: vec![crate::catalog::CatalogTable {
                schema: Some("public".into()),
                name: "t".into(),
                kind: tabularium_core::ObjectKind::Table,
                columns: vec![crate::catalog::CatalogColumn { name: "id".into(), data_type: "int".into() }],
            }],
            functions: vec![],
        };
        let iterations: usize = std::env::var("SQL_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(400);
        let seq = |s: &str, d: Dialect| -> Vec<String> {
            crate::lexer::tokenize(s, d)
                .into_iter()
                .filter(|t| t.kind != crate::lexer::TokenKind::Whitespace)
                .map(|t| match t.kind {
                    crate::lexer::TokenKind::Keyword
                    | crate::lexer::TokenKind::Type
                    | crate::lexer::TokenKind::Identifier
                    | crate::lexer::TokenKind::Function => t.text(s).to_ascii_uppercase(),
                    _ => t.text(s).trim_end().to_string(),
                })
                .collect()
        };
        for _ in 0..iterations {
            let n = (next() % 30) as usize + 1;
            let mut src = String::new();
            for _ in 0..n {
                src.push_str(FRAGS[(next() % FRAGS.len() as u64) as usize]);
                if next() % 3 != 0 {
                    src.push(' ');
                }
            }
            for d in Dialect::ALL {
                let toks = crate::lexer::tokenize(&src, d);
                let joined: String = toks.iter().map(|t| t.text(&src)).collect();
                assert_eq!(joined, src);
                for s in crate::split::split_statements(&src, d) {
                    assert!(src.is_char_boundary(s.start) && src.is_char_boundary(s.end));
                    assert_eq!(&src[s.start..s.end], s.text);
                }
                let cursor = (next() as usize) % (src.len() + 1);
                let _ = crate::split::statement_at(&src, cursor, d);
                let c = crate::complete::complete(&src, cursor, d, &cat);
                assert!(src.is_char_boundary(c.replace_start) && src.is_char_boundary(c.replace_end));
                assert!(c.replace_start <= c.replace_end);
                for dg in crate::inspect::inspect(&src, d, Some(&cat)) {
                    assert!(src.is_char_boundary(dg.start) && src.is_char_boundary(dg.end) && dg.start <= dg.end);
                }
                let _ = crate::analyze::referenced_tables(&src, d);
                let _ = crate::analyze::single_table_select(&src, d);
                let _ = crate::analyze::find_parameters(&src, d);
                let _ = crate::analyze::statement_kind(&src);
                let _ = crate::format::compact_sql(&src, d);
                let opts = crate::format::FormatOptions::default();
                let once = crate::format::format_sql(&src, d, &opts);
                let twice = crate::format::format_sql(&once, d, &opts);
                assert_eq!(once, twice, "not idempotent ({d:?}) for {src:?}");
                // Custom delimiters are glued to the statement (`END$$`), which whole-script lexing
                // sees differently; the corpus test covers DELIMITER scripts.
                let generic_delim = src.contains("DELIMITER");
                assert!(generic_delim || seq(&src, d) == seq(&once, d), "tokens changed ({d:?}) for {src:?}\n{once}");
            }
        }
    }
}
