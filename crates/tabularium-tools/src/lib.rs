#![warn(missing_docs)]
//! Data tooling for Tabularium:
//!
//! * [`ddl`] — `CREATE`/`DROP`/`TRUNCATE`/`ALTER` generation and type mapping per dialect.
//! * [`diff`] — schema comparison and migration scripts.
//! * [`export`] / [`import`] — result-set export (CSV, JSON, SQL, XLSX, ...) and file import.
//! * [`changes`] — data editor pending changes to DML.
//! * [`history`] — persistent query history.
//! * [`datagen`] — reproducible test data generation.
//! * [`textdiff`] — line diffs for "compare with" views.

pub mod changes;
pub mod datagen;
pub mod ddl;
pub mod diff;
pub mod export;
pub mod history;
pub mod import;
pub mod textdiff;
mod util;

pub use diff::{DiffOptions, SchemaChange, SchemaDiff, TableChange};
pub use export::{ExportFormat, ExportOptions, StreamingExporter};
pub use history::{HistoryEntry, QueryHistory};
pub use import::{ImportFormat, ImportOptions, ImportedData};
