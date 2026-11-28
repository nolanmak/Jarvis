//! #2574 — the nightly post-merge review.
//!
//! With `AUTOSHIP_REVIEW_MODE=nightly` (the default) autoship merges on green
//! and the independent review moves here: once a day, after 08:00 UTC, one
//! pass reviews every autoship PR merged in the last 24 hours with the native
//! Codex reviewer and files each finding as an `agent-fixable` issue linking
//! the PR. A fingerprint marker in the issue body dedupes across runs.
//! #2595: the day is recorded done only after a pass completes a review (or
//! finds nothing to review); a total failure retries on the next hourly tick.
//! Each attempt is counted before any model call and capped per day, so a
//! crash loop cannot repeat the spend unboundedly; the global rolling model
//! budget bounds the PR count.
use super::*;
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const DUE_HOUR_UTC: u32 = 8;
const WINDOW_HOURS: i64 = 24;
const MAX_PRS: usize = 15;
const MAX_DIFF_BYTES: usize = 120 * 1024;
const MAX_FINDINGS_PER_PR: usize = 5;
const FINDING_MARKER: &str = "autoship-nightly-review-finding:";
const MAX_ATTEMPTS_PER_DAY: u32 = 4;

const SYSTEM_PROMPT: &str = "You are the independent post-merge reviewer for an autonomous \
coding loop. The change below is already merged to main. Report only real defects a maintainer \
must fix: correctness bugs, data loss, security or privacy exposure, broken invariants, races, \
missing error handling that can wedge the system. Do not report style, naming, docs or \
speculative refactors. Output one line per finding, exactly:\n\
FINDING: <high|medium> | <short title> | <file:line and what breaks, one sentence>\n\
If there is nothing worth fixing, output exactly: NO FINDINGS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MergedPr {
    pub number: u64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Finding {
    pub severity: String,
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Report {
    /// Autoship PRs in the window that this pass tried to review.
    pub selected: usize,
    pub reviewed: Vec<u64>,
    pub filed: usize,
    pub duplicates: usize,
}

/// Persisted scheduler state. `last_success` is a day whose pass completed;
/// `attempts` counts passes started on `attempt_day`. The pre-#2595 key
/// `last_run` recorded a claim, not a success, and is deliberately ignored.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct DayState {
    pub last_success: Option<NaiveDate>,
    pub attempt_day: Option<NaiveDate>,
    pub attempts: u32,
}

impl DayState {
    fn load(path: &Path) -> Self {
        let Some(v) = std::fs::read_to_string(path).ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok()) else {
            return Self::default();
        };
        let date = |key: &str| v[key].as_str().and_then(|d| d.parse::<NaiveDate>().ok());
        Self {
            last_success: date("last_success"),
            attempt_day: date("attempt_day"),
            attempts: v["attempts"].as_u64().and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let date = |d: Option<NaiveDate>| d.map(|d| d.to_string());
        std::fs::write(path, serde_json::json!({
            "last_success": date(self.last_success),
            "attempt_day": date(self.attempt_day),
            "attempts": self.attempts,
        }).to_string())?;
        Ok(())
    }

    fn attempts_on(&self, day: NaiveDate) -> u32 {
        if self.attempt_day == Some(day) { self.attempts } else { 0 }
    }
}

/// Due at or after 08:00 UTC until a pass completes that day, within the
/// per-day attempt cap.
pub(super) fn due(now: DateTime<Utc>, state: &DayState) -> bool {
    let today = now.date_naive();
    now.hour() >= DUE_HOUR_UTC && state.last_success != Some(today)
        && state.attempts_on(today) < MAX_ATTEMPTS_PER_DAY
}

/// A pass completed when it reviewed at least one PR or had none to review.
/// A pass whose every review call failed is not done.
pub(super) fn completed(report: &Report) -> bool {
    report.selected == 0 || !report.reviewed.is_empty()
}

/// Count the attempt before `pass` spends anything, then record the day done
/// only if the pass completed. Errors leave the day open for the next tick.
pub(super) async fn attempt<Fut>(path: &Path, now: DateTime<Utc>, pass: Fut) -> Result<Report>
where
    Fut: std::future::Future<Output = Result<Report>>,
{
    let today = now.date_naive();
    let mut state = DayState::load(path);
    state.attempts = state.attempts_on(today) + 1;
    state.attempt_day = Some(today);
    state.save(path)?;
    let report = pass.await?;
    if completed(&report) {
        state.last_success = Some(today);
        state.save(path)?;
    } else {
        warn!(selected = report.selected, attempt = state.attempts,
            "nightly review: no review completed; will retry on a later tick");
    }
    Ok(report)
}

/// Reasoner options for the workflow-native reviewer. `build_workflow_pinned`
/// refuses dispatch unless the audit logger is protected and requires run
/// resources (#2595), so refuse here before any spend if it is not.
pub(super) fn reviewer_opts(
    audit: Arc<augmentagent_channel_core::AuditLogger>,
    cwd: PathBuf,
) -> Result<augmentagent_channel_core::ReasonerOpts> {
    anyhow::ensure!(audit.run_resources_required(), "nightly reviewer requires protected workflow policy");
    let mut opts = codex_review_opts(cwd, SYSTEM_PROMPT);
    // The diff is supplied in the prompt; the reviewer needs no host scope.
    opts.allowed_tools.clear();
    opts.add_dirs.clear();
    opts.env.clear();
    opts.restrict_env = true;
    opts.audit_logger = Some(audit);
    Ok(opts)
}

/// The autoship PRs merged in the 24 hours before `now`, oldest number first.
pub(super) fn select(raw: &str, now: DateTime<Utc>) -> Vec<MergedPr> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap_or_default();
    let since = now - chrono::Duration::hours(WINDOW_HOURS);
    let mut picked: Vec<MergedPr> = rows.iter().filter_map(|row| {
        let merged = row["mergedAt"].as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())?
            .with_timezone(&Utc);
        if merged <= since || merged > now {
            return None;
        }
        let owned = row["headRefName"].as_str().is_some_and(|b| b.starts_with(BRANCH_PREFIX))
            || row["body"].as_str().is_some_and(|b| b.contains(SELF_IMPROVE_BODY_MARKER));
        if !owned {
            return None;
        }
        Some(MergedPr {
            number: row["number"].as_u64()?,
            title: row["title"].as_str().unwrap_or_default().to_string(),
        })
    }).collect();
    picked.sort_by_key(|pr| pr.number);
    picked.dedup_by_key(|pr| pr.number);
    picked
}

pub(super) fn parse_findings(raw: &str) -> Vec<Finding> {
    raw.lines().filter_map(|line| {
        let line = line.trim().trim_start_matches(['-', '*', ' ', '`']);
        let rest = line.strip_prefix("FINDING:")?;
        let mut parts = rest.splitn(3, '|').map(str::trim);
        let severity = parts.next()?.to_ascii_lowercase();
        let title = parts.next().filter(|t| !t.is_empty())?.to_string();
        let detail = parts.next().unwrap_or_default().to_string();
        Some(Finding { severity, title, detail })
    }).take(MAX_FINDINGS_PER_PR).collect()
}

/// Stable across runs and rewordings of case/punctuation in the title.
pub(super) fn fingerprint(pr: u64, finding: &Finding) -> String {
    let normalized: String = finding.title.to_ascii_lowercase().chars()
        .filter(char::is_ascii_alphanumeric).collect();
    let digest = Sha256::digest(format!("{pr}:{normalized}").as_bytes());
    format!("{digest:x}")[..16].to_string()
}

/// Fingerprints already filed, from `gh issue list --json body`.
pub(super) fn filed_fingerprints(raw: &str) -> BTreeSet<String> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(raw).unwrap_or_default();
    rows.iter().filter_map(|row| row["body"].as_str()).flat_map(|body| {
        body.match_indices(FINDING_MARKER).filter_map(|(at, _)| {
            let tail = &body[at + FINDING_MARKER.len()..];
            let fp: String = tail.chars().take_while(char::is_ascii_hexdigit).collect();
            (fp.len() == 16).then_some(fp)
        }).collect::<Vec<_>>()
    }).collect()
}

fn issue_body(pr: u64, finding: &Finding, fp: &str) -> String {
    format!(
        "<!-- {FINDING_MARKER}{fp} -->\nThe nightly post-merge review (#2574) found this in #{pr}, \
         which autoship merged on green.\n\n**Severity:** {}\n\n{}\n\nFix forward against current \
         `main`; link #{pr} in the PR.",
        finding.severity, finding.detail
    )
}

/// One review pass. `review` is the reviewer call; injected for tests.
pub(super) async fn run_pass<F, Fut>(repo_root: &Path, gh: &str, now: DateTime<Utc>, max_prs: usize, mut review: F) -> Result<Report>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let since = (now - chrono::Duration::hours(WINDOW_HOURS)).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let search = format!("merged:>={since}");
    let (ok, raw, err) = run(gh, &["pr", "list", "--state", "merged", "--base", "main", "--search", &search,
        "--limit", "100", "--json", "number,title,headRefName,body,mergedAt"], repo_root).await?;
    anyhow::ensure!(ok, "merged PR listing failed: {}", truncate(&err, 200));
    let mut prs = select(&raw, now);
    prs.truncate(max_prs.min(MAX_PRS));
    let mut report = Report { selected: prs.len(), ..Report::default() };
    if prs.is_empty() {
        return Ok(report);
    }
    let (ok, raw, err) = run(gh, &["issue", "list", "--state", "all", "--search",
        &format!("\"{FINDING_MARKER}\" in:body"), "--limit", "500", "--json", "body"], repo_root).await?;
    anyhow::ensure!(ok, "finding listing failed; refusing to file without dedupe: {}", truncate(&err, 200));
    let mut filed = filed_fingerprints(&raw);
    for pr in prs {
        let (ok, diff, err) = run(gh, &["pr", "diff", &pr.number.to_string()], repo_root).await?;
        if !ok {
            warn!(pr = pr.number, "nightly review: diff unavailable: {}", truncate(&err, 200));
            continue;
        }
        let diff = if diff.len() > MAX_DIFF_BYTES {
            let mut cut = MAX_DIFF_BYTES;
            while !diff.is_char_boundary(cut) { cut -= 1; }
            format!("{}\n[diff truncated at {MAX_DIFF_BYTES} bytes]", &diff[..cut])
        } else { diff };
        let prompt = format!("Merged PR #{}: {}\n\n```diff\n{diff}\n```", pr.number, pr.title);
        let output = match review(prompt).await {
            Ok(output) => output,
            Err(error) => { warn!(pr = pr.number, "nightly review call failed: {error:#}"); continue; }
        };
        report.reviewed.push(pr.number);
        for finding in parse_findings(&output) {
            let fp = fingerprint(pr.number, &finding);
            if !filed.insert(fp.clone()) {
                report.duplicates += 1;
                continue;
            }
            let title = format!("Nightly review: {} (#{})", truncate(&finding.title, 120), pr.number);
            let body = issue_body(pr.number, &finding, &fp);
            match run(gh, &["issue", "create", "--label", FIXABLE_LABEL, "--title", &title, "--body", &body], repo_root).await {
                Ok((true, ..)) => report.filed += 1,
                Ok((false, _, err)) => warn!(pr = pr.number, "nightly review: issue not filed: {}", truncate(&err, 200)),
                Err(error) => warn!(pr = pr.number, "nightly review: issue not filed: {error:#}"),
            }
        }
    }
    Ok(report)
}

fn state_path() -> Result<PathBuf> {
    Ok(crate::build_storage::state_dir()?.join("autoship-nightly-review.json"))
}

/// Scheduler entry: safe on every tick. Runs until one pass completes per UTC
/// day, only in nightly mode, within the global rolling model budget.
pub(super) async fn run_if_due(repo_root: &Path) -> Result<Option<Report>> {
    if !crate::autoship_review::ReviewMode::nightly() {
        return Ok(None);
    }
    let now = Utc::now();
    let path = state_path()?;
    if !due(now, &DayState::load(&path)) {
        return Ok(None);
    }
    let budget = crate::autoship_workflow::WorkflowStore::open(&workflow::path())?
        .global_model_budget(now.timestamp_millis())?;
    if budget.exhausted() {
        info!("nightly review deferred: global model budget exhausted");
        return Ok(None);
    }
    // #2595: the workflow-native reviewer only dispatches under a protected,
    // run-resources-required audit with a fenced runtime owner. Take the same
    // diagnostic observation guard the standalone calibration reviewers use.
    let Some(guard) = workflow::WorkflowGuard::acquire_observation(
        &format!("nightly-review:{}", now.date_naive())).await? else {
        info!("nightly review deferred: observation lease or budget unavailable");
        return Ok(None);
    };
    let provider = augmentagent_channel_core::ProviderKind::Codex;
    let result = async {
        let reviewer = augmentagent_channel_core::build_workflow_pinned(provider)
            .context("nightly reviewer construction failed")?;
        guard.bind_reasoner(reviewer.as_ref())?;
        let isolated = tempfile::tempdir()?;
        let opts = reviewer_opts(guard.audit_logger(), isolated.path().to_path_buf())?;
        let max = usize::try_from(budget.remaining()).unwrap_or(MAX_PRS);
        attempt(&path, now, run_pass(repo_root, &gh_bin(), now, max, |prompt| {
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
    info!(selected = report.selected, reviewed = report.reviewed.len(), filed = report.filed,
        duplicates = report.duplicates, "nightly post-merge review pass finished");
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> { Utc.with_ymd_and_hms(2026, 11, 26, h, 0, 0).unwrap() }

    fn done_on(day: &str) -> DayState {
        DayState { last_success: day.parse().ok(), ..DayState::default() }
    }

    #[test]
    fn due_once_per_day_after_eight_utc() {
        assert!(!due(at(7), &DayState::default()));
        assert!(due(at(8), &DayState::default()));
        assert!(due(at(9), &done_on("2026-11-25")));
        assert!(!due(at(9), &done_on("2026-11-26")));
    }

    #[test]
    fn legacy_claim_does_not_mark_the_day_done() {
        // Pre-#2595 state recorded a claim before any call, even when every
        // call then failed. It must not suppress the retry.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, r#"{"last_run":"2026-11-26"}"#).unwrap();
        assert!(due(at(9), &DayState::load(&path)));
    }

    #[test]
    fn reviewer_opts_carry_the_protected_workflow_policy() {
        let dir = tempfile::tempdir().unwrap();
        let unprotected = workflow::workflow_audit(&dir.path().join("workflow.db"), "nightly-fixture", None).unwrap();
        // Without the controller requirement the workflow-native reasoner
        // refuses dispatch; refuse before any spend instead.
        let refused = reviewer_opts(unprotected.clone(), dir.path().to_path_buf()).unwrap_err();
        assert!(format!("{refused:#}").contains("protected workflow policy"), "{refused:#}");
        unprotected.require_run_resources().unwrap();
        let opts = reviewer_opts(unprotected, dir.path().to_path_buf()).unwrap();
        let logger = opts.audit_logger.as_ref().expect("reviewer must carry the workflow audit");
        assert!(logger.is_protected() && logger.run_resources_required());
        assert!(opts.allowed_tools.is_empty() && opts.add_dirs.is_empty() && opts.restrict_env);
    }

    #[test]
    fn selects_only_autoship_merges_in_the_last_day() {
        let raw = serde_json::json!([
            {"number": 12, "title": "fresh", "headRefName": "agent-fix/issue-1", "body": "", "mergedAt": "2026-11-26T07:00:00Z"},
            {"number": 11, "title": "by body", "headRefName": "x", "body": "Automated self-improvement for #2.", "mergedAt": "2026-11-25T09:00:00Z"},
            {"number": 10, "title": "too old", "headRefName": "agent-fix/issue-3", "body": "", "mergedAt": "2026-11-25T07:59:00Z"},
            {"number": 9, "title": "human", "headRefName": "feature", "body": "", "mergedAt": "2026-11-26T07:00:00Z"},
        ]).to_string();
        let picked: Vec<u64> = select(&raw, at(8)).into_iter().map(|p| p.number).collect();
        assert_eq!(picked, vec![11, 12]);
    }

    #[test]
    fn parses_findings_and_ignores_prose() {
        let out = "Looks mostly fine.\n- FINDING: high | Race in sweep | foo.rs:10 double merge\nFINDING: medium | | empty title\nNO FINDINGS";
        let found = parse_findings(out);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Race in sweep");
        assert!(parse_findings("NO FINDINGS").is_empty());
    }

    #[test]
    fn fingerprint_roundtrips_through_issue_body() {
        let f = Finding { severity: "high".into(), title: "Race in Sweep!".into(), detail: "d".into() };
        let same = Finding { title: "race in sweep".into(), ..f.clone() };
        let fp = fingerprint(7, &f);
        assert_eq!(fp, fingerprint(7, &same));
        assert_ne!(fp, fingerprint(8, &f));
        let raw = serde_json::json!([{"body": issue_body(7, &f, &fp)}]).to_string();
        assert!(filed_fingerprints(&raw).contains(&fp));
    }

    #[tokio::test]
    async fn pass_files_new_findings_once_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let filed = dir.path().join("filed.json");
        std::fs::write(&filed, "[]").unwrap();
        let merged = serde_json::json!([
            {"number": 21, "title": "a", "headRefName": "agent-fix/issue-5", "body": "", "mergedAt": "2026-11-26T03:00:00Z"},
            {"number": 22, "title": "b", "headRefName": "agent-fix/issue-6", "body": "", "mergedAt": "2026-11-24T03:00:00Z"},
        ]).to_string();
        std::fs::write(dir.path().join("merged.json"), merged).unwrap();
        let gh = dir.path().join("gh");
        augmentagent_test_support::write_executable(&gh, &format!(
            "#!/bin/sh\nD='{d}'\necho \"$*\" >> \"$D/gh.log\"\ncase \"$1 $2\" in\n\
             'pr list') cat \"$D/merged.json\";;\n\
             'pr diff') echo '+fn changed() {{}}';;\n\
             'issue list') cat \"$D/filed.json\";;\n\
             'issue create') shift 2; while [ $# -gt 0 ]; do if [ \"$1\" = --body ]; then m=$(printf '%s' \"$2\" | grep -o 'autoship-nightly-review-finding:[0-9a-f]*'); printf '[{{\"body\":\"<!-- %s -->\"}}]' \"$m\" > \"$D/filed.json\"; fi; shift; done; echo https://github.com/o/r/issues/99;;\n\
             *) exit 1;;\nesac\n", d = dir.path().display())).unwrap();
        let gh = gh.to_string_lossy().into_owned();
        let reviewer = |prompt: String| async move {
            assert!(prompt.contains("#21") && prompt.contains("+fn changed()"));
            Ok("FINDING: high | Double merge race | sweep.rs:1 merges twice".to_string())
        };
        let first = run_pass(dir.path(), &gh, at(8), 10, reviewer).await.unwrap();
        let second = run_pass(dir.path(), &gh, at(9), 10, reviewer).await.unwrap();
        assert_eq!(first, Report { selected: 1, reviewed: vec![21], filed: 1, duplicates: 0 });
        assert_eq!(second, Report { selected: 1, reviewed: vec![21], filed: 0, duplicates: 1 });
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("issue create").count(), 1, "{calls}");
        assert!(calls.contains("--label agent-fixable"), "{calls}");
        assert!(!calls.contains("pr diff 22"), "outside the window: {calls}");
    }

    #[tokio::test]
    async fn all_failed_pass_is_retried_not_recorded_done() {
        let dir = tempfile::tempdir().unwrap();
        let merged = serde_json::json!([
            {"number": 31, "title": "a", "headRefName": "agent-fix/issue-7", "body": "", "mergedAt": "2026-11-26T03:00:00Z"},
        ]).to_string();
        std::fs::write(dir.path().join("merged.json"), merged).unwrap();
        let gh = dir.path().join("gh");
        augmentagent_test_support::write_executable(&gh, &format!(
            "#!/bin/sh\nD='{d}'\ncase \"$1 $2\" in\n\
             'pr list') cat \"$D/merged.json\";;\n\
             'pr diff') echo '+fn changed() {{}}';;\n\
             'issue list') echo '[]';;\n\
             *) exit 1;;\nesac\n", d = dir.path().display())).unwrap();
        let gh = gh.to_string_lossy().into_owned();
        let state = dir.path().join("state.json");
        let failing = |_: String| async {
            Err::<String, _>(anyhow::anyhow!("workflow-native reasoner requires protected workflow policy"))
        };
        let report = attempt(&state, at(8), run_pass(dir.path(), &gh, at(8), 10, failing)).await.unwrap();
        assert_eq!(report, Report { selected: 1, ..Report::default() });
        assert!(due(at(9), &DayState::load(&state)), "a pass with no completed review must retry");
        // An infrastructure error before any review also leaves the day open.
        let broken = attempt(&state, at(9), async { Err::<Report, _>(anyhow::anyhow!("gh down")) }).await;
        assert!(broken.is_err());
        assert!(due(at(10), &DayState::load(&state)));
        let ok = |_: String| async { Ok("NO FINDINGS".to_string()) };
        let report = attempt(&state, at(10), run_pass(dir.path(), &gh, at(10), 10, ok)).await.unwrap();
        assert_eq!(report.reviewed, vec![31]);
        assert!(!due(at(11), &DayState::load(&state)), "one completed pass per day");
        assert!(due(at(8) + chrono::Duration::days(1), &DayState::load(&state)));
    }

    #[tokio::test]
    async fn failed_attempts_are_capped_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.json");
        for _ in 0..MAX_ATTEMPTS_PER_DAY {
            assert!(due(at(9), &DayState::load(&state)));
            let failed = Report { selected: 2, ..Report::default() };
            attempt(&state, at(9), async { Ok(failed) }).await.unwrap();
        }
        assert!(!due(at(23), &DayState::load(&state)), "a persistent failure must not retry without bound");
        assert!(due(at(8) + chrono::Duration::days(1), &DayState::load(&state)));
        // Nothing merged is a completed pass.
        let other = dir.path().join("empty.json");
        attempt(&other, at(9), async { Ok(Report::default()) }).await.unwrap();
        assert!(!due(at(10), &DayState::load(&other)));
    }
}
