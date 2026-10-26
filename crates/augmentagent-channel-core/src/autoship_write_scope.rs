//! Controller-authored literal write admission for native autoship workers.
//! Never grants the worktree root. Ancestors of pinned/control files are split
//! into siblings and individual file grants because Landlock cannot subtract.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredPath {
    pub path: String,
    pub directory: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    version: u32,
    worktree: PathBuf,
    contract_hash: String,
    allowed: Vec<DeclaredPath>,
    pinned: Vec<String>,
}
#[derive(Debug, Default)]
pub(crate) struct Grants {
    pub directories: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
}
const CONTROLS: &[&str] = &[".git", ".claude", ".mcp.json", ".github", ".gitmodules"];
fn relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && Path::new(path)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "write scope must be a literal relative path"
    );
    Ok(())
}
fn control(path: &Path) -> bool {
    path.components()
        .any(|c| CONTROLS.iter().any(|name| c.as_os_str() == *name))
}
fn location(state: &Path, worktree: &Path) -> PathBuf {
    state.join("autoship-write-admission").join(format!(
        "{:x}.json",
        Sha256::digest(worktree.as_os_str().as_encoded_bytes())
    ))
}
pub fn register(
    worktree: &Path,
    contract_hash: &str,
    allowed: Vec<DeclaredPath>,
    pinned: Vec<String>,
) -> Result<()> {
    let state = crate::state_dir::state_dir().context("scope control state unavailable")?;
    register_at(&state, worktree, contract_hash, allowed, pinned)
}
pub(crate) fn register_at(
    state: &Path,
    worktree: &Path,
    contract_hash: &str,
    allowed: Vec<DeclaredPath>,
    pinned: Vec<String>,
) -> Result<()> {
    let worktree = worktree.canonicalize()?;
    ensure!(
        !contract_hash.is_empty() && allowed.len() <= 256 && pinned.len() <= 256,
        "invalid scope admission"
    );
    for path in allowed.iter().map(|p| &p.path).chain(pinned.iter()) {
        relative(path)?;
    }
    let admission = Admission {
        version: 1,
        worktree: worktree.clone(),
        contract_hash: contract_hash.into(),
        allowed,
        pinned,
    };
    let path = location(state, &worktree);
    fs::create_dir_all(path.parent().unwrap())?;
    ensure!(
        !path
            .canonicalize()
            .unwrap_or(path.clone())
            .starts_with(&worktree),
        "write admission cannot live in candidate"
    );
    if path.try_exists()? {
        ensure!(
            read(&path)? == admission,
            "write contract changed without a new authorized workflow"
        );
        return Ok(());
    }
    let mut staged = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    staged.write_all(&serde_json::to_vec(&admission)?)?;
    staged.as_file().sync_all()?;
    staged.persist_noclobber(path).map_err(|e| e.error)?;
    Ok(())
}
fn read(path: &Path) -> Result<Admission> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let m = file.metadata()?;
    ensure!(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.len() <= 1024 * 1024,
        "write admission is not private"
    );
    Ok(serde_json::from_reader(file)?)
}
fn ensure_parent(root: &Path, relative_path: &Path) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in relative_path.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(m) => ensure!(
                m.is_dir() && !m.file_type().is_symlink(),
                "write parent is not a real directory"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}
pub(crate) fn grants(state: &Path, worktree: &Path) -> Result<Grants> {
    let worktree = worktree.canonicalize()?;
    let admission = read(&location(state, &worktree))
        .context("native worker has no registered literal write scope")?;
    ensure!(
        admission.version == 1 && admission.worktree == worktree,
        "write admission identity mismatch"
    );
    let pins: Vec<PathBuf> = admission.pinned.iter().map(|p| worktree.join(p)).collect();
    let mut grants = Grants::default();
    for declared in admission.allowed {
        relative(&declared.path)?;
        let relative_path = Path::new(&declared.path);
        if control(relative_path) {
            continue;
        }
        let path = worktree.join(relative_path);
        if pins.iter().any(|pin| path == *pin || path.starts_with(pin)) {
            continue;
        }
        if declared.directory {
            ensure_parent(&worktree, relative_path)?;
            split(&worktree, &path, &pins, &mut grants)?;
        } else {
            ensure_parent(&worktree, relative_path.parent().unwrap_or(Path::new("")))?;
            if !path.try_exists()? {
                fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&path)?;
            }
            file_grant(&path, &mut grants)?;
        }
    }
    grants.directories.sort();
    grants.directories.dedup();
    grants.files.sort();
    grants.files.dedup();
    ensure!(
        grants.files.len() + grants.directories.len() <= 10000,
        "write admission expands beyond limit"
    );
    Ok(grants)
}
fn file_grant(path: &Path, grants: &mut Grants) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink() && meta.nlink() == 1,
        "write file is not singly linked regular file"
    );
    grants.files.push(path.to_path_buf());
    Ok(())
}
fn split(root: &Path, path: &Path, pins: &[PathBuf], grants: &mut Grants) -> Result<()> {
    let mut protected = pins.iter().any(|pin| pin.starts_with(path));
    let mut entries = Vec::new();
    // Detect existing controls/symlinks anywhere beneath a prospective broad
    // grant. If any exist, recurse without granting their ancestor directory.
    fn has_control(root: &Path, path: &Path, count: &mut usize) -> Result<bool> {
        *count += 1;
        ensure!(*count <= 100000, "write scope scan exceeds limit");
        if control(path.strip_prefix(root)?) {
            return Ok(true);
        }
        let m = fs::symlink_metadata(path)?;
        if m.file_type().is_symlink() || (m.is_file() && m.nlink() != 1) {
            return Ok(true);
        }
        if m.is_dir() {
            for entry in fs::read_dir(path)? {
                if has_control(root, &entry?.path(), count)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    protected |= has_control(root, path, &mut 0)?;
    if !protected {
        grants.directories.push(path.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        entries.push(entry?.path())
    }
    for entry in entries {
        if control(entry.strip_prefix(root)?) || pins.iter().any(|p| entry == *p) {
            continue;
        }
        let m = fs::symlink_metadata(&entry)?;
        if m.file_type().is_symlink() || (m.is_file() && m.nlink() != 1) {
            continue;
        }
        if m.is_dir() {
            split(root, &entry, pins, grants)?
        } else if m.is_file() {
            file_grant(&entry, grants)?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_ancestors_are_split_and_new_literal_files_precreated() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("src/tests")).unwrap();
        fs::create_dir(root.path().join("src/impl")).unwrap();
        fs::write(root.path().join("src/tests/pinned.rs"), "trusted").unwrap();
        fs::write(root.path().join("src/tests/edit.rs"), "code").unwrap();
        register_at(
            state.path(),
            root.path(),
            "hash",
            vec![
                DeclaredPath {
                    path: "src".into(),
                    directory: true,
                },
                DeclaredPath {
                    path: "new.rs".into(),
                    directory: false,
                },
            ],
            vec!["src/tests/pinned.rs".into()],
        )
        .unwrap();
        let result = grants(state.path(), root.path()).unwrap();
        assert!(!result.directories.contains(&root.path().join("src")));
        assert!(!result.directories.contains(&root.path().join("src/tests")));
        assert!(result.directories.contains(&root.path().join("src/impl")));
        assert!(result
            .files
            .contains(&root.path().join("src/tests/edit.rs")));
        assert!(root.path().join("new.rs").is_file());
        assert!(!result
            .files
            .contains(&root.path().join("src/tests/pinned.rs")));
    }
}
