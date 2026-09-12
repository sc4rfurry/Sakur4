//! Error types for Sakur4 core.

use std::fmt;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Every failure mode Sakur4 core can report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("connection error: {0}")]
    Pool(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// Raised when an operation is rejected because it would mutate the
    /// append-only Episodic Stream (FR-1) or an LLM-derived write targets the
    /// Symbolic Ledger (FR-2).
    #[error("integrity violation: {0}")]
    Integrity(String),

    /// Raised when the caller asked for something that does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// Raised when caller-supplied input is structurally invalid.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// Raised when the configured budget cannot fit mandatory content. Per
    /// FR-4 this must surface as a *visible* warning, never a silent drop.
    #[error("budget overflow: {0}")]
    BudgetOverflow(String),

    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),

    #[error("language not supported: {0}")]
    UnsupportedLanguage(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Build an [`Error::Other`] from anything printable.
    pub fn other(msg: impl fmt::Display) -> Self {
        Error::Other(msg.to_string())
    }

    /// True when the failure means "the llama.cpp server cannot do this", which
    /// callers must degrade around rather than fail on (NFR-7).
    pub fn is_backend_unavailable(&self) -> bool {
        matches!(self, Error::BackendUnavailable(_))
    }
}

impl From<toml::de::Error> for Error {
    fn from(value: toml::de::Error) -> Self {
        Error::Config(value.to_string())
    }
}
