# Tabularium

A cross-platform database IDE written in Rust, modelled on JetBrains DataGrip.
It runs natively on **macOS, Linux and Windows** (egui/eframe, no Electron, no JVM)
and ships with a companion command line tool.

```
cargo run --release              # desktop app  (binary: tabularium)
cargo run --release -p tabularium-cli -- --help
```

## Supported databases

| Database                          | Driver                     | Notes                                   |
|-----------------------------------|----------------------------|-----------------------------------------|
| PostgreSQL, CockroachDB, Redshift | sqlx (pure Rust, rustls)   | full introspection via `pg_catalog`     |
| MySQL, MariaDB, TiDB              | sqlx (pure Rust, rustls)   | `information_schema` + `SHOW CREATE`    |
| SQLite                            | sqlx, bundled libsqlite3   | file or `:memory:`                      |
| Microsoft SQL Server              | tiberius (pure Rust)       | `sys.*` catalog                         |

All drivers are compiled in by default and need no client libraries. SSH tunnels
(password, key file, ssh-agent) and SSL/TLS modes are supported for every
network database.

## Features

### Data sources
- Data source editor with driver defaults, URL parsing, *Test Connection*,
  SSH tunnel and SSL settings, read-only mode, auto-commit default, startup SQL,
  schema filter, colors (tints tabs and consoles) and folders
- Saved in the platform config directory (`TABULARIUM_HOME` relocates it);
  passwords are only persisted when *Save password* is ticked, otherwise
  prompted once per session
- Import/export of data source definitions (passwords never exported)

### Database explorer
- Lazy tree: data sources → databases → schemas → tables, views, materialized
  views, foreign tables, sequences, functions, procedures, triggers, types,
  events → columns, keys, foreign keys, indexes, checks, triggers
- Filter box, row estimates and comments in tooltips, switching the current
  database
- Context actions: open data, show DDL, new console, copy (qualified) name,
  *Generate SQL* (SELECT/INSERT/UPDATE/DELETE/CREATE), modify table, import,
  export, generate test data, copy table to another data source, refresh
  materialized view, truncate, drop (with confirmation), ER diagram, compare
  schemas, full-text search, generate schema DDL

### Query console
- SQL editor with dialect-aware syntax highlighting, line numbers, current
  statement highlight, find/replace, line comment toggle, duplicate line
- Context-aware code completion (keywords, schemas, tables, columns via alias
  resolution, functions) and live templates (`sel`, `ins`, `upd`, `ct`, …)
- Inspections: syntax errors, unresolved tables/columns, `UPDATE`/`DELETE`
  without `WHERE`, unbalanced parentheses/quotes — shown inline and in the gutter
- Run statement at caret, selection or whole script; statement splitting
  understands dollar quotes, `BEGIN…END` bodies, MySQL `DELIMITER`, T-SQL `GO`
- Query parameters (`:name`, `?`, `$1`, `${var}`) with a prompt
- Each console is its own server session: Auto/Manual transaction mode,
  Commit/Rollback, open-transaction indicator and close prompt
- Cancel running statements (server-side cancel for PostgreSQL and MySQL)
- Multiple result tabs with pinning, an output log with timings and server
  notices, error position highlighting in the editor
- `EXPLAIN` / `EXPLAIN ANALYZE` plan view
- Database/schema switcher, SQL formatter, open/save `.sql` files, consoles
  restored on restart
- Confirmation before `UPDATE`/`DELETE` without `WHERE`

### Result grid and data editor
- Virtualized grid for large results, resizable columns, NULL rendering,
  numeric alignment, row numbers
- Selection (click, shift-click, keyboard, Ctrl+A), copy as TSV, paste from
  spreadsheets, *Copy as* CSV/TSV/JSON/SQL INSERT/UPDATE/Markdown/HTML/XML/text
- Selection aggregates (count, sum, avg, min, max)
- Sorting (in memory for consoles, server-side `ORDER BY` for tables), search
  in results, transposed (record) view, value viewer with JSON pretty-printing
  and hex dumps
- Table data editor with paging, row count, `WHERE`/`ORDER BY` filters,
  *Filter by this value*
- In-place editing: edit cells, set NULL, add/clone/delete rows; pending
  changes are highlighted and submitted as generated DML (preview available)
  using the primary key / unique key; simple single-table query results in
  consoles are editable too

### Schema tools
- DDL viewer for every object type (server-native DDL where available)
- Table designer: create or modify tables — columns, primary key, indexes,
  foreign keys, checks, comments — with live CREATE/ALTER preview; column
  renames emit `RENAME COLUMN` rather than drop/add
- Schema compare across data sources, with options, per-change selection,
  side-by-side DDL diff and migration script generation
- ER diagrams with auto layout, drag, pan/zoom, highlight of related tables,
  saved layouts and SVG export

### Data tools
- Export tables, queries or results to CSV, TSV, JSON, JSON Lines, SQL INSERT
  (single/multi-row), SQL UPDATE, Markdown, HTML, XML, Excel (XLSX) and text
  tables — to a file or the clipboard
- Import CSV/TSV (delimiter and header sniffing), JSON, JSON Lines and XLSX
  into new tables (inferred, editable column types) or existing tables (column
  mapping), batched and transactional with progress
- Copy tables between data sources with type conversion between dialects
- Test data generator (name/email/date/… heuristics, FK-aware, seedable)
- Full-text search for a value across all tables of a schema

### Administration
- Session manager / process list with auto-refresh and terminate
- Users & roles list
- Query history across all consoles with search, filters and re-run

### Command line (`tabularium-cli`)
`query`, `objects`, `describe`, `ddl`, `export`, `import`, `diff`, `format`,
`lint`, `history`, `sessions` and `connections list|add|remove|test`, sharing the
desktop app's saved data sources.

## Keyboard shortcuts

Cmd replaces Ctrl on macOS. *Help → Keyboard shortcuts* lists them all.

| Shortcut              | Action                                  |
|-----------------------|-----------------------------------------|
| Ctrl+Enter            | Execute statement at caret / selection  |
| Ctrl+Shift+Enter      | Execute script                          |
| Ctrl+Space            | Completion                              |
| Ctrl+Alt+L            | Reformat                                |
| Ctrl+/                | Toggle comment                          |
| Ctrl+Shift+Q          | New console                             |
| Ctrl+N                | Go to table/view                        |
| F4                    | Open selected object                    |
| F5 / Ctrl+Enter       | Reload / submit in the data editor      |

## Building

Requires Rust 1.85+.

- **Linux**: `libxkbcommon-dev libgtk-3-dev` (for windowing and native file dialogs)
- **macOS / Windows**: no extra dependencies

```
cargo build --release
cargo test --workspace
```

Driver integration tests run against real servers when these are set:

```
TABULARIUM_TEST_PG_URL=postgres://postgres@localhost:5432/postgres
TABULARIUM_TEST_MYSQL_URL=mysql://root:root@127.0.0.1:3306/tabularium
```

## Project layout

| Crate                | Purpose                                                          |
|----------------------|------------------------------------------------------------------|
| `tabularium-core`    | data model, `Connection` driver trait, values, dialects, config  |
| `tabularium-drivers` | PostgreSQL, MySQL, SQLite, SQL Server drivers and SSH tunnels    |
| `tabularium-sql`     | lexer, splitter, formatter, completion, inspections, templates   |
| `tabularium-tools`   | DDL, schema diff, export/import, change scripts, history, datagen|
| `tabularium-app`     | the desktop application                                          |
| `tabularium-cli`     | the command line tool                                            |

## Compared with DataGrip

Tabularium covers DataGrip's core workflow (data sources, explorer, consoles,
editing, schema tools, import/export). Not implemented yet:

- Databases without a pure-Rust driver (Oracle, DB2, Snowflake, BigQuery,
  MongoDB, Redis, Cassandra, ClickHouse, DuckDB, …)
- OS keychain password storage
- Graphical (tree/diagram) query plans — plans are shown as highlighted text
- Code refactorings (rename across scripts), find usages, VCS integration,
  local file history, custom data extractors and scripting
- Native dump/restore wrappers (`pg_dump`, `mysqldump`)
