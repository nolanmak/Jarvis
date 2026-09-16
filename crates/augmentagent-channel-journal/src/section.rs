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

/// Write (or overwrite) the entry's page under `wiki_root`; returns the
/// absolute path written.
pub fn write_entry(wiki_root: &Path, entry: &Entry, text: &str) -> io::Result<PathBuf> {
    let rel = entry_rel_path(entry);
    let path = wiki_root.join(&rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, entry_page(entry, text))?;
    Ok(path)
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
            1,
            "one entry, one file — edits never fork"
        );
    }

    fn walkdir_count(root: &Path) -> usize {
        fn rec(d: &Path, n: &mut usize) {
            for e in std::fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    rec(&p, n);
                } else {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        rec(root, &mut n);
        n
    }
}
