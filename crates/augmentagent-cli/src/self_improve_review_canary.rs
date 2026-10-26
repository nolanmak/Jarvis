//! A real native review of an immutable seeded regression, with durable daily
//! accounting. Coverage means evidence supplied, never proof of model attention.
use super::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
const CRITERION: &str = "Adding two integer inputs returns their arithmetic sum.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Report {
    pub run_id: String,
    pub started_ms: i64,
    pub finished_ms: i64,
    pub status: String,
    pub detail: String,
    pub evidence: Option<crate::autoship_review::ReviewEvidence>,
}

fn database() -> Result<PathBuf> {
    Ok(workflow::path()
        .parent()
        .context("workflow state parent missing")?
        .join("review-canaries.db"))
}
fn connect(path: &Path, read_only: bool) -> Result<rusqlite::Connection> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    let parent = path.parent().context("canary state parent missing")?;
    if !read_only {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let parent = parent.canonicalize()?;
    let directory = std::fs::metadata(&parent)?;
    anyhow::ensure!(
        directory.uid() == unsafe { libc::geteuid() }
            && directory.permissions().mode() & 0o022 == 0,
        "canary control directory must be daemon owned and not externally writable"
    );
    let path = parent.join(
        path.file_name()
            .context("canary database filename missing")?,
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .create(!read_only)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?;
    let before = file.metadata()?;
    anyhow::ensure!(
        before.is_file() && before.nlink() == 1 && before.uid() == unsafe { libc::geteuid() },
        "canary database must be a daemon owned singly linked regular file"
    );
    if read_only {
        anyhow::ensure!(
            before.permissions().mode() & 0o077 == 0,
            "canary database is not private"
        );
    } else {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    let access = if read_only {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        access | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let after = std::fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        after.is_file()
            && before.dev() == after.dev()
            && before.ino() == after.ino()
            && after.nlink() == 1
            && after.uid() == before.uid(),
        "canary database changed while opening"
    );
    conn.busy_timeout(Duration::from_secs(5))?;
    if read_only {
        conn.pragma_update(None, "query_only", true)?;
    } else {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
    }
    Ok(conn)
}
fn open(path: &Path) -> Result<rusqlite::Connection> {
    let conn = connect(path, false)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS review_canaries (run_id TEXT PRIMARY KEY, started_ms INTEGER NOT NULL, finished_ms INTEGER, status TEXT NOT NULL, report TEXT);
        CREATE TRIGGER IF NOT EXISTS canary_final_is_immutable BEFORE UPDATE ON review_canaries
        WHEN OLD.status != 'running' OR NEW.run_id != OLD.run_id OR NEW.started_ms != OLD.started_ms
          OR NEW.status NOT IN ('passed','drift','incomplete','interrupted')
        BEGIN SELECT RAISE(ABORT,'canary completion is immutable'); END;
        CREATE TRIGGER IF NOT EXISTS canary_audit_is_retained BEFORE DELETE ON review_canaries
        BEGIN SELECT RAISE(ABORT,'canary audit is append only'); END;")?;
    Ok(conn)
}
fn claim(path: &Path, now: i64) -> Result<Option<String>> {
    let mut conn = open(path)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let last: Option<i64> =
        tx.query_row("SELECT MAX(started_ms) FROM review_canaries", [], |row| {
            row.get(0)
        })?;
    if last.is_some_and(|last| now.saturating_sub(last) < INTERVAL_MS) {
        return Ok(None);
    }
    // Preserve the unfinished occurrence as incomplete, never infer a pass.
    tx.execute(
        "UPDATE review_canaries SET status='interrupted' WHERE status='running'",
        [],
    )?;
    let id = uuid::Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO review_canaries(run_id,started_ms,status) VALUES (?1,?2,'running')",
        rusqlite::params![id, now],
    )?;
    tx.commit()?;
    Ok(Some(id))
}
fn record(path: &Path, report: &Report) -> Result<()> {
    anyhow::ensure!(open(path)?.execute("UPDATE review_canaries SET finished_ms=?2,status=?3,report=?4 WHERE run_id=?1 AND status='running'",
        rusqlite::params![report.run_id,report.finished_ms,report.status,serde_json::to_string(report)?])? == 1,
        "canary occurrence is no longer current");
    Ok(())
}

/// Safe to invoke on every scheduler tick: at most one native probe per day,
/// including failed/interrupted occurrences. Findings are reports, not a hidden
/// unanimity gate on unrelated production PRs.
pub(super) async fn run_if_due() -> Result<Option<Report>> {
    let path = database()?;
    let now = chrono::Utc::now().timestamp_millis();
    let Some(id) = claim(&path, now)? else {
        return Ok(None);
    };
    let result = probe(&id).await;
    let (status, detail, evidence) = match result {
        Ok(evidence) => {
            let passed = detected_seed(&evidence);
            (
                if passed { "passed" } else { "drift" },
                if passed {
                    "Seeded zero-check regression detected; complete supplied evidence and exact-head integrity verified.".into()
                } else {
                    "Native review did not identify the seeded zero-check shipping regression."
                        .into()
                },
                Some(evidence),
            )
        }
        Err(error) => ("incomplete", format!("{error:#}"), None),
    };
    let report = Report {
        run_id: id,
        started_ms: now,
        finished_ms: chrono::Utc::now().timestamp_millis(),
        status: status.into(),
        detail,
        evidence,
    };
    record(&path, &report)?;
    Ok(Some(report))
}

/// Bounded digest summary; the complete structured receipt remains in the
/// protected database and workflow audit rather than being copied into chat.
pub(crate) fn summary() -> Result<String> {
    use rusqlite::OptionalExtension;
    let path = database()?;
    if !path.exists() {
        return Ok("Native review canary: not yet run.".into());
    }
    let row: Option<(i64, String)> = connect(&path, true)?
        .query_row(
            "SELECT started_ms,status FROM review_canaries ORDER BY started_ms DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((started,status)) => format!("Native Codex review canary: {status}; age {} hours. Measures seeded-defect detection and supplied-evidence integrity.", chrono::Utc::now().timestamp_millis().saturating_sub(started)/3_600_000),
        None => "Native review canary: not yet run.".into(),
    })
}

fn detected_seed(evidence: &crate::autoship_review::ReviewEvidence) -> bool {
    // Deterministic rubric: a completed independent pass must name the known
    // zero-check boundary and shipping/check predicate. Generic rejection or a
    // transport failure does not count as catching the planted defect.
    let mut structure = evidence.clone();
    for pass in &mut structure.passes {
        pass.findings.clear();
    }
    structure.adjudication = None;
    structure.authority().is_ok()
        && evidence.authority().is_err()
        && evidence
            .passes
            .iter()
            .flat_map(|p| &p.findings)
            .any(|finding| {
                let text = format!("{} {}", finding.claim, finding.requested_change).to_lowercase();
                (text.contains("zero") || text.contains("empty") || text.contains("0"))
                    && (text.contains("checks_complete")
                        || text.contains("ship")
                        || text.contains("total"))
            })
}

async fn git_fixture(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    anyhow::ensure!(
        out.status.success(),
        "canary git: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

async fn probe(id: &str) -> Result<crate::autoship_review::ReviewEvidence> {
    use augmentagent_channel_core::ProviderKind::Codex;
    let scratch = tempfile::tempdir()?;
    let root = scratch.path().join("candidate");
    let control = workflow::path()
        .parent()
        .context("workflow state parent missing")?
        .join("review-canary-artifacts")
        .join(id);
    std::fs::create_dir(&root)?;
    std::fs::create_dir_all(&control)?;
    git_fixture(&root, &["init", "-q"]).await?;
    git_fixture(&root, &["config", "user.name", "Synthetic Canary"]).await?;
    git_fixture(&root, &["config", "user.email", "canary@example.invalid"]).await?;
    std::fs::write(
        root.join("arithmetic.py"),
        "def add(a, b):\n    return a + b - 1\n",
    )?;
    std::fs::write(root.join("rules.rs"), "pub fn checks_complete(done: usize, total: usize) -> bool { total > 0 && done == total }\n")?;
    std::fs::write(root.join("shipping.rs"), "// Empty check sets must never authorize shipping.\npub fn should_ship(done: usize, total: usize) -> bool { crate::rules::checks_complete(done, total) }\n")?;
    git_fixture(&root, &["add", "."]).await?;
    git_fixture(&root, &["commit", "-qm", "baseline"]).await?;
    git_fixture(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]).await?;
    let proposal = acceptance::parse_proposal(
        &format!(
            "```autoship-contract\n{}\n```",
            serde_json::json!({
                "contract":{"schema_version":1,"revision":1,"issue":840,
                    "criteria":[{"id":"SUM","requirement":CRITERION,"verifier_ids":["sum"]}],
                    "allowed_paths":[{"kind":"file","path":"arithmetic.py"},{"kind":"file","path":"rules.rs"}],
                    "verifiers":[{"id":"sum","command":{"kind":"python_unittest","test":"autoship_verifiers.test_sum.Case.test_add"},"pinned_files":["autoship_verifiers/test_sum.py"],"failure_witness":"arithmetic sum regression"}]},
                "files":{"autoship_verifiers/test_sum.py":"import unittest\nfrom arithmetic import add\nclass Case(unittest.TestCase):\n def test_add(self):\n  for a,b in [(2,3),(-2,5),(0,0)]:\n   self.assertEqual(add(a,b),a+b,'arithmetic sum regression')\n"}
            })
        ),
        840,
    )?;
    let baseline = acceptance::register_and_reproduce(&root, proposal.clone()).await?;
    acceptance::persist_baseline(&control.join("baseline.json"), &baseline, &[root.clone()])?;
    let prepared = acceptance::PreparedAcceptance { proposal, baseline };
    std::fs::write(
        root.join("arithmetic.py"),
        "def add(a, b):\n    return a + b\n",
    )?;
    std::fs::write(
        root.join("rules.rs"),
        "pub fn checks_complete(done: usize, total: usize) -> bool { done >= total }\n",
    )?;
    git_fixture(&root, &["add", "."]).await?;
    git_fixture(&root, &["commit", "-qm", "candidate"]).await?;
    let reference =
        acceptance::verify_and_persist_candidate(&root, &prepared, &control, &[root.clone()])
            .await?;
    let repository = format!("autoship-canary/{id}");
    let mut guard = workflow::WorkflowGuard::acquire(workflow::Start {
        repo_root: &root,
        worktree: &root,
        repository: &repository,
        source_issue: Some(840),
        pr: None,
        branch: "canary",
        contract: "native-review-canary-v1",
        environment: "synthetic-canary",
    })
    .await?
    .context("canary workflow lease unavailable")?;
    guard.bind_acceptance(&prepared)?;
    guard
        .checkpoint(
            crate::autoship_workflow::Stage::Review,
            "canary",
            "review seeded candidate",
            0,
            &[serde_json::to_string(&reference)?],
        )
        .await?;
    let origin = adoption::origin_for_candidate(&root, Some(&guard)).await?;
    let issue = Issue {
        number: 840,
        title: "Correct integer addition".into(),
        body: CRITERION.into(),
        author: "synthetic-canary".into(),
        author_trusted: true,
        research_filed: false,
    };
    // Controller owns this synthetic fixture; no claim that a model built it.
    let result = aligned_review::run_inner(
        Codex,
        &[],
        &aligned_review::ReviewSubject {
            title: &issue.title,
            body: &issue.body,
        },
        root.clone(),
        &[CRITERION.into()],
        aligned_review::ReviewContext {
            qa_ok: true,
            qa_notes: "Pinned arithmetic regression passed.",
            repair_round: 0,
            previous: None,
            environment_id: "native-review-canary-v1",
            adjudication_attempts_already_made: 0,
            workflow: Some(&guard),
            acceptance_evidence: Some((&prepared, &reference)),
        },
        origin,
    )
    .await;
    guard.audit_health().await?;
    guard.finish()?;
    result?
        .structured
        .context("canary native review did not return structured evidence")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn private_fixture() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // tempfile uses the process umask on this host (0002), unlike the
        // controller-owned state boundary which requires no group writes.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    #[test]
    fn daily_canary_claim_survives_crash_and_never_records_false_completion() {
        let dir = private_fixture();
        let path = dir.path().join("canaries.db");
        let id = claim(&path, 100).unwrap().unwrap();
        assert!(claim(&path, 101).unwrap().is_none());
        let next = claim(&path, INTERVAL_MS + 101).unwrap().unwrap();
        assert_ne!(id, next);
        let state: String = open(&path)
            .unwrap()
            .query_row(
                "SELECT status FROM review_canaries WHERE run_id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "interrupted");
        assert!(record(
            &path,
            &Report {
                run_id: id,
                started_ms: 100,
                finished_ms: 200,
                status: "passed".into(),
                detail: "late writer".into(),
                evidence: None
            }
        )
        .is_err());
    }
    #[test]
    fn canary_store_is_private_read_only_when_reported_and_completions_are_immutable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_fixture();
        let path = dir.path().join("first-start/control/canaries.db");
        let id = claim(&path, 100).unwrap().unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let conn = open(&path).unwrap();
        let sync: i64 = conn
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2);
        let report = Report {
            run_id: id.clone(),
            started_ms: 100,
            finished_ms: 200,
            status: "incomplete".into(),
            detail: "fixture".into(),
            evidence: None,
        };
        record(&path, &report).unwrap();
        assert!(record(&path, &report).is_err());
        assert!(conn
            .execute(
                "UPDATE review_canaries SET status='passed' WHERE run_id=?1",
                [&id]
            )
            .is_err());
        assert!(conn.execute("DELETE FROM review_canaries", []).is_err());
        let reader = connect(&path, true).unwrap();
        assert!(reader
            .execute("CREATE TABLE unexpected(value INTEGER)", [])
            .is_err());
        assert_eq!(
            reader
                .query_row(
                    "SELECT status FROM review_canaries WHERE run_id=?1",
                    [&id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "incomplete"
        );
        let absent = dir.path().join("absent/canaries.db");
        assert!(connect(&absent, true).is_err());
        assert!(!absent.parent().unwrap().exists());
    }

    #[test]
    fn canary_store_refuses_redirected_and_multiply_linked_control_files() {
        use std::os::unix::fs::symlink;
        let dir = private_fixture();
        let real = dir.path().join("real.db");
        open(&real).unwrap();
        let link = dir.path().join("link.db");
        symlink(&real, &link).unwrap();
        assert!(open(&link).is_err());
        assert!(connect(&link, true).is_err());
        std::fs::remove_file(link).unwrap();
        std::fs::hard_link(&real, dir.path().join("hard.db")).unwrap();
        assert!(open(&real).is_err());
        assert!(connect(&real, true).is_err());
    }

    #[test]
    fn existing_group_writable_control_directory_is_rejected_without_chmod() {
        use std::os::unix::fs::PermissionsExt;
        let dir = private_fixture();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
        assert!(open(&dir.path().join("canaries.db")).is_err());
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o770
        );
        assert!(!dir.path().join("canaries.db").exists());
    }

    #[tokio::test]
    #[ignore = "paid real native Codex review; requires configured authentication and protected state"]
    async fn live_native_codex_detects_seed_through_production_adapter() {
        let evidence = probe(&uuid::Uuid::new_v4().to_string()).await.unwrap();
        assert!(
            detected_seed(&evidence),
            "seeded defect was not detected: {}",
            serde_json::to_string_pretty(&evidence).unwrap()
        );
    }
}
