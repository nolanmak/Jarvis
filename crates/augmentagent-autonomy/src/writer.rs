//! The single writer of `trusted-contacts.toml` (D3/NG4). Only the
//! owner-run `trust` CLI calls [`write_contacts`]; the daemon never does.

use std::path::Path;

use crate::config::ContactEntry;

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0} cannot be parsed ({1}); fix it by hand before using `trust`")]
    Unparseable(String, String),
    #[error("{0}")]
    Invalid(String),
}

/// Every `[[contact]]` entry in the file, verbatim. A missing file is an
/// empty list; a file that does not round-trip is an error, so the CLI never
/// silently drops an entry it could not read.
pub fn read_entries(path: &Path) -> Result<Vec<ContactEntry>, WriteError> {
    let _ = path;
    todo!()
}

/// THE writer: render `entries` as TOML and atomically replace `path`
/// (temp file + rename, mode 0600 on Unix). Creates the parent directory.
pub fn write_contacts(path: &Path, entries: &[ContactEntry]) -> Result<(), WriteError> {
    let _ = (path, entries);
    todo!()
}
