//! #2574 — the nightly post-merge review.
//!
//! With `AUTOSHIP_REVIEW_MODE=nightly` (the default) autoship merges on green
//! and the independent review moves here: once a day, after 08:00 UTC, one
//! pass reviews every autoship PR merged in the last 24 hours with the native
//! Codex reviewer and files each finding as an `agent-fixable` issue linking
//! the PR. A fingerprint marker in the issue body dedupes across runs.
//! The day is claimed before any model call, so a crash or restart never
//! repeats the spend; the global rolling model budget bounds the PR count.
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
    pub reviewed: Vec<u64>,
    pub filed: usize,
    pub duplicates: usize,
}

/// Due once per UTC date, at or after 08:00.
pub(super) fn due(now: DateTime<Utc>, last_run: Option<NaiveDate>) -> bool {
    now.hour() >= DUE_HOUR_UTC && last_run != Some(now.date_naive())
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
    let mut report = Report::default();
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

/// Scheduler entry: safe on every tick. Runs at most one pass per UTC day,
/// only in nightly mode, within the global rolling model budget.
pub(super) async fn run_if_due(repo_root: &Path) -> Result<Option<Report>> {
    if !crate::autoship_review::ReviewMode::nightly() {
        return Ok(None);
    }
    let now = Utc::now();
    let path = state_path()?;
    let last = std::fs::read_to_string(&path).ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v["last_run"].as_str().and_then(|d| d.parse::<NaiveDate>().ok()));
    if !due(now, last) {
        return Ok(None);
    }
    let budget = crate::autoship_workflow::WorkflowStore::open(&workflow::path())?
        .global_model_budget(now.timestamp_millis())?;
    if budget.exhausted() {
        info!("nightly review deferred: global model budget exhausted");
        return Ok(None);
    }
    // Claim the day before spending anything.
    std::fs::write(&path, serde_json::json!({"last_run": now.date_naive().to_string()}).to_string())?;
    let reviewer = augmentagent_channel_core::build_workflow_pinned(augmentagent_channel_core::ProviderKind::Codex)
        .context("nightly reviewer construction failed")?;
    let opts = codex_review_opts(repo_root.to_path_buf(), SYSTEM_PROMPT);
    let max = usize::try_from(budget.remaining()).unwrap_or(MAX_PRS);
    let report = run_pass(repo_root, &gh_bin(), now, max, |prompt| {
        let reviewer = reviewer.clone();
        let opts = opts.clone();
        async move { reviewer.call(&opts, &prompt).await }
    }).await?;
    info!(reviewed = report.reviewed.len(), filed = report.filed, duplicates = report.duplicates,
        "nightly post-merge review complete");
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> { Utc.with_ymd_and_hms(2026, 11, 26, h, 0, 0).unwrap() }

    #[test]
    fn due_once_per_day_after_eight_utc() {
        assert!(!due(at(7), None));
        assert!(due(at(8), None));
        assert!(due(at(9), "2026-11-25".parse().ok()));
        assert!(!due(at(9), "2026-11-26".parse().ok()));
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
        assert_eq!(first, Report { reviewed: vec![21], filed: 1, duplicates: 0 });
        assert_eq!(second, Report { reviewed: vec![21], filed: 0, duplicates: 1 });
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("issue create").count(), 1, "{calls}");
        assert!(calls.contains("--label agent-fixable"), "{calls}");
        assert!(!calls.contains("pr diff 22"), "outside the window: {calls}");
    }
}
