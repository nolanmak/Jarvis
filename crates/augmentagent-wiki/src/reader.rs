//! Hint strings for drafting-time wiki navigation.
//!
//! The drafting Claude call gets `--add-dir wiki/` + Read/Grep/Glob tools, so
//! it can open any page it wants. `WikiReader` doesn't do the reading — it
//! just produces a short hint string that points at likely-relevant pages, so
//! Claude doesn't have to guess file paths.

use std::path::{Path, PathBuf};

use augmentagent_store::Email;

use crate::layout::WikiLayout;

/// How many of the newest meeting files a hint scan reads. FlyOnTheWall names
/// them `YYYY-MM-DD-{slug}-{id8}.md`, so reverse-lexicographic is newest-first.
const MEETING_SCAN_LIMIT: usize = 20;
/// Most meeting paths a draft hint names.
const MEETING_HINT_MAX: usize = 3;
/// How much of a meeting file the scan reads. The title and summary sit right
/// below the frontmatter; the transcript underneath is megabytes we never want
/// on the hot path, so the cap is on the read, not just on the match.
const MEETING_HEAD_BYTES: u64 = 8192;
/// Ceilings on the FINISHED hint, not just on the block #921 appends: meeting
/// paths are absolute under a host-configured `AUGMENTAGENT_TRANSCRIPTS_DIR`,
/// so this crate cannot assume they are short. Triage keeps the ≤400 bytes
/// `triage_hint_stays_short` has always pinned; the draft hint gets a larger
/// line because its pre-#921 wiki prose alone runs ~370 bytes, and the
/// meetings block fills only what is left under it.
const TRIAGE_HINT_MAX_BYTES: usize = 400;
const DRAFT_HINT_MAX_BYTES: usize = 600;
/// Preamble for the draft hint's meetings block. Named so the byte budget can
/// account for it before the first path is appended.
const MEETING_HINT_HEADER: &str =
    "Recent meetings mentioning this person (full transcripts; wiki meeting facts cite fotw:<id>):";

pub struct WikiReader<'a> {
    pub layout: &'a WikiLayout,
    transcripts_meetings_dir: Option<PathBuf>,
}

impl<'a> WikiReader<'a> {
    pub fn new(layout: &'a WikiLayout) -> Self {
        Self {
            layout,
            transcripts_meetings_dir: None,
        }
    }

    /// Point the reader at the FlyOnTheWall clone's `meetings/` directory so
    /// hints can name the transcript that carries the actual words (#921).
    /// Without it the hints are exactly what they were before.
    pub fn with_transcripts_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.transcripts_meetings_dir = dir;
        self
    }

    /// Produce a hint string describing which wiki pages the drafting call
    /// should consider opening for this email. Empty-string return means "no
    /// prior context yet — rely on the raw email only".
    pub fn draft_hint(&self, email: &Email) -> String {
        let mut lines: Vec<String> = Vec::new();

        let person_page = self.layout.person_page(&email.from);
        if exists(&person_page) {
            lines.push(format!(
                "- {} (sender history + preferred tone)",
                relative_to_root(&self.layout.root, &person_page)
            ));
        }

        if let Some(tid) = &email.thread_id {
            let thread_page = self.layout.thread_page(tid);
            if exists(&thread_page) {
                lines.push(format!(
                    "- {} (prior messages in this thread)",
                    relative_to_root(&self.layout.root, &thread_page)
                ));
            }
        }

        let wiki_block = if lines.is_empty() {
            String::new()
        } else {
            format!(
                "Relevant wiki pages (you MAY open these with the Read tool; you may also Grep/Glob the wiki for additional context):\n{}\n\nAlways prefer wiki facts over assumptions. If the wiki contradicts the email, trust the email and flag the contradiction in your reasoning.",
                lines.join("\n")
            )
        };

        // The meetings block gets what is left of the whole hint's budget once
        // the wiki prose and the blank line joining them are paid for, so #921
        // can never push the finished hint past [`DRAFT_HINT_MAX_BYTES`].
        let spent = if wiki_block.is_empty() {
            0
        } else {
            wiki_block.len() + 2
        };
        let meetings = meeting_block(
            &self.meetings_mentioning(email, MEETING_HINT_MAX),
            DRAFT_HINT_MAX_BYTES.saturating_sub(spent),
        );
        match (wiki_block.is_empty(), meetings.is_empty()) {
            (true, _) => meetings,
            (_, true) => wiki_block,
            _ => format!("{wiki_block}\n\n{meetings}"),
        }
    }

    /// Produce a short nudge for the triage call. Single-line-per-page, no
    /// prose — triage is cost-sensitive so we keep the token footprint under
    /// 100 chars. Empty string when no relevant wiki page exists.
    pub fn triage_hint(&self, email: &Email) -> String {
        let mut lines: Vec<String> = Vec::new();

        let person_page = self.layout.person_page(&email.from);
        if exists(&person_page) {
            lines.push(format!(
                "- Sender has a wiki page ({}) — open with Read; weight importance by Relationship/Tone.",
                relative_to_root(&self.layout.root, &person_page)
            ));
        }

        if let Some(tid) = &email.thread_id {
            let thread_page = self.layout.thread_page(tid);
            if exists(&thread_page) {
                lines.push(format!(
                    "- Prior thread context at {} — open with Read if the email is a follow-up.",
                    relative_to_root(&self.layout.root, &thread_page)
                ));
            }
        }

        // One path only, and no "open it" nudge: triage's add-dir is the wiki
        // alone, so the transcript is a recency/importance signal here, not
        // something this call can read. Dropped outright when the wiki lines
        // plus this path would carry the hint past its budget.
        if let Some(path) = self.meetings_mentioning(email, 1).into_iter().next() {
            let line = format!("- Sender appeared in a recent meeting transcript: {path}");
            let used: usize = lines.iter().map(|l| l.len() + 1).sum();
            if used + line.len() <= TRIAGE_HINT_MAX_BYTES {
                lines.push(line);
            }
        }

        if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n")
        }
    }

    /// Newest-first paths of recent meeting files whose title or summary block
    /// names the sender. Empty whenever no transcript clone is configured, the
    /// sender has no display name, or nothing matches — which is what keeps
    /// the hints byte-identical to their pre-#921 text in the common case.
    ///
    /// Per-email cost is bounded by construction: one streaming `read_dir`
    /// pass holding only the [`MEETING_SCAN_LIMIT`] window (see
    /// [`recent_meeting_files`]), then at most that many reads capped at
    /// [`MEETING_HEAD_BYTES`] each (~160 KB worst case), short-circuited as
    /// soon as `max` hits are found. Every I/O failure degrades to no hint —
    /// a hint is advisory, so a slow or broken clone must never fail an email.
    fn meetings_mentioning(&self, email: &Email, max: usize) -> Vec<String> {
        let Some(dir) = self.transcripts_meetings_dir.as_deref() else {
            return Vec::new();
        };
        let Some(name) = display_name(&email.from) else {
            return Vec::new();
        };
        let needle = name.to_lowercase();
        // Under three characters is an initials-or-junk substring, and it would
        // match half the clone.
        if needle.chars().count() < 3 {
            return Vec::new();
        }

        let mut hits = Vec::new();
        for path in recent_meeting_files(dir) {
            if meeting_head(&path).is_some_and(|head| head.contains(&needle)) {
                hits.push(path.to_string_lossy().into_owned());
                if hits.len() == max {
                    break;
                }
            }
        }
        hits
    }
}

/// `AUGMENTAGENT_TRANSCRIPTS_DIR/meetings`, when the clone is actually there.
/// The daemon calls this once per hint; a missing clone is the normal state on
/// a host that doesn't run FlyOnTheWall.
pub fn transcripts_meetings_dir_from_env() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("AUGMENTAGENT_TRANSCRIPTS_DIR")?).join("meetings");
    dir.is_dir().then_some(dir)
}

/// `Display Name <addr>` → the display name. A bare address carries no name,
/// and a name is the only thing a transcript can be scanned for.
fn display_name(from: &str) -> Option<&str> {
    let (name, _) = from.split_once('<')?;
    let name = name.trim().trim_matches('"').trim();
    (!name.is_empty()).then_some(name)
}

/// The newest [`MEETING_SCAN_LIMIT`] transcript files, newest first. A live
/// clone accumulates meetings without bound and this runs per email, so the
/// window is kept by insertion during a single `read_dir` pass: no vector of
/// every entry, no sort of the whole directory, and the cheap name test runs
/// before the `file_type` syscall.
fn recent_meeting_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut newest: Vec<std::ffi::OsString> = Vec::with_capacity(MEETING_SCAN_LIMIT + 1);
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_dated_meeting) {
            continue;
        }
        if newest.len() == MEETING_SCAN_LIMIT && *newest.last().expect("non-empty") >= name {
            continue;
        }
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        // `newest` is sorted newest-first, so this is the descending insert.
        let at = newest.partition_point(|n| *n > name);
        newest.insert(at, name);
        newest.truncate(MEETING_SCAN_LIMIT);
    }
    newest.into_iter().map(|n| dir.join(n)).collect()
}

/// `YYYY-MM-DD-{slug}-{id8}.md`, the shape FlyOnTheWall exports. Demanding the
/// date prefix is what makes the lexicographic window above a newest-first
/// one: an undated file (`zzz.md`, a stray `README.md`, the OKF `index.md` /
/// `log.md`) sorts above every dated name and would otherwise evict real
/// meetings from the scan window.
fn is_dated_meeting(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() > 10
        && name.ends_with(".md")
        && b[10] == b'-'
        && b[..10].iter().enumerate().all(|(i, c)| {
            if matches!(i, 4 | 7) {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// Header plus one line per path, filled newest-first while the whole block
/// stays inside `budget` bytes. Empty when even one path does not fit: a hint
/// is advisory, so a path too long to name is simply not named.
fn meeting_block(paths: &[String], budget: usize) -> String {
    let mut block = String::new();
    for p in paths {
        let line = format!("\n- {p}");
        let base = if block.is_empty() {
            MEETING_HINT_HEADER.len()
        } else {
            block.len()
        };
        if base + line.len() > budget {
            break;
        }
        if block.is_empty() {
            block.push_str(MEETING_HINT_HEADER);
        }
        block.push_str(&line);
    }
    block
}

/// Lowercased `# Title` + summary block of a meeting file — everything above
/// the first `## ` section. Stopping there is the point: a name spoken in
/// `## Transcript` is not evidence that the meeting was *about* that person,
/// and the raw words never enter a prompt. An unreadable file yields `None`.
fn meeting_head(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MEETING_HEAD_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    // Lossy, because the cap can land mid-codepoint; a replacement char in
    // the tail cannot create a false match on a name.
    let text = String::from_utf8_lossy(&buf);
    let head = text.split("\n## ").next().unwrap_or_default();
    Some(head.to_lowercase())
}

fn exists(p: &Path) -> bool {
    p.is_file()
}

fn relative_to_root(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn email(from: &str, thread_id: Option<&str>) -> Email {
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "m1".into(),
            thread_id: thread_id.map(str::to_string),
            from: from.into(),
            subject: "s".into(),
            body: "b".into(),
            date: "2026-04-14".into(),
            account_entity_id: None,
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    #[test]
    fn empty_hint_when_no_pages_exist() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let r = WikiReader::new(&layout);
        assert_eq!(r.draft_hint(&email("a@b.com", Some("t1"))), "");
    }

    #[test]
    fn references_existing_person_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("a@b.com"), "# A\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.draft_hint(&email("a@b.com", None));
        assert!(hint.contains("people/a_at_b_com.md"));
    }

    #[test]
    fn references_existing_thread_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.draft_hint(&email("a@b.com", Some("t1")));
        assert!(hint.contains("threads/t1.md"));
    }

    #[test]
    fn triage_hint_empty_when_no_pages_exist() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let r = WikiReader::new(&layout);
        assert_eq!(r.triage_hint(&email("a@b.com", Some("t1"))), "");
    }

    #[test]
    fn triage_hint_references_person_page() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("a@b.com"), "# A\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.triage_hint(&email("a@b.com", None));
        assert!(hint.contains("people/a_at_b_com.md"));
        assert!(hint.contains("Relationship"));
    }

    #[test]
    fn triage_hint_includes_thread_when_present() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.triage_hint(&email("a@b.com", Some("t1")));
        assert!(hint.contains("threads/t1.md"));
    }

    #[test]
    fn triage_hint_stays_short() {
        // Token cost sanity — hint should fit inside ~200 chars per page so
        // triage prompts don't balloon. Two pages max = 400 chars.
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("a@b.com"), "# A\n").unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let r = WikiReader::new(&layout);
        let hint = r.triage_hint(&email("a@b.com", Some("t1")));
        assert!(hint.len() < 400, "triage hint too long: {} chars", hint.len());
    }

    /// A meeting file in the FlyOnTheWall export shape: frontmatter, `# Title`,
    /// summary prose, then the sections the hint scan must never read.
    fn meeting_file(dir: &Path, name: &str, title: &str, summary: &str, transcript: &str) {
        std::fs::write(
            dir.join(name),
            format!(
                "---\nid: \"{name}\"\ntype: meeting\n---\n\n# {title}\n\n{summary}\n\n## Transcript\n\n{transcript}\n"
            ),
        )
        .unwrap();
    }

    fn meetings_dir(d: &TempDir) -> PathBuf {
        let dir = d.path().join("transcripts").join("meetings");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The safety property: wiring a transcripts dir that matches nothing must
    /// leave the live draft/triage prompts byte-for-byte as they are today.
    #[test]
    fn no_match_leaves_the_hint_unchanged() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("dana@example.com"), "# Dana\n").unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let dir = meetings_dir(&d);
        meeting_file(
            &dir,
            "2026-08-20-platform-sync-aaaa1111.md",
            "Platform sync",
            "Priya Raman walked through the cost model.",
            "[00:01] S0: nothing to see here",
        );

        let e = email("Dana Reyes <dana@example.com>", Some("t1"));
        let before = WikiReader::new(&layout);
        let after = WikiReader::new(&layout).with_transcripts_dir(Some(dir));
        assert_eq!(after.draft_hint(&e), before.draft_hint(&e));
        assert_eq!(after.triage_hint(&e), before.triage_hint(&e));
    }

    #[test]
    fn draft_hint_names_meetings_that_mention_the_counterpart() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let dir = meetings_dir(&d);
        meeting_file(
            &dir,
            "2026-08-20-kubernetes-migration-aaaa1111.md",
            "Kubernetes migration with Dana Reyes",
            "Scoped the cutover.",
            "[00:01] S0: unrelated",
        );
        meeting_file(
            &dir,
            "2026-08-19-azure-cost-bbbb2222.md",
            "Azure cost review",
            "dana reyes owns the savings plan follow-up.",
            "[00:01] S0: unrelated",
        );
        // Named only in the diarised body — must NOT match.
        meeting_file(
            &dir,
            "2026-08-18-standup-cccc3333.md",
            "Standup",
            "Routine.",
            "[00:01] S0: Dana Reyes said she'd take it",
        );

        let r = WikiReader::new(&layout).with_transcripts_dir(Some(dir));
        let hint = r.draft_hint(&email("Dana Reyes <dana@example.com>", None));
        assert!(
            hint.contains("Recent meetings mentioning this person"),
            "missing meetings block: {hint}"
        );
        assert!(hint.contains("2026-08-20-kubernetes-migration-aaaa1111.md"));
        // Case-insensitive: the summary spells the name lowercase.
        assert!(hint.contains("2026-08-19-azure-cost-bbbb2222.md"));
        assert!(!hint.contains("2026-08-18-standup-cccc3333.md"));
    }

    #[test]
    fn draft_hint_skips_a_sender_with_no_display_name() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let dir = meetings_dir(&d);
        meeting_file(
            &dir,
            "2026-08-20-sync-aaaa1111.md",
            "Sync with dana@example.com",
            "Notes.",
            "[00:01] S0: unrelated",
        );

        let r = WikiReader::new(&layout).with_transcripts_dir(Some(dir));
        assert_eq!(r.draft_hint(&email("dana@example.com", None)), "");
    }

    /// PR review of #921 — the ceiling is on the FINISHED hint, not on the
    /// block this change appends: a sender with a person page, a thread page
    /// AND matching transcripts must still land inside the budget.
    #[test]
    fn draft_hint_is_bounded() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("dana@example.com"), "# Dana\n").unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let dir = meetings_dir(&d);
        for i in 0..40 {
            meeting_file(
                &dir,
                &format!("2026-08-{:02}-sync-{i:04}aaaa.md", (i % 28) + 1),
                "Weekly sync",
                "Dana Reyes attended.",
                "[00:01] S0: unrelated",
            );
        }

        let e = email("Dana Reyes <dana@example.com>", Some("t1"));
        let before = WikiReader::new(&layout).draft_hint(&e);
        let hint = WikiReader::new(&layout)
            .with_transcripts_dir(Some(dir))
            .draft_hint(&e);
        assert!(hint.starts_with(&before), "wiki block lost: {hint}");
        assert!(
            hint.len() <= DRAFT_HINT_MAX_BYTES,
            "draft hint too long: {} bytes",
            hint.len()
        );
        let paths: Vec<&str> = hint.lines().filter(|l| l.contains("-sync-")).collect();
        // 40 candidates, at most three named — and fewer still when the host's
        // temp root is long enough that a third line would breach the budget.
        assert!(
            (1..=MEETING_HINT_MAX).contains(&paths.len()),
            "expected 1..=3 paths: {paths:?}"
        );
        assert!(
            Path::new(paths[0].trim_start_matches("- ")).is_absolute(),
            "meeting path must be absolute: {}",
            paths[0]
        );
    }

    /// PR review of #921 — the meetings block appends absolute paths rooted at
    /// a host-configured `AUGMENTAGENT_TRANSCRIPTS_DIR`, so nothing about the
    /// deployment guarantees they are short: whatever budget the caller has
    /// left has to be enforced on the real output, path lengths included.
    #[test]
    fn meeting_block_never_exceeds_the_callers_budget() {
        let paths = |root: &str, n: usize| -> Vec<String> {
            (0..n)
                .map(|i| format!("{root}/2026-08-2{i}-weekly-sync-aaaa111{i}.md"))
                .collect()
        };
        let budget = 400;

        let short = meeting_block(&paths("/home/o/transcripts/meetings", MEETING_HINT_MAX), budget);
        assert_eq!(short.matches("\n- ").count(), MEETING_HINT_MAX);
        assert!(short.len() <= budget, "{short}");

        // A deep clone path: 150-byte entries fit once, not three times.
        let deep = meeting_block(
            &paths(&format!("/{}", "d".repeat(120)), MEETING_HINT_MAX),
            budget,
        );
        assert_eq!(deep.matches("\n- ").count(), 1, "{deep}");
        assert!(deep.len() <= budget, "{deep}");

        // Longer than the whole budget: no header, no dangling bullet.
        assert_eq!(meeting_block(&paths(&"d".repeat(500), 1), budget), "");
        // And a caller with nothing left to spend gets nothing.
        assert_eq!(meeting_block(&paths("/o", MEETING_HINT_MAX), 0), "");
    }

    /// PR review of #921 — same discipline on the triage side, where the wiki
    /// lines have already spent part of the budget: a transcript path from a
    /// deep clone is dropped rather than allowed to push the hint over.
    #[test]
    fn a_deep_clone_path_cannot_push_the_triage_hint_over_budget() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("dana@example.com"), "# Dana\n").unwrap();
        std::fs::write(layout.thread_page("t1"), "# t1\n").unwrap();
        let dir = d.path().join("d".repeat(200)).join("meetings");
        std::fs::create_dir_all(&dir).unwrap();
        meeting_file(
            &dir,
            "2026-08-20-kickoff-aaaa1111.md",
            "Kickoff with Dana Reyes",
            "Scoped the rollout.",
            "[00:01] S0: unrelated",
        );

        let r = WikiReader::new(&layout).with_transcripts_dir(Some(dir));
        let hint = r.triage_hint(&email("Dana Reyes <dana@example.com>", Some("t1")));
        assert!(hint.contains("threads/t1.md"), "wiki lines lost: {hint}");
        assert!(
            hint.len() <= TRIAGE_HINT_MAX_BYTES,
            "triage hint too long: {} chars",
            hint.len()
        );
    }

    /// PR review of #921 — this runs per email against a live clone that keeps
    /// growing, so the scan must never materialise or sort the whole directory:
    /// it hands back the newest [`MEETING_SCAN_LIMIT`] *dated* names and no
    /// more. Undated strays (`README.md`, the OKF `index.md` / `log.md`) sort
    /// above every `YYYY-MM-DD-` name and would otherwise evict real meetings.
    #[test]
    fn the_scan_window_is_bounded_and_newest_first() {
        let d = TempDir::new().unwrap();
        let dir = meetings_dir(&d);
        for i in 0..120 {
            let name = format!("2026-{:02}-{:02}-sync-a.md", i / 28 + 1, i % 28 + 1);
            std::fs::write(dir.join(name), "x").unwrap();
            std::fs::write(dir.join(format!("zzz-{i:03}.md")), "x").unwrap();
        }

        let names: Vec<String> = recent_meeting_files(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), MEETING_SCAN_LIMIT, "scan window is unbounded");
        let mut sorted = names.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(names, sorted, "window is not newest-first: {names:?}");
        assert_eq!(names[0], "2026-05-08-sync-a.md", "newest meeting missed");
    }

    #[test]
    fn triage_hint_names_at_most_one_meeting_and_stays_short() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        std::fs::write(layout.person_page("dana@example.com"), "# Dana\n").unwrap();
        let dir = meetings_dir(&d);
        for i in 0..3 {
            meeting_file(
                &dir,
                &format!("2026-08-1{i}-sync-{i:04}aaaa.md"),
                "Weekly sync",
                "Dana Reyes attended.",
                "[00:01] S0: unrelated",
            );
        }

        let r = WikiReader::new(&layout).with_transcripts_dir(Some(dir));
        let hint = r.triage_hint(&email("Dana Reyes <dana@example.com>", None));
        assert_eq!(
            hint.matches("-sync-").count(),
            1,
            "triage must name at most one meeting: {hint}"
        );
        assert!(hint.contains("2026-08-12-sync-0002aaaa.md"), "not newest");
        assert!(hint.len() <= TRIAGE_HINT_MAX_BYTES, "too long: {hint}");
    }

    #[test]
    fn unreadable_odd_or_absent_meeting_files_never_break_the_hint() {
        let d = TempDir::new().unwrap();
        let layout = WikiLayout::new(d.path().to_path_buf());
        layout.bootstrap().unwrap();
        let dir = meetings_dir(&d);
        std::fs::create_dir(dir.join("2026-08-21-a-directory.md")).unwrap();
        std::fs::write(dir.join("index.md"), "Dana Reyes\n").unwrap();
        std::fs::write(dir.join("log.md"), "Dana Reyes\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "Dana Reyes\n").unwrap();
        // No `## ` section at all, and larger than the head cap.
        std::fs::write(
            dir.join("2026-08-20-huge-aaaa1111.md"),
            format!("# Huge\n\n{}\nDana Reyes\n", "é".repeat(9000)),
        )
        .unwrap();

        let e = email("Dana Reyes <dana@example.com>", None);
        // The OKF index/log and the non-markdown file are excluded, the huge
        // file is read only up to the byte cap, and nothing panics.
        let r = WikiReader::new(&layout).with_transcripts_dir(Some(dir));
        assert_eq!(r.draft_hint(&e), "");
        // Same for a configured clone that isn't there at all.
        let gone = WikiReader::new(&layout).with_transcripts_dir(Some(d.path().join("nope")));
        assert_eq!(gone.draft_hint(&e), "");
        assert_eq!(gone.triage_hint(&e), "");
    }
}
