//! Scan the clone, notice what is new, hand it to the funnel (#917, #918).
//!
//! The scan is stateless on purpose. There is no watcher, no cursor file and no
//! "last seen" table: the `emails` row *is* the record that a meeting was
//! handled, which is the same substrate the gmail, gcal and voice channels
//! dedup on. That makes a rescan free, a half-synced tree harmless, and a lost
//! state file impossible.
//!
//! A file we cannot parse is warned about and skipped rather than failing the
//! run: mid-`git merge` the working tree legitimately contains a half-written
//! file, and one bad meeting must never stall the feed for the rest.

use std::path::{Path, PathBuf};

use augmentagent_store::Email;

use crate::distill::{admit, synthetic_meeting_email, RosterMember, Skip};
use crate::match_event::Match;
use crate::parse::{parse_meeting_file, MeetingDoc, ParseError};

/// The dedup substrate, behind a trait so the scan is testable with no
/// database. The real implementation is `Store::upsert_email`, which returns
/// `true` exactly when the row did not previously exist.
pub trait SeenLog {
    /// Record the meeting. `Ok(true)` means it is new and should be ingested.
    ///
    /// # Errors
    ///
    /// Whatever the store failed with; the caller logs and skips that meeting.
    fn record(&self, email: &Email) -> anyhow::Result<bool>;
}

/// What one scan did, for the log line and for the tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Meetings handed to the ingest funnel.
    pub ingested: Vec<String>,
    /// Already-recorded meetings, including retitled re-pushes.
    pub duplicates: usize,
    /// Meetings refused before any model call, with the reason.
    pub skipped: Vec<(String, Skip)>,
    /// Files that are not readable meetings — OKF `index.md`/`log.md` land here
    /// too, which is why this is not an error count.
    pub unreadable: Vec<PathBuf>,
}

/// Everything a scan needs from the outside world.
pub struct ScanOpts<'a> {
    /// The clone's `meetings/` directory.
    pub dir: &'a Path,
    /// Refuse a meeting whose export did not record disclosure.
    pub require_disclosed: bool,
    /// The operator's own address, excluded from any roster.
    pub my_email: &'a str,
}

/// Read every meeting file in `dir`, newest filename first.
///
/// Sorted because filenames lead with `YYYY-MM-DD`, so this makes a backfill
/// ingest the most recent meetings first — the ones whose facts are most likely
/// to matter while the backlog drains.
fn read_dir_sorted(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    files.reverse();
    Ok(files)
}

/// Parse one file, mapping io errors into the same "unreadable" bucket as a
/// parse failure — from the caller's side they are the same event.
fn read_meeting(path: &Path) -> Result<MeetingDoc, ParseError> {
    let text = std::fs::read_to_string(path).map_err(|_| ParseError::NoFrontmatter)?;
    parse_meeting_file(&text)
}

/// Scan, dedup and emit the meetings that should be ingested.
///
/// Returns the report and the emails to hand to `spawn_ingest`, rather than
/// spawning them here: the funnel needs a reasoner and a wiki root that this
/// crate has no business owning, and separating them is what lets the whole
/// scan be tested without a model.
///
/// # Errors
///
/// Only if `dir` cannot be listed at all. Everything below that is reported.
pub fn scan(
    opts: &ScanOpts<'_>,
    seen: &dyn SeenLog,
    resolve_event: &dyn Fn(&MeetingDoc) -> (Match, Vec<RosterMember>),
) -> anyhow::Result<(ScanReport, Vec<Email>)> {
    let mut report = ScanReport::default();
    let mut out = Vec::new();

    for path in read_dir_sorted(opts.dir)? {
        let doc = match read_meeting(&path) {
            Ok(d) => d,
            Err(_) => {
                report.unreadable.push(path);
                continue;
            }
        };

        // Consent and emptiness are decided before the calendar is consulted
        // and before any model call: an undisclosed recording must not even be
        // looked up, let alone summarized.
        if let Err(skip) = admit(&doc, opts.require_disclosed) {
            report.skipped.push((doc.id.clone(), skip));
            continue;
        }

        let (event, roster) = resolve_event(&doc);
        let email = synthetic_meeting_email(&doc, &event, &roster, opts.my_email);

        match seen.record(&email) {
            Ok(true) => {
                report.ingested.push(doc.id.clone());
                out.push(email);
            }
            Ok(false) => report.duplicates += 1,
            Err(e) => {
                tracing::warn!(meeting = %doc.id, error = %e, "could not record meeting; skipping");
                report.unreadable.push(path);
            }
        }
    }
    Ok((report, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use tempfile::TempDir;

    /// An in-memory `emails` table with the same "true means new" contract.
    #[derive(Default)]
    struct FakeLog {
        seen: RefCell<HashSet<String>>,
    }
    impl SeenLog for FakeLog {
        fn record(&self, email: &Email) -> anyhow::Result<bool> {
            Ok(self.seen.borrow_mut().insert(email.message_id.clone()))
        }
    }

    struct FailingLog;
    impl SeenLog for FailingLog {
        fn record(&self, _: &Email) -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("database is locked"))
        }
    }

    fn no_calendar(_: &MeetingDoc) -> (Match, Vec<RosterMember>) {
        (Match::None, Vec::new())
    }

    fn meeting(id: &str, title: &str, disclosed: bool) -> String {
        format!(
            "---\ntype: meeting-transcript\nid: \"{id}\"\ntitle: \"{title}\"\ndate: \"2026-09-01\"\nstarted_at_ms: 1788233602823\nduration: \"00:30:00\"\ndisclosed: {disclosed}\n---\n\n# {title}\n\nWe agreed to ship on Friday.\n\n## Transcript\n\n- [00:00:01] S0: hello\n"
        )
    }

    fn dir_with(files: &[(&str, String)]) -> TempDir {
        let d = TempDir::new().unwrap();
        for (name, body) in files {
            std::fs::write(d.path().join(name), body).unwrap();
        }
        d
    }

    fn opts<'a>(d: &'a TempDir) -> ScanOpts<'a> {
        ScanOpts {
            dir: d.path(),
            require_disclosed: false,
            my_email: "me@example.com",
        }
    }

    #[test]
    fn new_meetings_are_emitted_once() {
        let d = dir_with(&[
            ("2026-09-01-a-1.md", meeting("id-a", "Alpha", true)),
            ("2026-08-30-b-2.md", meeting("id-b", "Beta", true)),
        ]);
        let log = FakeLog::default();
        let (r, emails) = scan(&opts(&d), &log, &no_calendar).unwrap();
        assert_eq!(r.ingested.len(), 2);
        assert_eq!(emails.len(), 2);
        assert_eq!(r.duplicates, 0);
        // Newest first, so a backfill drains in the order that matters.
        assert_eq!(r.ingested, vec!["id-a", "id-b"]);
    }

    #[test]
    fn the_scan_is_idempotent() {
        let d = dir_with(&[("2026-09-01-a-1.md", meeting("id-a", "Alpha", true))]);
        let log = FakeLog::default();
        let (first, e1) = scan(&opts(&d), &log, &no_calendar).unwrap();
        let (second, e2) = scan(&opts(&d), &log, &no_calendar).unwrap();
        assert_eq!(first.ingested.len(), 1);
        assert_eq!(e1.len(), 1);
        assert!(second.ingested.is_empty(), "a rescan must ingest nothing");
        assert!(e2.is_empty());
        assert_eq!(second.duplicates, 1);
    }

    #[test]
    fn a_retitled_repush_is_not_reingested() {
        // FlyOnTheWall updates the same path when a meeting is retitled; the
        // frontmatter id is what stays stable, and it is the dedup key.
        let d = dir_with(&[("2026-09-01-a-1.md", meeting("id-a", "Alpha", true))]);
        let log = FakeLog::default();
        scan(&opts(&d), &log, &no_calendar).unwrap();

        std::fs::write(
            d.path().join("2026-09-01-a-1.md"),
            meeting("id-a", "Alpha — renamed after the fact", true),
        )
        .unwrap();
        let (r, emails) = scan(&opts(&d), &log, &no_calendar).unwrap();
        assert!(r.ingested.is_empty());
        assert!(emails.is_empty());
        assert_eq!(r.duplicates, 1);
    }

    #[test]
    fn okf_sidecars_and_malformed_files_are_skipped_without_stalling_the_run() {
        let d = dir_with(&[
            (
                "index.md",
                "---\nokf_version: \"0.2\"\n---\n\n# Meeting transcripts\n".to_string(),
            ),
            ("log.md", "# Change log\n\n## 2026-09-01\n".to_string()),
            (
                "truncated.md",
                "---\ntype: meeting-transcript\n".to_string(),
            ),
            ("2026-09-01-a-1.md", meeting("id-a", "Alpha", true)),
        ]);
        let log = FakeLog::default();
        let (r, emails) = scan(&opts(&d), &log, &no_calendar).unwrap();
        assert_eq!(r.ingested, vec!["id-a"], "the good file still ingests");
        assert_eq!(emails.len(), 1);
        assert_eq!(r.unreadable.len(), 3);
    }

    #[test]
    fn an_undisclosed_meeting_is_skipped_when_the_switch_is_on() {
        let d = dir_with(&[
            ("2026-09-01-a-1.md", meeting("id-a", "Alpha", false)),
            ("2026-09-01-b-2.md", meeting("id-b", "Beta", true)),
        ]);
        let log = FakeLog::default();
        let mut o = opts(&d);
        o.require_disclosed = true;
        let (r, emails) = scan(&o, &log, &no_calendar).unwrap();
        assert_eq!(r.ingested, vec!["id-b"]);
        assert_eq!(emails.len(), 1);
        assert_eq!(r.skipped, vec![("id-a".to_string(), Skip::NotDisclosed)]);
    }

    #[test]
    fn an_empty_recording_is_skipped_with_a_reason() {
        // A recorder left running: a title, a transcript, nothing to record.
        let empty = "---\ntype: meeting-transcript\nid: \"id-e\"\ntitle: \"Untitled recording\"\ndate: \"2026-08-27\"\nduration: \"00:00:40\"\ndisclosed: true\n---\n\n# Untitled recording\n\n## Transcript\n\n- [00:00:01] S0: uh\n";
        let d = dir_with(&[("2026-08-27-untitled-1.md", empty.to_string())]);
        let log = FakeLog::default();
        let (r, emails) = scan(&opts(&d), &log, &no_calendar).unwrap();
        assert!(r.ingested.is_empty());
        assert!(emails.is_empty());
        assert_eq!(r.skipped, vec![("id-e".to_string(), Skip::Empty)]);
    }

    #[test]
    fn a_store_failure_skips_one_meeting_rather_than_the_run() {
        let d = dir_with(&[("2026-09-01-a-1.md", meeting("id-a", "Alpha", true))]);
        let (r, emails) = scan(&opts(&d), &FailingLog, &no_calendar).unwrap();
        assert!(r.ingested.is_empty());
        assert!(emails.is_empty());
        assert_eq!(r.unreadable.len(), 1, "reported, not panicked");
    }

    #[test]
    fn a_missing_directory_is_the_one_hard_error() {
        let d = TempDir::new().unwrap();
        let missing = d.path().join("meetings");
        let o = ScanOpts {
            dir: &missing,
            require_disclosed: false,
            my_email: "me@example.com",
        };
        assert!(scan(&o, &FakeLog::default(), &no_calendar).is_err());
    }

    #[test]
    fn a_calendar_match_puts_the_roster_in_the_email() {
        use crate::match_event::EventWindow;
        let d = dir_with(&[("2026-09-01-a-1.md", meeting("id-a", "Alpha", true))]);
        let resolve = |_: &MeetingDoc| {
            (
                Match::Single(EventWindow {
                    event_id: "evt-9".into(),
                    start_ms: 0,
                    end_ms: 1,
                }),
                vec![RosterMember {
                    email: "priya@example.com".into(),
                    display_name: Some("Priya Raman".into()),
                    response_status: Some("accepted".into()),
                }],
            )
        };
        let (_, emails) = scan(&opts(&d), &FakeLog::default(), &resolve).unwrap();
        assert_eq!(emails.len(), 1);
        assert!(emails[0].body.contains("priya@example.com"));
        assert!(emails[0].body.contains("evt-9"));
    }
}
