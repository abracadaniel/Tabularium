//! Live templates (DataGrip-style abbreviations expanded by the editor).

use tabularium_core::Dialect;

/// A live template. `body` may contain `$CURSOR$` marking the caret position after expansion.
#[derive(Debug, Clone)]
pub struct Template {
    /// Abbreviation typed by the user (`sel`, `ins`, ...).
    pub abbrev: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Expansion text.
    pub body: &'static str,
}

/// Marker for the caret position inside [`Template::body`].
pub const CURSOR_MARKER: &str = "$CURSOR$";

const fn t(abbrev: &'static str, description: &'static str, body: &'static str) -> Template {
    Template { abbrev, description, body }
}

/// Live templates for `dialect`.
pub fn templates(dialect: Dialect) -> Vec<Template> {
    let mut v = vec![
        t("sel", "SELECT * FROM table", "SELECT * FROM $CURSOR$;"),
        t("selw", "SELECT * FROM table WHERE ...", "SELECT * FROM $CURSOR$ WHERE ;"),
        t("selc", "SELECT count(*) FROM table", "SELECT count(*) FROM $CURSOR$;"),
        t("seld", "SELECT DISTINCT column FROM table", "SELECT DISTINCT $CURSOR$ FROM ;"),
        t("selg", "SELECT ... GROUP BY", "SELECT $CURSOR$, count(*) FROM  GROUP BY ;"),
        t("selj", "SELECT with JOIN", "SELECT * FROM $CURSOR$ a JOIN  b ON a. = b.;"),
        t("ins", "INSERT INTO table VALUES", "INSERT INTO $CURSOR$ () VALUES ();"),
        t("inss", "INSERT INTO table SELECT", "INSERT INTO $CURSOR$ () SELECT  FROM ;"),
        t("upd", "UPDATE table SET ... WHERE", "UPDATE $CURSOR$ SET  = WHERE ;"),
        t("del", "DELETE FROM table WHERE", "DELETE FROM $CURSOR$ WHERE ;"),
        t("ci", "CREATE INDEX", "CREATE INDEX $CURSOR$ ON  ();"),
        t("cv", "CREATE VIEW", "CREATE VIEW $CURSOR$ AS\nSELECT * FROM ;"),
        t("at", "ALTER TABLE ADD COLUMN", "ALTER TABLE $CURSOR$ ADD COLUMN  ;"),
        t("dt", "DROP TABLE", "DROP TABLE $CURSOR$;"),
        t("cte", "WITH ... AS (...) SELECT", "WITH $CURSOR$ AS (\n    SELECT\n)\nSELECT * FROM ;"),
        t("case", "CASE WHEN ... END", "CASE WHEN $CURSOR$ THEN  ELSE  END"),
        t("exists", "WHERE EXISTS (subquery)", "EXISTS (SELECT 1 FROM $CURSOR$ WHERE )"),
    ];
    v.push(match dialect {
        Dialect::Postgres => t(
            "ct",
            "CREATE TABLE",
            "CREATE TABLE $CURSOR$ (\n    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n    created_at timestamptz NOT NULL DEFAULT now()\n);",
        ),
        Dialect::MySql => t(
            "ct",
            "CREATE TABLE",
            "CREATE TABLE $CURSOR$ (\n    id BIGINT AUTO_INCREMENT PRIMARY KEY,\n    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP\n);",
        ),
        Dialect::Sqlite => t(
            "ct",
            "CREATE TABLE",
            "CREATE TABLE $CURSOR$ (\n    id INTEGER PRIMARY KEY AUTOINCREMENT,\n    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP\n);",
        ),
        Dialect::MsSql => t(
            "ct",
            "CREATE TABLE",
            "CREATE TABLE $CURSOR$ (\n    id BIGINT IDENTITY(1,1) PRIMARY KEY,\n    created_at DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME()\n);",
        ),
        Dialect::Generic => t(
            "ct",
            "CREATE TABLE",
            "CREATE TABLE $CURSOR$ (\n    id INTEGER PRIMARY KEY,\n    name VARCHAR(255)\n);",
        ),
    });
    match dialect {
        Dialect::MsSql => {
            v.push(t("top", "SELECT TOP n * FROM table", "SELECT TOP 100 * FROM $CURSOR$;"));
            v.push(t("proc", "CREATE PROCEDURE", "CREATE PROCEDURE $CURSOR$\nAS\nBEGIN\n    SET NOCOUNT ON;\nEND\nGO"));
        }
        Dialect::Postgres => {
            v.push(t("sell", "SELECT * FROM table LIMIT n", "SELECT * FROM $CURSOR$ LIMIT 100;"));
            v.push(t(
                "fn",
                "CREATE FUNCTION (plpgsql)",
                "CREATE OR REPLACE FUNCTION $CURSOR$()\nRETURNS void\nLANGUAGE plpgsql\nAS $$\nBEGIN\nEND;\n$$;",
            ));
            v.push(t("ups", "INSERT ... ON CONFLICT DO UPDATE", "INSERT INTO $CURSOR$ () VALUES ()\nON CONFLICT () DO UPDATE SET ;"));
        }
        Dialect::MySql => {
            v.push(t("sell", "SELECT * FROM table LIMIT n", "SELECT * FROM $CURSOR$ LIMIT 100;"));
            v.push(t(
                "proc",
                "CREATE PROCEDURE",
                "DELIMITER $$\nCREATE PROCEDURE $CURSOR$()\nBEGIN\nEND$$\nDELIMITER ;",
            ));
        }
        Dialect::Sqlite | Dialect::Generic => {
            v.push(t("sell", "SELECT * FROM table LIMIT n", "SELECT * FROM $CURSOR$ LIMIT 100;"));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_are_well_formed() {
        for d in Dialect::ALL {
            let ts = templates(d);
            for name in ["sel", "selw", "ins", "upd", "del", "ct"] {
                assert!(ts.iter().any(|t| t.abbrev == name), "{name} missing for {d:?}");
            }
            let mut abbrevs: Vec<_> = ts.iter().map(|t| t.abbrev).collect();
            abbrevs.sort_unstable();
            let n = abbrevs.len();
            abbrevs.dedup();
            assert_eq!(n, abbrevs.len(), "duplicate abbreviations for {d:?}");
            for t in &ts {
                assert_eq!(t.body.matches(CURSOR_MARKER).count(), 1, "{}", t.abbrev);
            }
        }
    }
}
