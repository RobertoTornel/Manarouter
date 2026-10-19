//! The one error type this tool has.
//!
//! Every failure here is a hard error: something that stopped the measurement from being made
//! at all. A budget breach is not an error — it is a result — and is reported through the
//! verdict instead.

use std::fmt;

/// A hard error, with a message meant to be read by a person.
#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    /// Build an error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The result type used throughout the tool.
pub type Result<T> = std::result::Result<T, Error>;
