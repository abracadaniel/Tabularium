//! Lossless, dialect-aware SQL tokenizer used for highlighting and by every other module.
//!
//! The lexer never fails: unknown characters become [`TokenKind::Unknown`] tokens and
//! unterminated strings/comments/quoted identifiers extend to the end of input. The
//! concatenation of all token texts is always exactly the input.

use tabularium_core::Dialect;

use crate::words;

/// Classification of a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// Reserved or well-known SQL keyword (`SELECT`, `FROM`, ...).
    Keyword,
    /// Unquoted identifier.
    Identifier,
    /// Identifier in `"..."`, `` `...` `` or `[...]`.
    QuotedIdentifier,
    /// String literal, including prefixed (`E'..'`, `N'..'`, `X'..'`) and dollar-quoted strings.
    String,
    /// Numeric literal (integer, decimal, exponent, hex).
    Number,
    /// `-- ...`, `# ...` (MySQL) or `/* ... */` comment.
    Comment,
    /// Operator such as `=`, `<>`, `::`, `||`, `->>`.
    Operator,
    /// `(`, `)`, `,`, `;`, `.`, `[`, `]`, `{`, `}`.
    Punctuation,
    /// Run of whitespace.
    Whitespace,
    /// Query parameter / variable: `:name`, `?`, `$1`, `${name}`, `@var`.
    Parameter,
    /// Builtin function name directly followed by `(`.
    Function,
    /// Data type name (`int`, `varchar`, ...).
    Type,
    /// Anything else (a single character).
    Unknown,
}

/// A token: kind plus byte range into the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    /// Token classification.
    pub kind: TokenKind,
    /// Start byte offset (inclusive, on a char boundary).
    pub start: usize,
    /// End byte offset (exclusive, on a char boundary).
    pub end: usize,
}

impl Token {
    /// The token's text in `src` (empty if the range is invalid for `src`).
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        src.get(self.start..self.end).unwrap_or("")
    }

    /// True for whitespace and comments.
    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokenKind::Whitespace | TokenKind::Comment)
    }

    /// True for words that may name an object (identifier, keyword, type, function).
    pub(crate) fn is_word(&self) -> bool {
        matches!(
            self.kind,
            TokenKind::Identifier | TokenKind::Keyword | TokenKind::Type | TokenKind::Function
        )
    }
}

/// Tokenize `src` losslessly: tokens cover the whole input contiguously.
pub fn tokenize(src: &str, dialect: Dialect) -> Vec<Token> {
    let mut out = Vec::with_capacity(src.len() / 3 + 1);
    out.extend(Lexer::new(src, dialect));
    out
}

/// Tokenize and drop whitespace and comments.
pub(crate) fn significant(src: &str, dialect: Dialect) -> Vec<Token> {
    Lexer::new(src, dialect).filter(|t| !t.is_trivia()).collect()
}

/// Case-insensitive keyword test.
pub fn is_keyword(word: &str) -> bool {
    let mut buf = [0u8; 48];
    words::upper_in(word, &mut buf).is_some_and(|u| words::keyword_set().contains(u))
}

/// Case-insensitive data type name test (`int`, `varchar`, `timestamp`, ...).
pub fn is_type_name(word: &str) -> bool {
    let mut buf = [0u8; 48];
    words::upper_in(word, &mut buf).is_some_and(|u| words::type_set().contains(u))
}

/// Case-insensitive builtin function test (union over all dialects).
pub fn is_builtin_function(word: &str) -> bool {
    let mut buf = [0u8; 48];
    words::lower_in(word, &mut buf).is_some_and(|l| words::function_set().contains(l))
}

/// All keywords known to the lexer, upper case.
pub fn keywords() -> &'static [&'static str] {
    words::KEYWORDS
}

/// All data type names known to the lexer, upper case.
pub fn type_names() -> &'static [&'static str] {
    words::TYPES
}

/// Builtin functions of `dialect` (lower case, sorted). `Generic` returns the union.
pub fn builtin_functions(dialect: Dialect) -> &'static [&'static str] {
    words::functions_for(dialect)
}

/// Multi-character operators, longest first.
const OPERATORS: &[&str] = &[
    "->>", "#>>", "!~*", "<=>", "::", "<>", "!=", ">=", "<=", "||", "->", "@>", "<@", "#>", ":=",
    "=>", "<<", ">>", "&&", "~*", "!~", "**", "?|", "?&", "@@", "!!",
];

fn is_operator_byte(b: u8) -> bool {
    matches!(
        b,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'~' | b'^' | b'&' | b'|'
            | b'@' | b'#' | b':' | b'?'
    )
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic())
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphanumeric() || (!c.is_ascii() && c.is_alphanumeric())
}

/// Streaming lexer. Exposed to the crate so the splitter can restart at arbitrary offsets.
#[derive(Clone)]
pub(crate) struct Lexer<'a> {
    src: &'a str,
    b: &'a [u8],
    pub(crate) pos: usize,
    d: Dialect,
    prev_dot: bool,
    prev: Option<Token>,
    /// Set when the most recently returned token is an unterminated string/comment/identifier.
    pub(crate) unterminated: bool,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(src: &'a str, d: Dialect) -> Self {
        Lexer { src, b: src.as_bytes(), pos: 0, d, prev_dot: false, prev: None, unterminated: false }
    }

    /// Restart lexing at `pos` (must be a char boundary).
    pub(crate) fn set_pos(&mut self, pos: usize) {
        let mut p = pos.min(self.src.len());
        while !self.src.is_char_boundary(p) {
            p -= 1;
        }
        self.pos = p;
        self.prev_dot = false;
        self.prev = None;
    }

    fn pg_like(&self) -> bool {
        matches!(self.d, Dialect::Postgres | Dialect::Generic)
    }

    fn mysql(&self) -> bool {
        self.d == Dialect::MySql
    }

    fn peek(&self, off: usize) -> u8 {
        self.b.get(self.pos + off).copied().unwrap_or(0)
    }

    fn char_at(&self, pos: usize) -> Option<char> {
        self.src.get(pos..).and_then(|s| s.chars().next())
    }

    fn consume_ident_chars(&mut self) {
        while self.pos < self.b.len() {
            let b = self.b[self.pos];
            if b < 0x80 {
                if b == b'_' || b == b'$' || b.is_ascii_alphanumeric() {
                    self.pos += 1;
                } else {
                    break;
                }
            } else {
                match self.char_at(self.pos) {
                    Some(c) if is_ident_char(c) => self.pos += c.len_utf8(),
                    _ => break,
                }
            }
        }
    }

    fn line_comment(&mut self) -> TokenKind {
        let rest = &self.b[self.pos..];
        let len = memchr_nl(rest).unwrap_or(rest.len());
        let mut end = self.pos + len;
        if end > self.pos && self.b[end - 1] == b'\r' && end < self.b.len() {
            end -= 1;
        }
        self.pos = end;
        TokenKind::Comment
    }

    fn block_comment(&mut self) -> TokenKind {
        let nested = self.pg_like();
        let mut depth = 0usize;
        let mut i = self.pos;
        let n = self.b.len();
        while i < n {
            if self.b[i] == b'/' && i + 1 < n && self.b[i + 1] == b'*' {
                if depth == 0 || nested {
                    depth += 1;
                }
                i += 2;
            } else if self.b[i] == b'*' && i + 1 < n && self.b[i + 1] == b'/' {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    self.pos = i;
                    return TokenKind::Comment;
                }
            } else {
                i += 1;
            }
        }
        self.pos = n;
        self.unterminated = true;
        TokenKind::Comment
    }

    /// Quoted run starting at `self.pos` (pointing at the opening quote).
    /// `close` is the closing byte; doubling `close` escapes it; `backslash` enables `\x` escapes.
    fn quoted(&mut self, close: u8, backslash: bool) {
        let n = self.b.len();
        let mut i = self.pos + 1;
        while i < n {
            let c = self.b[i];
            if backslash && c == b'\\' {
                i += 2;
                continue;
            }
            if c == close {
                if i + 1 < n && self.b[i + 1] == close {
                    i += 2;
                    continue;
                }
                self.pos = i + 1;
                return;
            }
            i += 1;
        }
        self.pos = n;
        self.unterminated = true;
    }

    fn number(&mut self) -> TokenKind {
        let n = self.b.len();
        if self.peek(0) == b'0'
            && matches!(self.peek(1), b'x' | b'X')
            && self.peek(2).is_ascii_hexdigit()
        {
            self.pos += 2;
            while self.pos < n && self.b[self.pos].is_ascii_hexdigit() {
                self.pos += 1;
            }
            return TokenKind::Number;
        }
        while self.pos < n && self.b[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        if self.peek(0) == b'.' && self.peek(1) != b'.' {
            self.pos += 1;
            while self.pos < n && self.b[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if matches!(self.peek(0), b'e' | b'E') {
            let (a, b) = (self.peek(1), self.peek(2));
            if a.is_ascii_digit() {
                self.pos += 1;
            } else if matches!(a, b'+' | b'-') && b.is_ascii_digit() {
                self.pos += 2;
            } else {
                return TokenKind::Number;
            }
            while self.pos < n && self.b[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        TokenKind::Number
    }

    fn dollar(&mut self) -> TokenKind {
        let n = self.b.len();
        // ${name}
        if self.peek(1) == b'{' {
            let mut j = self.pos + 2;
            while j < n && self.b[j] != b'}' && self.b[j] != b'\n' && j - self.pos < 256 {
                j += 1;
            }
            if j < n && self.b[j] == b'}' && j > self.pos + 2 {
                self.pos = j + 1;
                return TokenKind::Parameter;
            }
        }
        // $1
        if self.peek(1).is_ascii_digit() {
            self.pos += 1;
            while self.pos < n && self.b[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            return TokenKind::Parameter;
        }
        if self.pg_like() {
            let mut j = self.pos + 1;
            while j < n && (self.b[j] == b'_' || self.b[j].is_ascii_alphanumeric()) {
                j += 1;
            }
            if j < n && self.b[j] == b'$' {
                let delim = &self.src[self.pos..=j];
                let body_start = j + 1;
                match self.src[body_start..].find(delim) {
                    Some(k) => self.pos = body_start + k + delim.len(),
                    None => {
                        self.pos = n;
                        self.unterminated = true;
                    }
                }
                return TokenKind::String;
            }
        }
        self.pos += 1;
        TokenKind::Unknown
    }

    fn operator(&mut self) -> TokenKind {
        let rest = &self.src[self.pos..];
        for op in OPERATORS {
            if rest.starts_with(op) {
                self.pos += op.len();
                return TokenKind::Operator;
            }
        }
        self.pos += 1;
        TokenKind::Operator
    }

    fn whitespace(&mut self) -> TokenKind {
        while self.pos < self.b.len() {
            let b = self.b[self.pos];
            if b < 0x80 {
                if b.is_ascii_whitespace() || b == 0x0b {
                    self.pos += 1;
                } else {
                    break;
                }
            } else {
                match self.char_at(self.pos) {
                    Some(c) if c.is_whitespace() => self.pos += c.len_utf8(),
                    _ => break,
                }
            }
        }
        TokenKind::Whitespace
    }

    /// Next non-whitespace byte at or after `pos`.
    fn next_non_space(&self, mut pos: usize) -> u8 {
        while pos < self.b.len() && self.b[pos].is_ascii_whitespace() {
            pos += 1;
        }
        self.b.get(pos).copied().unwrap_or(0)
    }

    fn word(&mut self, start: usize) -> TokenKind {
        self.consume_ident_chars();
        let len = self.pos - start;
        // Prefixed strings: E'..', N'..', X'..', B'..'
        if len == 1 && self.peek(0) == b'\'' {
            match self.b[start] {
                b'e' | b'E' if self.pg_like() => {
                    self.quoted(b'\'', true);
                    return TokenKind::String;
                }
                b'n' | b'N' | b'x' | b'X' | b'b' | b'B' => {
                    let mysql = self.mysql();
                    self.quoted(b'\'', mysql);
                    return TokenKind::String;
                }
                _ => {}
            }
        }
        let word = &self.src[start..self.pos];
        let next_pos = {
            let mut p = self.pos;
            while p < self.b.len() && self.b[p].is_ascii_whitespace() {
                p += 1;
            }
            p
        };
        // A following `.` qualifies this word unless it starts a number (`SELECT .5`).
        let dot_follows = self.b.get(next_pos) == Some(&b'.')
            && (next_pos == self.pos || !self.b.get(next_pos + 1).is_some_and(|c| c.is_ascii_digit()));
        if self.prev_dot || dot_follows {
            return TokenKind::Identifier;
        }
        let paren_follows = self.next_non_space(self.pos) == b'(';
        if paren_follows && is_builtin_function(word) && !is_type_name(word) {
            return TokenKind::Function;
        }
        if is_type_name(word) {
            return TokenKind::Type;
        }
        if is_keyword(word) {
            return TokenKind::Keyword;
        }
        TokenKind::Identifier
    }

    fn prev_is_wordlike_adjacent(&self, pos: usize) -> bool {
        match self.prev {
            Some(t) if t.end == pos => {
                matches!(
                    t.kind,
                    TokenKind::Identifier
                        | TokenKind::QuotedIdentifier
                        | TokenKind::Keyword
                        | TokenKind::Type
                        | TokenKind::Function
                        | TokenKind::Number
                ) || (t.kind == TokenKind::Punctuation && matches!(self.b[t.start], b')' | b']'))
            }
            _ => false,
        }
    }

    fn lex_one(&mut self) -> TokenKind {
        let start = self.pos;
        let c = self.b[start];
        let d = self.d;
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => self.whitespace(),
            b'-' if self.peek(1) == b'-' => self.line_comment(),
            b'/' if self.peek(1) == b'*' => self.block_comment(),
            b'#' if self.mysql() => self.line_comment(),
            b'#' if d == Dialect::MsSql && {
                let n = self.peek(1);
                n == b'#' || n == b'_' || n.is_ascii_alphabetic()
            } =>
            {
                self.pos += 1;
                if self.peek(0) == b'#' {
                    self.pos += 1;
                }
                self.consume_ident_chars();
                TokenKind::Identifier
            }
            b'\'' => {
                let bs = self.mysql();
                self.quoted(b'\'', bs);
                TokenKind::String
            }
            b'"' => {
                if self.mysql() {
                    self.quoted(b'"', true);
                    TokenKind::String
                } else {
                    self.quoted(b'"', false);
                    TokenKind::QuotedIdentifier
                }
            }
            b'`' => {
                self.quoted(b'`', false);
                TokenKind::QuotedIdentifier
            }
            b'[' if matches!(d, Dialect::MsSql | Dialect::Sqlite) => {
                self.quoted(b']', false);
                TokenKind::QuotedIdentifier
            }
            b'$' => self.dollar(),
            b':' => {
                let n = self.peek(1);
                if n == b':' || n == b'=' {
                    self.pos += 2;
                    TokenKind::Operator
                } else if n == b'_' || n.is_ascii_alphabetic() {
                    self.pos += 1;
                    self.consume_ident_chars();
                    TokenKind::Parameter
                } else {
                    self.pos += 1;
                    TokenKind::Operator
                }
            }
            b'?' => {
                let n = self.peek(1);
                if d == Dialect::Postgres && (n == b'|' || n == b'&') {
                    self.pos += 2;
                    TokenKind::Operator
                } else {
                    self.pos += 1;
                    while self.pos < self.b.len() && self.b[self.pos].is_ascii_digit() {
                        self.pos += 1;
                    }
                    TokenKind::Parameter
                }
            }
            b'@' if d != Dialect::Postgres && {
                let n = self.peek(1);
                n == b'@' || n == b'_' || n.is_ascii_alphabetic()
            } =>
            {
                self.pos += 1;
                if self.peek(0) == b'@' {
                    self.pos += 1;
                }
                self.consume_ident_chars();
                TokenKind::Parameter
            }
            b'0'..=b'9' => self.number(),
            b'.' if self.peek(1).is_ascii_digit() && !self.prev_is_wordlike_adjacent(start) => {
                self.number()
            }
            b'(' | b')' | b',' | b';' | b'.' | b'[' | b']' | b'{' | b'}' => {
                self.pos += 1;
                TokenKind::Punctuation
            }
            _ if is_operator_byte(c) => self.operator(),
            _ if c < 0x80 => {
                if c == b'_' || c.is_ascii_alphabetic() {
                    self.word(start)
                } else {
                    self.pos += 1;
                    TokenKind::Unknown
                }
            }
            _ => match self.char_at(start) {
                Some(ch) if ch.is_whitespace() => self.whitespace(),
                Some(ch) if is_ident_start(ch) => self.word(start),
                Some(ch) => {
                    self.pos += ch.len_utf8();
                    TokenKind::Unknown
                }
                None => {
                    // Not on a char boundary (cannot happen); advance to the next boundary.
                    self.pos += 1;
                    while !self.src.is_char_boundary(self.pos) {
                        self.pos += 1;
                    }
                    TokenKind::Unknown
                }
            },
        }
    }
}

fn memchr_nl(b: &[u8]) -> Option<usize> {
    b.iter().position(|&c| c == b'\n')
}

impl Iterator for Lexer<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        if self.pos >= self.b.len() {
            return None;
        }
        self.unterminated = false;
        let start = self.pos;
        let kind = self.lex_one();
        debug_assert!(self.pos > start);
        if self.pos <= start {
            self.pos = start + 1;
        }
        let tok = Token { kind, start, end: self.pos };
        if !tok.is_trivia() {
            self.prev_dot = kind == TokenKind::Punctuation && self.b[start] == b'.';
        }
        self.prev = Some(tok);
        Some(tok)
    }
}

/// Remove identifier quoting (`"a""b"` → `a"b`, `` `x` `` → `x`, `[x]` → `x`).
pub(crate) fn unquote(text: &str) -> String {
    let b = text.as_bytes();
    if b.len() >= 2 {
        let (open, close) = (b[0], b[b.len() - 1]);
        let pair = match open {
            b'"' => Some(b'"'),
            b'`' => Some(b'`'),
            b'[' => Some(b']'),
            _ => None,
        };
        if let Some(c) = pair {
            if close == c {
                let inner = &text[1..text.len() - 1];
                let cs = c as char;
                let doubled: String = [cs, cs].iter().collect();
                return inner.replace(&doubled, &cs.to_string());
            }
        }
    }
    if let Some(first) = b.first() {
        if matches!(first, b'"' | b'`' | b'[') {
            return text[1..].to_string();
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use TokenKind::*;

    fn kinds(src: &str, d: Dialect) -> Vec<(TokenKind, std::string::String)> {
        tokenize(src, d)
            .into_iter()
            .filter(|t| t.kind != Whitespace)
            .map(|t| (t.kind, t.text(src).to_string()))
            .collect()
    }

    fn assert_lossless(src: &str) {
        for d in Dialect::ALL {
            let toks = tokenize(src, d);
            let mut pos = 0;
            for t in &toks {
                assert_eq!(t.start, pos, "gap in {src:?} ({d:?})");
                assert!(t.end > t.start);
                assert!(src.is_char_boundary(t.start) && src.is_char_boundary(t.end));
                pos = t.end;
            }
            assert_eq!(pos, src.len(), "not covering {src:?} ({d:?})");
            let joined: std::string::String = toks.iter().map(|t| t.text(src)).collect();
            assert_eq!(joined, src);
        }
    }

    #[test]
    fn lossless_on_many_inputs() {
        let inputs = [
            "",
            " ",
            "select 1",
            "SELECT * FROM t WHERE a = 'it''s' AND b = \"x\"",
            "select 'unterminated",
            "select \"unterminated",
            "/* unterminated comment",
            "/* nested /* comment */ still */ select",
            "select $$ body $$, $tag$ x $$ y $tag$, $unterminated$ ...",
            "select ${param}, ${, $1, $, :name, ::int, :=, ?, ?1, @var, @@global",
            "select E'a\\'b', N'x', X'ff', B'01', e'\\\\'",
            "select `a``b`, [x]]y], [unterminated",
            "SELECT 'héllo wörld', \"Ünïcödé\" FROM таблица WHERE имя = 'значение' -- коммент",
            "select '😀', 😀, ★, «quoted» from t",
            "a->>'b' #>> '{a}' @> <@ || <> != >= <= -> !~* ~* **",
            "0x1F 1.5e10 .5 1e-3 1.e 12abc 3..4",
            "# mysql comment\nselect 1",
            "select 1 -- comment\r\nselect 2\r\n",
            "\u{a0}select\u{2003}1\u{3000}",
            "'\\",
            "\\",
            "$",
            "E'",
            "x'",
            "[",
            "`",
            "#",
            "-",
            "/",
            "/*",
            "--",
            ".",
            "..5",
            "\0\x01\x7f",
            "select #temp, ##g from #t",
        ];
        for s in inputs {
            assert_lossless(s);
        }
    }

    #[test]
    fn lossless_on_every_prefix() {
        let s = "SELECT \"é\", 'x''y', $a$ ü $a$ /* c */ -- d\n FROM [t] WHERE `x` = :p AND y::int = ${q}";
        for (i, _) in s.char_indices() {
            assert_lossless(&s[..i]);
            assert_lossless(&s[i..]);
        }
    }

    #[test]
    fn classifies_words() {
        let k = kinds("SELECT count(*), name, varchar(10), coalesce (a) FROM t", Dialect::Generic);
        assert_eq!(k[0], (Keyword, "SELECT".into()));
        assert_eq!(k[1], (Function, "count".into()));
        assert_eq!(k[6], (Identifier, "name".into()));
        assert_eq!(k[8], (Type, "varchar".into()));
        assert_eq!(k[13], (Function, "coalesce".into()));
        let k = kinds("t.select, user.name", Dialect::Postgres);
        assert_eq!(k[2], (Identifier, "select".into()));
        assert_eq!(k[4], (Identifier, "user".into()));
    }

    #[test]
    fn strings_and_comments() {
        let k = kinds("'a''b' E'x\\'y' -- c\n/* d */", Dialect::Postgres);
        assert_eq!(
            k,
            vec![
                (String, "'a''b'".into()),
                (String, "E'x\\'y'".into()),
                (Comment, "-- c".into()),
                (Comment, "/* d */".into())
            ]
        );
        // MySQL: backslash escapes, double-quoted strings and # comments.
        let k = kinds("'a\\'b' \"q\" # c", Dialect::MySql);
        assert_eq!(k, vec![(String, "'a\\'b'".into()), (String, "\"q\"".into()), (Comment, "# c".into())]);
        // Postgres: standard strings do not use backslash escapes.
        let k = kinds("'a\\' b", Dialect::Postgres);
        assert_eq!(k[0], (String, "'a\\'".into()));
        // Nested comments in Postgres only.
        let k = kinds("/* a /* b */ c */ x", Dialect::Postgres);
        assert_eq!(k.len(), 2);
        let k = kinds("/* a /* b */ c */ x", Dialect::MySql);
        assert_eq!(k[0], (Comment, "/* a /* b */".into()));
    }

    #[test]
    fn dollar_quotes_and_parameters() {
        let k = kinds("$$ a ; b $$ $fn$ x $fn$ $1 ${p} :name ?", Dialect::Postgres);
        assert_eq!(k[0], (String, "$$ a ; b $$".into()));
        assert_eq!(k[1], (String, "$fn$ x $fn$".into()));
        assert_eq!(k[2], (Parameter, "$1".into()));
        assert_eq!(k[3], (Parameter, "${p}".into()));
        assert_eq!(k[4], (Parameter, ":name".into()));
        assert_eq!(k[5], (Parameter, "?".into()));
        let k = kinds("a::int @var", Dialect::MsSql);
        assert_eq!(k[1], (Operator, "::".into()));
        assert_eq!(k[3], (Parameter, "@var".into()));
        let k = kinds("a @> b", Dialect::Postgres);
        assert_eq!(k[1], (Operator, "@>".into()));
    }

    #[test]
    fn quoted_identifiers() {
        assert_eq!(kinds("[a b]", Dialect::MsSql)[0], (QuotedIdentifier, "[a b]".into()));
        assert_eq!(kinds("`a`", Dialect::MySql)[0], (QuotedIdentifier, "`a`".into()));
        assert_eq!(kinds("\"a\"", Dialect::Postgres)[0], (QuotedIdentifier, "\"a\"".into()));
        assert_eq!(kinds("a[1]", Dialect::Postgres)[1], (Punctuation, "[".into()));
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote("[x]]y]"), "x]y");
        assert_eq!(unquote("plain"), "plain");
    }

    #[test]
    fn numbers_and_operators() {
        let k = kinds("0x1F 1.5e10 .5 1e-3 a.b", Dialect::Generic);
        assert_eq!(k[0], (Number, "0x1F".into()));
        assert_eq!(k[1], (Number, "1.5e10".into()));
        assert_eq!(k[2], (Number, ".5".into()));
        assert_eq!(k[3], (Number, "1e-3".into()));
        assert_eq!(k[5], (Punctuation, ".".into()));
        let k = kinds("a->>'b' <> c || d", Dialect::Postgres);
        assert_eq!(k[1], (Operator, "->>".into()));
        assert_eq!(k[3], (Operator, "<>".into()));
        assert_eq!(k[5], (Operator, "||".into()));
    }

    #[test]
    fn keyword_helpers() {
        assert!(is_keyword("select") && is_keyword("SeLeCt"));
        assert!(!is_keyword("users"));
        assert!(is_type_name("VarChar") && is_type_name("timestamptz"));
        assert!(is_builtin_function("COUNT") && is_builtin_function("coalesce"));
        assert!(!is_builtin_function("values"));
        assert!(builtin_functions(Dialect::Postgres).contains(&"string_agg"));
        assert!(builtin_functions(Dialect::MySql).contains(&"group_concat"));
        assert!(keywords().contains(&"SELECT"));
        assert!(!is_keyword("ß".repeat(40).as_str()));
    }

    #[test]
    fn unterminated_flag() {
        let mut lx = Lexer::new("select 'abc", Dialect::Generic);
        let mut flags = vec![];
        while let Some(t) = lx.next() {
            flags.push((t.kind, lx.unterminated));
        }
        assert_eq!(flags.last(), Some(&(String, true)));
    }
}
