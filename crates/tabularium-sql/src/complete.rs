//! Context-aware code completion.

use std::collections::HashSet;

use tabularium_core::{Dialect, ObjectKind};

use crate::analyze::{self, kw, punct, RefSpan};
use crate::catalog::{CatalogTable, SchemaCatalog};
use crate::lexer::{self, unquote, Token, TokenKind};
use crate::split::region_at;
use crate::templates::{templates, CURSOR_MARKER};

/// Kind of a completion item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    /// SQL keyword (possibly multi-word, e.g. `GROUP BY`).
    Keyword,
    /// Schema name.
    Schema,
    /// Table (or CTE / other relation that is not a view).
    Table,
    /// View or materialized view.
    View,
    /// Column of a table in scope.
    Column,
    /// Builtin or catalog function.
    Function,
    /// Table alias defined in the statement.
    Alias,
    /// Live template; `insert_text` is the template body (may contain `$CURSOR$`).
    Snippet,
    /// Data type name.
    Type,
}

/// One completion proposal.
#[derive(Debug, Clone)]
pub struct CompletionItem {
    /// Text shown in the popup.
    pub label: String,
    /// Text inserted in place of `replace_start..replace_end`. For snippets this is the
    /// template body, which may contain `$CURSOR$`.
    pub insert_text: String,
    /// Item kind (icon).
    pub kind: CompletionKind,
    /// Secondary text (type, owning table, schema...).
    pub detail: Option<String>,
}

/// Completion result: the byte range to replace and the ranked proposals.
#[derive(Debug, Clone, Default)]
pub struct Completion {
    /// Start byte offset of the identifier fragment to replace.
    pub replace_start: usize,
    /// End byte offset of the identifier fragment to replace (may extend past the cursor).
    pub replace_end: usize,
    /// Ranked proposals (best first), at most [`MAX_ITEMS`].
    pub items: Vec<CompletionItem>,
}

/// Maximum number of items returned.
pub const MAX_ITEMS: usize = 200;

/// Match quality of `label` against the typed `frag`: higher is better.
/// 4 = exact-case prefix, 3 = case-insensitive prefix, 2 = camel-hump/word initials, 1 = substring.
fn match_rank(label: &str, frag: &str) -> Option<u8> {
    if frag.is_empty() {
        return Some(1);
    }
    if label.starts_with(frag) {
        return Some(4);
    }
    let ll = label.to_lowercase();
    let fl = frag.to_lowercase();
    if ll.starts_with(&fl) {
        return Some(3);
    }
    if hump_match(label, &fl) {
        return Some(2);
    }
    if ll.contains(&fl) {
        return Some(1);
    }
    None
}

/// Camel-hump / word-initial match: each typed char continues the current word or starts
/// the next word (`ua` matches `user_accounts` and `UserAccounts`, `usac` too).
fn hump_match(label: &str, frag_lower: &str) -> bool {
    // Split the label into words.
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in label.chars() {
        if c == '_' || c == '-' || c == ' ' || c == '.' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let f: Vec<char> = frag_lower.chars().collect();
    fn rec(words: &[String], wi: usize, ci: usize, f: &[char], fi: usize, budget: &mut u32) -> bool {
        if fi == f.len() {
            return true;
        }
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        // continue current word
        if wi < words.len() {
            let w: Vec<char> = words[wi].chars().collect();
            if ci > 0 && ci < w.len() && w[ci] == f[fi] && rec(words, wi, ci + 1, f, fi + 1, budget) {
                return true;
            }
        }
        // start a later word
        for nw in wi + usize::from(ci > 0)..words.len() {
            if words[nw].starts_with(f[fi]) && rec(words, nw, 1, f, fi + 1, budget) {
                return true;
            }
        }
        false
    }
    let mut budget = 2000;
    words.len() > 1 && rec(&words, 0, 0, &f, 0, &mut budget)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Start,
    Keyword,
    Table,
    Column,
    Type,
}

struct Builder<'a> {
    frag: &'a str,
    items: Vec<(u8, u8, CompletionItem)>,
    seen: HashSet<(CompletionKind, String)>,
    lower_kw: bool,
}

impl<'a> Builder<'a> {
    fn add(&mut self, prio: u8, kind: CompletionKind, label: &str, insert: String, detail: Option<String>) {
        let Some(mut rank) = match_rank(label, self.frag) else { return };
        // Keyword-like items are case-insensitive: any prefix match is a full match.
        if rank == 3 && matches!(kind, CompletionKind::Keyword | CompletionKind::Snippet | CompletionKind::Type | CompletionKind::Function) {
            rank = 4;
        }
        if !self.seen.insert((kind, label.to_string())) {
            return;
        }
        self.items.push((rank, prio, CompletionItem { label: label.to_string(), insert_text: insert, kind, detail }));
    }

    fn keyword(&mut self, prio: u8, k: &str) {
        let ins = if self.lower_kw { k.to_ascii_lowercase() } else { k.to_string() };
        self.add(prio, CompletionKind::Keyword, k, ins, None);
    }

    fn finish(mut self) -> Vec<CompletionItem> {
        // Stable: ties keep insertion order (catalog / keyword list order).
        self.items.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.items.truncate(MAX_ITEMS);
        self.items.into_iter().map(|x| x.2).collect()
    }
}

fn table_kind(t: &CatalogTable) -> CompletionKind {
    match t.kind {
        ObjectKind::View | ObjectKind::MaterializedView => CompletionKind::View,
        _ => CompletionKind::Table,
    }
}

const START_KEYWORDS: &[&str] = &[
    "SELECT", "INSERT INTO", "UPDATE", "DELETE FROM", "WITH", "CREATE TABLE", "CREATE INDEX",
    "CREATE VIEW", "ALTER TABLE", "DROP TABLE", "TRUNCATE TABLE", "EXPLAIN", "BEGIN", "COMMIT",
    "ROLLBACK", "SET", "SHOW", "GRANT", "REVOKE", "CALL", "VALUES",
];
const CLAUSE_KEYWORDS: &[&str] = &[
    "FROM", "WHERE", "AND", "OR", "NOT", "IN", "IS NULL", "IS NOT NULL", "LIKE", "BETWEEN",
    "EXISTS", "JOIN", "LEFT JOIN", "RIGHT JOIN", "INNER JOIN", "FULL JOIN", "CROSS JOIN", "ON",
    "USING", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "OFFSET", "UNION", "UNION ALL", "AS",
    "DISTINCT", "CASE", "WHEN", "THEN", "ELSE", "END", "ASC", "DESC", "NULLS FIRST", "NULLS LAST",
    "SET", "VALUES", "RETURNING", "INTO", "NULL", "TRUE", "FALSE", "OVER", "PARTITION BY",
];

/// Compute completions at `cursor` (byte offset; clamped and snapped to a char boundary).
pub fn complete(src: &str, cursor: usize, dialect: Dialect, catalog: &SchemaCatalog) -> Completion {
    let mut cursor = cursor.min(src.len());
    while !src.is_char_boundary(cursor) {
        cursor -= 1;
    }
    let (rs, re) = region_at(src, cursor, dialect);
    let region = &src[rs..re];
    let local = cursor - rs;
    let toks = lexer::tokenize(region, dialect);

    // Identify the fragment under the cursor.
    let mut replace = (local, local);
    let mut frag = String::new();
    for t in &toks {
        if t.start < local && local < t.end || (t.end == local && t.start < local) {
            match t.kind {
                TokenKind::Identifier | TokenKind::Keyword | TokenKind::Type | TokenKind::Function => {
                    replace = (t.start, t.end);
                    frag = region[t.start..local].to_string();
                }
                TokenKind::QuotedIdentifier => {
                    replace = (t.start, t.end);
                    frag = unquote(&region[t.start..local]);
                }
                TokenKind::String | TokenKind::Comment => {
                    // Inside a literal or comment (an unterminated one counts at its end too).
                    let inside = local < t.end || !t.text(region).starts_with("/*") || {
                        let mut lx = lexer::Lexer::new(&region[t.start..], dialect);
                        lx.next();
                        lx.unterminated
                    };
                    if inside {
                        return Completion { replace_start: cursor, replace_end: cursor, items: vec![] };
                    }
                }
                TokenKind::Number | TokenKind::Parameter if local < t.end || t.kind == TokenKind::Number => {
                    return Completion { replace_start: cursor, replace_end: cursor, items: vec![] };
                }
                _ => {}
            }
            break;
        }
        if t.start >= local {
            break;
        }
    }
    if replace.0 == replace.1 {
        if let Some(t) = toks.iter().find(|t| t.start == local) {
            if matches!(t.kind, TokenKind::Identifier | TokenKind::Keyword | TokenKind::Type | TokenKind::Function) {
                replace.1 = t.end;
            }
        }
    }

    let sig: Vec<Token> = toks.iter().copied().filter(|t| !t.is_trivia()).collect();
    let before: Vec<Token> = sig.iter().copied().filter(|t| t.end <= replace.0).collect();
    let refs = analyze::referenced_spans_toks(&sig, region);
    let ctes = analyze::cte_names_toks(&sig, region);

    let lower_kw = !frag.is_empty() && frag.chars().all(|c| !c.is_alphabetic() || c.is_lowercase());
    let mut b = Builder { frag: &frag, items: Vec::new(), seen: HashSet::new(), lower_kw };

    // Qualified: `x.` immediately before the fragment.
    if before.last().is_some_and(|t| punct(t, region, ".") && t.end == replace.0) {
        let n = before.len();
        if n >= 2 && (before[n - 2].is_word() || before[n - 2].kind == TokenKind::QuotedIdentifier) {
            let q = unquote(before[n - 2].text(region));
            let schema_q = (n >= 4 && punct(&before[n - 3], region, ".")).then(|| unquote(before[n - 4].text(region)));
            qualified(&mut b, &q, schema_q.as_deref(), &refs, catalog, dialect);
        }
        return Completion { replace_start: rs + replace.0, replace_end: rs + replace.1, items: b.finish() };
    }

    let ctx = classify(&before, region);
    match ctx {
        Ctx::Start => {
            for k in START_KEYWORDS {
                b.keyword(0, k);
            }
            snippets(&mut b, dialect);
            for k in lexer::keywords() {
                b.keyword(3, k);
            }
        }
        Ctx::Table => {
            tables(&mut b, catalog, None, dialect, 0);
            for c in &ctes {
                b.add(0, CompletionKind::Table, c, dialect.quote_ident_if_needed(c), Some("CTE".into()));
            }
            for s in &catalog.schemas {
                b.add(1, CompletionKind::Schema, s, dialect.quote_ident_if_needed(s), Some("schema".into()));
            }
            for k in ["LATERAL", "ONLY", "SELECT"] {
                b.keyword(3, k);
            }
        }
        Ctx::Type => {
            for ty in lexer::type_names() {
                let ins = if lower_kw { ty.to_ascii_lowercase() } else { ty.to_string() };
                b.add(0, CompletionKind::Type, ty, ins, None);
            }
        }
        Ctx::Column | Ctx::Keyword => {
            let col_prio = if ctx == Ctx::Column { 0 } else { 2 };
            let kw_prio = if ctx == Ctx::Column { 4 } else { 0 };
            // INSERT INTO t ( | ) => only that table's columns.
            let insert_cols = insert_column_target(&before, region);
            let scope: Vec<&RefSpan> = match &insert_cols {
                Some(r) => vec![r],
                None => refs.iter().collect(),
            };
            let multi = scope.len() > 1;
            for r in &scope {
                if let Some(t) = resolve(catalog, r, &ctes) {
                    for c in &t.columns {
                        let detail = if multi {
                            format!("{} ({})", c.data_type, r.r.alias.as_deref().unwrap_or(&t.name))
                        } else {
                            c.data_type.clone()
                        };
                        b.add(col_prio, CompletionKind::Column, &c.name, dialect.quote_ident_if_needed(&c.name), Some(detail));
                    }
                }
            }
            if insert_cols.is_none() {
                for r in &refs {
                    if let Some(a) = &r.r.alias {
                        b.add(col_prio + 1, CompletionKind::Alias, a, dialect.quote_ident_if_needed(a), Some(r.r.name.clone()));
                    } else {
                        b.add(col_prio + 1, CompletionKind::Table, &r.r.name, dialect.quote_ident_if_needed(&r.r.name), None);
                    }
                }
                for f in &catalog.functions {
                    b.add(col_prio + 2, CompletionKind::Function, f, f.clone(), Some("function".into()));
                }
                for f in lexer::builtin_functions(dialect) {
                    let ins = if lower_kw || frag.is_empty() { f.to_string() } else { f.to_ascii_uppercase() };
                    b.add(col_prio + 2, CompletionKind::Function, f, ins, Some("function".into()));
                }
                for k in CLAUSE_KEYWORDS {
                    b.keyword(kw_prio, k);
                }
                for k in lexer::keywords() {
                    b.keyword(kw_prio + 2, k);
                }
                if ctx == Ctx::Keyword {
                    snippets(&mut b, dialect);
                }
            }
        }
    }
    Completion { replace_start: rs + replace.0, replace_end: rs + replace.1, items: b.finish() }
}

fn snippets(b: &mut Builder<'_>, dialect: Dialect) {
    if b.frag.is_empty() {
        return;
    }
    for t in templates(dialect) {
        if t.abbrev.starts_with(&b.frag.to_ascii_lowercase()) {
            let prio = if t.abbrev.eq_ignore_ascii_case(b.frag) { 1 } else { 5 };
            b.add(prio, CompletionKind::Snippet, t.abbrev, t.body.to_string(), Some(t.description.to_string()));
        }
    }
    let _ = CURSOR_MARKER;
}

fn tables(b: &mut Builder<'_>, catalog: &SchemaCatalog, schema: Option<&str>, d: Dialect, prio: u8) {
    let def = catalog.default_schema.as_deref();
    for t in &catalog.tables {
        if let Some(s) = schema {
            if !t.schema.as_deref().is_some_and(|ts| ts.eq_ignore_ascii_case(s)) {
                continue;
            }
        }
        let insert = match (&t.schema, schema, def) {
            (Some(ts), None, Some(def)) if !ts.eq_ignore_ascii_case(def) && d.supports_schemas() => {
                d.qualified(Some(ts), &t.name)
            }
            _ => d.quote_ident_if_needed(&t.name),
        };
        let detail = t.schema.clone().unwrap_or_else(|| t.kind.label().to_string());
        b.add(prio, table_kind(t), &t.name, insert, Some(detail));
    }
}

fn resolve<'c>(catalog: &'c SchemaCatalog, r: &RefSpan, ctes: &[String]) -> Option<&'c CatalogTable> {
    if r.r.schema.is_none() && ctes.iter().any(|c| c.eq_ignore_ascii_case(&r.r.name)) {
        return None;
    }
    catalog.find_table(r.r.schema.as_deref(), &r.r.name)
}

fn qualified(
    b: &mut Builder<'_>,
    q: &str,
    schema_q: Option<&str>,
    refs: &[RefSpan],
    catalog: &SchemaCatalog,
    d: Dialect,
) {
    // alias / table name in scope
    let mut found: Option<&CatalogTable> = None;
    for r in refs {
        let matches_alias = r.r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q));
        let matches_name = r.r.name.eq_ignore_ascii_case(q)
            && schema_q.is_none_or(|s| r.r.schema.as_deref().is_some_and(|rs| rs.eq_ignore_ascii_case(s)));
        if matches_alias || (r.r.alias.is_none() && matches_name) || matches_name {
            found = catalog.find_table(r.r.schema.as_deref(), &r.r.name);
            if found.is_some() {
                break;
            }
        }
    }
    if found.is_none() {
        // Table referenced by name directly (`schema.table.` or `table.`).
        if schema_q.is_some() || catalog.find_schema(q).is_none() {
            found = catalog.find_table(schema_q, q);
        }
    }
    if let Some(t) = found {
        for c in &t.columns {
            b.add(0, CompletionKind::Column, &c.name, d.quote_ident_if_needed(&c.name), Some(c.data_type.clone()));
        }
    }
    if schema_q.is_none() && catalog.find_schema(q).is_some() {
        tables(b, catalog, Some(q), d, 1);
    }
}

/// `INSERT INTO t (` with the cursor inside the column list.
fn insert_column_target(before: &[Token], src: &str) -> Option<RefSpan> {
    let mut depth = 0i32;
    let mut open = None;
    for (i, t) in before.iter().enumerate().rev() {
        if punct(t, src, ")") {
            depth += 1;
        } else if punct(t, src, "(") {
            if depth == 0 {
                open = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let o = open?;
    // Walk back over `name` or `schema . name` to INTO.
    let mut k = o.checked_sub(1)?;
    let name = before[k];
    let mut schema = None;
    if k >= 2 && punct(&before[k - 1], src, ".") {
        schema = Some(before[k - 2]);
        k -= 2;
    }
    let into = before.get(k.checked_sub(1)?)?;
    if !kw(into, src, "INTO") {
        return None;
    }
    Some(RefSpan {
        r: analyze::TableRef { schema: schema.map(|s| unquote(s.text(src))), name: unquote(name.text(src)), alias: None },
        start: name.start,
        end: name.end,
        schema_range: None,
        name_range: (name.start, name.end),
    })
}

const TABLE_KWS: &[&str] = &["FROM", "JOIN", "UPDATE", "INTO", "TABLE", "STRAIGHT_JOIN"];
const COLUMN_KWS: &[&str] = &[
    "SELECT", "WHERE", "ON", "HAVING", "BY", "SET", "AND", "OR", "NOT", "WHEN", "THEN", "ELSE",
    "CASE", "RETURNING", "DISTINCT", "USING", "IN", "LIKE", "ILIKE", "BETWEEN", "IS",
];

fn classify(before: &[Token], src: &str) -> Ctx {
    let Some(last) = before.last() else { return Ctx::Start };
    if last.kind == TokenKind::Operator && last.text(src) == "::" {
        return Ctx::Type;
    }
    if punct(last, src, ";") {
        return Ctx::Start;
    }
    if TABLE_KWS.iter().any(|w| kw(last, src, w)) {
        if kw(last, src, "UPDATE") && before.len() >= 2 && ["KEY", "DO", "FOR"].iter().any(|w| kw(&before[before.len() - 2], src, w)) {
            return Ctx::Column;
        }
        return Ctx::Table;
    }
    // CAST(x AS |
    if kw(last, src, "AS") {
        let mut depth = 0i32;
        for t in before.iter().rev().skip(1) {
            if punct(t, src, ")") {
                depth += 1;
            } else if punct(t, src, "(") {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            } else if depth == 0 && t.kind == TokenKind::Function && t.text(src).eq_ignore_ascii_case("cast") {
                return Ctx::Type;
            }
        }
    }
    // Column definition in CREATE TABLE: `( name |` or `, name |`.
    if before.len() >= 2
        && matches!(last.kind, TokenKind::Identifier | TokenKind::QuotedIdentifier)
        && (punct(&before[before.len() - 2], src, "(") || punct(&before[before.len() - 2], src, ","))
        && before.len() >= 3
        && kw(&before[0], src, "CREATE")
        && before.iter().take(6).any(|t| kw(t, src, "TABLE"))
    {
        return Ctx::Type;
    }
    // Find the governing clause keyword at depth 0 (scanning backwards).
    let mut depth = 0i32;
    let mut clause: Option<&Token> = None;
    for t in before.iter().rev() {
        if punct(t, src, ")") {
            depth += 1;
        } else if punct(t, src, "(") {
            if depth == 0 {
                // Unclosed paren: expression context unless a clause keyword follows inside.
                break;
            }
            depth -= 1;
        } else if depth == 0 && t.kind == TokenKind::Keyword {
            let s = t.text(src);
            if TABLE_KWS.iter().chain(COLUMN_KWS).any(|w| s.eq_ignore_ascii_case(w))
                || ["GROUP", "ORDER", "LIMIT", "VALUES", "OFFSET", "CREATE", "ALTER", "DROP"].iter().any(|w| s.eq_ignore_ascii_case(w))
            {
                clause = Some(t);
                break;
            }
        }
    }
    let after_list_comma = punct(last, src, ",");
    match clause {
        Some(c) if ["FROM", "JOIN", "UPDATE", "INTO", "TABLE"].iter().any(|w| kw(c, src, w)) => {
            if after_list_comma && kw(c, src, "FROM") {
                Ctx::Table
            } else {
                Ctx::Keyword
            }
        }
        Some(c) if ["LIMIT", "OFFSET", "CREATE", "ALTER", "DROP"].iter().any(|w| kw(c, src, w)) => Ctx::Keyword,
        Some(_) => Ctx::Column,
        None => {
            // Inside an unclosed paren without clause keyword, or at depth 0 with none.
            if before.iter().any(|t| punct(t, src, "(")) {
                Ctx::Column
            } else {
                Ctx::Keyword
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::tests::sample;

    fn labels(src_with_caret: &str, d: Dialect) -> (Completion, Vec<String>) {
        let cursor = src_with_caret.find('|').expect("caret");
        let src = src_with_caret.replacen('|', "", 1);
        let c = complete(&src, cursor, d, &sample());
        let l = c.items.iter().map(|i| i.label.clone()).collect();
        (c, l)
    }

    #[test]
    fn tables_after_from() {
        let (c, l) = labels("SELECT * FROM |", Dialect::Postgres);
        assert!(l.contains(&"users".to_string()) && l.contains(&"orders".to_string()));
        assert!(l.contains(&"sales".to_string()));
        let inv = c.items.iter().find(|i| i.label == "Invoices").unwrap();
        assert_eq!(inv.insert_text, "sales.\"Invoices\"");
        let view = c.items.iter().find(|i| i.label == "active_users").unwrap();
        assert_eq!(view.kind, CompletionKind::View);
        let (_, l) = labels("SELECT * FROM us|", Dialect::Postgres);
        assert_eq!(l[0], "users");
        let (_, l) = labels("SELECT * FROM users u JOIN o|", Dialect::Postgres);
        assert_eq!(l[0], "orders");
        let (_, l) = labels("SELECT * FROM users, |", Dialect::Postgres);
        assert!(l.contains(&"orders".to_string()));
    }

    #[test]
    fn schema_qualified_tables() {
        let (c, l) = labels("SELECT * FROM sales.|", Dialect::Postgres);
        assert_eq!(l, vec!["Invoices"]);
        assert_eq!(c.items[0].insert_text, "\"Invoices\"");
        let (c, _) = labels("SELECT * FROM sales.Inv|oices", Dialect::Postgres);
        assert_eq!(c.replace_start, "SELECT * FROM sales.".len());
        assert_eq!(c.replace_end, "SELECT * FROM sales.Invoices".len());
    }

    #[test]
    fn alias_columns() {
        let (c, l) = labels("SELECT u.| FROM users u", Dialect::Postgres);
        assert_eq!(l, vec!["id", "name", "email", "createdAt"]);
        let ca = c.items.iter().find(|i| i.label == "createdAt").unwrap();
        assert_eq!(ca.insert_text, "\"createdAt\"");
        let (_, l) = labels("SELECT * FROM orders o WHERE o.us|", Dialect::Postgres);
        assert_eq!(l, vec!["user_id"]);
        let (_, l) = labels("SELECT orders.| FROM orders", Dialect::Postgres);
        assert!(l.contains(&"total".to_string()));
        let (_, l) = labels("SELECT x.| FROM sales.Invoices x", Dialect::Postgres);
        assert_eq!(l, vec!["invoice_id", "amount"]);
    }

    #[test]
    fn columns_in_scope() {
        let (c, l) = labels("SELECT | FROM users u JOIN orders o ON o.user_id = u.id", Dialect::Postgres);
        assert!(l.contains(&"email".to_string()) && l.contains(&"total".to_string()));
        assert_eq!(c.items[0].kind, CompletionKind::Column);
        assert!(l.contains(&"u".to_string()));
        assert!(c.items.iter().any(|i| i.kind == CompletionKind::Function && i.label == "count"));
        let (c, _) = labels("SELECT * FROM users WHERE em|", Dialect::Postgres);
        assert_eq!(c.items[0].label, "email");
        let (c, _) = labels("SELECT * FROM orders WHERE total > 1 AND uid|", Dialect::Postgres);
        assert_eq!(c.items[0].label, "user_id", "camel hump: {:?}", c.items.iter().take(3).collect::<Vec<_>>());
        let (_, l) = labels("INSERT INTO orders (|) VALUES (1)", Dialect::Postgres);
        assert_eq!(l, vec!["id", "user_id", "total"]);
    }

    #[test]
    fn keywords_and_snippets() {
        let (c, l) = labels("sel|", Dialect::Postgres);
        assert_eq!(l[0], "SELECT");
        assert_eq!(c.items[0].insert_text, "select");
        assert!(c.items.iter().any(|i| i.kind == CompletionKind::Snippet && i.label == "sel"));
        let (_, l) = labels("SELECT * FROM users |", Dialect::Postgres);
        assert!(l.contains(&"WHERE".to_string()));
        let (_, l) = labels("SELECT 1; |", Dialect::Postgres);
        assert_eq!(l[0], "SELECT");
        let (_, l) = labels("SELECT x::|", Dialect::Postgres);
        assert!(l.contains(&"INT".to_string()));
        let (_, l) = labels("CREATE TABLE t (id |", Dialect::Postgres);
        assert!(l.contains(&"BIGINT".to_string()));
    }

    #[test]
    fn no_completion_in_strings_and_comments() {
        let (c, _) = labels("SELECT 'abc|' FROM t", Dialect::Postgres);
        assert!(c.items.is_empty());
        let (c, _) = labels("SELECT 1 -- com|", Dialect::Postgres);
        assert!(c.items.is_empty());
        let (c, _) = labels("SELECT 'unterminated|", Dialect::Postgres);
        assert!(c.items.is_empty());
    }

    #[test]
    fn ranking() {
        assert_eq!(match_rank("users", "us"), Some(4));
        assert_eq!(match_rank("Users", "us"), Some(3));
        assert_eq!(match_rank("user_accounts", "ua"), Some(2));
        assert_eq!(match_rank("UserAccounts", "ua"), Some(2));
        assert_eq!(match_rank("accounts", "count"), Some(1));
        assert_eq!(match_rank("accounts", "zz"), None);
        assert!(hump_match("created_at_utc", "cau"));
        assert!(hump_match("createdAtUtc", "crau"));
        assert!(!hump_match("created", "cd"));
    }

    #[test]
    fn caps_and_never_panics() {
        let mut cat = sample();
        for i in 0..1000 {
            cat.tables.push(CatalogTable { schema: Some("public".into()), name: format!("t{i}"), kind: ObjectKind::Table, columns: vec![] });
        }
        let c = complete("SELECT * FROM ", 14, Dialect::Postgres, &cat);
        assert_eq!(c.items.len(), MAX_ITEMS);
        let src = "SELECT 'é', \"ü\" FROM таблица t WHERE t.☃ = 1; /* x */ sel";
        for d in Dialect::ALL {
            for i in 0..=src.len() + 3 {
                let c = complete(src, i, d, &cat);
                assert!(src.is_char_boundary(c.replace_start.min(src.len())));
                assert!(c.replace_start <= c.replace_end && c.replace_end <= src.len());
            }
        }
    }
}
