//! Runtime-state storage. Implementation pending; the interface below is the contract the conductor
//! is written against.

use std::path::Path;

use crate::error::{Error, Result};

/// The SQLite-backed runtime store.
#[derive(Debug)]
pub struct Store {
    _private: (),
}

impl Store {
    /// Open or create a store at a path.
    pub fn open(_path: &Path) -> Result<Self> {
        Err(Error::Store("store not yet implemented".into()))
    }

    /// Open an in-memory store, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Err(Error::Store("store not yet implemented".into()))
    }

    /// The schema version currently applied.
    pub fn schema_version(&self) -> Result<u32> {
        Err(Error::Store("store not yet implemented".into()))
    }
}
