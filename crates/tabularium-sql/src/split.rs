//! Statement splitting ("execute statement under caret").
//!
//! Splits on `;` outside strings, comments and dollar quotes, keeps `BEGIN ... END`
//! bodies of routines/triggers together, honours MySQL `DELIMITER` directives and
//! MSSQL `GO` batch separators.

use tabularium_core::Dialect;

use crate::lexer::{Lexer, Token, TokenKind};

/// One statement of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSpan {
    /// Byte offset of the first character of the trimmed statement.
    pub start: usize,
    /// Byte offset just past the last character (the delimiter is not included).
    pub end: usize,
    /// The trimmed statement text without the trailing delimiter.
    pub text: String,
}

/// Internal, richer split result used by the formatter and completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Piece {
    /// A statement: trimmed range plus the delimiter that terminated it (if any).
    Stmt {
        start: usize,
        end: usize,
        delim: Option<(usize, usize)>,
        /// Routine with a `BEGIN ... END` body (formatted verbatim).
        block: bool,
    },
    /// A stray delimiter with no statement before it.
    Empty { delim: (usize, usize) },
    /// A `DELIMITER x` or `GO` line.
    Directive { start: usize, end: usize, go: bool },
}

impl Piece {
    /// Byte range of the delimiter/directive that ends this piece, if any.
    pub(crate) fn boundary(&self) -> Option<(usize, usize)> {
        match *self {
            Piece::Stmt { delim, .. } => delim,
            Piece::Empty { delim } => Some(delim),
            Piece::Directive { start, end, .. } => Some((start, end)),
        }
    }
}

const ROUTINE_WORDS: &[&str] = &["TRIGGER", "PROCEDURE", "FUNCTION", "EVENT", "PACKAGE", "PROC"];
const TX_AFTER_BEGIN: &[&str] =
    &["TRANSACTION", "WORK", "TRAN", "DEFERRED", "IMMEDIATE", "EXCLUSIVE", "ISOLATION", "READ", "DISTRIBUTED"];
const END_NOOP: &[&str] = &["IF", "LOOP", "WHILE", "REPEAT", "FOR"];

#[derive(Default)]
struct Cur {
    first: Option<usize>,
    last_end: usize,
    nwords: usize,
    first_word_create: bool,
    first_word_begin: bool,
    routine: bool,
    depth: i32,
    saw_begin: bool,
    skip_case: bool,
}

fn at_line_start(src: &str, pos: usize) -> bool {
    let b = src.as_bytes();
    let mut i = pos;
    while i > 0 {
        match b[i - 1] {
            b' ' | b'\t' | b'\r' => i -= 1,
            b'\n' => return true,
            _ => return false,
        }
    }
    true
}

fn line_end(src: &str, pos: usize) -> usize {
    src[pos..].find('\n').map_or(src.len(), |k| pos + k)
}

/// Next significant token (skipping whitespace/comments) without advancing `lx`.
fn peek_sig(lx: &Lexer<'_>) -> Option<Token> {
    let mut l = lx.clone();
    l.find(|t| !t.is_trivia())
}

fn word_eq(t: &Token, src: &str, w: &str) -> bool {
    t.is_word() && t.text(src).eq_ignore_ascii_case(w)
}

fn word_in(t: &Token, src: &str, list: &[&str]) -> bool {
    t.is_word() && list.iter().any(|w| t.text(src).eq_ignore_ascii_case(w))
}

/// Split into pieces (statements, empty statements and directives).
pub(crate) fn split_pieces(src: &str, d: Dialect) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut lx = Lexer::new(src, d);
    let mut delim: String = ";".to_string();
    let mut cur = Cur::default();
    let directives = matches!(d, Dialect::MySql | Dialect::Generic);

    fn finish(pieces: &mut Vec<Piece>, cur: &mut Cur, delim: Option<(usize, usize)>) {
        match cur.first {
            Some(start) => pieces.push(Piece::Stmt {
                start,
                end: cur.last_end,
                delim,
                block: cur.routine && cur.saw_begin,
            }),
            None => {
                if let Some(dl) = delim {
                    pieces.push(Piece::Empty { delim: dl });
                }
            }
        }
        *cur = Cur::default();
    }

    loop {
        let custom = delim != ";";
        if custom && src[lx.pos..].starts_with(delim.as_str()) {
            let p = lx.pos;
            finish(&mut pieces, &mut cur, Some((p, p + delim.len())));
            lx.set_pos(p + delim.len());
            continue;
        }
        let Some(tok) = lx.next() else { break };
        if tok.is_trivia() {
            continue;
        }
        let text = tok.text(src);

        // MSSQL batch separator: `GO [count]` at the start of a line (or starting a statement).
        if d == Dialect::MsSql && word_eq(&tok, src, "GO") {
            let le = line_end(src, tok.end);
            let rest = src[tok.end..le].trim();
            let alone = rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit());
            if at_line_start(src, tok.start) || cur.first.is_none() {
                finish(&mut pieces, &mut cur, None);
                let (end, resume) = if alone {
                    (tok.end + src[tok.end..le].trim_end().len(), le)
                } else {
                    (tok.end, tok.end)
                };
                pieces.push(Piece::Directive { start: tok.start, end, go: true });
                lx.set_pos(resume);
                continue;
            }
        }

        // MySQL `DELIMITER x` directive.
        if directives && cur.first.is_none() && word_eq(&tok, src, "DELIMITER") {
            let le = line_end(src, tok.end);
            let rest = src[tok.end..le].trim();
            if let Some(newd) = rest.split_whitespace().next() {
                delim = newd.to_string();
            }
            let end = tok.end + src[tok.end..le].trim_end().len();
            pieces.push(Piece::Directive { start: tok.start, end, go: false });
            lx.set_pos(le);
            continue;
        }

        if custom {
            if !matches!(tok.kind, TokenKind::String | TokenKind::QuotedIdentifier) {
                if let Some(k) = text.find(delim.as_str()) {
                    let at = tok.start + k;
                    if k > 0 {
                        cur.first.get_or_insert(tok.start);
                        cur.last_end = at;
                    }
                    finish(&mut pieces, &mut cur, Some((at, at + delim.len())));
                    lx.set_pos(at + delim.len());
                    continue;
                }
            }
        } else if tok.kind == TokenKind::Punctuation && text == ";" && cur.depth <= 0 {
            finish(&mut pieces, &mut cur, Some((tok.start, tok.end)));
            continue;
        }

        // Statement content.
        cur.first.get_or_insert(tok.start);
        cur.last_end = tok.end;
        if tok.is_word() {
            let idx = cur.nwords;
            cur.nwords += 1;
            if idx == 0 {
                cur.first_word_create = text.eq_ignore_ascii_case("CREATE");
                cur.first_word_begin = text.eq_ignore_ascii_case("BEGIN");
            } else if idx < 10 && cur.first_word_create && word_in(&tok, src, ROUTINE_WORDS) {
                cur.routine = true;
            }
            let tracking = cur.routine || cur.depth > 0 || (idx == 0 && cur.first_word_begin);
            if !tracking {
                continue;
            }
            if text.eq_ignore_ascii_case("BEGIN") {
                let next = peek_sig(&lx);
                let is_tx = match next {
                    None => true,
                    Some(n) => {
                        (n.kind == TokenKind::Punctuation && n.text(src) == ";")
                            || word_in(&n, src, TX_AFTER_BEGIN)
                    }
                };
                if !is_tx {
                    cur.depth += 1;
                    cur.saw_begin = true;
                    if idx == 0 {
                        cur.routine = true;
                    }
                }
            } else if text.eq_ignore_ascii_case("CASE") {
                if cur.skip_case {
                    cur.skip_case = false;
                } else {
                    cur.depth += 1;
                }
            } else if text.eq_ignore_ascii_case("END") && cur.depth > 0 {
                match peek_sig(&lx) {
                    Some(n) if word_in(&n, src, END_NOOP) => {}
                    Some(n) if word_eq(&n, src, "CASE") => {
                        cur.depth -= 1;
                        cur.skip_case = true;
                    }
                    _ => cur.depth -= 1,
                }
            }
        }
    }
    finish(&mut pieces, &mut cur, None);
    pieces
}

/// Split a script into statements (empty and comment-only statements are skipped).
pub fn split_statements(src: &str, dialect: Dialect) -> Vec<StatementSpan> {
    split_pieces(src, dialect)
        .into_iter()
        .filter_map(|p| match p {
            Piece::Stmt { start, end, .. } => {
                Some(StatementSpan { start, end, text: src[start..end].to_string() })
            }
            _ => None,
        })
        .collect()
}

/// Statement containing `cursor`, or the nearest previous statement when the cursor is in
/// whitespace after it (DataGrip behaviour). If the cursor is before the first statement,
/// the first statement is returned. `None` for scripts without statements.
pub fn statement_at(src: &str, cursor: usize, dialect: Dialect) -> Option<StatementSpan> {
    let stmts = split_statements(src, dialect);
    let mut prev: Option<&StatementSpan> = None;
    for s in &stmts {
        if cursor >= s.start && cursor <= s.end {
            return Some(s.clone());
        }
        if s.end <= cursor {
            prev = Some(s);
        }
    }
    prev.or(stmts.first()).cloned()
}

/// Raw byte region (between delimiters) containing `cursor`; used by completion.
pub(crate) fn region_at(src: &str, cursor: usize, d: Dialect) -> (usize, usize) {
    let mut start = 0;
    for p in split_pieces(src, d) {
        if let Some((bs, be)) = p.boundary() {
            if cursor <= bs {
                return (start.min(cursor), bs);
            }
            if cursor < be {
                // Inside a delimiter or directive line.
                return (cursor, cursor);
            }
            start = be;
        }
    }
    (start.min(cursor).min(src.len()), src.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(src: &str, d: Dialect) -> Vec<String> {
        split_statements(src, d).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn basic_split() {
        assert_eq!(texts("select 1; select 2;", Dialect::Generic), vec!["select 1", "select 2"]);
        assert_eq!(texts("select 1", Dialect::Generic), vec!["select 1"]);
        assert_eq!(texts("  ;; ; ", Dialect::Generic), Vec::<String>::new());
        assert_eq!(texts("", Dialect::Generic), Vec::<String>::new());
        assert_eq!(texts("-- only a comment\n/* x */;", Dialect::Generic), Vec::<String>::new());
        let src = "select 'a;b', \"c;d\" -- e;f\n/* g;h */ from t; select 2";
        assert_eq!(texts(src, Dialect::Postgres).len(), 2);
    }

    #[test]
    fn spans_are_trimmed() {
        let src = "  -- lead\n  select 1  ;\n\n select 2 ";
        let s = split_statements(src, Dialect::Generic);
        assert_eq!(s[0].text, "select 1");
        assert_eq!(&src[s[0].start..s[0].end], "select 1");
        assert_eq!(s[1].text, "select 2");
    }

    #[test]
    fn dollar_quoted_bodies() {
        let src = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql;\nSELECT f();";
        let t = texts(src, Dialect::Postgres);
        assert_eq!(t.len(), 2);
        assert!(t[0].ends_with("plpgsql"));
        let src = "DO $body$ BEGIN PERFORM 1; END $body$; select 1";
        assert_eq!(texts(src, Dialect::Postgres).len(), 2);
    }

    #[test]
    fn sqlite_trigger_block() {
        let src = "CREATE TRIGGER trg AFTER INSERT ON t BEGIN\n  UPDATE x SET n = n + 1;\n  INSERT INTO log VALUES (CASE WHEN 1 THEN 2 END);\nEND;\nSELECT 1;";
        let t = texts(src, Dialect::Sqlite);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].ends_with("END"));
        assert_eq!(t[1], "SELECT 1");
    }

    #[test]
    fn mysql_procedure_without_delimiter() {
        let src = "CREATE PROCEDURE p() BEGIN\n DECLARE x INT;\n IF x > 1 THEN SELECT 1; END IF;\n WHILE x < 3 DO SET x = x + 1; END WHILE;\n CASE x WHEN 1 THEN SELECT 1; END CASE;\nEND;\nSELECT 2;";
        let t = texts(src, Dialect::MySql);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[1], "SELECT 2");
    }

    #[test]
    fn transactions_are_not_blocks() {
        let t = texts("BEGIN; UPDATE t SET a = 1; COMMIT;", Dialect::Postgres);
        assert_eq!(t, vec!["BEGIN", "UPDATE t SET a = 1", "COMMIT"]);
        let t = texts("BEGIN TRANSACTION; SELECT 1; END;", Dialect::Sqlite);
        assert_eq!(t.len(), 3);
        let t = texts("SELECT CASE WHEN a THEN 1 END; SELECT 2", Dialect::Generic);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn mysql_delimiter() {
        let src = "DELIMITER $$\nCREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND$$\nDELIMITER ;\nCALL p();\nSELECT 3;";
        let t = texts(src, Dialect::MySql);
        assert_eq!(t.len(), 3, "{t:?}");
        assert!(t[0].starts_with("CREATE PROCEDURE") && t[0].ends_with("END"));
        assert_eq!(t[1], "CALL p()");
        assert_eq!(t[2], "SELECT 3");
        let src = "DELIMITER //\nSELECT 1//\nSELECT 2 //\nDELIMITER ;\nSELECT 3;";
        assert_eq!(texts(src, Dialect::MySql), vec!["SELECT 1", "SELECT 2", "SELECT 3"]);
    }

    #[test]
    fn mssql_go() {
        let src = "CREATE PROCEDURE p AS\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND\nGO\nSELECT 3\ngo 2\nSELECT 4; SELECT 5";
        let t = texts(src, Dialect::MsSql);
        assert_eq!(t.len(), 4, "{t:?}");
        assert_eq!(t[1], "SELECT 3");
        // GO is only a separator on its own line.
        assert_eq!(texts("SELECT go FROM t", Dialect::MsSql).len(), 1);
    }

    #[test]
    fn statement_at_positions() {
        let src = "select 1;\n\nselect 2;  \n";
        let s1 = statement_at(src, 0, Dialect::Generic).unwrap();
        assert_eq!(s1.text, "select 1");
        assert_eq!(statement_at(src, 8, Dialect::Generic).unwrap().text, "select 1");
        assert_eq!(statement_at(src, 9, Dialect::Generic).unwrap().text, "select 1");
        assert_eq!(statement_at(src, 10, Dialect::Generic).unwrap().text, "select 1");
        assert_eq!(statement_at(src, 11, Dialect::Generic).unwrap().text, "select 2");
        assert_eq!(statement_at(src, src.len(), Dialect::Generic).unwrap().text, "select 2");
        assert_eq!(statement_at("  \n select 1", 0, Dialect::Generic).unwrap().text, "select 1");
        assert!(statement_at("  -- x\n", 3, Dialect::Generic).is_none());
        assert!(statement_at("", 0, Dialect::Generic).is_none());
        assert_eq!(statement_at(src, 10_000, Dialect::Generic).unwrap().text, "select 2");
    }

    #[test]
    fn regions() {
        let src = "select 1; sel";
        assert_eq!(region_at(src, src.len(), Dialect::Generic), (9, src.len()));
        assert_eq!(region_at(src, 3, Dialect::Generic), (0, 8));
    }

    #[test]
    fn unicode_never_panics() {
        let src = "select 'ü;'; -- ☃;\nselect \"é\"; DELIMITER ü\nselect 1ü select 2";
        for d in Dialect::ALL {
            let _ = split_statements(src, d);
            for i in 0..=src.len() + 2 {
                let _ = statement_at(src, i, d);
                let _ = region_at(src, i, d);
            }
        }
    }
}
