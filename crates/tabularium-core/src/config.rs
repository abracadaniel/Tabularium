//! Connection ("data source") configuration and its persistence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::dialect::Dialect;
use crate::error::{Error, Result};

/// Database engine of a data source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DriverKind {
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
    MsSql,
    CockroachDb,
    Redshift,
    TiDb,
}

impl DriverKind {
    pub const ALL: [DriverKind; 8] = [
        DriverKind::Postgres,
        DriverKind::MySql,
        DriverKind::MariaDb,
        DriverKind::Sqlite,
        DriverKind::MsSql,
        DriverKind::CockroachDb,
        DriverKind::Redshift,
        DriverKind::TiDb,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DriverKind::Postgres => "PostgreSQL",
            DriverKind::MySql => "MySQL",
            DriverKind::MariaDb => "MariaDB",
            DriverKind::Sqlite => "SQLite",
            DriverKind::MsSql => "Microsoft SQL Server",
            DriverKind::CockroachDb => "CockroachDB",
            DriverKind::Redshift => "Amazon Redshift",
            DriverKind::TiDb => "TiDB",
        }
    }

    pub fn dialect(self) -> Dialect {
        match self {
            DriverKind::Postgres | DriverKind::CockroachDb | DriverKind::Redshift => Dialect::Postgres,
            DriverKind::MySql | DriverKind::MariaDb | DriverKind::TiDb => Dialect::MySql,
            DriverKind::Sqlite => Dialect::Sqlite,
            DriverKind::MsSql => Dialect::MsSql,
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DriverKind::Postgres => 5432,
            DriverKind::CockroachDb => 26257,
            DriverKind::Redshift => 5439,
            DriverKind::MySql | DriverKind::MariaDb => 3306,
            DriverKind::TiDb => 4000,
            DriverKind::MsSql => 1433,
            DriverKind::Sqlite => 0,
        }
    }

    pub fn default_user(self) -> &'static str {
        match self {
            DriverKind::Postgres | DriverKind::Redshift => "postgres",
            DriverKind::CockroachDb => "root",
            DriverKind::MySql | DriverKind::MariaDb | DriverKind::TiDb => "root",
            DriverKind::MsSql => "sa",
            DriverKind::Sqlite => "",
        }
    }

    /// File-based databases have no host/port/user.
    pub fn is_file_based(self) -> bool {
        matches!(self, DriverKind::Sqlite)
    }

    pub fn url_scheme(self) -> &'static str {
        match self {
            DriverKind::Postgres | DriverKind::CockroachDb | DriverKind::Redshift => "postgres",
            DriverKind::MySql | DriverKind::MariaDb | DriverKind::TiDb => "mysql",
            DriverKind::Sqlite => "sqlite",
            DriverKind::MsSql => "mssql",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    pub const ALL: [SslMode; 5] =
        [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull];

    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SslConfig {
    pub mode: SslMode,
    pub ca_cert: Option<PathBuf>,
    pub client_cert: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SshAuth {
    Password(String),
    /// Private key file with optional passphrase.
    Key { path: PathBuf, passphrase: Option<String> },
    /// Use the running ssh-agent.
    Agent,
}

impl Default for SshAuth {
    fn default() -> Self {
        SshAuth::Password(String::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SshConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

/// A saved data source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionConfig {
    pub id: Uuid,
    pub name: String,
    pub driver: DriverKind,
    pub host: String,
    pub port: u16,
    /// Initial database (or file path for SQLite).
    pub database: String,
    pub user: String,
    /// Stored password. Only persisted when `save_password` is set; the app may move
    /// it to the OS keychain and leave this empty.
    pub password: Option<String>,
    pub save_password: bool,
    /// Raw URL that overrides host/port/database when non-empty.
    pub url: Option<String>,
    pub ssl: SslConfig,
    pub ssh: SshConfig,
    /// Reject statements that modify data or schema.
    pub read_only: bool,
    /// Commit every statement automatically (default) or require explicit commit.
    pub auto_commit: bool,
    /// Explorer folder, e.g. "Production/EU".
    pub group: Option<String>,
    /// RGB color used to tint consoles and tabs of this data source.
    pub color: Option<[u8; 3]>,
    pub comment: Option<String>,
    /// SQL executed right after connecting (e.g. `SET search_path ...`).
    pub init_sql: Option<String>,
    /// Restrict explorer to these schemas (empty = all non-system schemas).
    pub schema_filter: Vec<String>,
    /// Show system schemas (pg_catalog, information_schema, ...).
    pub show_system_objects: bool,
    /// Extra driver options passed through as connection parameters.
    pub options: BTreeMap<String, String>,
    /// Connect timeout in seconds.
    pub connect_timeout_secs: u64,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self::new(DriverKind::Postgres)
    }
}

impl ConnectionConfig {
    pub fn new(driver: DriverKind) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: driver.label().to_string(),
            driver,
            host: if driver.is_file_based() { String::new() } else { "localhost".into() },
            port: driver.default_port(),
            database: String::new(),
            user: driver.default_user().into(),
            password: None,
            save_password: true,
            url: None,
            ssl: SslConfig::default(),
            ssh: SshConfig { port: 22, ..Default::default() },
            read_only: false,
            auto_commit: true,
            group: None,
            color: None,
            comment: None,
            init_sql: None,
            schema_filter: vec![],
            show_system_objects: false,
            options: BTreeMap::new(),
            connect_timeout_secs: 15,
        }
    }

    /// Convenience constructor for SQLite files (or `:memory:`).
    pub fn sqlite(path: impl Into<String>) -> Self {
        let path = path.into();
        let mut c = Self::new(DriverKind::Sqlite);
        c.name = Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        c.database = path;
        c
    }

    pub fn dialect(&self) -> Dialect {
        self.driver.dialect()
    }

    /// Parse a URL such as `postgres://user:pw@host:5432/db?sslmode=require`,
    /// `mysql://...`, `sqlite:///path/to.db`, `mssql://...`.
    pub fn from_url(url: &str) -> Result<Self> {
        let (scheme, rest) =
            url.split_once("://").ok_or_else(|| Error::Config(format!("invalid URL: {url}")))?;
        let driver = match scheme.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => DriverKind::Postgres,
            "cockroach" | "cockroachdb" => DriverKind::CockroachDb,
            "redshift" => DriverKind::Redshift,
            "mysql" => DriverKind::MySql,
            "mariadb" => DriverKind::MariaDb,
            "tidb" => DriverKind::TiDb,
            "sqlite" | "file" => DriverKind::Sqlite,
            "mssql" | "sqlserver" => DriverKind::MsSql,
            other => return Err(Error::Config(format!("unsupported URL scheme: {other}"))),
        };
        if driver == DriverKind::Sqlite {
            let path = rest.split('?').next().unwrap_or(rest);
            // sqlite:///abs/path -> /abs/path ; sqlite://rel.db -> rel.db
            return Ok(Self::sqlite(percent_decode(path)));
        }
        let mut cfg = Self::new(driver);
        let (main, query) = match rest.split_once('?') {
            Some((m, q)) => (m, Some(q)),
            None => (rest, None),
        };
        let (authority, path) = match main.split_once('/') {
            Some((a, p)) => (a, p),
            None => (main, ""),
        };
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        if let Some(ui) = userinfo {
            match ui.split_once(':') {
                Some((u, p)) => {
                    cfg.user = percent_decode(u);
                    cfg.password = Some(percent_decode(p));
                }
                None => cfg.user = percent_decode(ui),
            }
        }
        let (host, port) = if let Some(stripped) = hostport.strip_prefix('[') {
            // [ipv6]:port
            let (h, after) = stripped.split_once(']').unwrap_or((stripped, ""));
            (h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok()))
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse().ok()),
                None => (hostport.to_string(), None),
            }
        };
        if !host.is_empty() {
            cfg.host = host;
        }
        if let Some(p) = port {
            cfg.port = p;
        }
        cfg.database = percent_decode(path);
        if let Some(q) = query {
            for pair in q.split('&').filter(|p| !p.is_empty()) {
                let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
                let (k, v) = (percent_decode(k), percent_decode(v));
                match k.as_str() {
                    "sslmode" | "ssl-mode" => {
                        cfg.ssl.mode = match v.to_ascii_lowercase().as_str() {
                            "disable" | "disabled" | "false" => SslMode::Disable,
                            "require" | "required" | "true" => SslMode::Require,
                            "verify-ca" | "verify_ca" => SslMode::VerifyCa,
                            "verify-full" | "verify_identity" => SslMode::VerifyFull,
                            _ => SslMode::Prefer,
                        }
                    }
                    "sslrootcert" | "ssl-ca" => cfg.ssl.ca_cert = Some(v.into()),
                    "sslcert" | "ssl-cert" => cfg.ssl.client_cert = Some(v.into()),
                    "sslkey" | "ssl-key" => cfg.ssl.client_key = Some(v.into()),
                    _ => {
                        cfg.options.insert(k, v);
                    }
                }
            }
        }
        cfg.name = if cfg.database.is_empty() {
            format!("{}@{}", cfg.user, cfg.host)
        } else {
            format!("{}@{}", cfg.database, cfg.host)
        };
        Ok(cfg)
    }

    /// Build a URL for this config (password omitted unless `with_password`).
    pub fn to_url(&self, with_password: bool) -> String {
        if let Some(url) = self.url.as_ref().filter(|u| !u.is_empty()) {
            return url.clone();
        }
        if self.driver.is_file_based() {
            return format!("sqlite://{}", self.database);
        }
        let mut auth = percent_encode(&self.user);
        if with_password {
            if let Some(pw) = self.password.as_ref().filter(|p| !p.is_empty()) {
                auth.push(':');
                auth.push_str(&percent_encode(pw));
            }
        }
        if !auth.is_empty() {
            auth.push('@');
        }
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("{}://{}{}:{}/{}", self.driver.url_scheme(), auth, host, self.port, percent_encode(&self.database))
    }

    /// Short description for tooltips: `user@host:port/db`.
    pub fn describe(&self) -> String {
        if self.driver.is_file_based() {
            self.database.clone()
        } else {
            format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database)
        }
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Persisted set of data sources (`connections.json` in the config directory).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectionStore {
    pub connections: Vec<ConnectionConfig>,
}

impl ConnectionStore {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Save atomically; passwords are dropped for configs with `save_password == false`.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut copy = self.clone();
        for c in &mut copy.connections {
            if !c.save_password {
                c.password = None;
            }
        }
        write_atomic(path, serde_json::to_string_pretty(&copy)?.as_bytes())
    }

    pub fn get(&self, id: Uuid) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn find_by_name(&self, name: &str) -> Option<&ConnectionConfig> {
        self.connections.iter().find(|c| c.name == name)
            .or_else(|| self.connections.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
    }

    pub fn upsert(&mut self, cfg: ConnectionConfig) {
        match self.connections.iter_mut().find(|c| c.id == cfg.id) {
            Some(existing) => *existing = cfg,
            None => self.connections.push(cfg),
        }
    }

    pub fn remove(&mut self, id: Uuid) -> Option<ConnectionConfig> {
        let idx = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(idx))
    }
}

/// Write via a temporary file + rename so a crash never leaves a truncated file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Platform-specific directories (XDG on Linux, ~/Library on macOS, %APPDATA% on Windows).
pub struct AppDirs {
    pub config: PathBuf,
    pub data: PathBuf,
}

impl AppDirs {
    /// Honors `TABULARIUM_HOME` to relocate everything (useful for tests and portable installs).
    pub fn discover() -> Self {
        if let Some(home) = std::env::var_os("TABULARIUM_HOME") {
            let home = PathBuf::from(home);
            return Self { config: home.join("config"), data: home.join("data") };
        }
        match directories::ProjectDirs::from("dev", "Tabularium", "Tabularium") {
            Some(d) => Self { config: d.config_dir().to_path_buf(), data: d.data_dir().to_path_buf() },
            None => {
                let base = PathBuf::from(".tabularium");
                Self { config: base.join("config"), data: base.join("data") }
            }
        }
    }

    pub fn connections_file(&self) -> PathBuf {
        self.config.join("connections.json")
    }
    pub fn settings_file(&self) -> PathBuf {
        self.config.join("settings.json")
    }
    pub fn history_file(&self) -> PathBuf {
        self.data.join("history.jsonl")
    }
    pub fn consoles_dir(&self) -> PathBuf {
        self.data.join("consoles")
    }
    pub fn scratches_dir(&self) -> PathBuf {
        self.data.join("scratches")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pg_url() {
        let c = ConnectionConfig::from_url("postgres://bob:p%40ss@db.local:6543/shop?sslmode=require&application_name=x").unwrap();
        assert_eq!(c.driver, DriverKind::Postgres);
        assert_eq!(c.user, "bob");
        assert_eq!(c.password.as_deref(), Some("p@ss"));
        assert_eq!(c.host, "db.local");
        assert_eq!(c.port, 6543);
        assert_eq!(c.database, "shop");
        assert_eq!(c.ssl.mode, SslMode::Require);
        assert_eq!(c.options.get("application_name").map(String::as_str), Some("x"));
        assert_eq!(c.to_url(true), "postgres://bob:p%40ss@db.local:6543/shop");
    }

    #[test]
    fn parse_mysql_and_sqlite() {
        let c = ConnectionConfig::from_url("mysql://root@localhost/app").unwrap();
        assert_eq!(c.port, 3306);
        assert_eq!(c.password, None);
        let s = ConnectionConfig::from_url("sqlite:///tmp/x.db").unwrap();
        assert_eq!(s.database, "/tmp/x.db");
        assert_eq!(s.name, "x.db");
    }

    #[test]
    fn store_roundtrip_drops_unsaved_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let mut store = ConnectionStore::default();
        let mut a = ConnectionConfig::new(DriverKind::Postgres);
        a.password = Some("secret".into());
        a.save_password = false;
        store.upsert(a.clone());
        store.save(&path).unwrap();
        let loaded = ConnectionStore::load(&path).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].password, None);
        assert_eq!(loaded.connections[0].id, a.id);
    }
}
