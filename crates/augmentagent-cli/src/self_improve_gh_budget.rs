//! #2444 — GitHub request budget for the auto-ship worker.
//!
//! The worker shares one 5,000/hour REST core budget with the merge train and
//! every other agent. Reads that do not change within a tick are made once per
//! tick (and expire after [`TICK_TTL`]); the repository identity is read once
//! per process. Any GitHub write clears the tick's reads, so a decision made
//! after a mutation always sees fresh state. Mutation-time routing and control
//! checks never use these reads; they observe GitHub themselves.
use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A tick can chain long builds. Reads older than this are fetched again.
const TICK_TTL: Duration = Duration::from_secs(60);
/// Below this many remaining REST core requests, optional polling stops.
const DEFAULT_FLOOR: u64 = 1_500;
const FLOOR_ENV: &str = "AUGMENTAGENT_AUTOPR_GH_BUDGET_FLOOR";
/// `GET /rate_limit` is free, but there is no reason to ask more often.
const RATE_LIMIT_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct TickReads {
    entries: Mutex<HashMap<String, (Instant, String)>>,
}

tokio::task_local! {
    static TICK: Arc<TickReads>;
}

/// Run one worker tick (builder tick or reconciliation sweep) with shared
/// reads. Outside a tick scope every read goes to GitHub, as before.
pub(super) async fn tick_scope<F: std::future::Future>(future: F) -> F::Output {
    TICK.scope(Arc::new(TickReads::default()), future).await
}

/// A read made at most once per tick for `key`. Failures are never kept.
pub(super) async fn tick_read<F, Fut>(key: String, fetch: F) -> Result<String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let tick = TICK.try_with(Arc::clone).ok();
    if let Some(tick) = &tick {
        let entries = tick.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((at, value)) = entries.get(&key) {
            if at.elapsed() < TICK_TTL {
                return Ok(value.clone());
            }
        }
    }
    let value = fetch().await?;
    if let Some(tick) = tick {
        tick.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, (Instant::now(), value.clone()));
    }
    Ok(value)
}

/// GitHub reads that cannot change GitHub state. Everything else (including
/// any `gh api` call with a method or fields) counts as a write.
pub(super) fn is_read(args: &[&str]) -> bool {
    match args {
        ["pr", "list" | "view" | "diff" | "checks" | "status", ..]
        | ["issue", "list" | "view" | "status", ..]
        | ["repo", "view", ..]
        | ["label", "list", ..]
        | ["auth", "status" | "token", ..] => true,
        ["api", rest @ ..] => {
            let mut rest = rest.iter();
            while let Some(arg) = rest.next() {
                match *arg {
                    "-X" | "--method" => {
                        if !rest.next().is_some_and(|m| m.eq_ignore_ascii_case("GET")) {
                            return false;
                        }
                    }
                    "-f" | "-F" | "--field" | "--raw-field" | "--input" => return false,
                    _ if arg.starts_with("--method=") || (arg.starts_with("-X") && arg.len() > 2) => {
                        if !arg.rsplit(['=', 'X']).next().is_some_and(|m| m.eq_ignore_ascii_case("GET")) {
                            return false;
                        }
                    }
                    _ if arg.starts_with("--field=") || arg.starts_with("--raw-field=") || arg.starts_with("--input=") => return false,
                    _ => {}
                }
            }
            true
        }
        _ => false,
    }
}

/// Called by the command runner after every GitHub command.
pub(super) fn observed(cmd: &str, args: &[&str]) {
    if cmd == gh_bin() && !is_read(args) {
        let _ = TICK.try_with(|tick| tick.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear());
    }
}

/// `gh repo view --json nameWithOwner` cannot change while the process runs.
static REPOSITORY: Mutex<Option<HashMap<(String, PathBuf), String>>> = Mutex::new(None);

pub(super) async fn repository_name<F, Fut>(root: &Path, fetch: F) -> Result<String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let key = (gh_bin(), root.to_path_buf());
    if let Some(name) = REPOSITORY.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref().and_then(|names| names.get(&key)) {
        return Ok(name.clone());
    }
    let name = fetch().await?;
    REPOSITORY.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashMap::new).insert(key, name.clone());
    Ok(name)
}

/// Source-issue labels for routing, from one open-issue inventory per tick.
/// `None` for an issue this inventory does not contain (closed, or newer than
/// the inventory); callers read that issue directly.
pub(super) struct OpenIssues(HashMap<u64, serde_json::Value>);
impl OpenIssues {
    pub(super) async fn read(root: &Path, repository: &str) -> Option<Self> {
        // #2512: title and body ride the same listing so a rescope stop can
        // see an issue edit without another read.
        let raw = complete_repository_inventory(root, "issue", Some(repository), "number,labels,state,title,body").await.ok()?;
        Self::from_rows(serde_json::from_str(&raw).ok()?)
    }
    pub(super) fn from_rows(rows: Vec<serde_json::Value>) -> Option<Self> {
        let mut issues = HashMap::new();
        for row in rows {
            let number = row["number"].as_u64().filter(|n| *n > 0)?;
            issues.insert(number, row);
        }
        Some(Self(issues))
    }
    pub(super) fn get(&self, issue: u64) -> Option<&serde_json::Value> {
        self.0.get(&issue)
    }
    /// The issue's request hash as an own-PR rescope stop records it (#2512).
    /// `None` when the listing lacks the issue or its text.
    pub(super) fn request(&self, issue: u64) -> Option<String> {
        let row = self.get(issue)?;
        let title = row.get("title")?.as_str()?;
        let body = match row.get("body")? { serde_json::Value::Null => "", body => body.as_str()? };
        Some(acceptance::own_pr_rescope::request_digest(title, body))
    }
    /// Listed with an open state in this tick's inventory.
    pub(super) fn is_open(&self, issue: u64) -> bool {
        self.get(issue).and_then(|row| row["state"].as_str()).is_some_and(|state| state.eq_ignore_ascii_case("open"))
    }
}

pub(super) fn floor() -> u64 {
    std::env::var(FLOOR_ENV).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_FLOOR)
}

static REMAINING: Mutex<Option<(Instant, Option<u64>)>> = Mutex::new(None);

/// Remaining REST core requests, from `gh api rate_limit` at most once a
/// minute. That endpoint does not count against the limit. Unknown is `None`.
async fn core_remaining(root: &Path) -> Option<u64> {
    if let Some((at, remaining)) = *REMAINING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
        if at.elapsed() < RATE_LIMIT_TTL {
            return remaining;
        }
    }
    let remaining = match run(&gh_bin(), &["api", "rate_limit", "--jq", ".resources.core.remaining"], root).await {
        Ok((true, out, _)) => out.trim().parse().ok(),
        _ => None,
    };
    *REMAINING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((Instant::now(), remaining));
    remaining
}

/// Ratebudget: requests kept back for the merge path (the sweep's hosted CI
/// read, its pre-merge re-read, and the merge itself). Below this, optional
/// hosted CI reads (CI repair diagnosis) defer instead of spending them.
const DEFAULT_MERGE_RESERVE: u64 = 400;
const MERGE_RESERVE_ENV: &str = "AUGMENTAGENT_AUTOPR_GH_MERGE_RESERVE";

fn merge_reserve() -> u64 {
    std::env::var(MERGE_RESERVE_ENV).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_MERGE_RESERVE)
}

/// Reason an optional read gives when it stands down for the merge reserve.
/// Callers treat it like any unavailable evidence: deferred, never red.
pub(super) const RESERVED: &str = "GitHub REST budget reserved for merges; evidence deferred";

/// Remaining REST core requests (cached for a minute), `None` when unknown.
pub(super) async fn remaining(root: &Path) -> Option<u64> {
    core_remaining(root).await
}

/// Run an optional GitHub read unless the known budget is inside the merge
/// reserve. `None` means deferred; the caller must not treat it as a fact.
pub(super) async fn unless_reserved<T, F, Fut>(remaining: Option<u64>, what: &str, read: F) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let reserve = merge_reserve();
    if below_floor(remaining, reserve) {
        warn!(remaining = remaining.unwrap_or_default(), reserve, what,
            "GitHub REST budget inside the merge reserve; deferring optional read");
        return None;
    }
    Some(read().await)
}

/// How long a not-green sweep verdict may stand without a fresh read, while
/// the PR's head, base and check rollup are unchanged. Doubled-up on a low
/// budget: the fingerprint still forces a re-read the moment anything moves.
const VERDICT_TTL: Duration = Duration::from_secs(15 * 60);
const LOW_BUDGET_VERDICT_TTL: Duration = Duration::from_secs(60 * 60);

/// A reused verdict is keyed by the GitHub identity it was read from, as the
/// repository name is: one `gh` endpoint, one repository, one PR. A PR number
/// alone is not an identity — the sweep examines adopted PRs in repositories
/// other than the checkout's, and PR numbers repeat across them.
type VerdictKey = (String, String, u64);
static SWEEP_VERDICTS: Mutex<Option<HashMap<VerdictKey, (String, Instant)>>> = Mutex::new(None);

/// The CI-relevant state of a PR row from the shared inventory: head, base
/// and every check's status and conclusion. `None` when any is missing, in
/// which case nothing is reused.
pub(super) fn ci_fingerprint(row: &serde_json::Value) -> Option<String> {
    use sha2::{Digest, Sha256};
    let head = row.get("headRefOid")?.as_str().filter(|s| !s.is_empty())?;
    let base = row.get("baseRefOid")?.as_str().filter(|s| !s.is_empty())?;
    let rollup = row.get("statusCheckRollup")?;
    rollup.as_array()?;
    Some(format!("{:x}", Sha256::digest(format!("{head}\0{base}\0{rollup}").as_bytes())))
}

/// Ratebudget: the merge sweep re-reads hosted CI (dozens of REST calls) for
/// every approved PR every pass. A PR that was not green and whose head,
/// base and checks have not changed since cannot have turned green, so its
/// verdict stands without a read. Only `Some(false)` is ever kept: a green
/// verdict is always read fresh (and re-read before merge), and an unknown
/// or deferred read (`None`, e.g. a spent quota) is never recorded as a fact.
pub(super) async fn sweep_checks<F, Fut>(repository: &str, pr: u64, fingerprint: Option<String>, low: bool, read: F) -> Option<bool>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<bool>>,
{
    let key: VerdictKey = (gh_bin(), repository.to_owned(), pr);
    let ttl = if low { LOW_BUDGET_VERDICT_TTL } else { VERDICT_TTL };
    if let Some(fingerprint) = &fingerprint {
        let verdicts = SWEEP_VERDICTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((seen, at)) = verdicts.as_ref().and_then(|v| v.get(&key)) {
            if seen == fingerprint && at.elapsed() < ttl {
                info!(pr, repository, "merge sweep: CI unchanged since a not-green read; skipping hosted CI read");
                return Some(false);
            }
        }
    }
    let verdict = read().await;
    let mut verdicts = SWEEP_VERDICTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let verdicts = verdicts.get_or_insert_with(HashMap::new);
    match (verdict, fingerprint) {
        (Some(false), Some(fingerprint)) => {
            if verdicts.len() >= 1_024 { verdicts.clear(); }
            verdicts.insert(key, (fingerprint, Instant::now()));
        }
        _ => { verdicts.remove(&key); }
    }
    verdict
}

pub(super) fn below_floor(remaining: Option<u64>, floor: u64) -> bool {
    remaining.is_some_and(|remaining| remaining < floor)
}

/// Whether optional polling should stand down this tick. An unreadable budget
/// is not low: required work never waits on this guard.
pub(super) async fn low(root: &Path, skipped: &str) -> bool {
    let (remaining, floor) = (core_remaining(root).await, floor());
    let low = below_floor(remaining, floor);
    if low {
        warn!(remaining = remaining.unwrap_or_default(), floor, skipped,
            "GitHub REST budget low; skipping optional auto-ship polling this tick");
    }
    low
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_reads_keep_tick_reads() {
        for read in [
            &["pr", "list", "--state", "open"][..], &["pr", "view", "9", "--json", "labels"],
            &["issue", "list"], &["repo", "view", "--json", "nameWithOwner"],
            &["api", "repos/owner/repo/issues/7"], &["api", "-X", "GET", "repos/owner/repo/pulls"],
            &["api", "rate_limit", "--jq", ".resources.core.remaining"],
        ] {
            assert!(is_read(read), "{read:?}");
        }
        for write in [
            &["pr", "merge", "9"][..], &["pr", "ready", "9", "--undo"], &["pr", "close", "9"],
            &["pr", "comment", "9", "--body", "x"], &["issue", "edit", "7", "--add-label", "x"],
            &["issue", "comment", "7"], &["api", "-X", "PATCH", "repos/owner/repo/issues/7"],
            &["api", "--method=POST", "repos/owner/repo/issues"], &["api", "graphql", "-f", "query=mutation"],
            &["api", "repos/owner/repo/issues/7/labels", "--input", "-"], &["unknown"],
        ] {
            assert!(!is_read(write), "{write:?}");
        }
    }

    #[test]
    fn budget_floor_skips_only_known_low_budgets() {
        assert!(below_floor(Some(712), 1_500));
        assert!(!below_floor(Some(1_500), 1_500));
        assert!(!below_floor(None, 1_500), "an unreadable budget never stops polling");
    }

    const REPO: &str = "owner/repo";

    fn row(head: &str, check: &str) -> serde_json::Value {
        serde_json::json!({"headRefOid": head, "baseRefOid": "b".repeat(40),
            "statusCheckRollup": [{"name": "rust", "status": check, "conclusion": null}]})
    }

    /// The verdict reuse is only worth what the row it fingerprints carries.
    /// The sweep asked GitHub for an inventory without `baseRefOid` or
    /// `statusCheckRollup`, so `ci_fingerprint` was always `None`, every
    /// examined PR paid its full hosted CI read again, and the sweep kept
    /// draining the shared REST core budget it is meant to preserve.
    #[test]
    fn the_sweep_asks_github_for_every_field_the_fingerprint_reads() {
        let src = include_str!("self_improve.rs");
        let start = src.find("async fn merge_sweep_selected(").expect("the sweep");
        let sweep = &src[start..start + src[start..].find("\n}\n").expect("end")];
        let lists: Vec<&str> = sweep.match_indices("\"number,")
            .map(|(at, _)| sweep[at + 1..].split('"').next().expect("a field list"))
            .collect();
        assert_eq!(lists.len(), 2, "the selected PR read and the repository inventory");
        for list in lists {
            let fields: Vec<&str> = list.split(',').collect();
            for field in ["headRefOid", "baseRefOid", "statusCheckRollup"] {
                assert!(fields.contains(&field), "the fingerprint reads {field}, absent from {list}");
            }
        }
        assert!(ci_fingerprint(&serde_json::json!({"headRefOid": "a", "baseRefOid": "b",
            "statusCheckRollup": []})).is_some(), "and those are the fields it reads");
    }

    #[tokio::test]
    async fn unchanged_not_green_pr_skips_the_hosted_read() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = AtomicUsize::new(0);
        let read = |verdict: Option<bool>| { let reads = &reads; move || async move { reads.fetch_add(1, Ordering::SeqCst); verdict } };
        let pending = ci_fingerprint(&row(&"a".repeat(40), "IN_PROGRESS"));
        assert_eq!(sweep_checks(REPO, 990_001, pending.clone(), false, read(Some(false))).await, Some(false));
        assert_eq!(sweep_checks(REPO, 990_001, pending.clone(), false, read(Some(true))).await, Some(false), "cache hit");
        assert_eq!(reads.load(Ordering::SeqCst), 1, "an unchanged pending PR costs no hosted read");
        // Any change in checks or head forces a fresh read.
        let done = ci_fingerprint(&row(&"a".repeat(40), "COMPLETED"));
        assert_eq!(sweep_checks(REPO, 990_001, done.clone(), false, read(Some(true))).await, Some(true));
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        // Green is never reused.
        assert_eq!(sweep_checks(REPO, 990_001, done.clone(), false, read(Some(true))).await, Some(true));
        assert_eq!(reads.load(Ordering::SeqCst), 3, "a green verdict is always read fresh");
        // A deferred (rate-limited) read is never recorded as a fact.
        let other = ci_fingerprint(&row(&"c".repeat(40), "IN_PROGRESS"));
        assert_eq!(sweep_checks(REPO, 990_002, other.clone(), false, read(None)).await, None);
        assert_eq!(sweep_checks(REPO, 990_002, other.clone(), false, read(None)).await, None);
        assert_eq!(reads.load(Ordering::SeqCst), 5, "unknown is re-read, not cached as red");
        // Without a fingerprint nothing is reused.
        sweep_checks(REPO, 990_003, None, false, read(Some(false))).await;
        sweep_checks(REPO, 990_003, None, false, read(Some(false))).await;
        assert_eq!(reads.load(Ordering::SeqCst), 7);
        assert_eq!(ci_fingerprint(&serde_json::json!({"headRefOid": "a"})), None);
    }

    /// A reused verdict is a fact about one pull request, and a PR number is
    /// only a pull request within one repository. Keyed by the number alone,
    /// the not-green verdict of one repository's PR answered for another
    /// repository's PR of the same number: the sweep skipped the hosted read
    /// that would have found it green, and held a mergeable PR for the TTL.
    #[tokio::test]
    async fn a_not_green_verdict_never_answers_for_another_repository() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = AtomicUsize::new(0);
        let read = |verdict: Option<bool>| { let reads = &reads; move || async move { reads.fetch_add(1, Ordering::SeqCst); verdict } };
        let pending = ci_fingerprint(&row(&"d".repeat(40), "IN_PROGRESS"));
        assert_eq!(sweep_checks("owner/alpha", 991_001, pending.clone(), false, read(Some(false))).await, Some(false));
        assert_eq!(sweep_checks("owner/beta", 991_001, pending.clone(), false, read(Some(true))).await, Some(true),
            "another repository's PR of the same number is its own read");
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(sweep_checks("owner/alpha", 991_001, pending.clone(), false, read(Some(true))).await, Some(false),
            "and the verdict it does hold still stands");
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn merge_reserve_defers_optional_reads_only_on_a_known_low_budget() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = AtomicUsize::new(0);
        let read = || async { reads.fetch_add(1, Ordering::SeqCst); "evidence" };
        assert_eq!(unless_reserved(Some(DEFAULT_MERGE_RESERVE - 1), "ci repair", read).await, None);
        assert_eq!(reads.load(Ordering::SeqCst), 0, "inside the reserve the read is never made");
        assert_eq!(unless_reserved(Some(DEFAULT_MERGE_RESERVE), "ci repair", read).await, Some("evidence"));
        assert_eq!(unless_reserved(None, "ci repair", read).await, Some("evidence"), "an unknown budget never blocks");
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn tick_reads_are_shared_until_a_write() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fetched = AtomicUsize::new(0);
        let read = || async {
            tick_read("inventory".into(), || async { fetched.fetch_add(1, Ordering::SeqCst); Ok("rows".to_string()) }).await.unwrap()
        };
        read().await;
        read().await;
        assert_eq!(fetched.load(Ordering::SeqCst), 2, "outside a tick nothing is shared");
        tick_scope(async {
            read().await;
            read().await;
            assert_eq!(fetched.load(Ordering::SeqCst), 3, "one read per tick");
            observed(&gh_bin(), &["pr", "view", "9"]);
            read().await;
            assert_eq!(fetched.load(Ordering::SeqCst), 3, "reads keep the tick's inventory");
            observed(&gh_bin(), &["pr", "merge", "9"]);
            read().await;
            assert_eq!(fetched.load(Ordering::SeqCst), 4, "a write clears the tick's reads");
        }).await;
    }
}
