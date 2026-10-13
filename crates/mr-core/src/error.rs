//! Error type shared across Manarouter.

/// Errors produced by `mr-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A configuration file could not be read, parsed or validated.
    #[error("config error: {0}")]
    Config(String),

    /// A compiled routing snapshot was malformed, truncated or of an unknown version.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// The on-disk store failed.
    #[error("store error: {0}")]
    Store(String),

    /// The secret vault failed to open, decrypt or authenticate.
    #[error("secret error: {0}")]
    Secret(String),

    /// Something was requested that does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// An operation was attempted that the current state does not allow.
    #[error("invalid state: {0}")]
    InvalidState(String),

    /// JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// TOML (de)serialization failed.
    #[error("toml error: {0}")]
    Toml(#[from] toml::de::Error),

    /// SQLite failed.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Underlying I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Convenience alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
