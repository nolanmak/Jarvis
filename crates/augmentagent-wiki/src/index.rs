//! Derived `index.md` (#642).
//!
//! The index is a *generated* catalog of every page under `people/`,
//! `threads/`, and `projects/` — never hand-kept. Ingest used to ask the
//! model to append an entry per new page (step 4 of the ingest workflow),
//! which drifted structurally: failures were warn-only and coverage never
//! converged. This module regenerates the whole file from the pages on
//! disk, so a single missed edit can no longer become permanent drift.
//!
//! Write discipline: temp file + rename in the same directory (atomic on
//! POSIX). Deliberately NOT a lockfile — `wiki sync` hard-fails if any
//! `*.lock` is tracked or staged in the wiki repo. The wiki repo's own
//! `.gitignore` covers `*.tmp`, so the transient file can never be staged
//! by a racing sync. In-process writers serialize via `with_page_lock` at
//! the call site; cross-process races end in one rename winning, which is
//! fine for a file that is derived from scratch every time.

use std::path::Path;

use anyhow::{Context, Result};

use crate::freshness::{self, PageStatus};
use crate::migrate::split_frontmatter;

/// Maps a cited message id to `emails.firstSeenAt` (epoch ms). The index
/// rebuilder never talks to the store directly — callers inject this so the
/// crate stays io/schema-agnostic and tests stay hermetic. A resolver that
/// always returns `None` renders every page "facts unknown" (G1: unknown is
/// the safe direction, never fresh).
pub type FirstSeenResolver<'a> = &'a (dyn Fn(&str) -> Option<i64> + 'a);

/// Section order in the generated file. Matches `WikiLayout::bootstrap`.
const SECTIONS: &[(&str, &str)] = &[
    ("People", "people"),
    ("Threads", "threads"),
    ("Projects", "projects"),
];

/// Longest summary we will emit after the `— ` separator. Long enough for
/// the one-line gists ingest writes into page bodies, short enough that the
/// index stays a catalog rather than a mirror.
const SUMMARY_MAX_CHARS: usize = 220;

/// Placeholder when a page yields no derivable text (empty body, headings
/// only). The page is still listed — the index is a catalog of every page,
/// and an entry with no summary still fixes retrieval.
const NO_SUMMARY: &str = "(no summary)";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStats {
    pub people: usize,
    pub threads: usize,
    pub projects: usize,
    /// Pages that could not be read (I/O error or non-UTF8). Listed nowhere;
    /// counted so callers can surface the gap instead of silently shrinking
    /// the index.
    pub unreadable: usize,
}

impl IndexStats {
    pub fn total(&self) -> usize {
        self.people + self.threads + self.projects
    }
}

/// Regenerate `<root>/index.md` from the pages on disk. Atomic: the new
/// content lands via temp-file + rename, so readers never observe a
/// half-written index. The temp name is unique per process + call so two
/// concurrent rebuilds (daemon ingest tail vs. a CLI invocation) can never
/// write through each other's handle — last rename wins, each rename is
/// whole. Names keep the `.tmp` suffix the wiki repo's `.gitignore` covers.
pub fn rebuild_index(root: &Path, resolve: FirstSeenResolver) -> Result<IndexStats> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (doc, stats) = render_index(root, resolve)?;
    let tmp = root.join(format!(
        "index.md.{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let dst = root.join("index.md");
    std::fs::write(&tmp, &doc).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &dst).map_err(|e| {
        // Never leave a stray temp behind on failure — the wiki dir is
        // the owner's browsable KB.
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("rename {} -> {}: {e}", tmp.display(), dst.display())
    })?;
    Ok(stats)
}

/// Render the full index document from the pages on disk without writing.
pub fn render_index(root: &Path, resolve: FirstSeenResolver) -> Result<(String, IndexStats)> {
    let mut stats = IndexStats::default();
    let mut doc = String::from(
        "# Wiki Index\n\n*Derived from the pages on disk by `augmentagent wiki index --rebuild`. \
         Do not edit by hand — edits are overwritten on the next rebuild. \
         `facts as of` = newest cited evidence (`emails.firstSeenAt`) or owner \
         verification; `facts unknown` = no cited message resolved (never \
         assume fresh).*\n",
    );

    for (title, dir) in SECTIONS {
        let entries = collect_entries(root, dir, resolve, &mut stats.unreadable)?;
        match *dir {
            "people" => stats.people = entries.len(),
            "threads" => stats.threads = entries.len(),
            "projects" => stats.projects = entries.len(),
            _ => unreachable!(),
        }
        doc.push_str(&format!("\n## {title}\n\n"));
        for (rel, summary) in entries {
            doc.push_str(&format!("- [{rel}]({rel}) — {summary}\n"));
        }
    }

    Ok((doc, stats))
}

/// Walk `<root>/<dir>/*.md` and produce `(relative_path, summary · freshness)`
/// pairs, sorted by path for a deterministic, diff-friendly output.
fn collect_entries(
    root: &Path,
    dir: &str,
    resolve: FirstSeenResolver,
    unreadable: &mut usize,
) -> Result<Vec<(String, String)>> {
    let abs = root.join(dir);
    let mut entries = Vec::new();
    let rd = match std::fs::read_dir(&abs) {
        Ok(rd) => rd,
        // A wiki without e.g. projects/ yet is valid — empty section.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(e) => return Err(e).with_context(|| format!("read_dir {}", abs.display())),
    };
    for ent in rd {
        let ent = ent.with_context(|| format!("read_dir entry in {}", abs.display()))?;
        let path = ent.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            *unreadable += 1;
            continue;
        };
        let rel = format!("{dir}/{name}");
        match std::fs::read_to_string(&path) {
            Ok(page) => {
                let summary = derive_summary(&page).unwrap_or_else(|| NO_SUMMARY.to_string());
                let marker = freshness_marker(&page, resolve);
                entries.push((rel, format!("{summary} · {marker}")));
            }
            Err(_) => *unreadable += 1,
        }
    }
    entries.sort();
    Ok(entries)
}

/// Derive a one-line summary from a page: the first `# H1` heading or the
/// first line of prose, whichever appears first in the body (frontmatter
/// excluded). Returns `None` when the page has no derivable text.
pub fn derive_summary(page: &str) -> Option<String> {
    let body = match split_frontmatter(page) {
        Some((_, close_off)) => {
            // `close_off` is the byte offset of the closing `---` line;
            // the body starts after that line's newline.
            match page[close_off..].find('\n') {
                Some(nl) => &page[close_off + nl + 1..],
                None => "",
            }
        }
        None => page,
    };

    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line == "---" {
            continue;
        }
        if let Some(h1) = line.strip_prefix("# ") {
            return normalize_summary(h1);
        }
        if line.starts_with('#') {
            // ## and deeper are section scaffolding, not content.
            continue;
        }
        let line = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line);
        return normalize_summary(line);
    }
    None
}

/// The freshness column (#642 phase 2): appended after the summary as
/// `· facts as of YYYY-MM-DD`, `· facts unknown` (G1 — no cited message
/// resolved; never rendered as fresh), or `· deprecated` (OKF `status:`,
/// which makes evidence age moot).
fn freshness_marker(page: &str, resolve: FirstSeenResolver) -> String {
    let f = freshness::compute(page, resolve);
    if f.status == PageStatus::Deprecated {
        return "deprecated".to_string();
    }
    match f.as_of {
        Some(date) => format!("facts as of {date}"),
        None => "facts unknown".to_string(),
    }
}

/// Collapse whitespace and cap length so the entry stays one line.
fn normalize_summary(s: &str) -> Option<String> {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= SUMMARY_MAX_CHARS {
        return Some(collapsed);
    }
    let truncated: String = collapsed.chars().take(SUMMARY_MAX_CHARS).collect();
    Some(format!("{}…", truncated.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    const PERSON: &str = "---\nkind: person\nkey: ada\ncreated: 2026-01-01\nupdated: 2026-01-02\nsources: [aa11]\n---\n\n## Identity\n\nAda Lovelace, mathematician at Analytical Engines Ltd.\n\n## Tone\n\nFormal.\n";

    #[test]
    fn summary_prefers_first_prose_line_after_frontmatter() {
        assert_eq!(
            derive_summary(PERSON).unwrap(),
            "Ada Lovelace, mathematician at Analytical Engines Ltd."
        );
    }

    #[test]
    fn summary_uses_h1_when_it_comes_first() {
        let page = "---\nkind: project\n---\n\n# Q2 Launch Plan\n\nDetails follow.\n";
        assert_eq!(derive_summary(page).unwrap(), "Q2 Launch Plan");
    }

    #[test]
    fn summary_strips_bullet_and_collapses_whitespace() {
        let page = "---\nkind: thread\n---\n\n## Timeline\n\n- **2026-04-20**  |  hello\n   world\n";
        assert_eq!(derive_summary(page).unwrap(), "**2026-04-20** | hello");
    }

    #[test]
    fn summary_is_none_for_headings_only_page() {
        let page = "---\nkind: person\n---\n\n## Identity\n\n## Tone\n";
        assert_eq!(derive_summary(page), None);
    }

    #[test]
    fn summary_handles_missing_frontmatter() {
        assert_eq!(derive_summary("Just prose.\n").unwrap(), "Just prose.");
    }

    #[test]
    fn summary_truncates_long_lines_on_char_boundary() {
        let long = "é".repeat(500);
        let s = derive_summary(&format!("---\nkind: person\n---\n\n{long}\n")).unwrap();
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), SUMMARY_MAX_CHARS + 1);
    }

    #[test]
    fn rebuild_lists_every_page_in_sorted_sections() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(root, "people/zed.md", PERSON);
        write(root, "people/ada.md", PERSON);
        write(
            root,
            "threads/t1.md",
            "---\nkind: thread\n---\n\n## Subject\nRe: contract\n",
        );
        write(root, "projects/q2.md", "---\nkind: project\n---\n\n## Project\n\nQ2 launch.\n");
        // Non-md and nested files are ignored.
        write(root, "people/notes.txt", "not a page");

        // aa11 (PERSON's source) resolves to 2026-04-18; everything else is unknown.
        let resolve = |id: &str| (id == "aa11").then_some(1776549330000i64);
        let stats = rebuild_index(root, &resolve).unwrap();
        assert_eq!(stats.people, 2);
        assert_eq!(stats.threads, 1);
        assert_eq!(stats.projects, 1);
        assert_eq!(stats.unreadable, 0);

        let doc = std::fs::read_to_string(root.join("index.md")).unwrap();
        let ada = doc.find("- [people/ada.md](people/ada.md) — ").unwrap();
        let zed = doc.find("- [people/zed.md](people/zed.md) — ").unwrap();
        assert!(ada < zed, "entries must be sorted by path");
        assert!(
            doc.contains("- [people/ada.md](people/ada.md) — Ada Lovelace, mathematician at Analytical Engines Ltd. · facts as of 2026-04-18"),
            "resolved sources must stamp the freshness column: {doc}"
        );
        assert!(doc.contains("- [threads/t1.md](threads/t1.md) — Re: contract · facts unknown"));
        assert!(doc.contains("- [projects/q2.md](projects/q2.md) — Q2 launch. · facts unknown"));
        assert!(!doc.contains("notes.txt"));
        let leftover_tmp = std::fs::read_dir(root)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leftover_tmp, "temp file must be renamed away");
    }

    #[test]
    fn rebuild_replaces_stale_index_and_tolerates_missing_dirs() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        std::fs::write(root.join("index.md"), "# Wiki Index\n\n- [people/gone.md](people/gone.md) — stale\n").unwrap();
        write(root, "people/here.md", PERSON);

        let stats = rebuild_index(root, &|_| None).unwrap();
        assert_eq!(stats.total(), 1);
        let doc = std::fs::read_to_string(root.join("index.md")).unwrap();
        assert!(!doc.contains("gone.md"), "stale entries must not survive a rebuild");
        assert!(doc.contains("here.md"));
        // threads/ and projects/ don't exist — sections render empty.
        assert!(doc.contains("## Threads"));
        assert!(doc.contains("## Projects"));
    }

    #[test]
    fn pages_with_no_derivable_text_still_get_listed() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(root, "people/blank.md", "---\nkind: person\n---\n");
        rebuild_index(root, &|_| None).unwrap();
        let doc = std::fs::read_to_string(root.join("index.md")).unwrap();
        assert!(doc.contains(&format!(
            "- [people/blank.md](people/blank.md) — {NO_SUMMARY} · facts unknown"
        )));
    }

    #[test]
    fn deprecated_status_wins_the_freshness_column() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(
            root,
            "projects/old.md",
            "---\nkind: project\nstatus: deprecated\nsources: [aa11]\n---\n\nRetired effort.\n",
        );
        rebuild_index(root, &|_| Some(1776549330000)).unwrap();
        let doc = std::fs::read_to_string(root.join("index.md")).unwrap();
        assert!(doc.contains("- [projects/old.md](projects/old.md) — Retired effort. · deprecated"));
    }
}
