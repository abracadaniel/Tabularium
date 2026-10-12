//! Token-based SQL pretty printer.
//!
//! The output depends only on the sequence of significant tokens (plus, for comments,
//! whether they started on their own line), which makes formatting idempotent. No token
//! is ever dropped or altered except for keyword (and data type) letter case.

use tabularium_core::Dialect;

use crate::lexer::{self, Lexer, TokenKind};
use crate::split::{split_pieces, Piece};

/// Keyword letter case applied by the formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordCase {
    /// `SELECT`
    Upper,
    /// `select`
    Lower,
    /// Keep as written.
    Preserve,
}

/// Formatter options.
#[derive(Debug, Clone)]
pub struct FormatOptions {
    /// Spaces per indentation level.
    pub indent: usize,
    /// Keyword case.
    pub keyword_case: KeywordCase,
    /// Lines inside inline parenthesised lists (function arguments, `IN (...)`, column lists)
    /// are wrapped after a comma once they would exceed this many characters.
    pub max_line_length: usize,
    /// Put commas at the start of lines in multi-line lists.
    pub comma_first: bool,
    /// Blank lines between statements.
    pub lines_between_statements: usize,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions {
            indent: 4,
            keyword_case: KeywordCase::Upper,
            max_line_length: 100,
            comma_first: false,
            lines_between_statements: 1,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Spacing rules shared by `format_sql` and `compact_sql`.

const OBJECT_KEYWORDS: &[&str] = &["INTO", "TABLE", "REFERENCES", "VIEW", "EXISTS"];
const VALUE_KEYWORDS: &[&str] = &[
    "END", "NULL", "TRUE", "FALSE", "CURRENT_DATE", "CURRENT_TIME", "CURRENT_TIMESTAMP",
    "LOCALTIME", "LOCALTIMESTAMP", "CURRENT_USER", "SESSION_USER", "USER", "UNKNOWN",
];

fn is_op_char(c: char) -> bool {
    "+-*/%=<>!~^&|@#:?".contains(c)
}

fn upper_eq_any(text: &str, list: &[&str]) -> bool {
    list.iter().any(|w| text.eq_ignore_ascii_case(w))
}

#[derive(Default)]
struct Spacer<'a> {
    /// Previous emitted token (including comments).
    prev: Option<(TokenKind, &'a str)>,
    /// Previous significant token.
    last_sig: Option<(TokenKind, &'a str)>,
    unary: bool,
    object: bool,
}

fn is_number_text(a: &str) -> bool {
    let b = a.as_bytes();
    b.first().is_some_and(u8::is_ascii_digit) || (b.len() > 1 && b[0] == b'.' && b[1].is_ascii_digit())
}

/// Would gluing `a` and `b` together change how they are lexed?
fn glue_clash(a: &str, b: &str) -> bool {
    let (Some(x), Some(y)) = (a.chars().last(), b.chars().next()) else { return false };
    (is_op_char(x) && is_op_char(y))
        || (is_number_text(a) && (y == '.' || y.is_alphanumeric() || y == '_'))
        || (x == '.' && y.is_ascii_digit())
        || ((x.is_alphanumeric() || x == '_' || x == '$') && (y.is_alphanumeric() || y == '_' || y == '$'))
        || (x == '$' && y == '{')
        || (x == y && matches!(x, ']' | '"' | '\'' | '`'))
        || (matches!(x, 'e' | 'E' | 'n' | 'N' | 'x' | 'X' | 'b' | 'B') && y == '\'')
}

impl<'a> Spacer<'a> {
    fn space_before(&self, kind: TokenKind, text: &str) -> bool {
        let Some((pk, pt)) = self.prev else { return false };
        if pk == TokenKind::Comment {
            return true;
        }
        self.space_rule(pk, pt, kind, text) || glue_clash(pt, text)
    }

    fn space_rule(&self, pk: TokenKind, pt: &str, kind: TokenKind, text: &str) -> bool {
        let punct = kind == TokenKind::Punctuation;
        if punct && matches!(text, "," | ";" | ")" | "]" | ".") {
            return false;
        }
        if pk == TokenKind::Punctuation && matches!(pt, "." | "(" | "[") {
            return false;
        }
        if self.unary {
            // `glue_clash` keeps `- -1` from becoming `--1`.
            return false;
        }
        if (pk == TokenKind::Operator && pt == "::") || (kind == TokenKind::Operator && text == "::") {
            return false;
        }
        if punct && text == "(" {
            return match pk {
                TokenKind::Type => false,
                TokenKind::Identifier | TokenKind::QuotedIdentifier | TokenKind::Function => self.object,
                _ => true,
            };
        }
        if punct && text == "[" {
            let tight = matches!(pk, TokenKind::Identifier | TokenKind::QuotedIdentifier | TokenKind::Type | TokenKind::Function)
                || (pk == TokenKind::Keyword && pt.eq_ignore_ascii_case("ARRAY"))
                || (pk == TokenKind::Punctuation && matches!(pt, ")" | "]"));
            return !tight;
        }
        true
    }

    fn push(&mut self, kind: TokenKind, text: &'a str) {
        self.prev = Some((kind, text));
        if kind == TokenKind::Comment || kind == TokenKind::Whitespace {
            return;
        }
        self.unary = kind == TokenKind::Operator
            && matches!(text, "-" | "+" | "~")
            && match self.last_sig {
                None => true,
                Some((pk, pt)) => match pk {
                    TokenKind::Operator => true,
                    TokenKind::Punctuation => matches!(pt, "(" | "," | "[" | ";"),
                    TokenKind::Keyword => !upper_eq_any(pt, VALUE_KEYWORDS),
                    _ => false,
                },
            };
        self.object = match kind {
            TokenKind::Keyword => upper_eq_any(text, OBJECT_KEYWORDS),
            TokenKind::Identifier | TokenKind::QuotedIdentifier | TokenKind::Function => self.object,
            TokenKind::Punctuation if text == "." => self.object,
            _ => false,
        };
        self.last_sig = Some((kind, text));
    }
}

// ---------------------------------------------------------------------------------------------
// Statement formatter.

#[derive(Clone, Copy)]
struct FTok<'a> {
    kind: TokenKind,
    text: &'a str,
    /// Comment starts on its own line.
    nl_before: bool,
}

impl FTok<'_> {
    fn is_kw(&self, w: &str) -> bool {
        self.kind == TokenKind::Keyword && self.text.eq_ignore_ascii_case(w)
    }
    fn is_punct(&self, p: &str) -> bool {
        self.kind == TokenKind::Punctuation && self.text == p
    }
    fn is_line_comment(&self) -> bool {
        self.kind == TokenKind::Comment && !self.text.starts_with("/*")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CtxKind {
    Query,
    Paren,
    Defs,
    Case,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    None,
    Select,
    From,
    Where,
    On,
    Having,
    Set,
    Values,
    With,
    Other,
}

#[derive(Clone, Copy)]
struct Ctx {
    kind: CtxKind,
    base: usize,
    ret: usize,
    clause: Clause,
    between: bool,
    values_block: bool,
    from_paren: bool,
}

impl Ctx {
    fn new(kind: CtxKind, base: usize, ret: usize, from_paren: bool) -> Self {
        Ctx { kind, base, ret, clause: Clause::None, between: false, values_block: false, from_paren }
    }
}

/// Keywords that are values/modifiers and legitimately appear in identifier positions.
const IDENT_EXCLUDED: &[&str] = &[
    "NULL", "TRUE", "FALSE", "DEFAULT", "ASC", "DESC", "CURRENT_DATE", "CURRENT_TIME",
    "CURRENT_TIMESTAMP", "CURRENT_USER", "CURRENT_ROLE", "CURRENT_SCHEMA", "LOCALTIME",
    "LOCALTIMESTAMP", "SESSION_USER", "SYSTEM_USER", "USER", "END", "ALL", "DISTINCT", "UNKNOWN",
    "ROW", "ON", "OFF", "CASCADE", "RESTRICT", "NOTHING", "NULLS", "FIRST", "LAST", "PRIMARY",
    "UNIQUE", "CHECK", "FOREIGN", "CONSTRAINT",
];

const JOIN_MODS: &[&str] = &["LEFT", "RIGHT", "FULL", "INNER", "OUTER", "CROSS", "NATURAL"];
const SELECT_END: &[&str] = &[
    "FROM", "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "UNION", "INTERSECT", "EXCEPT", "INTO",
    "WINDOW", "OFFSET", "FETCH", "FOR", "MINUS",
];

struct StmtFormatter<'a, 'o> {
    toks: Vec<FTok<'a>>,
    matches: Vec<Option<usize>>,
    opts: &'o FormatOptions,
    out: String,
    line_start: usize,
    line_indent: usize,
    pending: Option<usize>,
    stack: Vec<Ctx>,
    sp: Spacer<'a>,
    item_nl: Option<(usize, usize)>,
    is_create_table: bool,
    is_routine: bool,
    mssql: bool,
    defs_done: bool,
    first_word: String,
    last_line_comment: bool,
    last_sig_idx: Option<usize>,
}

impl<'a, 'o> StmtFormatter<'a, 'o> {
    fn new(text: &'a str, d: Dialect, opts: &'o FormatOptions) -> Self {
        let mut toks = Vec::new();
        let mut saw_nl = true;
        for t in Lexer::new(text, d) {
            let s = t.text(text);
            match t.kind {
                TokenKind::Whitespace => saw_nl |= s.contains('\n'),
                TokenKind::Comment => {
                    let s = if s.starts_with("/*") { s } else { s.trim_end() };
                    toks.push(FTok { kind: t.kind, text: s, nl_before: saw_nl });
                    saw_nl = false;
                }
                _ => {
                    toks.push(FTok { kind: t.kind, text: s, nl_before: saw_nl });
                    saw_nl = false;
                }
            }
        }
        let mut matches = vec![None; toks.len()];
        let mut st = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            if t.is_punct("(") {
                st.push(i);
            } else if t.is_punct(")") {
                if let Some(o) = st.pop() {
                    matches[o] = Some(i);
                    matches[i] = Some(o);
                }
            }
        }
        let words: Vec<String> = toks
            .iter()
            .filter(|t| t.kind != TokenKind::Comment)
            .take(6)
            .map(|t| t.text.to_ascii_uppercase())
            .collect();
        let first_word = words.first().cloned().unwrap_or_default();
        let is_create_table = first_word == "CREATE" && {
            let mut k = 1;
            while k < words.len()
                && matches!(
                    words[k].as_str(),
                    "OR" | "REPLACE" | "GLOBAL" | "LOCAL" | "TEMP" | "TEMPORARY" | "UNLOGGED" | "VIRTUAL"
                )
            {
                k += 1;
            }
            words.get(k).is_some_and(|w| w == "TABLE")
        };
        let is_routine = first_word == "CREATE"
            && words.iter().take(5).any(|w| matches!(w.as_str(), "FUNCTION" | "PROCEDURE"));
        let bottom_kind = if matches!(first_word.as_str(), "GRANT" | "REVOKE") {
            CtxKind::Paren
        } else {
            CtxKind::Query
        };
        StmtFormatter {
            toks,
            matches,
            opts,
            out: String::with_capacity(text.len() + text.len() / 4),
            line_start: 0,
            line_indent: 0,
            pending: None,
            stack: vec![Ctx::new(bottom_kind, 0, 0, false)],
            sp: Spacer::default(),
            item_nl: None,
            is_create_table,
            is_routine,
            mssql: d == Dialect::MsSql,
            defs_done: false,
            first_word,
            last_line_comment: false,
            last_sig_idx: None,
        }
    }

    fn next_sig(&self, i: usize) -> Option<usize> {
        (i + 1..self.toks.len()).find(|&k| self.toks[k].kind != TokenKind::Comment)
    }

    fn top(&mut self) -> &mut Ctx {
        // The stack is never empty: the bottom context is never popped.
        let n = self.stack.len();
        &mut self.stack[n - 1]
    }

    fn line_is_empty(&self) -> bool {
        self.out[self.line_start..].bytes().all(|b| b == b' ')
    }

    fn newline(&mut self, level: usize) {
        if self.line_is_empty() {
            self.out.truncate(self.line_start);
        } else {
            self.out.push('\n');
            self.line_start = self.out.len();
        }
        for _ in 0..level * self.opts.indent {
            self.out.push(' ');
        }
        self.line_indent = level;
    }

    fn push_text(&mut self, text: &str) {
        self.out.push_str(text);
        if let Some(k) = text.rfind('\n') {
            self.line_start = self.out.len() - (text.len() - k - 1);
        }
    }

    fn line_len(&self) -> usize {
        self.out[self.line_start..].chars().count()
    }

    fn case_kw(&self, text: &str) -> String {
        match self.opts.keyword_case {
            KeywordCase::Upper => text.to_ascii_uppercase(),
            KeywordCase::Lower => text.to_ascii_lowercase(),
            KeywordCase::Preserve => text.to_string(),
        }
    }

    fn display_text(&self, i: usize) -> String {
        let t = self.toks[i];
        match t.kind {
            TokenKind::Keyword if self.keyword_as_identifier(i) => t.text.to_string(),
            TokenKind::Keyword => self.case_kw(t.text),
            TokenKind::Type => {
                let in_paren = self.stack.last().is_some_and(|c| c.kind == CtxKind::Paren);
                let in_defs = self.stack.last().is_some_and(|c| c.kind == CtxKind::Defs);
                let typed_ctx = self.sp.last_sig.is_some_and(|(k, p)| match k {
                    TokenKind::Operator => p == "::",
                    TokenKind::Identifier | TokenKind::QuotedIdentifier | TokenKind::Type => true,
                    TokenKind::Keyword => {
                        (in_paren && p.eq_ignore_ascii_case("AS")) || p.eq_ignore_ascii_case("RETURNS") || in_defs
                    }
                    _ => false,
                });
                if typed_ctx {
                    self.case_kw(t.text)
                } else {
                    t.text.to_string()
                }
            }
            _ => t.text.to_string(),
        }
    }

    /// A non-value keyword in identifier position (`(key, value)`, `SET locked = 1`, a column
    /// definition named like a keyword): keep its case.
    fn keyword_as_identifier(&self, i: usize) -> bool {
        let t = self.toks[i];
        if upper_eq_any(t.text, IDENT_EXCLUDED) {
            return false;
        }
        let Some(p) = self.last_sig_idx.map(|k| self.toks[k]) else { return false };
        let Some(n) = self.next_sig(i).map(|k| self.toks[k]) else { return false };
        let prev_ok = p.is_punct("(") || p.is_punct(",") || p.is_kw("SET") || p.is_kw("SELECT");
        let next_ok = n.is_punct(",")
            || n.is_punct(")")
            || (n.kind == TokenKind::Operator && matches!(n.text, "=" | "<>" | "!=" | "<" | ">" | "<=" | ">="))
            || (n.kind == TokenKind::Type && self.stack.last().is_some_and(|c| c.kind == CtxKind::Defs));
        prev_ok && next_ok
    }

    fn emit(&mut self, i: usize, want: Option<usize>) {
        let t = self.toks[i];
        let text = self.display_text(i);
        let mut level = want.or(self.pending.take());
        self.pending = None;
        let keep_on_line =
            self.mssql && t.kind != TokenKind::String && t.text.eq_ignore_ascii_case("GO") && !self.last_line_comment;
        if keep_on_line {
            // A `GO` at the start of a line would become a batch separator.
            level = None;
        }
        match level {
            Some(l) => self.newline(l),
            None => {
                let top = *self.stack.last().unwrap_or(&Ctx::new(CtxKind::Query, 0, 0, false));
                let after_comma = self.sp.last_sig.is_some_and(|(k, p)| k == TokenKind::Punctuation && p == ",")
                    && self.sp.prev.is_some_and(|(k, _)| k != TokenKind::Comment);
                if top.kind == CtxKind::Paren
                    && !keep_on_line
                    && after_comma
                    && self.line_len() + 1 + text.chars().count() > self.opts.max_line_length
                    && !self.line_is_empty()
                {
                    self.newline(top.base + 1);
                } else if self.sp.space_before(t.kind, t.text) && !self.line_is_empty() {
                    self.out.push(' ');
                }
            }
        }
        self.push_text(&text);
        self.sp.push(t.kind, t.text);
        self.last_line_comment = false;
        self.last_sig_idx = Some(i);
    }

    fn comment(&mut self, i: usize) {
        let t = self.toks[i];
        let line = t.is_line_comment();
        if t.nl_before && !self.line_is_empty() {
            let level = self.pending.take().unwrap_or(self.line_indent);
            self.newline(level);
            self.push_text(t.text);
            self.pending = Some(level);
        } else if self.line_is_empty() {
            self.push_text(t.text);
            if line || t.nl_before {
                self.pending = Some(self.pending.unwrap_or(self.line_indent));
            }
        } else {
            self.out.push(' ');
            self.push_text(t.text);
            if line {
                self.pending = Some(self.pending.unwrap_or(self.line_indent));
            }
        }
        self.sp.push(TokenKind::Comment, t.text);
        self.last_line_comment = line;
    }

    fn prev_kw(&self) -> Option<String> {
        self.last_sig_idx
            .map(|k| self.toks[k])
            .filter(|t| t.kind == TokenKind::Keyword)
            .map(|t| t.text.to_ascii_uppercase())
    }

    /// Index of the first select item after `SELECT` at `i`.
    fn select_items_start(&self, i: usize) -> Option<usize> {
        let mut k = self.next_sig(i)?;
        loop {
            let t = self.toks[k];
            if t.is_kw("DISTINCT") {
                k = self.next_sig(k)?;
                if self.toks[k].is_kw("ON") {
                    let p = self.next_sig(k)?;
                    if self.toks[p].is_punct("(") {
                        k = self.next_sig(self.matches[p]?)?;
                    }
                }
            } else if t.is_kw("ALL")
                || ["SQL_CALC_FOUND_ROWS", "STRAIGHT_JOIN", "HIGH_PRIORITY", "SQL_NO_CACHE", "DISTINCTROW"]
                    .iter()
                    .any(|w| t.text.eq_ignore_ascii_case(w))
            {
                k = self.next_sig(k)?;
            } else if t.is_kw("TOP") {
                let n = self.next_sig(k)?;
                k = if self.toks[n].is_punct("(") { self.next_sig(self.matches[n]?)? } else { self.next_sig(n)? };
                if self.toks[k].is_kw("PERCENT") {
                    k = self.next_sig(k)?;
                }
                if self.toks[k].is_kw("WITH") {
                    k = self.next_sig(self.next_sig(k)?)?;
                }
            } else {
                return Some(k);
            }
        }
    }

    fn list_has_comma(&self, j: usize) -> bool {
        let mut depth = 0i32;
        for k in j..self.toks.len() {
            let t = self.toks[k];
            if t.is_punct("(") {
                depth += 1;
            } else if t.is_punct(")") {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            } else if depth == 0 {
                if t.is_punct(",") {
                    return true;
                }
                if t.is_punct(";") || (t.kind == TokenKind::Keyword && upper_eq_any(t.text, SELECT_END)) {
                    return false;
                }
            }
        }
        false
    }

    fn is_join_phrase(&self, i: usize) -> bool {
        let mut k = Some(i);
        while let Some(j) = k {
            let t = self.toks[j];
            if t.is_kw("JOIN") || t.is_kw("APPLY") {
                return true;
            }
            if !(t.kind == TokenKind::Keyword && upper_eq_any(t.text, JOIN_MODS)) {
                return false;
            }
            k = self.next_sig(j);
        }
        false
    }

    fn next_is_kw(&self, i: usize, list: &[&str]) -> bool {
        self.next_sig(i).is_some_and(|k| self.toks[k].kind == TokenKind::Keyword && upper_eq_any(self.toks[k].text, list))
    }

    /// Clause handling for a keyword in a query context. Returns the wanted newline level.
    fn query_keyword(&mut self, i: usize, up: &str) -> Option<usize> {
        let base = self.top().base;
        let first = self.last_sig_idx.is_none();
        let prev_kw = self.prev_kw();
        let prev_is_kw = prev_kw.is_some();
        let prev_kw = prev_kw.unwrap_or_default();
        let prev_open = self.last_sig_idx.is_some_and(|k| self.toks[k].is_punct("("));
        let set_clause = |s: &mut Self, c: Clause| s.top().clause = c;
        match up {
            "SELECT" => {
                set_clause(self, Clause::Select);
                if let Some(j) = self.select_items_start(i) {
                    if self.list_has_comma(j) {
                        self.item_nl = Some((j, base + 1));
                    }
                }
                Some(base)
            }
            "FROM" => {
                set_clause(self, Clause::From);
                if prev_kw == "DELETE" {
                    None
                } else {
                    Some(base)
                }
            }
            "WHERE" => {
                set_clause(self, Clause::Where);
                Some(base)
            }
            "GROUP" | "ORDER" if self.next_is_kw(i, &["BY"]) => {
                set_clause(self, Clause::Other);
                Some(base)
            }
            "HAVING" => {
                set_clause(self, Clause::Having);
                Some(base)
            }
            "LIMIT" | "OFFSET" | "FETCH" | "WINDOW" | "RETURNING" => {
                set_clause(self, Clause::Other);
                Some(base)
            }
            "UNION" | "INTERSECT" | "EXCEPT" | "MINUS" => {
                set_clause(self, Clause::None);
                Some(base)
            }
            "VALUES" => {
                set_clause(self, Clause::Values);
                let block = self.next_sig(i).is_some_and(|p| {
                    self.toks[p].is_punct("(")
                        && self.matches[p]
                            .and_then(|c| self.next_sig(c))
                            .is_some_and(|n| self.toks[n].is_punct(","))
                });
                self.top().values_block = block;
                if block {
                    if let Some(p) = self.next_sig(i) {
                        self.item_nl = Some((p, base + 1));
                    }
                }
                Some(base)
            }
            "SET" => {
                if !first && matches!(self.first_word.as_str(), "UPDATE" | "INSERT" | "WITH" | "REPLACE") {
                    set_clause(self, Clause::Set);
                    Some(base)
                } else {
                    None
                }
            }
            "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" => {
                if first || (!prev_is_kw && !(up == "REPLACE" && !self.next_is_kw(i, &["INTO"]))) {
                    set_clause(self, Clause::Other);
                    Some(base)
                } else {
                    None
                }
            }
            "RETURNS" | "LANGUAGE" if self.is_routine && self.stack.len() == 1 => Some(base),
            "AS" if self.is_routine
                && self.stack.len() == 1
                && self.next_sig(i).is_some_and(|k| self.toks[k].kind == TokenKind::String) =>
            {
                Some(base)
            }
            "WITH" if first || prev_open => {
                set_clause(self, Clause::With);
                Some(base)
            }
            "ON" => {
                if self.next_is_kw(i, &["CONFLICT", "DUPLICATE"]) {
                    set_clause(self, Clause::Other);
                    Some(base)
                } else if self.top().clause == Clause::From {
                    set_clause(self, Clause::On);
                    Some(base + 1)
                } else {
                    None
                }
            }
            "AND" | "OR" => {
                let top = self.top();
                if matches!(top.clause, Clause::Where | Clause::Having | Clause::On) {
                    if top.between {
                        top.between = false;
                        None
                    } else {
                        Some(base + 1)
                    }
                } else {
                    None
                }
            }
            "BETWEEN" => {
                self.top().between = true;
                None
            }
            "JOIN" | "APPLY" | "STRAIGHT_JOIN" => {
                set_clause(self, Clause::From);
                if upper_eq_any(&prev_kw, JOIN_MODS) {
                    None
                } else {
                    Some(base)
                }
            }
            _ if upper_eq_any(up, JOIN_MODS) && !upper_eq_any(&prev_kw, JOIN_MODS) && self.is_join_phrase(i) => {
                set_clause(self, Clause::From);
                Some(base)
            }
            _ => None,
        }
    }

    fn run(mut self) -> (String, bool) {
        for i in 0..self.toks.len() {
            let t = self.toks[i];
            if t.kind == TokenKind::Comment {
                self.comment(i);
                continue;
            }
            let mut want = None;
            if let Some((idx, lvl)) = self.item_nl {
                if idx == i {
                    want = Some(lvl);
                    self.item_nl = None;
                }
            }
            let top_kind = self.top().kind;
            let mut push_after: Option<Ctx> = None;
            let mut pop_after = false;
            let mut pending_after: Option<usize> = None;
            match t.kind {
                TokenKind::Keyword => {
                    let up = t.text.to_ascii_uppercase();
                    if top_kind == CtxKind::Case && matches!(up.as_str(), "WHEN" | "ELSE" | "END") {
                        let top = *self.top();
                        if up == "END" {
                            want = Some(top.ret);
                            pop_after = true;
                        } else {
                            want = Some(top.base);
                        }
                    } else if top_kind == CtxKind::Query {
                        if let Some(w) = self.query_keyword(i, &up) {
                            want = Some(w);
                        }
                    }
                    if up == "CASE" {
                        // Indentation is decided after the newline (if any) is applied.
                        push_after = Some(Ctx::new(CtxKind::Case, 0, 0, false));
                    }
                }
                TokenKind::Punctuation if t.text == "(" => {
                    let ns = self.next_sig(i).map(|k| self.toks[k]);
                    let sub = ns.is_some_and(|n| n.is_kw("SELECT") || n.is_kw("WITH") || n.is_kw("VALUES"));
                    if sub {
                        push_after = Some(Ctx::new(CtxKind::Query, 0, 0, true));
                    } else if self.is_create_table && !self.defs_done && self.stack.len() == 1 {
                        self.defs_done = true;
                        push_after = Some(Ctx::new(CtxKind::Defs, 0, 0, true));
                    } else {
                        push_after = Some(Ctx::new(CtxKind::Paren, 0, 0, true));
                    }
                }
                TokenKind::Punctuation if t.text == ")" => {
                    while self.stack.len() > 1 && self.top().kind == CtxKind::Case {
                        self.stack.pop();
                    }
                    if self.stack.len() > 1 && self.top().from_paren {
                        let c = self.stack.pop().unwrap_or(Ctx::new(CtxKind::Paren, 0, 0, true));
                        if matches!(c.kind, CtxKind::Query | CtxKind::Defs) {
                            want = Some(c.ret);
                        }
                    }
                }
                TokenKind::Punctuation if t.text == "," => {
                    let top = *self.top();
                    let brk = match top.kind {
                        CtxKind::Query => match top.clause {
                            Clause::Select | Clause::Set => Some(top.base + 1),
                            Clause::With => Some(top.base),
                            Clause::Values if top.values_block => Some(top.base + 1),
                            _ => None,
                        },
                        CtxKind::Defs => Some(top.base),
                        _ => None,
                    };
                    if let Some(l) = brk {
                        if self.opts.comma_first {
                            want = Some(l);
                        } else {
                            pending_after = Some(l);
                        }
                    }
                }
                _ => {}
            }
            self.emit(i, want);
            if let Some(l) = pending_after {
                self.pending = Some(l);
            }
            if pop_after && self.stack.len() > 1 {
                self.stack.pop();
            }
            if let Some(mut c) = push_after {
                let li = self.line_indent;
                c.base = li + 1;
                c.ret = li;
                if c.kind == CtxKind::Defs {
                    self.pending = Some(c.base);
                }
                self.stack.push(c);
            }
        }
        (self.out, self.last_line_comment)
    }
}

/// Format a single statement (no delimiter). Returns the text and whether it ends with a
/// line comment (so a following delimiter must go on the next line).
fn format_statement(text: &str, d: Dialect, opts: &FormatOptions) -> (String, bool) {
    StmtFormatter::new(text, d, opts).run()
}

fn ends_with_line_comment(text: &str, d: Dialect) -> bool {
    lexer::tokenize(text, d)
        .iter()
        .rev()
        .find(|t| t.kind != TokenKind::Whitespace)
        .is_some_and(|t| t.kind == TokenKind::Comment && !t.text(text).starts_with("/*"))
}

/// Would appending the custom delimiter `dl` directly to `text` change how either is lexed?
fn delimiter_needs_space(text: &str, dl: &str) -> bool {
    if format!("{text}{dl}").find(dl) != Some(text.len()) {
        return true;
    }
    let (Some(x), Some(y)) = (text.chars().last(), dl.chars().next()) else { return false };
    let word = |c: char| c.is_alphanumeric() || c == '_';
    (word(x) && word(y)) || (is_op_char(x) && is_op_char(y)) || (x.is_ascii_digit() && y == '.') || (x == '.' && y.is_ascii_digit())
}

enum Item {
    Block(String),
    Directive { text: String, go: bool },
    Comment { text: String, inline: bool, line: bool },
}

fn collect_comments(src: &str, from: usize, to: usize, d: Dialect, items: &mut Vec<Item>) {
    if from >= to {
        return;
    }
    let gap = &src[from..to];
    let mut nl = from == 0;
    for t in Lexer::new(gap, d) {
        let s = t.text(gap);
        match t.kind {
            TokenKind::Whitespace => nl |= s.contains('\n'),
            TokenKind::Comment => {
                let line = !s.starts_with("/*");
                let text = if line { s.trim_end() } else { s };
                items.push(Item::Comment { text: text.to_string(), inline: !nl, line });
                nl = false;
            }
            _ => {
                // Should not happen (gaps only hold trivia); keep the text anyway.
                items.push(Item::Comment { text: s.to_string(), inline: !nl, line: false });
                nl = false;
            }
        }
    }
}

/// Pretty-print a whole script.
pub fn format_sql(src: &str, dialect: Dialect, opts: &FormatOptions) -> String {
    let mut items = Vec::new();
    let mut cursor = 0usize;
    for piece in split_pieces(src, dialect) {
        match piece {
            Piece::Stmt { start, end, delim, block } => {
                collect_comments(src, cursor, start, dialect, &mut items);
                let body_end = delim.map_or(end, |(ds, _)| ds);
                let (mut text, line_end) = if block {
                    let raw = src[start..body_end].trim_end();
                    (raw.to_string(), ends_with_line_comment(raw, dialect))
                } else {
                    format_statement(&src[start..body_end], dialect, opts)
                };
                match delim {
                    Some((ds, de)) => {
                        let dl = &src[ds..de];
                        if line_end {
                            text.push('\n');
                        } else if dl != ";" && delimiter_needs_space(&text, dl) {
                            // Gluing would make the delimiter start inside the last token.
                            text.push(' ');
                        }
                        text.push_str(dl);
                        cursor = de;
                    }
                    None => cursor = end,
                }
                items.push(Item::Block(text));
            }
            Piece::Empty { delim: (ds, de) } => {
                collect_comments(src, cursor, ds, dialect, &mut items);
                items.push(Item::Block(src[ds..de].to_string()));
                cursor = de;
            }
            Piece::Directive { start, end, go } => {
                collect_comments(src, cursor, start, dialect, &mut items);
                items.push(Item::Directive { text: src[start..end].trim().to_string(), go });
                cursor = end;
            }
        }
    }
    collect_comments(src, cursor, src.len(), dialect, &mut items);

    let mut out = String::with_capacity(src.len() + 64);
    // Kind of the last non-inline item: 0 = none, 1 = block/directive, 2 = own-line comment.
    let mut prev = 0u8;
    let mut need_nl = false;
    let blank = "\n".repeat(opts.lines_between_statements);
    for item in items {
        match item {
            Item::Comment { text, inline: true, line } if prev != 0 && !need_nl => {
                out.push(' ');
                out.push_str(&text);
                need_nl = line;
            }
            Item::Comment { text, line, .. } => {
                match prev {
                    0 => {}
                    2 => out.push('\n'),
                    _ => {
                        out.push('\n');
                        out.push_str(&blank);
                    }
                }
                out.push_str(&text);
                prev = 2;
                need_nl = line;
            }
            Item::Directive { text, go: true } => {
                if prev != 0 {
                    out.push('\n');
                }
                out.push_str(&text);
                prev = 1;
                need_nl = false;
            }
            Item::Block(text) | Item::Directive { text, go: false } => {
                match prev {
                    0 => {}
                    2 => out.push('\n'),
                    _ => {
                        out.push('\n');
                        out.push_str(&blank);
                    }
                }
                out.push_str(&text);
                prev = 1;
                need_nl = false;
            }
        }
    }
    out
}

/// Collapse a script to a single line (for copying/logging). Line comments are turned into
/// block comments; string literals are kept verbatim.
pub fn compact_sql(src: &str, dialect: Dialect) -> String {
    let mut out = String::with_capacity(src.len());
    let mut sp = Spacer::default();
    for t in Lexer::new(src, dialect) {
        let text = t.text(src);
        match t.kind {
            TokenKind::Whitespace => {}
            TokenKind::Comment => {
                let body = if let Some(b) = text.strip_prefix("--") {
                    b
                } else if let Some(b) = text.strip_prefix('#') {
                    b
                } else {
                    text
                };
                let rendered;
                let c: &str = if body.len() == text.len() {
                    text
                } else {
                    let b = body.trim();
                    if b.contains("*/") {
                        continue;
                    }
                    rendered = format!("/* {b} */");
                    &rendered
                };
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(c);
                sp.push(TokenKind::Comment, text);
            }
            _ => {
                if sp.space_before(t.kind, text) {
                    out.push(' ');
                }
                out.push_str(text);
                sp.push(t.kind, text);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::CORPUS;

    fn fmt(src: &str, d: Dialect) -> String {
        format_sql(src, d, &FormatOptions::default())
    }

    /// Significant tokens (and comments) with keywords/types upper-cased.
    fn token_seq(src: &str, d: Dialect) -> Vec<String> {
        lexer::tokenize(src, d)
            .into_iter()
            .filter(|t| t.kind != TokenKind::Whitespace)
            .map(|t| {
                let s = t.text(src);
                match t.kind {
                    TokenKind::Keyword | TokenKind::Type => s.to_ascii_uppercase(),
                    TokenKind::Comment => s.trim_end().to_string(),
                    _ => s.to_string(),
                }
            })
            .collect()
    }

    #[test]
    fn corpus_preserves_tokens_and_is_idempotent() {
        for opts in [
            FormatOptions::default(),
            FormatOptions { keyword_case: KeywordCase::Lower, comma_first: true, indent: 2, max_line_length: 30, lines_between_statements: 2 },
            FormatOptions { keyword_case: KeywordCase::Preserve, ..Default::default() },
        ] {
            for (name, d, sql) in CORPUS {
                let once = format_sql(sql, *d, &opts);
                assert_eq!(token_seq(sql, *d), token_seq(&once, *d), "tokens changed for {name}:\n{once}");
                let twice = format_sql(&once, *d, &opts);
                assert_eq!(once, twice, "not idempotent for {name}");
                assert_eq!(
                    crate::split::split_statements(sql, *d).len(),
                    crate::split::split_statements(&once, *d).len(),
                    "statement count changed for {name}:\n{once}"
                );
            }
        }
    }

    #[test]
    fn basic_layout() {
        let out = fmt("select a, b from t where x = 1 and y = 2 order by a", Dialect::Generic);
        assert_eq!(out, "SELECT\n    a,\n    b\nFROM t\nWHERE x = 1\n    AND y = 2\nORDER BY a");
        assert_eq!(fmt("select * from t", Dialect::Generic), "SELECT *\nFROM t");
        assert_eq!(fmt("select count(*) from t", Dialect::Generic), "SELECT count(*)\nFROM t");
    }

    #[test]
    fn joins_and_subqueries() {
        let out = fmt(
            "select u.id from users u left join orders o on o.user_id = u.id and o.x between 1 and 2 where u.id in (select id from t)",
            Dialect::Postgres,
        );
        assert_eq!(
            out,
            "SELECT u.id\nFROM users u\nLEFT JOIN orders o\n    ON o.user_id = u.id\n    AND o.x BETWEEN 1 AND 2\nWHERE u.id IN (\n    SELECT id\n    FROM t\n)"
        );
    }

    #[test]
    fn case_and_cte() {
        let out = fmt("with a as (select 1 x) select case when x = 1 then 'a' else 'b' end as y, x from a", Dialect::Generic);
        assert_eq!(
            out,
            "WITH a AS (\n    SELECT 1 x\n)\nSELECT\n    CASE\n        WHEN x = 1 THEN 'a'\n        ELSE 'b'\n    END AS y,\n    x\nFROM a"
        );
    }

    #[test]
    fn create_table_and_insert() {
        let out = fmt("create table t (id int primary key, name varchar(10) not null, check (id > 0))", Dialect::Generic);
        assert_eq!(out, "CREATE TABLE t (\n    id INT PRIMARY KEY,\n    name VARCHAR(10) NOT NULL,\n    CHECK (id > 0)\n)");
        let out = fmt("insert into t(a, b) values (1, 2), (3, -4)", Dialect::Generic);
        assert_eq!(out, "INSERT INTO t (a, b)\nVALUES\n    (1, 2),\n    (3, -4)");
        let out = fmt("update t set a = 1, b = x::int where id = $1", Dialect::Postgres);
        assert_eq!(out, "UPDATE t\nSET a = 1,\n    b = x::INT\nWHERE id = $1");
    }

    #[test]
    fn comments_and_statements() {
        let src = "-- header\nselect 1; -- trailing\n/* block */\nselect a, -- why\n b from t;";
        let out = fmt(src, Dialect::Generic);
        assert_eq!(out, "-- header\nSELECT 1; -- trailing\n\n/* block */\nSELECT\n    a, -- why\n    b\nFROM t;");
        let out = fmt("select 1 -- c\n;", Dialect::Generic);
        assert_eq!(out, "SELECT 1 -- c\n;");
    }

    #[test]
    fn preserves_strings_and_identifiers() {
        let src = "select 'Hello   World', \"MixedCase\", `x` from MyTable";
        let out = fmt(src, Dialect::MySql);
        assert!(out.contains("'Hello   World'") && out.contains("\"MixedCase\"") && out.contains("MyTable"));
    }

    #[test]
    fn unary_and_operators() {
        assert_eq!(fmt("select - -1, a-b, -(x)", Dialect::Generic), "SELECT\n    - -1,\n    a - b,\n    -(x)");
        assert_eq!(compact_sql("select\n  a ->> 'b'  -- c\nfrom t;\n\nselect 2", Dialect::Postgres), "select a ->> 'b' /* c */ from t; select 2");
    }

    #[test]
    fn delimiter_and_go_scripts() {
        let src = "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\nDELIMITER ;\nselect 1;";
        let out = fmt(src, Dialect::MySql);
        assert_eq!(out, "DELIMITER $$\n\nCREATE PROCEDURE p() BEGIN SELECT 1; END$$\n\nDELIMITER ;\n\nSELECT 1;");
        let out = fmt("select 1\ngo\nselect 2", Dialect::MsSql);
        assert_eq!(out, "SELECT 1\ngo\n\nSELECT 2");
    }

    #[test]
    fn never_panics_on_garbage() {
        let inputs = [
            "", ")))", "(((", "select (", "case when", "end end", ") select (", "'", "/*", "--",
            "select 'ü' from 表 where ☃ = 1", "select ,,,", "GO", "DELIMITER", "DELIMITER $$", ";;;",
            "select * from (select * from (select", "values (", "with", "with a as (", "))(( case",
        ];
        for s in inputs {
            for d in Dialect::ALL {
                let a = fmt(s, d);
                let _ = compact_sql(s, d);
                assert_eq!(fmt(&a, d), a, "{s:?} {d:?}");
            }
        }
    }
}
