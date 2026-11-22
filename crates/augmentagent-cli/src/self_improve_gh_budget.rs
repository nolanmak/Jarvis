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
        let raw = complete_repository_inventory(root, "issue", Some(repository), "number,labels,state").await.ok()?;
        let rows: Vec<serde_json::Value> = serde_json::from_str(&raw).ok()?;
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
    /// Listed with an open state in this tick's inventory.
    pub(super) fn is_open(&self, issue: u64) -> bool {
        self.get(issue).and_then(|row| row["state"].as_str()).is_some_and(|state| state.eq_ignore_ascii_case("open"))
    }
}

fn floor() -> u64 {
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
