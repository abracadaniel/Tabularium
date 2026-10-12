use std::fmt;

/// Error type shared by drivers and tools.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Could not establish a connection (network, auth, TLS, SSH tunnel...).
    #[error("connection error: {0}")]
    Connection(String),
    /// The server rejected a statement.
    #[error("{0}")]
    Database(DatabaseError),
    /// The operation is not supported by this driver.
    #[error("not supported: {0}")]
    Unsupported(String),
    /// The query was cancelled by the user.
    #[error("query cancelled")]
    Cancelled,
    /// The connection is configured read-only and the statement would modify data.
    #[error("connection is read-only: {0}")]
    ReadOnly(String),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Structured server error, as close to what the server reported as possible.
#[derive(Debug, Clone, Default)]
pub struct DatabaseError {
    pub message: String,
    /// SQLSTATE or vendor error code.
    pub code: Option<String>,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// 1-based character offset into the statement text, when the server reports one.
    pub position: Option<usize>,
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(code) = &self.code {
            write!(f, "[{code}] ")?;
        }
        f.write_str(&self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, "\n  Detail: {detail}")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, "\n  Hint: {hint}")?;
        }
        if let Some(pos) = self.position {
            write!(f, "\n  Position: {pos}")?;
        }
        Ok(())
    }
}

impl Error {
    pub fn db(message: impl Into<String>) -> Self {
        Error::Database(DatabaseError { message: message.into(), ..Default::default() })
    }
    pub fn other(message: impl fmt::Display) -> Self {
        Error::Other(message.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Other(format!("json: {e}"))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
