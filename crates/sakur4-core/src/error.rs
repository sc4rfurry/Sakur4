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

    // # There was an `UnsupportedLanguage` variant here, and nothing could construct it
    //
    // It read `#[error("language not supported: {0}")] UnsupportedLanguage(String)`, and a grep for every
    // spelling of the name — `Error::UnsupportedLanguage`, `UnsupportedLanguage(`, the message text — found
    // **only this file**. Not a dead caller: an unconstructible variant.
    //
    // The reason is that an unsupported file is not an error here. `repo.rs` counts it and skips it:
    //
    // ```text
    // if language == Language::Unsupported {
    //     report.files_unsupported += 1;
    //     … continue;
    // }
    // ```
    //
    // So the variant described a failure mode that Sakur4 does not have, and the doc comment above the enum
    // said the enum held *"every failure mode Sakur4 core can report"*. **That claim is stronger than
    // "these are the ones we use"** — a caller matching on this enum exhaustively was handed an arm for a
    // condition the library never produces, and a reader was told the list was complete when one entry was
    // not real.
    //
    // Removed rather than left. The five earlier instances of this pattern were all the opposite direction —
    // a capability built and never called — and each was solved by wiring it up. **This one has nothing to
    // wire: the behaviour it named is deliberately a skip.** Deleting it is the whole fix, and the doc
    // comment is now true by construction.
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
