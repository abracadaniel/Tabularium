//! Small helpers shared by the DDL generator, the schema diff and the exporters:
//! identifier quoting, type-name parsing/normalisation and expression normalisation.

use tabularium_core::{Dialect, ObjectRef};

/// Quote an identifier only when needed.
pub(crate) fn q(d: Dialect, ident: &str) -> String {
    d.quote_ident_if_needed(ident)
}

/// Qualified object name. SQLite's implicit `main` schema is omitted.
pub(crate) fn qname(obj: &ObjectRef, d: Dialect) -> String {
    match (d, obj.schema.as_deref()) {
        (Dialect::Sqlite, Some("main") | None) => q(d, &obj.name),
        (Dialect::Sqlite, Some(s)) => format!("{}.{}", q(d, s), q(d, &obj.name)),
        _ => obj.qualified(d),
    }
}

/// Comma separated, quoted identifier list.
pub(crate) fn ident_list(d: Dialect, names: &[String]) -> String {
    names.iter().map(|n| q(d, n)).collect::<Vec<_>>().join(", ")
}

/// Wrap `expr` in parentheses unless it is already fully parenthesised.
pub(crate) fn paren(expr: &str) -> String {
    let e = expr.trim();
    if is_fully_parenthesised(e) {
        e.to_string()
    } else {
        format!("({e})")
    }
}

/// `true` when the outermost parentheses enclose the whole expression, e.g. `(a > 0)`
/// but not `(a) + (b)`.
pub(crate) fn is_fully_parenthesised(e: &str) -> bool {
    if !e.starts_with('(') || !e.ends_with(')') {
        return false;
    }
    let mut depth = 0i32;
    let mut in_quote: Option<char> = None;
    let chars: Vec<char> = e.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if let Some(qc) = in_quote {
            if c == qc {
                in_quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => in_quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != chars.len() - 1 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Strip redundant outer parentheses: `((0))` -> `0`.
pub(crate) fn strip_outer_parens(e: &str) -> &str {
    let mut e = e.trim();
    while is_fully_parenthesised(e) {
        e = e[1..e.len() - 1].trim();
    }
    e
}

/// Structured view of a column type spelling.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedType {
    /// Lower-case base name with single spaces, without arguments, e.g. `character varying`.
    pub base: String,
    /// Arguments inside the first parentheses, e.g. `["10", "2"]`.
    pub args: Vec<String>,
    pub unsigned: bool,
    pub array: bool,
    /// `Some(true)` for `with time zone`, `Some(false)` for `without time zone`.
    pub tz: Option<bool>,
    /// `auto_increment` / `identity(..)` was part of the spelling.
    pub auto: bool,
}

pub(crate) fn parse_type(t: &str) -> ParsedType {
    let mut s = t.trim().to_ascii_lowercase();
    let mut out = ParsedType::default();
    while s.ends_with("[]") {
        out.array = true;
        s.truncate(s.len() - 2);
        s = s.trim_end().to_string();
    }
    if let Some(pos) = s.find("identity") {
        out.auto = true;
        let rest = &s[pos + "identity".len()..];
        let rest_trim = rest.trim_start();
        let end = if rest_trim.starts_with('(') {
            let offset = rest.len() - rest_trim.len();
            rest.find(')').map(|p| p + 1).unwrap_or(offset)
        } else {
            0
        };
        s = format!("{}{}", &s[..pos], &rest[end..]);
    }
    for kw in ["auto_increment", "autoincrement"] {
        if s.contains(kw) {
            out.auto = true;
            s = s.replace(kw, " ");
        }
    }
    if s.contains("without time zone") {
        out.tz = Some(false);
        s = s.replace("without time zone", " ");
    } else if s.contains("with time zone") {
        out.tz = Some(true);
        s = s.replace("with time zone", " ");
    }
    if let Some(open) = s.find('(') {
        let close = s[open..].find(')').map(|p| open + p).unwrap_or(s.len());
        let inner = &s[open + 1..close.min(s.len())];
        out.args = split_args(inner);
        let after = if close < s.len() { &s[close + 1..] } else { "" };
        s = format!("{} {}", &s[..open], after);
    }
    let mut words = Vec::new();
    for w in s.split_whitespace() {
        match w {
            "unsigned" => out.unsigned = true,
            "signed" | "zerofill" => {}
            _ => words.push(w),
        }
    }
    out.base = words.join(" ");
    out
}

fn split_args(inner: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in inner.chars() {
        match c {
            '\'' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            ',' if !in_quote => {
                args.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        args.push(cur.trim().to_string());
    }
    args
}

pub(crate) fn is_serial_type(t: &str) -> bool {
    matches!(
        parse_type(t).base.as_str(),
        "serial" | "bigserial" | "smallserial" | "serial2" | "serial4" | "serial8"
    )
}

pub(crate) fn is_integer_base(base: &str) -> bool {
    matches!(
        base,
        "int" | "integer" | "int2" | "int4" | "int8" | "smallint" | "bigint" | "tinyint"
            | "mediumint" | "serial" | "bigserial" | "smallserial" | "serial2" | "serial4"
            | "serial8"
    )
}

pub(crate) fn is_nextval(default: Option<&str>) -> bool {
    default.is_some_and(|d| d.trim().to_ascii_lowercase().starts_with("nextval("))
}

/// Canonical spelling of a type for comparisons: aliases unified (`int4` == `integer`,
/// `varchar` == `character varying`, `serial` == `integer`), integer display widths dropped,
/// case and whitespace normalised.
pub(crate) fn canonical_type(t: &str) -> String {
    let p = parse_type(t);
    let base = match p.base.as_str() {
        "int" | "int4" | "integer" | "serial" | "serial4" => "integer",
        "int8" | "bigint" | "bigserial" | "serial8" => "bigint",
        "int2" | "smallint" | "smallserial" | "serial2" => "smallint",
        "bool" | "boolean" => "boolean",
        "float8" | "double" | "double precision" => "double precision",
        "float" if p.args.is_empty() => "double precision",
        "float4" | "real" => "real",
        "character varying" | "varchar" => "varchar",
        "character" | "char" | "bpchar" => "char",
        "decimal" | "numeric" | "dec" => "numeric",
        "timestamptz" => "timestamp with time zone",
        "timetz" => "time with time zone",
        "timestamp" if p.tz == Some(true) => "timestamp with time zone",
        "time" if p.tz == Some(true) => "time with time zone",
        other => other,
    };
    let mut s = base.to_string();
    if !p.args.is_empty() && !is_integer_base(&p.base) {
        s.push('(');
        s.push_str(&p.args.join(","));
        s.push(')');
    }
    if p.unsigned {
        s.push_str(" unsigned");
    }
    if p.array {
        s.push_str("[]");
    }
    s
}

/// Lower-case everything outside quoted literals.
fn lower_outside_quotes(e: &str) -> String {
    let mut out = String::with_capacity(e.len());
    let mut in_quote = false;
    for c in e.chars() {
        if c == '\'' {
            in_quote = !in_quote;
            out.push(c);
        } else if in_quote {
            out.push(c);
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Remove trailing Postgres casts (`'x'::text`, `0::bigint`, `'a'::character varying`).
fn strip_casts(e: &str) -> String {
    let mut out = String::with_capacity(e.len());
    let chars: Vec<char> = e.chars().collect();
    let mut i = 0;
    let mut in_quote = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            in_quote = !in_quote;
            out.push(c);
            i += 1;
            continue;
        }
        if !in_quote && c == ':' && chars.get(i + 1) == Some(&':') {
            i += 2;
            // skip the type name: identifier chars, spaces between words, (args), [] and quotes
            loop {
                while i < chars.len() && chars[i] == ' ' {
                    i += 1;
                }
                let start = i;
                while i < chars.len()
                    && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '"' | '.' | '[' | ']'))
                {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '(' {
                    while i < chars.len() && chars[i] != ')' {
                        i += 1;
                    }
                    i += 1;
                }
                // multi-word types: "character varying", "double precision", "time zone"
                let word: String = chars[start..i.min(chars.len())].iter().collect();
                let mut j = i;
                while j < chars.len() && chars[j] == ' ' {
                    j += 1;
                }
                let next: String = chars[j..].iter().take_while(|c| c.is_alphanumeric()).collect();
                let continues = matches!(
                    (word.as_str(), next.as_str()),
                    ("character", "varying")
                        | ("double", "precision")
                        | ("with", _)
                        | ("without", _)
                        | ("time", "zone")
                        | (_, "with")
                        | (_, "without")
                ) && !next.is_empty();
                if continues {
                    i = j;
                } else {
                    break;
                }
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Normalise a column default for comparisons. `None` and `NULL` are equivalent.
pub(crate) fn normalize_default(d: Option<&str>) -> Option<String> {
    let d = d?.trim();
    if d.is_empty() {
        return None;
    }
    let lowered = lower_outside_quotes(strip_outer_parens(d));
    let stripped = strip_casts(&lowered);
    let mut e = strip_outer_parens(&stripped).trim().to_string();
    // MSSQL N'..' literals
    if e.starts_with("n'") {
        e.remove(0);
    }
    let e = match e.as_str() {
        "null" => return None,
        "now()" | "current_timestamp()" | "getdate()" | "sysdatetime()" | "localtimestamp"
        | "current_timestamp(6)" => "current_timestamp".to_string(),
        "true" | "'t'" | "b'1'" => "1".to_string(),
        "false" | "'f'" | "b'0'" => "0".to_string(),
        _ => e,
    };
    Some(e)
}

/// Normalise an arbitrary SQL expression (CHECK bodies, view definitions) for equality:
/// lower-cased outside literals, whitespace, parentheses, identifier quotes and casts removed.
pub(crate) fn normalize_expr(e: &str) -> String {
    let lowered = lower_outside_quotes(e.trim().trim_end_matches(';'));
    let lowered = lowered.strip_prefix("check").map(str::to_string).unwrap_or(lowered);
    let no_casts = strip_casts(&lowered);
    let mut out = String::with_capacity(no_casts.len());
    let mut in_quote = false;
    for c in no_casts.chars() {
        if c == '\'' {
            in_quote = !in_quote;
            out.push(c);
            continue;
        }
        if in_quote {
            out.push(c);
            continue;
        }
        if c.is_whitespace() || matches!(c, '(' | ')' | '"' | '`' | '[' | ']') {
            continue;
        }
        out.push(c);
    }
    out
}

/// Names equal, optionally ignoring ASCII case.
pub(crate) fn names_eq(a: &str, b: &str, ignore_case: bool) -> bool {
    if ignore_case {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

pub(crate) fn lists_eq(a: &[String], b: &[String], ignore_case: bool) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| names_eq(x, y, ignore_case))
}

/// Make names unique by appending `_2`, `_3`, ... to duplicates.
pub(crate) fn dedupe_names(names: &[String]) -> Vec<String> {
    let mut used = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        if used.insert(n.clone()) {
            out.push(n.clone());
            continue;
        }
        let mut i = 2;
        loop {
            let cand = format!("{n}_{i}");
            if !names.contains(&cand) && used.insert(cand.clone()) {
                out.push(cand);
                break;
            }
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_parsing() {
        let p = parse_type("timestamp(3) with time zone");
        assert_eq!(p.base, "timestamp");
        assert_eq!(p.args, vec!["3"]);
        assert_eq!(p.tz, Some(true));
        let p = parse_type("int(10) unsigned");
        assert_eq!(p.base, "int");
        assert!(p.unsigned);
        let p = parse_type("int IDENTITY(1,1)");
        assert_eq!(p.base, "int");
        assert!(p.auto);
        assert!(p.args.is_empty());
        let p = parse_type("character varying(255)[]");
        assert_eq!(p.base, "character varying");
        assert!(p.array);
    }

    #[test]
    fn canonical() {
        assert_eq!(canonical_type("int4"), canonical_type("INTEGER"));
        assert_eq!(canonical_type("int(11)"), "integer");
        assert_eq!(canonical_type("character varying(20)"), "varchar(20)");
        assert_eq!(canonical_type("timestamptz"), canonical_type("timestamp with time zone"));
        assert_eq!(canonical_type("timestamp without time zone"), "timestamp");
        assert_eq!(canonical_type("numeric(10, 2)"), canonical_type("decimal(10,2)"));
        assert_ne!(canonical_type("varchar(10)"), canonical_type("varchar(20)"));
    }

    #[test]
    fn defaults() {
        assert_eq!(normalize_default(Some("'x'::text")), Some("'x'".into()));
        assert_eq!(normalize_default(Some("('active'::character varying)")), Some("'active'".into()));
        assert_eq!(normalize_default(Some("((0))")), Some("0".into()));
        assert_eq!(normalize_default(Some("NULL")), None);
        assert_eq!(normalize_default(Some("now()")), normalize_default(Some("CURRENT_TIMESTAMP")));
        assert_eq!(normalize_default(Some("'A'")), Some("'A'".into()));
    }

    #[test]
    fn exprs() {
        assert_eq!(normalize_expr("CHECK ((price > (0)::numeric))"), normalize_expr("price > 0"));
        assert!(is_fully_parenthesised("(a > 0)"));
        assert!(!is_fully_parenthesised("(a) + (b)"));
        assert_eq!(paren("a > 0"), "(a > 0)");
    }

    #[test]
    fn dedupe() {
        let n = dedupe_names(&["a".into(), "a".into(), "a_2".into(), "b".into()]);
        assert_eq!(n, vec!["a", "a_3", "a_2", "b"]);
    }
}
