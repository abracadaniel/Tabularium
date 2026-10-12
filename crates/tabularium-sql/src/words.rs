//! Static word lists: keywords, type names and builtin functions.

use std::collections::HashSet;
use std::sync::OnceLock;
use tabularium_core::Dialect;

/// Keywords (upper case) recognised by the lexer: ANSI + Postgres + MySQL + SQLite + T-SQL.
///
/// Very common column names (`name`, `type`, `status`, `value`, `data`, ...) are
/// intentionally absent so that they are highlighted (and formatted) as identifiers.
pub(crate) const KEYWORDS: &[&str] = &[
    "ABORT", "ABSOLUTE", "ACCESS", "ACTION", "ADD", "ADMIN", "AFTER", "AGGREGATE", "ALGORITHM",
    "ALL", "ALSO", "ALTER", "ALWAYS", "ANALYSE", "ANALYZE", "AND", "ANY", "APPLY", "ARRAY", "AS",
    "ASC", "ASENSITIVE", "ASSERTION", "ASSIGNMENT", "ASYMMETRIC", "AT", "ATOMIC", "ATTACH",
    "AUTHORIZATION", "AUTOINCREMENT", "AUTO_INCREMENT", "BACKUP", "BACKWARD", "BEFORE", "BEGIN",
    "BETWEEN", "BOTH", "BREAK", "BROWSE", "BULK", "BY", "CACHE", "CALL", "CALLED", "CASCADE",
    "CASCADED", "CASE", "CAST", "CATALOG", "CATCH", "CHAIN", "CHANGE", "CHARACTERISTICS",
    "CHARSET", "CHECK", "CHECKPOINT", "CLOSE", "CLUSTER", "CLUSTERED", "COLLATE", "COLLATION",
    "COLUMN", "COLUMNS", "COMMENT", "COMMENTS", "COMMIT", "COMMITTED", "COMPUTE", "CONCURRENTLY",
    "CONFIGURATION", "CONFLICT", "CONNECT", "CONNECTION", "CONSTRAINT", "CONSTRAINTS", "CONTAINS",
    "CONTINUE", "COPY", "COST", "CREATE", "CROSS", "CSV", "CUBE", "CURRENT", "CURRENT_DATE",
    "CURRENT_ROLE", "CURRENT_SCHEMA", "CURRENT_TIME", "CURRENT_TIMESTAMP", "CURRENT_USER",
    "CURSOR", "CYCLE", "DATABASE", "DATABASES", "DAY", "DEALLOCATE", "DECLARE", "DEFAULT",
    "DEFAULTS", "DEFERRABLE", "DEFERRED", "DEFINER", "DELAYED", "DELETE", "DELIMITER",
    "DELIMITERS", "DENY", "DEPENDS", "DESC", "DESCRIBE", "DETACH", "DETERMINISTIC", "DICTIONARY",
    "DISABLE", "DISCARD", "DISTINCT", "DISTINCTROW", "DISTRIBUTED", "DO", "DOMAIN", "DROP",
    "DUAL", "DUMP", "DUPLICATE", "EACH", "ELSE", "ELSEIF", "ELSIF", "ENABLE", "ENCLOSED",
    "ENCODING", "ENCRYPTED", "END", "ENGINE", "ESCAPE", "ESCAPED", "EVENT", "EXCEPT", "EXCEPTION",
    "EXCLUDE", "EXCLUDING", "EXCLUSIVE", "EXEC", "EXECUTE", "EXISTS", "EXIT", "EXPLAIN",
    "EXPRESSION", "EXTENSION", "EXTERNAL", "FAIL", "FALSE", "FAMILY", "FETCH", "FIELDS", "FILE",
    "FILLFACTOR", "FILTER", "FINAL", "FIRST", "FOLLOWING", "FOR", "FORCE", "FOREIGN", "FORMAT",
    "FORWARD", "FREEZE", "FROM", "FULL", "FULLTEXT", "FUNCTION", "FUNCTIONS", "GENERATED", "GLOB",
    "GLOBAL", "GO", "GOTO", "GRANT", "GRANTED", "GRANTS", "GROUP", "GROUPING", "GROUPS",
    "HANDLER", "HASH", "HAVING", "HEADER", "HIGH_PRIORITY", "HOLD", "HOUR", "IDENTITY",
    "IDENTITY_INSERT", "IF", "IGNORE", "ILIKE", "IMMEDIATE", "IMMUTABLE", "IMPLICIT", "IMPORT",
    "IN", "INCLUDE", "INCLUDING", "INCREMENT", "INDEX", "INDEXED", "INDEXES", "INFILE", "INHERIT",
    "INHERITS", "INITIALLY", "INLINE", "INNER", "INOUT", "INPUT", "INSENSITIVE", "INSERT",
    "INSTEAD", "INTERSECT", "INTO", "INVOKER", "IS", "ISNULL", "ISOLATION", "ITERATE", "JOIN",
    "KEY", "KEYS", "KILL", "LANGUAGE", "LARGE", "LAST", "LATERAL", "LEADING", "LEAKPROOF",
    "LEAVE", "LEFT", "LEVEL", "LIKE", "LIMIT", "LINES", "LISTEN", "LOAD", "LOCAL", "LOCALTIME",
    "LOCALTIMESTAMP", "LOCATION", "LOCK", "LOCKED", "LOGGED", "LOOP", "LOW_PRIORITY", "MATCH",
    "MATCHED", "MATERIALIZED", "MAXVALUE", "MERGE", "METHOD", "MINUS", "MINUTE", "MINVALUE",
    "MODE", "MODIFY", "MONTH", "MOVE", "NAMES", "NATIONAL", "NATURAL", "NEXT", "NO", "NOCHECK",
    "NOCOUNT", "NOLOCK", "NONCLUSTERED", "NONE", "NOT", "NOTHING", "NOTIFY", "NOTNULL", "NOWAIT",
    "NULL", "NULLS", "OBJECT", "OF", "OFF", "OFFSET", "OIDS", "ON", "ONLY", "OPEN", "OPERATOR",
    "OPTIMIZE", "OPTION", "OPTIONS", "OR", "ORDER", "ORDINALITY", "OTHERS", "OUT", "OUTER",
    "OUTFILE", "OUTPUT", "OVER", "OVERLAPS", "OVERRIDING", "OWNED", "OWNER", "PARALLEL",
    "PARTIAL", "PARTITION", "PARTITIONS", "PASSWORD", "PERCENT", "PIVOT", "PLACING", "PLAN",
    "POLICY", "PRAGMA", "PRECEDING", "PREPARE", "PREPARED", "PRESERVE", "PRIMARY", "PRINT",
    "PRIOR", "PRIVILEGES", "PROC", "PROCEDURAL", "PROCEDURE", "PROCEDURES", "PUBLICATION",
    "PURGE", "QUALIFY", "QUERY", "QUICK", "RAISE", "RAISERROR", "RANGE", "READ", "READS",
    "RECURSIVE", "REF", "REFERENCES", "REFERENCING", "REFRESH", "REGEXP", "REINDEX", "RELATIVE",
    "RELEASE", "RENAME", "REPEAT", "REPEATABLE", "REPLACE", "REPLICA", "REPLICATION", "REQUIRE",
    "RESET", "RESIGNAL", "RESTART", "RESTRICT", "RETURN", "RETURNING", "RETURNS", "REVOKE",
    "RIGHT", "RLIKE", "ROLE", "ROLLBACK", "ROLLUP", "ROUTINE", "ROW", "ROWCOUNT", "ROWID", "ROWS",
    "RULE", "SAVEPOINT", "SCHEMA", "SCHEMAS", "SCROLL", "SEARCH", "SECOND", "SECURITY", "SELECT",
    "SEQUENCE", "SEQUENCES", "SERIALIZABLE", "SERVER", "SESSION", "SESSION_USER", "SET", "SETOF",
    "SETS", "SHARE", "SHOW", "SIGNAL", "SIMILAR", "SIMPLE", "SKIP", "SNAPSHOT", "SOME", "SQL",
    "SQLEXCEPTION", "SQLSTATE", "SQLWARNING", "SQL_CALC_FOUND_ROWS", "STABLE", "START",
    "STATEMENT", "STATISTICS", "STDIN", "STDOUT", "STORAGE", "STORED", "STRAIGHT_JOIN", "STRICT",
    "SUBSCRIPTION", "SYMMETRIC", "SYNONYM", "SYSTEM", "SYSTEM_USER", "TABLE", "TABLES",
    "TABLESAMPLE", "TABLESPACE", "TEMP", "TEMPLATE", "TEMPORARY", "TERMINATED", "THEN", "THROW",
    "TIES", "TO", "TOP", "TRAILING", "TRAN", "TRANSACTION", "TRIGGER", "TRUE", "TRUNCATE",
    "TRUSTED", "TRY", "UNBOUNDED", "UNCOMMITTED", "UNION", "UNIQUE", "UNKNOWN", "UNLISTEN",
    "UNLOCK", "UNLOGGED", "UNPIVOT", "UNSIGNED", "UNTIL", "UPDATE", "UPSERT", "USAGE", "USE",
    "USER", "USING", "VACUUM", "VALID", "VALIDATE", "VALUES", "VARIABLES", "VARIADIC", "VERBOSE",
    "VIEW", "VIEWS", "VIRTUAL", "VOLATILE", "WAITFOR", "WHEN", "WHERE", "WHILE", "WINDOW", "WITH",
    "WITHIN", "WITHOUT", "WORK", "WRAPPER", "WRITE", "XOR", "ZEROFILL", "ZONE",
];

/// Data type names (upper case).
pub(crate) const TYPES: &[&str] = &[
    "BIGINT", "BIGSERIAL", "BINARY", "BIT", "BLOB", "BOOL", "BOOLEAN", "BOX", "BYTEA", "CHAR",
    "CHARACTER", "CIDR", "CITEXT", "CLOB", "DATE", "DATERANGE", "DATETIME", "DATETIME2",
    "DATETIMEOFFSET", "DEC", "DECIMAL", "DOUBLE", "FLOAT", "FLOAT4", "FLOAT8", "GEOGRAPHY",
    "GEOMETRY", "HIERARCHYID", "HSTORE", "IMAGE", "INET", "INT", "INT2", "INT4", "INT4RANGE",
    "INT8", "INT8RANGE", "INTEGER", "INTERVAL", "JSON", "JSONB", "LONGBLOB", "LONGTEXT",
    "MACADDR", "MEDIUMBLOB", "MEDIUMINT", "MEDIUMTEXT", "MONEY", "NCHAR", "NTEXT", "NUMERIC",
    "NUMRANGE", "NVARCHAR", "OID", "POLYGON", "PRECISION", "REAL", "REGCLASS", "ROWVERSION",
    "SERIAL", "SERIAL4", "SERIAL8", "SMALLDATETIME", "SMALLINT", "SMALLMONEY", "SMALLSERIAL",
    "SQL_VARIANT", "TEXT", "TIME", "TIMESTAMP", "TIMESTAMPTZ", "TIMETZ", "TINYBLOB", "TINYINT",
    "TINYTEXT", "TSQUERY", "TSRANGE", "TSTZRANGE", "TSVECTOR", "UNIQUEIDENTIFIER", "UUID",
    "VARBINARY", "VARCHAR", "VARCHAR2", "VARYING", "XML",
];

/// Functions available in every dialect (lower case).
const COMMON_FUNCTIONS: &[&str] = &[
    "abs", "avg", "cast", "ceil", "ceiling", "coalesce", "concat", "count", "cume_dist",
    "current_date", "current_time", "current_timestamp", "dense_rank", "exp", "first_value",
    "floor", "lag", "last_value", "lead", "length", "ln", "log", "lower", "ltrim", "max", "min",
    "mod", "nth_value", "ntile", "nullif", "percent_rank", "power", "rank", "replace", "round",
    "row_number", "rtrim", "sign", "sqrt", "substr", "substring", "sum", "trim", "upper",
];

const POSTGRES_FUNCTIONS: &[&str] = &[
    "age", "array_agg", "array_append", "array_length", "array_position", "array_remove",
    "array_to_string", "bool_and", "bool_or", "btrim", "char_length", "character_length",
    "clock_timestamp", "concat_ws", "corr", "currval", "date_part", "date_trunc", "decode",
    "encode", "every", "extract", "format", "generate_series", "gen_random_uuid", "greatest",
    "initcap", "json_agg", "json_array_elements", "json_array_length", "json_build_array",
    "json_build_object", "json_each", "json_object_agg", "jsonb_agg", "jsonb_array_elements",
    "jsonb_build_array", "jsonb_build_object", "jsonb_each", "jsonb_object_agg", "jsonb_set",
    "lastval", "least", "left", "lpad", "make_date", "make_interval", "md5", "nextval", "now",
    "octet_length", "percentile_cont", "percentile_disc", "pg_sleep", "position", "quote_ident",
    "quote_literal", "random", "regexp_match", "regexp_matches", "regexp_replace",
    "regexp_split_to_table", "repeat", "reverse", "right", "rpad", "setval", "split_part",
    "statement_timestamp", "stddev", "string_agg", "string_to_array", "strpos", "to_char",
    "to_date", "to_json", "to_jsonb", "to_number", "to_timestamp", "translate", "trunc",
    "unnest", "uuid_generate_v4", "variance", "current_setting", "set_config", "pi",
];

const MYSQL_FUNCTIONS: &[&str] = &[
    "adddate", "char_length", "concat_ws", "convert", "curdate", "curtime", "date_add",
    "date_format", "date_sub", "datediff", "day", "dayofweek", "elt", "extract", "field",
    "find_in_set", "format", "found_rows", "from_unixtime", "greatest", "group_concat", "hex",
    "hour", "if", "ifnull", "instr", "json_array", "json_contains", "json_extract",
    "json_object", "json_unquote", "last_insert_id", "lcase", "least", "left", "locate", "lpad",
    "md5", "minute", "month", "now", "octet_length", "position", "rand", "regexp_replace",
    "repeat", "reverse", "right", "rpad", "second", "sha1", "sha2", "sleep", "str_to_date",
    "strcmp", "substring_index", "sysdate", "timestampdiff", "truncate", "ucase",
    "unix_timestamp", "uuid", "week", "year", "pi",
];

const SQLITE_FUNCTIONS: &[&str] = &[
    "changes", "char", "date", "datetime", "glob", "group_concat", "hex", "ifnull", "iif",
    "instr", "json", "json_array", "json_each", "json_extract", "json_object", "julianday",
    "last_insert_rowid", "like", "likely", "printf", "quote", "random", "randomblob",
    "sqlite_version", "strftime", "time", "total", "total_changes", "typeof", "unicode",
    "unixepoch", "zeroblob",
];

const MSSQL_FUNCTIONS: &[&str] = &[
    "charindex", "choose", "concat_ws", "convert", "count_big", "datalength", "dateadd",
    "datediff", "datename", "datepart", "day", "db_name", "eomonth", "format", "getdate",
    "getutcdate", "iif", "isjson", "isnull", "isnumeric", "json_query", "json_value", "left",
    "len", "month", "newid", "object_id", "patindex", "quotename", "rand", "replicate", "reverse",
    "right", "scope_identity", "space", "str", "string_agg", "string_split", "stuff",
    "sysdatetime", "try_cast", "try_convert", "year", "pi",
];

/// Aggregate functions (lower case); used for editable-result detection.
pub(crate) const AGGREGATES: &[&str] = &[
    "array_agg", "avg", "bool_and", "bool_or", "count", "count_big", "every", "group_concat",
    "json_agg", "json_object_agg", "jsonb_agg", "jsonb_object_agg", "listagg", "max", "min",
    "percentile_cont", "percentile_disc", "stddev", "stddev_pop", "stddev_samp", "string_agg",
    "sum", "total", "var_pop", "var_samp", "variance",
];

fn upper_set(words: &'static [&'static str]) -> HashSet<&'static str> {
    words.iter().copied().collect()
}

pub(crate) fn keyword_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| upper_set(KEYWORDS))
}

pub(crate) fn type_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| upper_set(TYPES))
}

fn dialect_index(d: Dialect) -> usize {
    match d {
        Dialect::Postgres => 0,
        Dialect::MySql => 1,
        Dialect::Sqlite => 2,
        Dialect::MsSql => 3,
        Dialect::Generic => 4,
    }
}

fn build_functions(d: Dialect) -> Vec<&'static str> {
    let extra: Vec<&'static [&'static str]> = match d {
        Dialect::Postgres => vec![POSTGRES_FUNCTIONS],
        Dialect::MySql => vec![MYSQL_FUNCTIONS],
        Dialect::Sqlite => vec![SQLITE_FUNCTIONS],
        Dialect::MsSql => vec![MSSQL_FUNCTIONS],
        Dialect::Generic => {
            vec![POSTGRES_FUNCTIONS, MYSQL_FUNCTIONS, SQLITE_FUNCTIONS, MSSQL_FUNCTIONS, AGGREGATES]
        }
    };
    let mut v: Vec<&'static str> = COMMON_FUNCTIONS.to_vec();
    for e in extra {
        v.extend_from_slice(e);
    }
    v.sort_unstable();
    v.dedup();
    v
}

pub(crate) fn functions_for(d: Dialect) -> &'static [&'static str] {
    static ALL: OnceLock<[Vec<&'static str>; 5]> = OnceLock::new();
    let all = ALL.get_or_init(|| Dialect::ALL.map(build_functions));
    &all[dialect_index(d)]
}

/// Union of all functions (lower case) of every dialect.
pub(crate) fn function_set() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| functions_for(Dialect::Generic).iter().copied().collect())
}

/// Write the ASCII-upper-cased `word` into `buf`; `None` when too long or non-ASCII.
pub(crate) fn upper_in<'b>(word: &str, buf: &'b mut [u8; 48]) -> Option<&'b str> {
    let bytes = word.as_bytes();
    if bytes.len() > buf.len() || !word.is_ascii() {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        buf[i] = b.to_ascii_uppercase();
    }
    std::str::from_utf8(&buf[..bytes.len()]).ok()
}

/// Write the ASCII-lower-cased `word` into `buf`.
pub(crate) fn lower_in<'b>(word: &str, buf: &'b mut [u8; 48]) -> Option<&'b str> {
    let bytes = word.as_bytes();
    if bytes.len() > buf.len() || !word.is_ascii() {
        return None;
    }
    for (i, b) in bytes.iter().enumerate() {
        buf[i] = b.to_ascii_lowercase();
    }
    std::str::from_utf8(&buf[..bytes.len()]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicates() {
        for list in [KEYWORDS, TYPES] {
            let set: HashSet<_> = list.iter().collect();
            assert_eq!(set.len(), list.len(), "duplicate entries");
            assert!(list.iter().all(|w| w.to_ascii_uppercase() == *w));
        }
        for d in Dialect::ALL {
            assert!(functions_for(d).iter().all(|w| w.to_ascii_lowercase() == *w));
        }
    }

    #[test]
    fn no_overlap_between_keywords_and_types() {
        for t in TYPES {
            assert!(!keyword_set().contains(t), "{t} is both a type and a keyword");
        }
    }
}
