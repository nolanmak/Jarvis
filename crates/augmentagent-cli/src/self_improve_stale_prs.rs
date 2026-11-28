//! The stale PR sweep.
//!
//! A pull request that nobody touches is invisible to the rest of the loop:
//! one stacked on a branch that already merged never becomes reviewable, and
//! one labelled `autoship-manual` is excluded from automatic routing for
//! good. Both sat for two weeks while the loop only started new work.
//!
//! Every `AUTOSHIP_STALE_PR_DAYS` days (default 2, `0` disables) one pass
//! looks at every open, trusted, same-repository PR that has been idle that
//! long and gives it one of three outcomes:
//!
//! - **close**: nothing is left to land, or the reviewer judges an unhealthy
//!   PR superseded. A mergeable PR with green checks is never closed on an
//!   opinion.
//! - **merge** / **continue**: the PR is handed to the adopted-PR pipeline,
//!   which re-verifies it on current `main`, repairs it, reviews it and
//!   merges on LGTM. Handing over means removing what kept it out: a base
//!   branch that already landed is retargeted to `main`, and the PR's own
//!   `autoship-manual` label is removed (`AUTOSHIP_STALE_PR_RELEASE_MANUAL=0`
//!   keeps the label and leaves the PR alone). The sweep never merges by
//!   itself, so there is still exactly one merge path and one approval rule.
//!
//! A PR stacked on a parent that is still open waits for the parent.
use super::*;
use chrono::{DateTime, NaiveDate, Utc};

pub(super) const STALE_DAYS_ENV: &str = "AUTOSHIP_STALE_PR_DAYS";
pub(super) const RELEASE_MANUAL_ENV: &str = "AUTOSHIP_STALE_PR_RELEASE_MANUAL";
const DEFAULT_STALE_DAYS: i64 = 2;
/// Each verdict is one call against the daily observation cap the nightly
/// review shares, so a pass takes at most half of the default cap.
const MAX_PRS: usize = 6;
/// A due pass that was deferred retries this often, until the next hourly tick.
pub(super) const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
pub(super) const RETRIES_PER_HOUR: u32 = 11;
const MAX_DIFF_BYTES: usize = 60 * 1024;
const MAX_ATTEMPTS_PER_DAY: u32 = 4;
const MARKER: &str = "<!-- autoship:stale-pr-sweep -->";
const MAIN: &str = "main";

const SYSTEM_PROMPT: &str = "You triage pull requests that have been sitting untouched in a \
repository worked by an autonomous coding loop. For the pull request below decide exactly one:\n\
MERGE: the change is complete and still wanted; it only needs re-verification on current main.\n\
CONTINUE: the change is still wanted but unfinished, conflicting or failing; it should be built out.\n\
CLOSE: the change is obsolete, superseded or abandoned and finishing it would waste work.\n\
Prefer CONTINUE over CLOSE when unsure. Output exactly one line:\n\
VERDICT: <MERGE|CONTINUE|CLOSE> | <one sentence reason>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Checks { Green, Red, Pending }

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sitting {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub head: String,
    pub base: String,
    pub base_oid: String,
    pub conflicting: bool,
    pub manual: bool,
    pub checks: Checks,
    pub idle_days: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verdict { Merge, Continue, Close }

/// Where the PR's base stands relative to `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Base {
    Main,
    /// The base is the head of another open PR.
    ParentOpen(u64),
    /// Every commit on the base branch is already on `main`.
    Landed,
    /// The base branch holds commits `main` does not have, or is unknown.
    Unlanded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    Close { reason: String },
    /// Remove what keeps the PR out of the adopted-PR pipeline.
    Release { retarget: bool, unlabel: bool, verdict: Verdict, reason: String },
    /// Nothing to change this pass.
    Leave { reason: String },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Report {
    pub selected: usize,
    pub closed: Vec<u64>,
    pub released: Vec<u64>,
    pub left: Vec<u64>,
    /// PRs whose facts or verdict could not be obtained.
    pub failed: usize,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct State {
    pub last_success_ms: Option<i64>,
    pub attempt_day: Option<NaiveDate>,
    pub attempts: u32,
}

impl State {
    fn load(path: &Path) -> Self {
        let Some(v) = std::fs::read_to_string(path).ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok()) else {
            return Self::default();
        };
        Self {
            last_success_ms: v["last_success_ms"].as_i64(),
            attempt_day: v["attempt_day"].as_str().and_then(|d| d.parse().ok()),
            attempts: v["attempts"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, serde_json::json!({
            "last_success_ms": self.last_success_ms,
            "attempt_day": self.attempt_day.map(|d| d.to_string()),
            "attempts": self.attempts,
        }).to_string())?;
        Ok(())
    }

    fn attempts_on(&self, day: NaiveDate) -> u32 {
        if self.attempt_day == Some(day) { self.attempts } else { 0 }
    }
}

pub(super) fn stale_days() -> i64 {
    std::env::var(STALE_DAYS_ENV).ok().and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|days| *days >= 0).unwrap_or(DEFAULT_STALE_DAYS)
}

fn release_manual() -> bool {
    !std::env::var(RELEASE_MANUAL_ENV).is_ok_and(|raw| matches!(raw.trim(), "0" | "false" | "off"))
}

/// Due when `days` have passed since the last completed pass, within the
/// per-day attempt cap. `days == 0` disables the sweep.
pub(super) fn due(now: DateTime<Utc>, state: &State, days: i64) -> bool {
    days > 0
        && state.last_success_ms.is_none_or(|last| now.timestamp_millis() - last >= days * 86_400_000)
        && state.attempts_on(now.date_naive()) < MAX_ATTEMPTS_PER_DAY
}

/// A pass is complete unless every PR it looked at failed.
pub(super) fn completed(report: &Report) -> bool {
    report.selected == 0 || report.failed < report.selected
}

fn checks(row: &serde_json::Value) -> Checks {
    if adoption::rollup_green(row) {
        return Checks::Green;
    }
    let failed = row["statusCheckRollup"].as_array().into_iter().flatten().any(|check| {
        let text = |key: &str| check[key].as_str().unwrap_or_default();
        matches!(text("conclusion"), "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ACTION_REQUIRED")
            || matches!(text("state"), "FAILURE" | "ERROR")
    });
    if failed { Checks::Red } else { Checks::Pending }
}

/// Open PRs idle for at least `days`, oldest number first. Only GitHub's
/// same-repository flag and a trusted author qualify a PR, as for adoption.
pub(super) fn select(raw: &str, now: DateTime<Utc>, days: i64, allowlist: &[String]) -> Vec<Sitting> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap_or_default();
    let mut picked: Vec<Sitting> = rows.iter().filter_map(|row| {
        if row["isCrossRepository"].as_bool()? {
            return None;
        }
        let author = row.pointer("/author/login")?.as_str()?;
        if !author_is_trusted(author, row["authorAssociation"].as_str().unwrap_or(""), allowlist) {
            return None;
        }
        let updated = DateTime::parse_from_rfc3339(row["updatedAt"].as_str()?).ok()?.with_timezone(&Utc);
        let idle_days = (now - updated).num_days();
        if idle_days < days {
            return None;
        }
        let text = |key: &str| row[key].as_str().unwrap_or_default().to_string();
        let (head, base) = (text("headRefOid"), text("baseRefName"));
        if head.is_empty() || base.is_empty() {
            return None;
        }
        Some(Sitting {
            number: row["number"].as_u64().filter(|n| *n > 0)?,
            title: text("title"),
            body: text("body"),
            draft: row["isDraft"].as_bool().unwrap_or(false),
            head,
            base,
            base_oid: text("baseRefOid"),
            conflicting: row["mergeable"].as_str() == Some("CONFLICTING"),
            manual: row["labels"].as_array().into_iter().flatten()
                .any(|label| label["name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(MANUAL_ENGINEERING_LABEL))),
            checks: checks(row),
            idle_days,
        })
    }).collect();
    picked.sort_by_key(|pr| pr.number);
    picked.dedup_by_key(|pr| pr.number);
    picked
}

/// The open PR whose head branch is `base`, if any.
pub(super) fn open_parent(raw: &str, base: &str) -> Option<u64> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap_or_default();
    rows.iter().find(|row| row["headRefName"].as_str() == Some(base)
        && row["isCrossRepository"].as_bool() == Some(false))
        .and_then(|row| row["number"].as_u64())
}

pub(super) fn parse_verdict(raw: &str) -> Option<(Verdict, String)> {
    raw.lines().find_map(|line| {
        let rest = line.trim().trim_start_matches(['-', '*', ' ', '`']).strip_prefix("VERDICT:")?;
        let (word, reason) = rest.split_once('|').unwrap_or((rest, ""));
        let verdict = match word.trim().trim_end_matches('`').to_ascii_uppercase().as_str() {
            "MERGE" => Verdict::Merge,
            "CONTINUE" => Verdict::Continue,
            "CLOSE" => Verdict::Close,
            _ => return None,
        };
        Some((verdict, reason.trim().trim_end_matches('`').to_string()))
    })
}

/// What the sweep does with one PR. `ahead` is how many commits the head has
/// that `main` does not.
pub(super) fn plan(pr: &Sitting, ahead: u64, base: &Base, verdict: Option<(Verdict, String)>, release_manual: bool) -> Action {
    if ahead == 0 {
        return Action::Close { reason: "every commit on this branch is already on `main`, so nothing is left to land".into() };
    }
    let Some((verdict, reason)) = verdict else {
        return Action::Leave { reason: "no verdict".into() };
    };
    if verdict == Verdict::Close {
        // An opinion never closes healthy work.
        return if pr.conflicting || pr.checks == Checks::Red {
            Action::Close { reason }
        } else {
            Action::Leave { reason: format!("judged closable but mergeable with no failing check: {reason}") }
        };
    }
    let retarget = match base {
        Base::Main => false,
        Base::Landed => true,
        Base::ParentOpen(parent) => return Action::Leave { reason: format!("stacked on open #{parent}") },
        Base::Unlanded => return Action::Leave { reason: format!("base `{}` has not landed on `main`", pr.base) },
    };
    if pr.manual && !release_manual {
        return Action::Leave { reason: format!("`{MANUAL_ENGINEERING_LABEL}` kept by {RELEASE_MANUAL_ENV}") };
    }
    if !retarget && !pr.manual {
        return Action::Leave { reason: "already routed to the adopted-PR pipeline".into() };
    }
    Action::Release { retarget, unlabel: pr.manual, verdict, reason }
}

fn close_comment(pr: &Sitting, reason: &str) -> String {
    format!("{MARKER}\nAutoship stale PR sweep: closing. This PR sat for {} days and {reason}.\n\n\
        The branch is kept. Reopen the PR if this is wrong.", pr.idle_days)
}

fn release_comment(pr: &Sitting, retarget: bool, unlabel: bool, verdict: Verdict, reason: &str) -> String {
    let mut done = Vec::new();
    if retarget { done.push(format!("retargeted from `{}` (already on `main`) to `main`", pr.base)); }
    if unlabel { done.push(format!("removed `{MANUAL_ENGINEERING_LABEL}`")); }
    let next = match verdict {
        Verdict::Merge => "Autoship re-verifies it on current `main` and merges it on LGTM",
        _ => "Autoship builds it out on current `main`, reviews it and merges it on LGTM",
    };
    format!("{MARKER}\nAutoship stale PR sweep: this PR sat for {} days. Verdict: **{}**. {reason}\n\n\
        To hand it over, the sweep {}. {next}. Add `{MANUAL_ENGINEERING_LABEL}` again to keep it manual.",
        pr.idle_days, if verdict == Verdict::Merge { "merge" } else { "continue build" }, done.join(" and "))
}

async fn ahead_of_main(repo_root: &Path, gh: &str, repository: &str, oid: &str) -> Result<u64> {
    let endpoint = format!("repos/{repository}/compare/{MAIN}...{oid}");
    let (ok, raw, err) = run(gh, &["api", &endpoint, "--jq", ".ahead_by"], repo_root).await?;
    anyhow::ensure!(ok, "compare failed: {}", truncate(&err, 200));
    raw.trim().parse().context("compare returned no ahead_by")
}

async fn apply(repo_root: &Path, gh: &str, pr: &Sitting, action: &Action) -> Result<()> {
    let number = pr.number.to_string();
    let gh_ok = |step: &'static str, out: (bool, String, String)| {
        anyhow::ensure!(out.0, "{step} failed: {}", truncate(&out.2, 200));
        Ok(())
    };
    match action {
        Action::Close { reason } => gh_ok("close", run(gh, &["pr", "close", &number, "--comment", &close_comment(pr, reason)], repo_root).await?),
        Action::Release { retarget, unlabel, verdict, reason } => {
            if *retarget {
                gh_ok("retarget", run(gh, &["pr", "edit", &number, "--base", MAIN], repo_root).await?)?;
            }
            if *unlabel {
                gh_ok("unlabel", run(gh, &["pr", "edit", &number, "--remove-label", MANUAL_ENGINEERING_LABEL], repo_root).await?)?;
            }
            let comment = release_comment(pr, *retarget, *unlabel, *verdict, reason);
            gh_ok("comment", run(gh, &["pr", "comment", &number, "--body", &comment], repo_root).await?)
        }
        Action::Leave { .. } => Ok(()),
    }
}

/// One sweep pass. `judge` is the reviewer call; injected for tests.
pub(super) async fn run_pass<F, Fut>(repo_root: &Path, gh: &str, repository: &str, allowlist: &[String],
    now: DateTime<Utc>, days: i64, max_prs: usize, release_manual: bool, mut judge: F) -> Result<Report>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let (ok, raw, err) = run(gh, &["pr", "list", "--state", "open", "--limit", "200", "--json",
        "number,title,body,isDraft,headRefName,headRefOid,baseRefName,baseRefOid,mergeable,isCrossRepository,author,labels,updatedAt,statusCheckRollup"],
        repo_root).await?;
    anyhow::ensure!(ok, "open PR listing failed: {}", truncate(&err, 200));
    let mut prs = select(&raw, now, days, allowlist);
    prs.truncate(max_prs.min(MAX_PRS));
    let mut report = Report { selected: prs.len(), ..Report::default() };
    for pr in prs {
        let ahead = match ahead_of_main(repo_root, gh, repository, &pr.head).await {
            Ok(ahead) => ahead,
            Err(error) => { warn!(pr = pr.number, "stale PR sweep: {error:#}"); report.failed += 1; continue; }
        };
        let base = if pr.base == MAIN {
            Base::Main
        } else if let Some(parent) = open_parent(&raw, &pr.base) {
            Base::ParentOpen(parent)
        } else if !pr.base_oid.is_empty()
            && ahead_of_main(repo_root, gh, repository, &pr.base_oid).await.is_ok_and(|ahead| ahead == 0) {
            Base::Landed
        } else {
            Base::Unlanded
        };
        let verdict = if ahead == 0 { None } else {
            let (ok, diff, err) = run(gh, &["pr", "diff", &pr.number.to_string()], repo_root).await?;
            if !ok {
                warn!(pr = pr.number, "stale PR sweep: diff unavailable: {}", truncate(&err, 200));
                report.failed += 1;
                continue;
            }
            let diff = if diff.len() > MAX_DIFF_BYTES {
                let mut cut = MAX_DIFF_BYTES;
                while !diff.is_char_boundary(cut) { cut -= 1; }
                format!("{}\n[diff truncated at {MAX_DIFF_BYTES} bytes]", &diff[..cut])
            } else { diff };
            let prompt = format!(
                "Pull request #{}: {}\nIdle for {} days. Draft: {}. Base branch: {}. Commits not on main: {ahead}. \
                 Merge conflict with base: {}. Checks: {:?}.\n\nDescription:\n{}\n\n```diff\n{diff}\n```",
                pr.number, pr.title, pr.idle_days, pr.draft, pr.base, pr.conflicting, pr.checks, truncate(&pr.body, 4000));
            match judge(prompt).await.map(|output| parse_verdict(&output)) {
                Ok(Some(verdict)) => Some(verdict),
                Ok(None) => { warn!(pr = pr.number, "stale PR sweep: unparseable verdict"); report.failed += 1; continue; }
                Err(error) => { warn!(pr = pr.number, "stale PR sweep: verdict call failed: {error:#}"); report.failed += 1; continue; }
            }
        };
        let action = plan(&pr, ahead, &base, verdict, release_manual);
        if let Err(error) = apply(repo_root, gh, &pr, &action).await {
            warn!(pr = pr.number, ?action, "stale PR sweep: {error:#}");
            report.failed += 1;
            continue;
        }
        info!(pr = pr.number, idle_days = pr.idle_days, ?action, "stale PR sweep");
        match action {
            Action::Close { .. } => report.closed.push(pr.number),
            Action::Release { .. } => report.released.push(pr.number),
            Action::Leave { .. } => report.left.push(pr.number),
        }
    }
    Ok(report)
}

/// Count the attempt before `pass` spends anything; record the pass done only
/// if it completed, so a total failure retries on a later tick.
pub(super) async fn attempt<Fut>(path: &Path, now: DateTime<Utc>, pass: Fut) -> Result<Report>
where
    Fut: std::future::Future<Output = Result<Report>>,
{
    let today = now.date_naive();
    let mut state = State::load(path);
    state.attempts = state.attempts_on(today) + 1;
    state.attempt_day = Some(today);
    state.save(path)?;
    let report = pass.await?;
    if completed(&report) {
        state.last_success_ms = Some(now.timestamp_millis());
        state.save(path)?;
    } else {
        warn!(selected = report.selected, attempt = state.attempts, "stale PR sweep: nothing completed; will retry on a later tick");
    }
    Ok(report)
}

fn state_path() -> Result<PathBuf> {
    Ok(crate::build_storage::state_dir()?.join("autoship-stale-pr-sweep.json"))
}

/// A pass is due and has not completed: it was deferred or has yet to run.
pub(super) fn pending() -> bool {
    state_path().is_ok_and(|path| due(Utc::now(), &State::load(&path), stale_days()))
}

/// Scheduler entry: safe on every tick. Runs one pass every [`stale_days`]
/// days, within the global rolling model budget.
pub(super) async fn run_if_due(repo_root: &Path) -> Result<Option<Report>> {
    let now = Utc::now();
    let days = stale_days();
    let path = state_path()?;
    if !due(now, &State::load(&path), days) {
        return Ok(None);
    }
    let budget = crate::autoship_workflow::WorkflowStore::open(&workflow::path())?
        .global_model_budget(now.timestamp_millis())?;
    if budget.exhausted() {
        info!("stale PR sweep deferred: global model budget exhausted");
        return Ok(None);
    }
    let Some(guard) = workflow::WorkflowGuard::acquire_observation(
        &format!("stale-pr-sweep:{}", now.date_naive())).await? else {
        info!("stale PR sweep deferred: observation lease or budget unavailable");
        return Ok(None);
    };
    let provider = augmentagent_channel_core::ProviderKind::Codex;
    let result = async {
        let repository = workflow::canonical_repository(repo_root).await?;
        let mut allowlist = trusted_authors(repo_root).await;
        if let Some(owner) = repository.split('/').next().map(str::to_ascii_lowercase) {
            if !allowlist.contains(&owner) { allowlist.push(owner); }
        }
        let reviewer = augmentagent_channel_core::build_workflow_pinned(provider)
            .context("stale PR reviewer construction failed")?;
        guard.bind_reasoner(reviewer.as_ref())?;
        let isolated = tempfile::tempdir()?;
        let mut opts = nightly_review::reviewer_opts(guard.audit_logger(), isolated.path().to_path_buf())?;
        opts.system_prompt = SYSTEM_PROMPT.to_string();
        let max = usize::try_from(budget.remaining()).unwrap_or(MAX_PRS);
        attempt(&path, now, run_pass(repo_root, &gh_bin(), &repository, &allowlist, now, days, max, release_manual(), |prompt| {
            let reviewer = reviewer.clone();
            let opts = opts.clone();
            async move {
                let (output, _session) = augmentagent_channel_core::native_session::fresh_review_scope(
                    provider,
                    augmentagent_channel_core::model_router::native_reviewer_scope(
                        provider, reviewer.call(&opts, &prompt)),
                ).await?;
                Ok(output)
            }
        })).await
    }.await;
    guard.finish()?;
    let report = result?;
    info!(selected = report.selected, closed = ?report.closed, released = ?report.released,
        left = ?report.left, failed = report.failed, "stale PR sweep pass finished");
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> { Utc.with_ymd_and_hms(2026, 11, 28, 9, 0, 0).unwrap() }

    fn row(number: u64, updated: &str) -> serde_json::Value {
        serde_json::json!({
            "number": number, "title": "t", "body": "b", "isDraft": false,
            "headRefName": format!("feat/{number}"), "headRefOid": format!("head{number}"),
            "baseRefName": "main", "baseRefOid": "mainoid", "mergeable": "MERGEABLE",
            "isCrossRepository": false, "author": {"login": "owner"}, "labels": [],
            "updatedAt": updated,
            "statusCheckRollup": [{"__typename": "CheckRun", "name": "check", "status": "COMPLETED", "conclusion": "SUCCESS"}],
        })
    }

    fn sitting(rows: serde_json::Value) -> Vec<Sitting> {
        select(&rows.to_string(), now(), 2, &["owner".to_string()])
    }

    fn one(row: serde_json::Value) -> Sitting {
        sitting(serde_json::json!([row])).remove(0)
    }

    fn says(verdict: Verdict) -> Option<(Verdict, String)> { Some((verdict, "why".into())) }

    #[test]
    fn due_every_two_days_and_never_when_disabled() {
        let done = |hours_ago: i64| State {
            last_success_ms: Some((now() - chrono::Duration::hours(hours_ago)).timestamp_millis()),
            ..State::default()
        };
        assert!(due(now(), &State::default(), 2));
        assert!(!due(now(), &done(47), 2));
        assert!(due(now(), &done(48), 2));
        assert!(!due(now(), &State::default(), 0));
        let spent = State { attempt_day: Some(now().date_naive()), attempts: MAX_ATTEMPTS_PER_DAY, ..State::default() };
        assert!(!due(now(), &spent, 2));
    }

    #[test]
    fn only_idle_trusted_same_repository_prs_are_selected() {
        let mut fork = row(3, "2026-11-01T00:00:00Z");
        fork["isCrossRepository"] = true.into();
        let mut stranger = row(4, "2026-11-01T00:00:00Z");
        stranger["author"]["login"] = "someone".into();
        let picked = sitting(serde_json::json!([
            row(2, "2026-11-26T08:59:00Z"), row(1, "2026-11-27T09:00:00Z"), fork, stranger,
        ]));
        assert_eq!(picked.iter().map(|pr| pr.number).collect::<Vec<_>>(), vec![2]);
        assert_eq!(picked[0].idle_days, 2);
        assert_eq!(picked[0].checks, Checks::Green);
    }

    #[test]
    fn verdict_line_is_parsed_and_anything_else_is_not() {
        assert_eq!(parse_verdict("thinking\n- VERDICT: continue | needs a rebase"),
            Some((Verdict::Continue, "needs a rebase".into())));
        assert_eq!(parse_verdict("VERDICT: CLOSE"), Some((Verdict::Close, String::new())));
        assert_eq!(parse_verdict("VERDICT: MAYBE | x"), None);
        assert_eq!(parse_verdict("close it"), None);
    }

    #[test]
    fn landed_branch_closes_without_a_verdict() {
        let pr = one(row(1, "2026-11-01T00:00:00Z"));
        assert!(matches!(plan(&pr, 0, &Base::Main, None, true), Action::Close { .. }));
        assert!(matches!(plan(&pr, 3, &Base::Main, None, true), Action::Leave { .. }));
    }

    #[test]
    fn a_healthy_pr_is_never_closed_on_an_opinion() {
        let healthy = one(row(1, "2026-11-01T00:00:00Z"));
        assert!(matches!(plan(&healthy, 3, &Base::Main, says(Verdict::Close), true), Action::Leave { .. }));
        let mut conflicting = row(1, "2026-11-01T00:00:00Z");
        conflicting["mergeable"] = "CONFLICTING".into();
        assert_eq!(plan(&one(conflicting), 3, &Base::Main, says(Verdict::Close), true),
            Action::Close { reason: "why".into() });
        let mut red = row(1, "2026-11-01T00:00:00Z");
        red["statusCheckRollup"][0]["conclusion"] = "FAILURE".into();
        assert!(matches!(plan(&one(red), 3, &Base::Main, says(Verdict::Close), true), Action::Close { .. }));
    }

    #[test]
    fn wanted_work_is_released_only_when_something_holds_it() {
        let mut manual = row(1, "2026-11-01T00:00:00Z");
        manual["labels"] = serde_json::json!([{"name": "Autoship-Manual"}]);
        manual["baseRefName"] = "feat/parent".into();
        let pr = one(manual);
        assert_eq!(plan(&pr, 3, &Base::Landed, says(Verdict::Continue), true),
            Action::Release { retarget: true, unlabel: true, verdict: Verdict::Continue, reason: "why".into() });
        assert!(matches!(plan(&pr, 3, &Base::Landed, says(Verdict::Merge), false), Action::Leave { .. }));
        assert!(matches!(plan(&pr, 3, &Base::ParentOpen(9), says(Verdict::Merge), true), Action::Leave { .. }));
        assert!(matches!(plan(&pr, 3, &Base::Unlanded, says(Verdict::Merge), true), Action::Leave { .. }));
        let routed = one(row(2, "2026-11-01T00:00:00Z"));
        assert!(matches!(plan(&routed, 3, &Base::Main, says(Verdict::Merge), true), Action::Leave { .. }));
    }

    /// A fake `gh`: PR 1 is fully on main, PR 2 is manual and stacked on a
    /// landed branch, PR 3 is stacked on open PR 2, PR 4 is fresh.
    fn fake_gh(dir: &Path) -> String {
        let mut landed = row(1, "2026-11-01T00:00:00Z");
        landed["headRefOid"] = "landed".into();
        let mut stacked = row(2, "2026-11-01T00:00:00Z");
        stacked["baseRefName"] = "feat/merged-parent".into();
        stacked["baseRefOid"] = "landed".into();
        stacked["labels"] = serde_json::json!([{"name": "autoship-manual"}]);
        let mut child = row(3, "2026-11-01T00:00:00Z");
        child["baseRefName"] = "feat/2".into();
        let rows = serde_json::json!([landed, stacked, child, row(4, "2026-11-28T08:00:00Z")]);
        std::fs::write(dir.join("open.json"), rows.to_string()).unwrap();
        let gh = dir.join("gh");
        augmentagent_test_support::write_executable(&gh, &format!(
            "#!/bin/sh\nD='{d}'\necho \"$*\" >> \"$D/gh.log\"\ncase \"$1 $2\" in\n\
             'pr list') cat \"$D/open.json\";;\n\
             'api repos/o/r/compare/main...landed') echo 0;;\n\
             api\\ *) echo 4;;\n\
             'pr diff') echo '+fn changed() {{}}';;\n\
             'pr close'|'pr edit'|'pr comment') ;;\n\
             *) exit 1;;\nesac\n", d = dir.display())).unwrap();
        gh.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn pass_closes_landed_work_and_releases_held_work() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        let judge = |prompt: String| async move {
            assert!(prompt.contains("+fn changed()"), "{prompt}");
            Ok("VERDICT: CONTINUE | still wanted".to_string())
        };
        let report = run_pass(dir.path(), &gh, "o/r", &["owner".to_string()], now(), 2, 10, true, judge).await.unwrap();
        assert_eq!(report, Report { selected: 3, closed: vec![1], released: vec![2], left: vec![3], failed: 0 });
        let calls = std::fs::read_to_string(dir.path().join("gh.log")).unwrap();
        assert!(calls.contains("pr close 1 --comment"), "{calls}");
        assert!(!calls.contains("pr diff 1"), "landed work needs no verdict: {calls}");
        assert!(calls.contains("pr edit 2 --base main"), "{calls}");
        assert!(calls.contains("pr edit 2 --remove-label autoship-manual"), "{calls}");
        assert!(calls.contains("pr comment 2 --body"), "{calls}");
        assert!(!calls.contains("pr edit 3") && !calls.contains("pr close 3"), "{calls}");
        assert!(!calls.contains("pr diff 4") && !calls.contains("pr edit 4") && !calls.contains("pr close 4"),
            "a fresh PR is untouched: {calls}");
    }

    #[tokio::test]
    async fn a_pass_with_no_verdicts_is_retried_not_recorded_done() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path());
        std::fs::write(dir.path().join("open.json"),
            serde_json::json!([row(2, "2026-11-01T00:00:00Z")]).to_string()).unwrap();
        let state = dir.path().join("state.json");
        let allow = ["owner".to_string()];
        let failing = |_prompt: String| async move { anyhow::bail!("reviewer down") };
        let report = attempt(&state, now(), run_pass(dir.path(), &gh, "o/r", &allow, now(), 2, 10, true, failing)).await.unwrap();
        assert_eq!((report.selected, report.failed), (1, 1));
        let after = State::load(&state);
        assert_eq!((after.last_success_ms, after.attempts), (None, 1));
        assert!(due(now(), &after, 2));
        let ok = |_prompt: String| async move { Ok("VERDICT: MERGE | done".to_string()) };
        attempt(&state, now(), run_pass(dir.path(), &gh, "o/r", &allow, now(), 2, 10, true, ok)).await.unwrap();
        assert!(!due(now(), &State::load(&state), 2));
    }
}
