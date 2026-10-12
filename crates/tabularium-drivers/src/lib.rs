//! Database drivers for Tabularium.
//!
//! Every driver implements [`tabularium_core::Connection`] on top of exactly one physical
//! server session guarded by a `tokio::sync::Mutex`, so transactions behave like a
//! DataGrip console session. Use [`connect`] to open a session for a
//! [`ConnectionConfig`]; it transparently sets up an SSH tunnel when configured.
//!
//! | Driver kinds                     | Implementation                 |
//! |----------------------------------|--------------------------------|
//! | Postgres, CockroachDB, Redshift  | [`postgres::PostgresConnection`] (sqlx) |
//! | MySQL, MariaDB, TiDB             | [`mysql::MySqlConnection`] (sqlx)       |
//! | SQLite                           | [`sqlite::SqliteConnection`] (sqlx, bundled SQLite) |
//! | SQL Server                       | [`mssql::MsSqlConnection`] (tiberius)   |
//! | ClickHouse                       | [`clickhouse::ClickHouseConnection`] (HTTP, reqwest + rustls) |
//! | DuckDB (feature `duckdb`)        | [`duckdb::DuckDbConnection`] (duckdb-rs, bundled) |

use std::sync::Arc;

pub use tabularium_core;
use tabularium_core::{Connection, ConnectionConfig, DriverKind, Error, Result};

// Shared helpers; some are unused when only a subset of drivers is compiled.
#[cfg_attr(
    not(all(feature = "postgres", feature = "mysql", feature = "sqlite", feature = "mssql")),
    allow(dead_code)
)]
mod common;

pub mod ssh;

#[cfg(feature = "postgres")]
pub mod postgres;

#[cfg(feature = "mysql")]
pub mod mysql;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "mssql")]
pub mod mssql;

#[cfg(feature = "clickhouse")]
pub mod clickhouse;

#[cfg(feature = "duckdb")]
pub mod duckdb;

/// Open a new session for the config.
///
/// Sets up the SSH tunnel first if `cfg.ssh.enabled`, applies SSL settings, runs
/// `cfg.init_sql`, honours `cfg.auto_commit` (initial transaction mode) and
/// `cfg.read_only` (modifying statements are rejected with [`Error::ReadOnly`]; Postgres
/// and MySQL sessions are additionally switched to read-only transactions).
pub async fn connect(cfg: &ConnectionConfig) -> Result<Arc<dyn Connection>> {
    log::debug!("connecting to {} ({})", cfg.describe(), cfg.driver.label());
    match cfg.driver {
        DriverKind::Postgres | DriverKind::CockroachDb | DriverKind::Redshift => {
            #[cfg(feature = "postgres")]
            {
                let c = postgres::PostgresConnection::connect(cfg).await?;
                Ok(Arc::new(c))
            }
            #[cfg(not(feature = "postgres"))]
            Err(not_compiled(cfg.driver))
        }
        DriverKind::MySql | DriverKind::MariaDb | DriverKind::TiDb => {
            #[cfg(feature = "mysql")]
            {
                let c = mysql::MySqlConnection::connect(cfg).await?;
                Ok(Arc::new(c))
            }
            #[cfg(not(feature = "mysql"))]
            Err(not_compiled(cfg.driver))
        }
        DriverKind::Sqlite => {
            #[cfg(feature = "sqlite")]
            {
                let c = sqlite::SqliteConnection::connect(cfg).await?;
                Ok(Arc::new(c))
            }
            #[cfg(not(feature = "sqlite"))]
            Err(not_compiled(cfg.driver))
        }
        DriverKind::MsSql => {
            #[cfg(feature = "mssql")]
            {
                let c = mssql::MsSqlConnection::connect(cfg).await?;
                Ok(Arc::new(c))
            }
            #[cfg(not(feature = "mssql"))]
            Err(not_compiled(cfg.driver))
        }
        DriverKind::ClickHouse => {
            #[cfg(feature = "clickhouse")]
            {
                let c = clickhouse::ClickHouseConnection::connect(cfg).await?;
                Ok(Arc::new(c))
            }
            #[cfg(not(feature = "clickhouse"))]
            Err(not_compiled(cfg.driver))
        }
        DriverKind::DuckDb => {
            #[cfg(feature = "duckdb")]
            {
                let c = duckdb::DuckDbConnection::connect(cfg).await?;
                Ok(Arc::new(c))
            }
            #[cfg(not(feature = "duckdb"))]
            Err(not_compiled(cfg.driver))
        }
    }
}

/// Connect, fetch the server version, close. Returns the version string.
pub async fn test_connection(cfg: &ConnectionConfig) -> Result<String> {
    let conn = connect(cfg).await?;
    let version = conn.server_version().await;
    if let Err(e) = conn.close().await {
        log::debug!("closing test connection failed: {e}");
    }
    version
}

/// Driver kinds compiled into this build.
pub fn supported_drivers() -> Vec<DriverKind> {
    DriverKind::ALL
        .into_iter()
        .filter(|k| match k {
            DriverKind::Postgres | DriverKind::CockroachDb | DriverKind::Redshift => cfg!(feature = "postgres"),
            DriverKind::MySql | DriverKind::MariaDb | DriverKind::TiDb => cfg!(feature = "mysql"),
            DriverKind::Sqlite => cfg!(feature = "sqlite"),
            DriverKind::MsSql => cfg!(feature = "mssql"),
            DriverKind::ClickHouse => cfg!(feature = "clickhouse"),
            DriverKind::DuckDb => cfg!(feature = "duckdb"),
        })
        .collect()
}

#[allow(dead_code)]
fn not_compiled(kind: DriverKind) -> Error {
    Error::Unsupported(format!("the {} driver is not compiled into this build", kind.label()))
}

/// Network endpoint a driver should connect to, after SSH tunnelling.
#[allow(dead_code)]
pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
    /// Keeps the tunnel open for as long as the connection lives.
    pub tunnel: Option<Arc<ssh::SshTunnel>>,
}

/// Open the SSH tunnel (if configured) to `host:port` and return where to connect.
#[allow(dead_code)]
pub(crate) async fn resolve_endpoint(cfg: &ConnectionConfig, host: &str, port: u16) -> Result<Endpoint> {
    if cfg.ssh.enabled {
        let tunnel = ssh::open_tunnel(&cfg.ssh, host, port).await?;
        Ok(Endpoint { host: "127.0.0.1".into(), port: tunnel.local_port, tunnel: Some(Arc::new(tunnel)) })
    } else {
        Ok(Endpoint { host: host.to_string(), port, tunnel: None })
    }
}

/// Compile-time check: the futures of the public entry points are `Send` (the GUI spawns
/// them on a multi-threaded runtime) and connections are `Send + Sync`.
#[allow(dead_code)]
fn assert_send<T: Send>(_: T) {}
#[allow(dead_code)]
fn assert_send_sync<T: Send + Sync + ?Sized>() {}
#[allow(dead_code)]
fn connect_futures_are_send(cfg: &ConnectionConfig) {
    assert_send(connect(cfg));
    assert_send(test_connection(cfg));
    assert_send(ssh::open_tunnel(&cfg.ssh, "localhost", 1));
    assert_send_sync::<dyn Connection>();
    assert_send_sync::<ssh::SshTunnel>();
    #[cfg(feature = "clickhouse")]
    {
        assert_send(clickhouse::ClickHouseConnection::connect(cfg));
        assert_send_sync::<clickhouse::ClickHouseConnection>();
    }
    #[cfg(feature = "duckdb")]
    {
        assert_send(duckdb::DuckDbConnection::connect(cfg));
        assert_send_sync::<duckdb::DuckDbConnection>();
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    #[cfg(all(feature = "postgres", feature = "mysql", feature = "sqlite", feature = "mssql", feature = "clickhouse"))]
    fn all_default_drivers_supported() {
        let d = supported_drivers();
        let expected = DriverKind::ALL.len() - usize::from(!cfg!(feature = "duckdb"));
        assert_eq!(d.len(), expected);
        assert_eq!(d.contains(&DriverKind::DuckDb), cfg!(feature = "duckdb"));
    }
}
