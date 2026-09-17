//! Deterministic `wiki/journal/` section (#1010).
//!
//! Every synced entry lands as one markdown page under the wiki root —
//! `journal/<YYYY>/<YYYY-MM-DD>-<id8>.md` — so the journal is browsable in
//! the PRIVATE knowledge-base mirror without going through LLM ingest.
//! The path is derived from `created_at` + the entry id and is stable
//! across edits: a new `_version` of the same entry overwrites in place.
//!
//! PRIVACY: journal text exists only under the wiki root (gitignored in the
//! public code repo, mirrored solely to the private KB repo). Tests in this
//! module use synthetic fixtures only.

use std::io;
use std::path::{Path, PathBuf};

use crate::client::Entry;

/// Wiki-relative path for an entry's page. Stable across versions.
pub fn entry_rel_path(entry: &Entry) -> PathBuf {
    let id8: String = entry
        .id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    let id8 = if id8.is_empty() { "entry".to_string() } else { id8 };
    match entry.created_at.get(..10).filter(|d| looks_like_date(d)) {
        Some(d) => PathBuf::from("journal")
            .join(&d[..4])
            .join(format!("{d}-{id8}.md")),
        None => PathBuf::from("journal")
            .join("undated")
            .join(format!("{id8}.md")),
    }
}

fn looks_like_date(d: &str) -> bool {
    d.len() == 10
        && d.chars().enumerate().all(|(i, c)| match i {
            4 | 7 => c == '-',
            _ => c.is_ascii_digit(),
        })
}

/// Render the entry's markdown page: `kind: journal` frontmatter + title +
/// plain-text body.
pub fn entry_page(entry: &Entry, text: &str) -> String {
    let date = entry
        .created_at
        .get(..10)
        .filter(|d| looks_like_date(d))
        .unwrap_or("undated");
    let mut page = String::from("---\nkind: journal\n");
    page.push_str(&format!("id: {}\n", entry.id));
    page.push_str(&format!("created: {}\n", entry.created_at));
    if let Some(u) = entry.updated_at.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        page.push_str(&format!("updated: {u}\n"));
    }
    page.push_str(&format!("version: {}\n", entry.version.unwrap_or(0)));
    if let Some(t) = entry.topic.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        page.push_str(&format!("topic: {}\n", yaml_quote(t)));
    }
    if entry.bookmarked == Some(true) {
        page.push_str("bookmarked: true\n");
    }
    page.push_str("---\n\n");
    let title = entry
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.replace(['\n', '\r'], " "))
        .unwrap_or_else(|| format!("Journal — {date}"));
    page.push_str(&format!("# {title}\n\n"));
    page.push_str(text.trim_end());
    page.push('\n');
    page
}

fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

const MAX_PAGE_BYTES: u64 = 8 * 1024 * 1024;

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn checked_dir(root: &Path, relative: &Path, create: bool) -> io::Result<PathBuf> {
    let mut path = root.canonicalize()?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(io::Error::other("invalid archive path"));
        };
        path.push(name);
        if create {
            match std::fs::create_dir(&path) {
                Ok(()) => { std::fs::File::open(path.parent().unwrap())?.sync_all()?; },
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {},
                Err(e) => return Err(e),
            }
        }
        let info = std::fs::symlink_metadata(&path)?;
        if !info.is_dir() || info.file_type().is_symlink() {
            return Err(io::Error::other("archive directory must not be a symlink"));
        }
    }
    Ok(path)
}

fn read_page(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_PAGE_BYTES {
        return Err(io::Error::other("archive page is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    (&mut file).take(MAX_PAGE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PAGE_BYTES { return Err(io::Error::other("archive page too large")); }
    Ok(bytes)
}

fn atomic_page(path: &Path, bytes: &[u8], immutable: bool) -> io::Result<()> {
    use std::io::Write;
    if bytes.len() as u64 > MAX_PAGE_BYTES { return Err(io::Error::other("archive page too large")); }
    let parent = path.parent().ok_or_else(|| io::Error::other("missing archive parent"))?;
    // The mirror excludes *.lock: an in-progress temporary cannot be committed.
    let mut file = tempfile::Builder::new().prefix(".journal-").suffix(".lock").tempfile_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    if immutable {
        match file.persist_noclobber(path) {
            Ok(_) => {},
            Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {
                if read_page(path)? != bytes { return Err(io::Error::other("archive checksum conflict")); }
            },
            Err(e) => return Err(e.error),
        }
    } else { file.persist(path).map_err(|e| e.error)?; }
    std::fs::File::open(parent)?.sync_all()
}

fn archive_dir(root: &Path, entry_id: &str, create: bool) -> io::Result<PathBuf> {
    checked_dir(root, &PathBuf::from("journal/history").join(digest(entry_id.as_bytes())), create)
}

/// Check the durable current page before trusting legacy dedupe records.
pub fn has_entry_version(root: &Path, entry: &Entry) -> bool {
    let rel = entry_rel_path(entry);
    let Ok(parent) = checked_dir(root, rel.parent().unwrap(), false) else { return false };
    let Ok(bytes) = read_page(&parent.join(rel.file_name().unwrap())) else { return false };
    let Ok(page) = std::str::from_utf8(&bytes) else { return false };
    let Some(header) = page.strip_prefix("---\n").and_then(|s| s.split("\n---").next()) else { return false };
    let id_matches = header.lines().any(|line| line == format!("id: {}", entry.id));
    let version = header.lines().find_map(|line| line.strip_prefix("version: "))
        .and_then(|s| s.parse::<i64>().ok());
    id_matches && version.is_some_and(|version| version >= entry.version.unwrap_or(0))
}

/// Content-addressed revisions survive multiple observed edits between Git syncs.
/// Journal text and metadata stay exclusively in the private wiki repository.
pub fn write_entry(wiki_root: &Path, entry: &Entry, text: &str) -> io::Result<PathBuf> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let rel = entry_rel_path(entry);
    let parent = checked_dir(wiki_root, rel.parent().unwrap(), true)?;
    let path = parent.join(rel.file_name().unwrap());
    let history = archive_dir(wiki_root, &entry.id, true)?;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(history.join("writer.lock"))?;
    if !lock.metadata()?.is_file() { return Err(io::Error::other("invalid archive lock")); }
    // A competing importer retries through its existing cursor, never races a replacement.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let previous = match read_page(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(bytes) = &previous {
        atomic_page(&history.join(format!("{}.md", digest(bytes))), bytes, true)?;
    }
    let next = entry_page(entry, text);
    atomic_page(&history.join(format!("{}.md", digest(next.as_bytes()))), next.as_bytes(), true)?;
    if previous.as_deref() != Some(next.as_bytes()) {
        // Retain late-arriving older versions in history without downgrading the current page.
        let previous_version = previous.as_ref().and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|page| page.strip_prefix("---\n"))
            .and_then(|page| page.split("\n---").next())
            .and_then(|header| header.lines().find_map(|line| line.strip_prefix("version: ")))
            .and_then(|value| value.parse::<i64>().ok());
        if !previous_version.is_some_and(|version| version > entry.version.unwrap_or(0)) {
            atomic_page(&path, next.as_bytes(), false)?;
        }
    }
    Ok(path)
}

/// List immutable revision hashes; no content or source mutation occurs.
pub fn revisions(root: &Path, entry_id: &str) -> io::Result<Vec<String>> {
    let directory = match archive_dir(root, entry_id, false) {
        Ok(path) => path,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut revisions = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|value| value == "md") {
            let hash = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(io::Error::other("invalid revision name"));
            }
            revisions.push(hash.to_string());
            if revisions.len() > 10000 { return Err(io::Error::other("too many revisions; inspect Git history")); }
        }
    }
    revisions.sort();
    Ok(revisions)
}

/// Return an exact saved page, checking its content address before export.
pub fn read_revision(root: &Path, entry_id: &str, revision: &str) -> io::Result<Vec<u8>> {
    if revision.len() != 64 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(io::Error::other("revision must be a full SHA-256 hash"));
    }
    let path = archive_dir(root, entry_id, false)?.join(format!("{revision}.md"));
    let bytes = read_page(&path)?;
    if digest(&bytes) != revision { return Err(io::Error::other("revision checksum mismatch")); }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, created_at: &str) -> Entry {
        Entry {
            id: id.into(),
            owner_id: "owner-1".into(),
            created_at: created_at.into(),
            content: None,
            title: Some("A day".into()),
            topic: Some("Journal".into()),
            bookmarked: Some(false),
            updated_at: None,
            version: Some(3),
            deleted: Some(false),
            last_changed_at: None,
            owner: None,
        }
    }

    #[test]
    fn revision_recovery_is_exact_and_checksum_protected() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry("fixture-recover", "2026-07-01T08:00:00.000Z");
        let current = write_entry(dir.path(), &e, "first invite\n").unwrap();
        let first = std::fs::read(&current).unwrap();
        let hashes = revisions(dir.path(), &e.id).unwrap();
        assert_eq!(hashes.len(), 1);
        write_entry(dir.path(), &e, "replacement").unwrap();
        let updated = std::fs::read(&current).unwrap();
        assert_eq!(read_revision(dir.path(), &e.id, &hashes[0]).unwrap(), first);
        assert_eq!(std::fs::read(&current).unwrap(), updated);
        assert!(read_revision(dir.path(), &e.id, "../escape").is_err());
        let archive = archive_dir(dir.path(), &e.id, false).unwrap().join(format!("{}.md", hashes[0]));
        std::fs::write(&archive, "corrupt").unwrap();
        assert!(read_revision(dir.path(), &e.id, &hashes[0]).is_err());
        assert!(write_entry(dir.path(), &e, "first invite\n").is_err());
        assert_eq!(std::fs::read(&current).unwrap(), updated);
    }

    #[test]
    fn failed_archive_keeps_current_page_and_symlinks_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry("fixture-safe", "2026-07-01T08:00:00.000Z");
        let current = write_entry(dir.path(), &e, "retain me").unwrap();
        let original = std::fs::read(&current).unwrap();
        let archive = archive_dir(dir.path(), &e.id, false).unwrap();
        std::fs::remove_dir_all(&archive).unwrap();
        std::fs::write(&archive, "blocked").unwrap();
        assert!(write_entry(dir.path(), &e, "replacement").is_err());
        assert_eq!(std::fs::read(&current).unwrap(), original);
        std::fs::remove_file(&archive).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), &archive).unwrap();
        assert!(write_entry(dir.path(), &e, "replacement").is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[test]
    fn two_observed_edits_preserve_legacy_content_before_git_sync() {
        let dir = tempfile::tempdir().unwrap();
        let mut e = entry("fixture-history", "2026-07-01T08:00:00.000Z");
        let path = dir.path().join(entry_rel_path(&e));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "legacy invite: Guest A\n").unwrap();
        write_entry(dir.path(), &e, "invite Guest B").unwrap();
        e.version = Some(4);
        write_entry(dir.path(), &e, "invite Guest C").unwrap();
        write_entry(dir.path(), &e, "invite Guest C").unwrap();
        fn contents(root: &Path) -> Vec<String> {
            let mut out = Vec::new();
            for row in std::fs::read_dir(root).unwrap() {
                let p = row.unwrap().path();
                if p.is_dir() { out.extend(contents(&p)); }
                else if p.extension().is_some_and(|x| x == "md") {
                    out.push(std::fs::read_to_string(p).unwrap());
                }
            }
            out
        }
        let revisions = contents(&dir.path().join("journal/history"));
        assert_eq!(revisions.len(), 3);
        for expected in ["Guest A", "Guest B", "Guest C"] {
            assert!(revisions.iter().any(|text| text.contains(expected)));
        }
        assert!(std::fs::read_to_string(path).unwrap().contains("Guest C"));
    }

    #[test]
    fn rel_path_is_dated_and_stable_across_versions() {
        let mut e = entry("abc123-def456-XYZ", "2026-07-01T08:00:00.000Z");
        assert_eq!(
            entry_rel_path(&e),
            PathBuf::from("journal/2026/2026-07-01-abc123de.md")
        );
        e.version = Some(9); // edited entry — same page, overwritten in place
        assert_eq!(
            entry_rel_path(&e),
            PathBuf::from("journal/2026/2026-07-01-abc123de.md")
        );
    }

    #[test]
    fn rel_path_unparseable_date_goes_to_undated() {
        let e = entry("e1", "not-a-date");
        assert_eq!(entry_rel_path(&e), PathBuf::from("journal/undated/e1.md"));
    }

    #[test]
    fn page_has_frontmatter_title_and_body() {
        let e = entry("e1", "2026-07-01T08:00:00.000Z");
        let page = entry_page(&e, "synthetic test body\n\nsecond paragraph");
        assert!(page.starts_with("---\nkind: journal\n"), "{page}");
        assert!(page.contains("id: e1\n"), "{page}");
        assert!(page.contains("created: 2026-07-01T08:00:00.000Z\n"), "{page}");
        assert!(page.contains("version: 3\n"), "{page}");
        assert!(page.contains("topic: \"Journal\"\n"), "{page}");
        assert!(!page.contains("bookmarked"), "false is omitted: {page}");
        assert!(page.contains("\n# A day\n"), "{page}");
        assert!(page.ends_with("synthetic test body\n\nsecond paragraph\n"), "{page}");
    }

    #[test]
    fn page_title_falls_back_to_the_date() {
        let mut e = entry("e1", "2026-07-01T08:00:00.000Z");
        e.title = None;
        e.bookmarked = Some(true);
        assert!(entry_page(&e, "x").contains("\n# Journal — 2026-07-01\n"));
        assert!(entry_page(&e, "x").contains("bookmarked: true\n"));
    }

    #[test]
    fn write_entry_creates_then_overwrites_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let e = entry("e1", "2026-07-01T08:00:00.000Z");
        let p1 = write_entry(dir.path(), &e, "first").unwrap();
        let p2 = write_entry(dir.path(), &e, "edited").unwrap();
        assert_eq!(p1, p2);
        assert!(std::fs::read_to_string(&p2).unwrap().contains("edited"));
        assert_eq!(
            walkdir_count(dir.path()),
            3,
            "one current page plus both observed revisions"
        );
    }

    fn walkdir_count(root: &Path) -> usize {
        fn rec(d: &Path, n: &mut usize) {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    rec(&p, n);
                } else if p.extension().is_some_and(|x| x == "md") {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        rec(root, &mut n);
        n
    }
}
