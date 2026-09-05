//! Hint strings for drafting-time wiki navigation.
//!
//! The drafting Claude call gets `--add-dir wiki/` + Read/Grep/Glob tools, so
//! it can open any page it wants. `WikiReader` doesn't do the reading — it
//! just produces a short hint string that points at likely-relevant pages, so
//! Claude doesn't have to guess file paths.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use augmentagent_store::Email;

use crate::layout::WikiLayout;

/// How many of the newest meeting files a hint scan reads. FlyOnTheWall names
/// them `YYYY-MM-DD-{slug}-{id8}.md`, so reverse-lexicographic is newest-first.
const MEETING_SCAN_LIMIT: usize = 20;
/// How long one clone scan is reused process-wide (see [`memoized_meetings`]).
const MEETING_SCAN_TTL: Duration = Duration::from_secs(60);
/// Most meeting paths a draft hint names.
const MEETING_HINT_MAX: usize = 3;
/// The title and summary sit right below the frontmatter; the transcript
/// underneath is megabytes, so the cap is on the read, not just on the match.
const MEETING_HEAD_BYTES: u64 = 8192;
/// Ceilings on the FINISHED hint, not just on the block #921 appends: meeting
/// paths are absolute under a host-configured dir, so nothing here can assume
/// they are short. The pre-#921 400-byte rule (`triage_hint_stays_short`) is a
/// *triage* invariant and triage keeps it exactly; it cannot also be the draft
/// ceiling, because the draft hint's own boilerplate already spends most of 400
/// before a page is named (`draft_hint_is_bounded` pins that), so reusing it
/// there would silently disable #921 in the common case. Draft gets headroom.
const TRIAGE_HINT_MAX_BYTES: usize = 400;
const DRAFT_HINT_MAX_BYTES: usize = TRIAGE_HINT_MAX_BYTES + 200;
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
        // the wiki prose and the blank line joining them are paid for.
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
        // alone, so the transcript is a recency signal, not something this call
        // can read. Dropped outright when it would breach the budget.
        if let Some(path) = self.meetings_mentioning(email, 1).into_iter().next() {
            let line = format!("- Sender appeared in a recent meeting transcript: {path}");
            let used: usize = lines.iter().map(|l| l.len() + 1).sum();
            if used + line.len() < TRIAGE_HINT_MAX_BYTES {
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
    /// names the sender. Empty with no clone configured, no display name, or no
    /// match — which keeps the hints byte-identical to their pre-#921 text in
    /// the common case. Inside a TTL this is a memo lookup and a substring
    /// compare, no syscall at all (see [`memoized_meetings`]); I/O failure on a
    /// refresh degrades to no hint, never to a failed hint.
    fn meetings_mentioning(&self, email: &Email, max: usize) -> Vec<String> {
        let Some(dir) = self.transcripts_meetings_dir.as_deref() else {
            return Vec::new();
        };
        let Some(name) = display_name(&email.from) else {
            return Vec::new();
        };
        let needle = name.to_lowercase();
        // An initials-or-junk substring would match half the clone.
        if needle.chars().count() < 3 {
            return Vec::new();
        }

        let mut hits = Vec::new();
        for (path, head) in memoized_meetings(&MEETING_SCAN_MEMO, dir).iter() {
            if head.contains(&needle) {
                hits.push(path.to_string_lossy().into_owned());
                if hits.len() == max {
                    break;
                }
            }
        }
        hits
    }
}

/// `AUGMENTAGENT_TRANSCRIPTS_DIR/meetings`, path arithmetic only: callers
/// resolve this per email, and a clone that isn't there — the normal state on a
/// host without FlyOnTheWall — is already absorbed by the memoised scan.
pub fn transcripts_meetings_dir_from_env() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("AUGMENTAGENT_TRANSCRIPTS_DIR")?).join("meetings"))
}

/// `Display Name <addr>` → the display name, the only thing worth scanning for.
fn display_name(from: &str) -> Option<&str> {
    let (name, _) = from.split_once('<')?;
    let name = name.trim().trim_matches('"').trim();
    (!name.is_empty()).then_some(name)
}

/// `Arc` so a cache hit is a refcount bump, not a copy of every head.
type Meetings = Arc<[(PathBuf, String)]>;
type ScanMemo = Mutex<Option<(PathBuf, Instant, Meetings)>>;
static MEETING_SCAN_MEMO: ScanMemo = Mutex::new(None);

/// The scan window *and its heads*, re-derived at most once per
/// [`MEETING_SCAN_TTL`]. Hints run per email on the unattended inbox, so
/// uncached each message would pay a whole-clone `read_dir` plus
/// [`MEETING_SCAN_LIMIT`] head reads — latency a slow configured mount cannot
/// afford. Caching the heads, not just the paths, is what keeps the hot path
/// free of syscalls; the cost is a just-synced meeting missing one TTL of
/// hints, and a hint is advisory. The memo is `try_lock`ed and never held
/// across the I/O, so a hung mount stalls only the email that triggered the
/// refresh: a concurrent hint takes the miss and scans on its own thread.
fn memoized_meetings(memo: &ScanMemo, dir: &Path) -> Meetings {
    if let Ok(state) = memo.try_lock() {
        if let Some((cached, taken, meetings)) = state.as_ref() {
            if cached == dir && taken.elapsed() < MEETING_SCAN_TTL {
                return Arc::clone(meetings);
            }
        }
    }
    let meetings: Meetings = scan_meeting_files(dir)
        .into_iter()
        .filter_map(|p| meeting_head(&p).map(|head| (p, head)))
        .collect();
    if let Ok(mut state) = memo.try_lock() {
        *state = Some((dir.to_path_buf(), Instant::now(), Arc::clone(&meetings)));
    }
    meetings
}

/// The newest [`MEETING_SCAN_LIMIT`] transcript files, newest first. The window
/// is kept by insertion during a single `read_dir` pass: no vector of every
/// entry, no sort, and the cheap name test runs before the `file_type` syscall.
fn scan_meeting_files(dir: &Path) -> Vec<PathBuf> {
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

/// `YYYY-MM-DD-{slug}-{id8}.md`, the shape FlyOnTheWall exports. The date
/// prefix is what makes the lexicographic window newest-first: an undated stray
/// (the OKF `index.md` / `log.md`) sorts above every dated name.
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
/// stays inside `budget` bytes. A path too long to name is simply not named.
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

/// Lowercased `# Title` + summary block — everything above the first `## `
/// section. Stopping there is the point: a name spoken in `## Transcript` is
/// not evidence the meeting was *about* that person. Unreadable → `None`.
fn meeting_head(path: &Path) -> Option<String> {
    use std::io::Read;

    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MEETING_HEAD_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    // Lossy: the cap can land mid-codepoint, and a replacement char cannot
    // create a false match on a name.
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

    /// The FlyOnTheWall export shape: frontmatter, `# Title`, summary prose,
    /// then the sections the hint scan must never read.
    fn meeting_file(dir: &Path, name: &str, title: &str, summary: &str, transcript: &str) {
        let body = format!(
            "---\nid: \"{name}\"\ntype: meeting\n---\n\n# {title}\n\n{summary}\n\n## Transcript\n\n{transcript}\n"
        );
        std::fs::write(dir.join(name), body).unwrap();
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
        // A bare address carries no display name to scan the clone for.
        assert_eq!(r.draft_hint(&email("dana@example.com", None)), "");
    }

    /// PR review of #921 — the ceiling is on the FINISHED hint, not just on the
    /// block this change appends: a person page, a thread page AND matching
    /// transcripts together must still land inside the budget.
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
        assert!(hint.len() <= DRAFT_HINT_MAX_BYTES, "too long: {hint}");
        // PR review of #921 asked for triage's 400 here too; this is why it
        // cannot be. The untouched pre-#921 prose already spends nearly all of
        // it, so a 400 ceiling would name no meeting at all in the common case.
        let floor = TRIAGE_HINT_MAX_BYTES - MEETING_HINT_HEADER.len();
        assert!(before.len() > floor, "draft prose shrank: {before}");
        let paths: Vec<&str> = hint.lines().filter(|l| l.contains("-sync-")).collect();
        // 40 candidates, at most three named — fewer on a long temp root.
        assert!(
            (1..=MEETING_HINT_MAX).contains(&paths.len()),
            "expected 1..=3 paths: {paths:?}"
        );
        let p = paths[0].trim_start_matches("- ");
        assert!(Path::new(p).is_absolute(), "path must be absolute: {p}");
    }

    /// PR review of #921 — the caller's remaining budget is enforced on the
    /// real output, path lengths included.
    #[test]
    fn meeting_block_never_exceeds_the_callers_budget() {
        let paths = |root: &str, n: usize| -> Vec<String> {
            (0..n)
                .map(|i| format!("{root}/2026-08-2{i}-weekly-sync-aaaa111{i}.md"))
                .collect()
        };
        let budget = 400;
        let short = meeting_block(&paths("/home/o/transcripts", MEETING_HINT_MAX), budget);
        assert_eq!(short.matches("\n- ").count(), MEETING_HINT_MAX);
        assert!(short.len() <= budget, "{short}");
        // A deep clone root: entries that long fit once, not three times.
        let deep = meeting_block(&paths(&"/dddd".repeat(30), MEETING_HINT_MAX), budget);
        assert_eq!(deep.matches("\n- ").count(), 1, "{deep}");
        assert!(deep.len() <= budget, "{deep}");
        // Longer than the whole budget: no header, no dangling bullet. And a
        // caller with nothing left to spend gets nothing.
        assert_eq!(meeting_block(&paths(&"d".repeat(500), 1), budget), "");
        assert_eq!(meeting_block(&paths("/o", MEETING_HINT_MAX), 0), "");
    }

    /// PR review of #921 — same discipline on the triage side, where the wiki
    /// lines have already spent part of the budget: a deep clone's path is
    /// dropped rather than allowed to push the hint over.
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
        assert!(hint.len() <= TRIAGE_HINT_MAX_BYTES, "too long: {hint}");
    }

    /// PR review of #921 — the scan never materialises or sorts the whole
    /// directory: only the newest [`MEETING_SCAN_LIMIT`] *dated* names survive,
    /// so undated strays cannot evict real meetings.
    #[test]
    fn the_scan_window_is_bounded_and_newest_first() {
        let d = TempDir::new().unwrap();
        let dir = meetings_dir(&d);
        for i in 0..120 {
            let name = format!("2026-{:02}-{:02}-sync-a.md", i / 28 + 1, i % 28 + 1);
            std::fs::write(dir.join(name), "x").unwrap();
            std::fs::write(dir.join(format!("zzz-{i:03}.md")), "x").unwrap();
        }
        let names: Vec<String> = scan_meeting_files(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), MEETING_SCAN_LIMIT, "scan window is unbounded");
        let mut sorted = names.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(names, sorted, "window is not newest-first: {names:?}");
        assert_eq!(names[0], "2026-05-08-sync-a.md", "newest meeting missed");
    }

    /// PR review of #921 — a hint on the unattended inbox's hot path must not
    /// touch the clone at all: neither the `read_dir` nor the per-candidate head
    /// reads may run per email, so BOTH are derived once per TTL. Memo injected
    /// here; production has one, static.
    #[test]
    fn clone_discovery_and_heads_are_memoized_across_hints() {
        let d = TempDir::new().unwrap();
        let dir = meetings_dir(&d);
        std::fs::write(dir.join("2026-08-20-sync-aaaa1111.md"), "# dana reyes").unwrap();
        let memo = ScanMemo::new(None);
        let first = memoized_meetings(&memo, &dir);
        assert_eq!(first.len(), 1);
        // A new meeting AND a rewritten head are both invisible until expiry.
        std::fs::write(dir.join("2026-08-21-sync-bbbb2222.md"), "x").unwrap();
        std::fs::write(dir.join("2026-08-20-sync-aaaa1111.md"), "# gone").unwrap();
        let cached = memoized_meetings(&memo, &dir);
        assert_eq!(cached.len(), 1, "clone re-enumerated");
        assert_eq!(cached[0].1, "# dana reyes", "head re-read on the hot path");

        // Expiry, and a different clone, both re-scan.
        *memo.lock().unwrap() = None;
        assert_eq!(memoized_meetings(&memo, &dir).len(), 2, "never refreshes");
        assert!(memoized_meetings(&memo, d.path()).is_empty(), "wrong clone");
    }

    /// PR review of #921 — no hint may wait on another hint's clone I/O. A
    /// refresh in flight (stood in for by a held memo) costs the next email a
    /// private scan, not a queue: holding the lock across `read_dir` would park
    /// every concurrent triage behind one hung mount.
    #[test]
    fn a_refresh_in_flight_never_blocks_another_hint() {
        let d = TempDir::new().unwrap();
        let dir = meetings_dir(&d);
        std::fs::write(dir.join("2026-08-20-sync-aaaa1111.md"), "x").unwrap();
        let memo = ScanMemo::new(None);
        let held = memo.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| tx.send(memoized_meetings(&memo, &dir).len()).unwrap());
            let got = rx.recv_timeout(Duration::from_secs(10));
            drop(held);
            assert_eq!(got, Ok(1), "hint queued behind an in-flight refresh");
        });
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
        // OKF sidecars and non-markdown are excluded, the huge file is read
        // only up to the byte cap, and nothing panics.
        let r = WikiReader::new(&layout).with_transcripts_dir(Some(dir));
        assert_eq!(r.draft_hint(&e), "");
        // Same for a configured clone that isn't there at all.
        let gone = WikiReader::new(&layout).with_transcripts_dir(Some(d.path().join("nope")));
        assert_eq!(gone.draft_hint(&e), "");
        assert_eq!(gone.triage_hint(&e), "");
    }
}
