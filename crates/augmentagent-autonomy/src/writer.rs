//! The single writer of `trusted-contacts.toml` (D3/NG4). Only the
//! owner-run `trust` CLI calls [`write_contacts`]; the daemon never does.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{parse_tables, ContactEntry};

const HEADER: &str = "\
# Trusted contacts for autonomy (#1833). Written by `augmentagent trust`;
# see config/trusted-contacts.example.toml. New or changed entries stay
# pending until `augmentagent trust confirm <slug>`.
";

#[derive(Serialize)]
struct FileOut<'a> {
    contact: &'a [ContactEntry],
}

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
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let unparseable = |why: String| WriteError::Unparseable(path.display().to_string(), why);
    let tables = parse_tables(&text).map_err(unparseable)?;
    tables
        .into_iter()
        .enumerate()
        .map(|(i, t)| {
            ContactEntry::deserialize(toml::Value::Table(t))
                .map_err(|e| unparseable(format!("contact #{}: {}", i + 1, e.to_string().trim())))
        })
        .collect()
}

/// The exact file text [`write_contacts`] would write, so the CLI can
/// validate the result (`LoadReport::from_text`) before writing it.
pub fn render(entries: &[ContactEntry]) -> Result<String, WriteError> {
    let body = toml::to_string(&FileOut { contact: entries })
        .map_err(|e| WriteError::Invalid(format!("render trusted contacts: {e}")))?;
    Ok(format!("{HEADER}\n{body}"))
}

/// THE writer: render `entries` as TOML and atomically replace `path`
/// (temp file + rename, mode 0600 on Unix). Creates the parent directory.
pub fn write_contacts(path: &Path, entries: &[ContactEntry]) -> Result<(), WriteError> {
    let text = render(entries)?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("trusted-contacts.toml");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}
