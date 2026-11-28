//! #2610 — two ways one issue could take every tick.
//!
//! A red-main issue outlives the red `main` it was filed for: once main is
//! green on its required checks again, the issue (and its draft) describe a
//! failure that no longer exists, and working it can only burn ticks. And a
//! tick that ends in a refusal that does not change between ticks (an
//! unparsed owner criteria section, recovered edits under a guarded path)
//! used to hand the same issue straight back to the picker, about every 40
//! seconds, starving everything behind it.
use super::*;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Stale red-main issues
// ---------------------------------------------------------------------------

/// Set by `run_selected` once this run has observed `main` green on its
/// required hosted checks; reset at the start of every run. Process-global
/// for the same reason as `CURRENT_ISSUE`: #816's run lock allows one run.
static MAIN_GREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn note_main_green(green: bool) {
    MAIN_GREEN.store(green, std::sync::atomic::Ordering::Relaxed);
}

fn main_green() -> bool {
    MAIN_GREEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the picker must pass over this candidate: a red-main issue while
/// main is green describes a failure that is already gone.
pub(super) fn stale_red_main(body: &str, main_green: bool) -> bool {
    main_green && is_red_main_issue(body)
}

/// The picker's view: the current run's observation.
pub(super) fn skip_as_stale_red_main(body: &str) -> bool {
    stale_red_main(body, main_green())
}

/// Open auto-filed red-main issues in a REST open-issue listing. Pull
/// requests share the endpoint and are never issues.
pub(super) fn open_red_main_issues(rows: &serde_json::Value) -> Vec<u64> {
    rows.as_array().into_iter().flatten()
        .filter(|row| row.get("pull_request").is_none())
        .filter(|row| row["state"].as_str().is_none_or(|state| state.eq_ignore_ascii_case("open")))
        .filter(|row| row["body"].as_str().is_some_and(is_red_main_issue))
        .filter_map(|row| row["number"].as_u64())
        .collect()
}

pub(super) fn resolved_comment(main: Option<&str>) -> String {
    let at = main.map(|sha| format!(" at `{}`", sha.get(..12).unwrap_or(sha))).unwrap_or_default();
    format!("Autoship: `origin/main`{at} is green on its required GitHub Actions checks, so the red main this \
        issue was filed for is resolved. Closing it (and its draft fix) instead of working a failure that no \
        longer exists. A new red main files a new issue.")
}

/// The writes, separable from GitHub so the decision is testable.
#[async_trait::async_trait]
pub(super) trait StaleCloser: Send {
    /// The draft this loop opened for `issue`, if it recorded one.
    fn own_pr(&self, issue: u64) -> Option<u64>;
    async fn close_pr(&mut self, pr: u64, issue: u64, comment: &str) -> Result<()>;
    async fn close_issue(&mut self, issue: u64, comment: &str) -> Result<()>;
}

/// Close every listed red-main issue (draft first, so the issue never closes
/// while its PR is still resumable). Returns the issues actually closed; a
/// failure on one never stops the others, and the picker skips them anyway.
pub(super) async fn close_stale_with(closer: &mut impl StaleCloser, issues: &[u64], main: Option<&str>) -> Vec<u64> {
    let comment = resolved_comment(main);
    let mut closed = Vec::new();
    for &issue in issues {
        if let Some(pr) = closer.own_pr(issue) {
            if let Err(error) = closer.close_pr(pr, issue, &comment).await {
                warn!(issue, pr, "stale red-main draft not closed: {error:#}");
                continue;
            }
        }
        match closer.close_issue(issue, &comment).await {
            Ok(()) => {
                info!(issue, "main is green; closed stale red-main issue as resolved");
                closed.push(issue);
            }
            Err(error) => warn!(issue, "stale red-main issue not closed: {error:#}"),
        }
    }
    closed
}

struct GithubCloser<'a> { root: &'a Path }

#[async_trait::async_trait]
impl StaleCloser for GithubCloser<'_> {
    fn own_pr(&self, issue: u64) -> Option<u64> {
        opened_prs::get(issue).map(|record| record.pr)
    }
    async fn close_pr(&mut self, pr: u64, issue: u64, comment: &str) -> Result<()> {
        anyhow::ensure!(manual_routing::terminal_allowed(self.root, issue, Some(pr), true).await,
            "terminal controls or routing held before closing the draft");
        let (ok, _, error) = run(&gh_bin(), &["pr", "close", &pr.to_string(), "--comment", comment], self.root).await?;
        anyhow::ensure!(ok, "closing PR #{pr} failed: {}", truncate(&error, 300));
        Ok(())
    }
    async fn close_issue(&mut self, issue: u64, comment: &str) -> Result<()> {
        anyhow::ensure!(manual_routing::terminal_allowed(self.root, issue, None, true).await,
            "terminal controls or routing held before closing the issue");
        let (ok, _, error) = run(&gh_bin(),
            &["issue", "close", &issue.to_string(), "--reason", "completed", "--comment", comment], self.root).await?;
        anyhow::ensure!(ok, "closing issue #{issue} failed: {}", truncate(&error, 300));
        Ok(())
    }
}

/// Called once `main` is known green. Reads the tick-shared open-issue
/// inventory the picker reads next (no extra listing), and stands the writes
/// down when the REST budget is low (#2585): the picker still skips them.
pub(super) async fn close_stale_red_main(repo_root: &Path, dry_run: bool) {
    if dry_run {
        return;
    }
    let rows = match open_issue_rest_inventory(repo_root).await {
        Ok(rows) => rows,
        Err(error) => { warn!("stale red-main check deferred: {error:#}"); return; }
    };
    let stale = open_red_main_issues(&rows);
    if stale.is_empty() || gh_budget::low(repo_root, "stale red-main closure").await {
        return;
    }
    let main = origin_main_sha(repo_root).await;
    close_stale_with(&mut GithubCloser { root: repo_root }, &stale, main.as_deref()).await;
}

// ---------------------------------------------------------------------------
// Deterministic refusals back off
// ---------------------------------------------------------------------------

/// Refusals that cannot change between ticks unless a human edits something.
const KNOWN_REFUSALS: &[&str] = &[
    "owner acceptance criteria contain an unparsed line",
    "owner acceptance criteria exceed bounded contract capacity",
    "owner acceptance criteria encoding",
    "recovered edits touch a guarded path",
    "recovered edits retain conflict markers",
    "integration resolution touches a guarded path",
];
const BASE_BACKOFF_MS: i64 = 30 * 60 * 1000;
const MAX_BACKOFF_MS: i64 = 8 * 60 * 60 * 1000;
/// Refusals before the issue is parked out of the pool.
pub(super) const PARK_AFTER: u32 = 3;
/// A refusal this old no longer counts toward parking.
const FORGET_AFTER_MS: i64 = 24 * 60 * 60 * 1000;
pub(super) const GIVE_UP_REPEATED_REFUSAL: &str = "held:repeated-refusal";

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Entry {
    pub count: u32,
    pub last_at_ms: i64,
    pub until_ms: i64,
    pub fingerprint: String,
    pub reason: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Backoff {
    issues: BTreeMap<u64, Entry>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    /// Not (yet) known to be deterministic: retry on the ordinary cadence.
    Retry,
    BackOff { until_ms: i64, count: u32 },
    Park { reason: String, count: u32 },
}

/// What the error says, with run-specific numbers masked, so the same
/// refusal on a later tick has the same fingerprint.
fn fingerprint(text: &str) -> String {
    text.chars().take(240).map(|c| if c.is_ascii_digit() { '#' } else { c }).collect()
}

impl Backoff {
    pub(super) fn holds(&self, issue: u64, now_ms: i64) -> bool {
        self.issues.get(&issue).is_some_and(|entry| entry.until_ms > now_ms)
    }

    pub(super) fn entry(&self, issue: u64) -> Option<&Entry> {
        self.issues.get(&issue)
    }

    /// Record a failed tick against `issue`. A known refusal backs off at
    /// once; any other failure backs off when the same failure repeats.
    pub(super) fn record(&mut self, issue: u64, error_text: &str, now_ms: i64) -> Decision {
        let print = fingerprint(error_text);
        let known = KNOWN_REFUSALS.iter().find(|refusal| error_text.contains(**refusal));
        let entry = self.issues.entry(issue).or_default();
        if now_ms.saturating_sub(entry.last_at_ms) > FORGET_AFTER_MS {
            *entry = Entry::default();
        }
        let repeated = !entry.fingerprint.is_empty() && entry.fingerprint == print;
        entry.fingerprint = print;
        entry.last_at_ms = now_ms;
        if known.is_none() && !repeated {
            return Decision::Retry;
        }
        entry.count = entry.count.saturating_add(1);
        entry.reason = known.map_or_else(|| truncate(error_text, 300), |refusal| (*refusal).to_string());
        let wait = BASE_BACKOFF_MS.saturating_mul(1_i64 << (entry.count - 1).min(16)).min(MAX_BACKOFF_MS);
        entry.until_ms = now_ms.saturating_add(wait);
        if entry.count >= PARK_AFTER {
            return Decision::Park { reason: entry.reason.clone(), count: entry.count };
        }
        Decision::BackOff { until_ms: entry.until_ms, count: entry.count }
    }

    /// Progress (a billed outcome) forgets the issue's refusals.
    pub(super) fn clear(&mut self, issue: u64) -> bool {
        self.issues.remove(&issue).is_some()
    }
}

/// `AUGMENTAGENT_AUTOPR_REFUSAL_BACKOFF_FILE` override (tests), else the
/// daemon state dir next to the attempt ledger.
pub(super) fn path() -> PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_AUTOPR_REFUSAL_BACKOFF_FILE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    augmentagent_channel_core::state_dir::state_dir()
        .map(|dir| dir.join("autopr-refusal-backoff.json"))
        .unwrap_or_else(|| PathBuf::from("autopr-refusal-backoff.json"))
}

/// Unreadable or corrupt state is empty: backoff only ever withholds work.
pub(super) fn load(path: &Path) -> Backoff {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Read-modify-write under a sibling flock; the resume and build lanes are
/// separate processes. The write is a rename, so a reader never sees half.
pub(super) fn update<T>(path: &Path, edit: impl FnOnce(&mut Backoff) -> T) -> Result<T> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true)
        .open(path.with_extension("lock"))?;
    // SAFETY: `lock` owns the fd; flock neither reads nor writes it.
    let rc = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    anyhow::ensure!(rc == 0, "refusal backoff lock failed");
    let mut state = load(path);
    let out = edit(&mut state);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&state)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(out)
}

/// Whether `issue` is inside a refusal backoff right now.
pub(super) fn holds(issue: u64, now_ms: i64) -> bool {
    load(&path()).holds(issue, now_ms)
}

/// The loop's handling of a failed tick attributed to `issue`. Errors that
/// are holds rather than refusals (another PR claims the issue) are not
/// counted: they clear on their own.
pub(super) async fn record_failed_tick(repo_root: &Path, store: &Path, issue: u64, error: &anyhow::Error, now_ms: i64) {
    if issue == 0 || error.downcast_ref::<linked_claims::Hold>().is_some() {
        return;
    }
    let text = format!("{error:#}");
    let decision = match update(store, |state| state.record(issue, &text, now_ms)) {
        Ok(decision) => decision,
        Err(error) => { warn!(issue, "refusal backoff not recorded: {error:#}"); return; }
    };
    match decision {
        Decision::Retry => {}
        Decision::BackOff { until_ms, count } => {
            let until = chrono::DateTime::from_timestamp_millis(until_ms)
                .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_default();
            warn!(issue, count, %until, "deterministic refusal; issue backs off before it can be selected again");
        }
        Decision::Park { reason, count } => {
            let detail = format!("The last {count} attempts on this issue ended in the same refusal, which does not \
                change between attempts:\n\n> {reason}\n\nIt needs an owner: fix the cause (the issue, its draft PR or \
                its criteria), then put it back in the pool.");
            match label_gave_up(repo_root, issue, GIVE_UP_REPEATED_REFUSAL, true, None, GaveUpNote::Detailed(&detail)).await {
                Ok(()) => warn!(issue, count, %reason, "repeated refusal; issue parked for its owner"),
                Err(error) => warn!(issue, "repeated refusal; parking failed, backoff still holds: {error:#}"),
            }
        }
    }
}

/// A billed outcome is progress: the issue's refusal history no longer applies.
pub(super) fn record_progress(store: &Path, issue: u64) {
    if issue == 0 {
        return;
    }
    if let Err(error) = update(store, |state| state.clear(issue)) {
        warn!(issue, "refusal backoff not cleared: {error:#}");
    }
}

pub(super) fn current_issue() -> u64 {
    CURRENT_ISSUE.load(std::sync::atomic::Ordering::Relaxed)
}
