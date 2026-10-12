//! `tabularium-cli`: scriptable access to Tabularium's drivers and tools.
//!
//! Connections can be given as a saved data source name (shared with the desktop app),
//! a URL (`postgres://…`, `mysql://…`, `sqlite:///path.db`, `mssql://…`) or a path to a
//! SQLite file.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tabularium_core::config::{AppDirs, ConnectionStore};
use tabularium_core::driver::ExecOptions;
use tabularium_core::{Connection, ConnectionConfig, Dialect, ObjectKind, ObjectRef};
use tabularium_tools::export::{ExportFormat, ExportOptions};

#[derive(Parser)]
#[command(name = "tabularium-cli", version, about = "Tabularium database tools from the command line")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage saved data sources (shared with the desktop app).
    Connections {
        #[command(subcommand)]
        action: ConnAction,
    },
    /// Execute SQL (a string, a file with -f, or stdin with -f -).
    Query {
        conn: String,
        sql: Option<String>,
        #[arg(short, long)]
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "table")]
        format: OutFormat,
        /// Maximum rows per result (0 = unlimited).
        #[arg(long, default_value_t = 1000)]
        max_rows: usize,
        /// Keep going after a failing statement.
        #[arg(long)]
        continue_on_error: bool,
    },
    /// List schemas and objects.
    Objects {
        conn: String,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Describe a table: columns, keys, indexes, foreign keys.
    Describe { conn: String, table: String },
    /// Print DDL for an object, or for a whole schema with --schema.
    Ddl {
        conn: String,
        object: Option<String>,
        #[arg(long)]
        schema: Option<String>,
    },
    /// Export a table or query result to a file (format from extension or --format).
    Export {
        conn: String,
        /// Table name (schema.table) or, with --query, a SELECT statement.
        source: String,
        #[arg(long)]
        query: bool,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long, value_enum)]
        format: Option<OutFormat>,
    },
    /// Import CSV/TSV/JSON/XLSX into a table.
    Import {
        conn: String,
        file: PathBuf,
        #[arg(long)]
        table: String,
        /// Create the table with inferred column types first.
        #[arg(long)]
        create: bool,
        #[arg(long, default_value_t = 500)]
        batch: usize,
    },
    /// Compare two schemas and print the differences or a migration script.
    Diff {
        /// Source (desired state): connection[:schema]
        source: String,
        /// Target (to be changed): connection[:schema]
        target: String,
        /// Print the migration SQL instead of a summary.
        #[arg(long)]
        script: bool,
    },
    /// Reformat SQL from a file or stdin.
    Format {
        file: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "generic")]
        dialect: DialectArg,
        #[arg(long)]
        lower: bool,
    },
    /// Run inspections (syntax errors, risky statements) on a SQL file.
    Lint {
        file: PathBuf,
        #[arg(long, value_enum, default_value = "generic")]
        dialect: DialectArg,
    },
    /// Show or search the query history recorded by the desktop app.
    History {
        search: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// List active server sessions.
    Sessions { conn: String },
}

#[derive(Subcommand)]
enum ConnAction {
    List,
    /// Add a data source from a URL.
    Add {
        url: String,
        #[arg(long)]
        name: Option<String>,
    },
    Remove { name: String },
    /// Test connectivity and print the server version.
    Test { conn: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum OutFormat {
    Table,
    Csv,
    Tsv,
    Json,
    Jsonl,
    Sql,
    Markdown,
    Html,
    Xml,
    Xlsx,
}

impl OutFormat {
    fn export_format(self) -> ExportFormat {
        match self {
            OutFormat::Table => ExportFormat::TextTable,
            OutFormat::Csv => ExportFormat::Csv,
            OutFormat::Tsv => ExportFormat::Tsv,
            OutFormat::Json => ExportFormat::Json,
            OutFormat::Jsonl => ExportFormat::JsonLines,
            OutFormat::Sql => ExportFormat::SqlInsert,
            OutFormat::Markdown => ExportFormat::Markdown,
            OutFormat::Html => ExportFormat::Html,
            OutFormat::Xml => ExportFormat::Xml,
            OutFormat::Xlsx => ExportFormat::Xlsx,
        }
    }

    fn from_extension(path: &std::path::Path) -> Option<Self> {
        Some(match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "csv" => OutFormat::Csv,
            "tsv" | "tab" => OutFormat::Tsv,
            "json" => OutFormat::Json,
            "jsonl" | "ndjson" => OutFormat::Jsonl,
            "sql" => OutFormat::Sql,
            "md" | "markdown" => OutFormat::Markdown,
            "html" | "htm" => OutFormat::Html,
            "xml" => OutFormat::Xml,
            "xlsx" => OutFormat::Xlsx,
            "txt" => OutFormat::Table,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum DialectArg {
    Generic,
    Postgres,
    Mysql,
    Sqlite,
    Mssql,
}

impl From<DialectArg> for Dialect {
    fn from(d: DialectArg) -> Self {
        match d {
            DialectArg::Generic => Dialect::Generic,
            DialectArg::Postgres => Dialect::Postgres,
            DialectArg::Mysql => Dialect::MySql,
            DialectArg::Sqlite => Dialect::Sqlite,
            DialectArg::Mssql => Dialect::MsSql,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(cli)) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn resolve_config(spec: &str) -> Result<ConnectionConfig> {
    if spec.contains("://") {
        return Ok(ConnectionConfig::from_url(spec)?);
    }
    let dirs = AppDirs::discover();
    let store = ConnectionStore::load(&dirs.connections_file())?;
    if let Some(c) = store.find_by_name(spec) {
        let mut c = c.clone();
        if c.password.is_none() && !c.driver.is_file_based() {
            if let Ok(pw) = std::env::var("TABULARIUM_PASSWORD") {
                c.password = Some(pw);
            }
        }
        return Ok(c);
    }
    let path = std::path::Path::new(spec);
    if path.exists() || spec == ":memory:" || spec.ends_with(".db") || spec.ends_with(".sqlite") {
        return Ok(ConnectionConfig::sqlite(spec));
    }
    bail!("unknown data source '{spec}' (not a saved name, URL or SQLite file)")
}

async fn connect(spec: &str) -> Result<Arc<dyn Connection>> {
    let cfg = resolve_config(spec)?;
    tabularium_drivers::connect(&cfg).await.with_context(|| format!("connecting to {}", cfg.describe()))
}

/// `schema.name` → ObjectRef (schema optional).
fn parse_object(name: &str, kind: ObjectKind) -> ObjectRef {
    match name.split_once('.') {
        Some((s, n)) => ObjectRef::new(Some(s), n, kind),
        None => ObjectRef::new(None, name, kind),
    }
}

async fn resolve_object(conn: &dyn Connection, name: &str) -> Result<ObjectRef> {
    let mut obj = parse_object(name, ObjectKind::Table);
    if obj.schema.is_none() {
        obj.schema = conn.current_schema().await.ok().flatten();
    }
    // Find the real kind (table/view/...) when possible.
    if let Ok(list) = conn.list_objects(obj.schema.as_deref(), &[
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::Sequence,
        ObjectKind::Function,
        ObjectKind::Procedure,
        ObjectKind::Trigger,
    ]).await {
        if let Some(found) = list.iter().find(|o| o.object.name == obj.name).or_else(|| list.iter().find(|o| o.object.name.eq_ignore_ascii_case(&obj.name))) {
            return Ok(found.object.clone());
        }
    }
    Ok(obj)
}

fn print_rows(columns: &[tabularium_core::ColumnMeta], rows: &[Vec<tabularium_core::Value>], format: OutFormat, dialect: Dialect) -> Result<()> {
    if matches!(format, OutFormat::Xlsx) {
        bail!("xlsx output needs a file: use the export command");
    }
    let mut opts = ExportOptions::new(format.export_format());
    opts.dialect = dialect;
    opts.null_text = if matches!(format, OutFormat::Table) { "NULL".into() } else { String::new() };
    let out = tabularium_tools::export::export_to_string(columns, rows, &opts)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    if !out.ends_with('\n') {
        writeln!(stdout)?;
    }
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Connections { action } => connections(action).await,
        Command::Query { conn, sql, file, format, max_rows, continue_on_error } => {
            let text = match (sql, file) {
                (Some(s), _) => s,
                (None, Some(f)) if f.as_os_str() == "-" => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s
                }
                (None, Some(f)) => std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?,
                (None, None) => bail!("give SQL as an argument or with --file"),
            };
            let c = connect(&conn).await?;
            let dialect = c.dialect();
            let statements = tabularium_sql::split::split_statements(&text, dialect);
            let opts = ExecOptions { max_rows: if max_rows == 0 { None } else { Some(max_rows) }, params: vec![] };
            let mut failures = 0;
            for st in statements {
                match c.execute(&st.text, &opts).await {
                    Ok(r) => {
                        if r.has_result_set() {
                            print_rows(&r.columns, &r.rows, format, dialect)?;
                        }
                        for n in &r.notices {
                            eprintln!("notice: {n}");
                        }
                        eprintln!("{}", r.summary());
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("error in statement: {}\n{e}", one_line(&st.text));
                        if !continue_on_error {
                            break;
                        }
                    }
                }
            }
            c.close().await.ok();
            if failures > 0 {
                bail!("{failures} statement(s) failed");
            }
            Ok(())
        }
        Command::Objects { conn, schema } => {
            let c = connect(&conn).await?;
            let schemas = match schema {
                Some(s) => vec![Some(s)],
                None => {
                    let s = c.list_schemas().await?;
                    if s.is_empty() { vec![None] } else { s.into_iter().map(Some).collect() }
                }
            };
            for s in schemas {
                println!("{}", s.as_deref().unwrap_or("(default)"));
                let mut objects = c
                    .list_objects(s.as_deref(), &[
                        ObjectKind::Table,
                        ObjectKind::View,
                        ObjectKind::MaterializedView,
                        ObjectKind::ForeignTable,
                        ObjectKind::Sequence,
                        ObjectKind::Function,
                        ObjectKind::Procedure,
                        ObjectKind::Trigger,
                        ObjectKind::Type,
                    ])
                    .await?;
                objects.sort_by(|a, b| a.object.kind.cmp(&b.object.kind).then(a.object.name.cmp(&b.object.name)));
                for o in objects {
                    let extra = o.signature.clone().or(o.parent.clone().map(|p| format!(" on {p}"))).unwrap_or_default();
                    println!("  {:<18} {}{}", o.object.kind.label(), o.object.name, extra);
                }
            }
            Ok(())
        }
        Command::Describe { conn, table } => {
            let c = connect(&conn).await?;
            let obj = resolve_object(c.as_ref(), &table).await?;
            let t = c.describe_table(&obj).await?;
            println!("{} {}", obj.kind.label(), obj.display_name());
            if let Some(cm) = &t.comment {
                println!("  -- {cm}");
            }
            for col in &t.columns {
                let mut line = format!("  {:<28} {}", col.name, col.data_type);
                if !col.nullable {
                    line.push_str(" NOT NULL");
                }
                if let Some(d) = &col.default {
                    line.push_str(&format!(" DEFAULT {d}"));
                }
                if col.auto_increment {
                    line.push_str(" AUTO_INCREMENT");
                }
                println!("{line}");
            }
            if let Some(pk) = &t.primary_key {
                println!("  PRIMARY KEY ({})", pk.columns.join(", "));
            }
            for u in &t.unique_keys {
                println!("  UNIQUE {} ({})", u.name.clone().unwrap_or_default(), u.columns.join(", "));
            }
            for fk in &t.foreign_keys {
                println!("  FOREIGN KEY ({}) REFERENCES {} ({})", fk.columns.join(", "), fk.ref_table, fk.ref_columns.join(", "));
            }
            for i in &t.indexes {
                println!("  INDEX {}{} ({})", if i.unique { "UNIQUE " } else { "" }, i.name, i.columns.join(", "));
            }
            Ok(())
        }
        Command::Ddl { conn, object, schema } => {
            let c = connect(&conn).await?;
            match object {
                Some(name) => {
                    let obj = resolve_object(c.as_ref(), &name).await?;
                    let ddl = match obj.kind {
                        ObjectKind::Function | ObjectKind::Procedure => c.describe_routine(&obj, None).await?.definition,
                        _ => c.object_ddl(&obj).await?,
                    };
                    println!("{ddl}");
                }
                None => {
                    let snap = c.snapshot_schema(schema.as_deref()).await?;
                    println!("{}", tabularium_tools::ddl::snapshot_ddl(&snap, c.dialect()));
                }
            }
            Ok(())
        }
        Command::Export { conn, source, query, out, format } => {
            let format = format.or_else(|| OutFormat::from_extension(&out)).ok_or_else(|| anyhow!("cannot infer format from {}; use --format", out.display()))?;
            let c = connect(&conn).await?;
            let dialect = c.dialect();
            let (sql, table) = if query {
                (source.clone(), None)
            } else {
                let obj = resolve_object(c.as_ref(), &source).await?;
                (format!("SELECT * FROM {}", obj.qualified(dialect)), Some(obj))
            };
            let r = c.execute(&sql, &ExecOptions::unlimited()).await?;
            let mut opts = ExportOptions::new(format.export_format());
            opts.dialect = dialect;
            opts.table = table;
            tabularium_tools::export::export_to_file(&r.columns, &r.rows, &opts, &out)?;
            eprintln!("exported {} rows to {}", r.rows.len(), out.display());
            Ok(())
        }
        Command::Import { conn, file, table, create, batch } => {
            use tabularium_tools::import::{self, ImportFormat, ImportOptions};
            let fmt = ImportFormat::from_path(&file).ok_or_else(|| anyhow!("unsupported file type: {}", file.display()))?;
            let mut opts = ImportOptions::new(fmt);
            if matches!(fmt, ImportFormat::Csv | ImportFormat::Tsv) {
                let sample: String = std::fs::read_to_string(&file)?.chars().take(64 * 1024).collect();
                let (delim, header) = import::sniff_csv(&sample);
                opts.delimiter = Some(delim);
                opts.has_header = header;
            }
            let data = import::read_file(&file, &opts)?;
            let c = connect(&conn).await?;
            let dialect = c.dialect();
            let obj = parse_object(&table, ObjectKind::Table);
            let types = import::infer_column_types(&data, dialect);
            if create {
                let ddl = import::create_table_sql(&obj, &data.columns, &types, dialect);
                c.execute(&ddl, &ExecOptions::default()).await.context("creating table")?;
            }
            let statements = import::insert_statements(&obj, &data.columns, &data.rows, Some(&types), dialect, batch.max(1));
            for s in &statements {
                c.execute(s, &ExecOptions::default()).await?;
            }
            eprintln!("imported {} rows into {}", data.rows.len(), obj.display_name());
            Ok(())
        }
        Command::Diff { source, target, script } => {
            let (src_conn, src_schema) = split_conn_schema(&source);
            let (dst_conn, dst_schema) = split_conn_schema(&target);
            let a = connect(src_conn).await?;
            let b = connect(dst_conn).await?;
            let sa = a.snapshot_schema(src_schema.as_deref()).await?;
            let sb = b.snapshot_schema(dst_schema.as_deref()).await?;
            let diff = tabularium_tools::diff::diff_schemas(&sa, &sb, &Default::default());
            if script {
                for s in tabularium_tools::diff::migration_script(&diff, b.dialect(), dst_schema.as_deref()) {
                    println!("{s};");
                }
            } else if diff.changes.is_empty() {
                println!("schemas are identical");
            } else {
                for ch in &diff.changes {
                    println!("{}", ch.description());
                }
            }
            Ok(())
        }
        Command::Format { file, dialect, lower } => {
            let text = read_input(file)?;
            let mut opts = tabularium_sql::format::FormatOptions::default();
            if lower {
                opts.keyword_case = tabularium_sql::format::KeywordCase::Lower;
            }
            println!("{}", tabularium_sql::format::format_sql(&text, dialect.into(), &opts));
            Ok(())
        }
        Command::Lint { file, dialect } => {
            let text = std::fs::read_to_string(&file)?;
            let diags = tabularium_sql::inspect::inspect(&text, dialect.into(), None);
            let mut errors = 0;
            for d in &diags {
                let line = text[..d.start.min(text.len())].matches('\n').count() + 1;
                let col = d.start - text[..d.start.min(text.len())].rfind('\n').map(|i| i + 1).unwrap_or(0) + 1;
                if d.severity == tabularium_sql::inspect::Severity::Error {
                    errors += 1;
                }
                println!("{}:{line}:{col}: {:?}: {}", file.display(), d.severity, d.message);
            }
            if errors > 0 {
                bail!("{errors} error(s)");
            }
            Ok(())
        }
        Command::History { search, limit } => {
            let dirs = AppDirs::discover();
            let h = tabularium_tools::history::QueryHistory::open(dirs.history_file())?;
            for e in h.search(search.as_deref().unwrap_or(""), None).into_iter().take(limit) {
                let status = if e.error.is_some() { "ERR" } else { "ok " };
                println!("{} {status} [{}] {}", e.timestamp.format("%Y-%m-%d %H:%M:%S"), e.connection, one_line(&e.sql));
            }
            Ok(())
        }
        Command::Sessions { conn } => {
            let c = connect(&conn).await?;
            for s in c.list_sessions().await? {
                println!(
                    "{:<10} {:<12} {:<12} {:<10} {}",
                    s.id,
                    s.user.unwrap_or_default(),
                    s.database.unwrap_or_default(),
                    s.state.unwrap_or_default(),
                    one_line(&s.query.unwrap_or_default())
                );
            }
            Ok(())
        }
    }
}

async fn connections(action: ConnAction) -> Result<()> {
    let dirs = AppDirs::discover();
    let path = dirs.connections_file();
    let mut store = ConnectionStore::load(&path)?;
    match action {
        ConnAction::List => {
            for c in &store.connections {
                println!("{:<24} {:<22} {}", c.name, c.driver.label(), c.describe());
            }
        }
        ConnAction::Add { url, name } => {
            let mut cfg = ConnectionConfig::from_url(&url)?;
            if let Some(n) = name {
                cfg.name = n;
            }
            println!("added {}", cfg.name);
            store.upsert(cfg);
            store.save(&path)?;
        }
        ConnAction::Remove { name } => {
            let id = store.find_by_name(&name).map(|c| c.id).ok_or_else(|| anyhow!("no data source named {name}"))?;
            store.remove(id);
            store.save(&path)?;
            println!("removed {name}");
        }
        ConnAction::Test { conn } => {
            let cfg = resolve_config(&conn)?;
            let v = tabularium_drivers::test_connection(&cfg).await?;
            println!("{v}");
        }
    }
    Ok(())
}

fn split_conn_schema(spec: &str) -> (&str, Option<String>) {
    // URLs contain ':' themselves; the schema suffix is after the last ':' that follows the path.
    if spec.contains("://") {
        if let Some((a, b)) = spec.rsplit_once("::") {
            return (a, Some(b.to_string()));
        }
        return (spec, None);
    }
    match spec.rsplit_once(':') {
        Some((a, b)) if !b.is_empty() && !a.is_empty() => (a, Some(b.to_string())),
        _ => (spec, None),
    }
}

fn read_input(file: Option<PathBuf>) -> Result<String> {
    match file {
        Some(f) if f.as_os_str() != "-" => Ok(std::fs::read_to_string(f)?),
        _ => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
    }
}

fn one_line(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 120 { one.chars().take(120).collect::<String>() + "…" } else { one }
}
