//! Core data model, driver API and configuration shared by every Tabularium crate.
//!
//! The crate deliberately contains no database client code: drivers live in
//! `tabularium-drivers`, SQL tooling in `tabularium-sql`, data tooling in
//! `tabularium-tools`. Everything here is plain data plus the [`driver::Connection`]
//! trait that the drivers implement.

pub mod config;
pub mod dialect;
pub mod driver;
pub mod error;
pub mod model;
pub mod result;
pub mod value;

pub use config::{ConnectionConfig, DriverKind, SshAuth, SshConfig, SslConfig, SslMode};
pub use dialect::Dialect;
pub use driver::{CancelHandle, Connection, ExecOptions, TransactionMode};
pub use error::{Error, Result};
pub use model::*;
pub use result::{ColumnMeta, StatementResult};
pub use value::Value;
