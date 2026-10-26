//! Free admission for already-reviewed PRs. Approval is durable; holds are
//! derived from current policy facts, never from dates or model-written text.
use super::*;

fn status_db(path: &Path) -> Result<rusqlite::Connection> {
    // Initialize the same protected workflow database and its permissions.
    let _ = crate::autoship_workflow::WorkflowStore::open(path)?;
    let conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS policy_hold_status (
        repository TEXT NOT NULL, pr INTEGER NOT NULL, head TEXT NOT NULL,
        reason TEXT NOT NULL, observed_ms INTEGER NOT NULL,
        PRIMARY KEY(repository,pr))",
    )?;
    Ok(conn)
}

fn observe(
    conn: &rusqlite::Connection,
    repository: &str,
    pr: u64,
    head: &str,
    blocker: Option<&str>,
    now: i64,
) -> Result<()> {
    if let Some(blocker) = blocker {
        conn.execute(
            "INSERT INTO policy_hold_status VALUES (?1,?2,?3,?4,?5)
            ON CONFLICT(repository,pr) DO UPDATE SET head=excluded.head,
            reason=excluded.reason,observed_ms=excluded.observed_ms",
            rusqlite::params![repository, pr, head, blocker, now],
        )?;
    } else {
        conn.execute(
            "DELETE FROM policy_hold_status WHERE repository=?1 AND pr=?2",
            rusqlite::params![repository, pr],
        )?;
    }
    Ok(())
}

pub(super) fn statuses() -> Vec<(u64, String)> {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        workflow::path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return vec![];
    };
    let Ok(mut statement) =
        conn.prepare("SELECT pr,reason FROM policy_hold_status WHERE observed_ms>=?1 ORDER BY pr")
    else {
        return vec![];
    };
    // Historical observations must not conceal a newly stalled PR when the
    // reconciler itself has stopped. Approval still persists independently.
    let cutoff = chrono::Utc::now().timestamp_millis() - 90 * 60 * 1000;
    let Ok(rows) = statement.query_map([cutoff], |row| Ok((row.get(0)?, row.get(1)?))) else {
        return vec![];
    };
    rows.filter_map(|row| row.ok()).collect()
}

fn reason(
    record: Option<&OpenedPr>,
    pr: u64,
    head: &str,
    policy: &MergePolicy,
) -> Option<&'static str> {
    if !recorded_approval(record, pr, Some(head)).reviewed {
        return None;
    }
    merge_policy_blocker(policy)
}

pub(super) async fn filter(repo: &Path, candidates: &mut Vec<adoption::AdoptedPr>, dry_run: bool) {
    let owner = merge_policy_owner(repo).await;
    let status = if dry_run {
        None
    } else {
        status_db(&workflow::path()).ok()
    };
    let mut eligible = Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        let Some(issue) = candidate.source_issue else {
            eligible.push(candidate);
            continue;
        };
        let record = opened_prs::get(issue);
        let approval = recorded_approval(record.as_ref(), candidate.pr, Some(&candidate.head));
        if !approval.reviewed {
            if let Some(conn) = &status {
                let _ = observe(
                    conn,
                    &candidate.repository,
                    candidate.pr,
                    &candidate.head,
                    None,
                    chrono::Utc::now().timestamp_millis(),
                );
            }
            eligible.push(candidate);
            continue;
        }
        let (body, author) = issue_facts(repo, issue).await;
        let gated = pr_gated_path(repo, candidate.pr).await;
        // Missing remote evidence is an infrastructure dependency, never a
        // reason to pay for another review of the identical approved patch.
        let unavailable = author.is_empty() || gated.as_deref() == Some(UNREADABLE_DIFF);
        let policy = MergePolicy {
            automerge_enabled: automerge_enabled_value(
                std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE")
                    .ok()
                    .as_deref(),
            ),
            complexity: complexity_from_pr_body(&candidate.body),
            codex_approved: approval.codex,
            reviews_approved: approval.reviewed,
            research_filed: is_research_filed(&body),
            receipt_gated_file: gated,
            lgtm_overrides_receipt: std::env::var("AUGMENTAGENT_AUTOPR_LGTM_OVERRIDES_RECEIPT")
                .ok(),
            issue_author: author,
            repo_owner: owner.clone(),
            automerge_authors: std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE_AUTHORS").ok(),
        };
        let blocker = if unavailable {
            Some("policy-evidence-unavailable")
        } else {
            reason(record.as_ref(), candidate.pr, &candidate.head, &policy)
        };
        if let Some(conn) = &status {
            if let Err(error) = observe(
                conn,
                &candidate.repository,
                candidate.pr,
                &candidate.head,
                blocker,
                chrono::Utc::now().timestamp_millis(),
            ) {
                warn!(
                    pr = candidate.pr,
                    "policy hold health observation failed: {error:#}"
                );
            }
        }
        if let Some(blocker) = blocker {
            info!(pr=candidate.pr, issue, head=%candidate.head, blocker,
                "autoship policy-held: approved head excluded from billed resume; free reconciliation retries dependencies");
        } else {
            eligible.push(candidate);
        }
    }
    *candidates = eligible;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approved() -> OpenedPr {
        OpenedPr {
            pr: 42,
            head: "approved-head".into(),
            verdict: Some(RecordedVerdict {
                provider: "codex".into(),
                backend: "native".into(),
                model: "test-native".into(),
                diff_ok: true,
                system_ok: true,
                structured: None,
            }),
        }
    }
    fn policy() -> MergePolicy {
        MergePolicy {
            automerge_enabled: false,
            complexity: Complexity::Simple,
            codex_approved: true,
            reviews_approved: true,
            research_filed: false,
            receipt_gated_file: None,
            lgtm_overrides_receipt: None,
            issue_author: "owner".into(),
            repo_owner: Some("owner".into()),
            automerge_authors: None,
        }
    }

    #[test]
    fn unchanged_approval_holds_across_restart_and_days_without_attempts() {
        let saved = serde_json::to_string(&approved()).unwrap();
        for _day in 0..3 {
            let restored = serde_json::from_str(&saved).unwrap();
            assert_eq!(
                reason(Some(&restored), 42, "approved-head", &policy()),
                Some("automerge-disabled")
            );
        }
    }

    #[test]
    fn new_head_missing_approval_and_other_pr_are_not_policy_held() {
        let record = approved();
        assert_eq!(reason(Some(&record), 42, "changed-head", &policy()), None);
        assert_eq!(reason(Some(&record), 43, "approved-head", &policy()), None);
        assert_eq!(reason(None, 42, "approved-head", &policy()), None);
        let rejected = OpenedPr {
            verdict: None,
            ..record
        };
        assert_eq!(
            reason(Some(&rejected), 42, "approved-head", &policy()),
            None
        );
    }

    #[test]
    fn policy_and_receipt_changes_release_without_new_review() {
        let record = approved();
        let mut p = policy();
        p.automerge_enabled = true;
        assert_eq!(reason(Some(&record), 42, "approved-head", &p), None);
        p.receipt_gated_file = Some("protected.rs".into());
        assert_eq!(
            reason(Some(&record), 42, "approved-head", &p),
            Some("receipt-required")
        );
        p.lgtm_overrides_receipt = Some("1".into());
        assert_eq!(reason(Some(&record), 42, "approved-head", &p), None);
        p.issue_author = "untrusted".into();
        assert_eq!(
            reason(Some(&record), 42, "approved-head", &p),
            Some("author-authorization-required")
        );
        p.automerge_authors = Some("untrusted".into());
        assert_eq!(reason(Some(&record), 42, "approved-head", &p), None);
    }

    #[test]
    fn health_observation_survives_restart_and_clears_on_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.sqlite");
        let conn = status_db(&path).unwrap();
        observe(
            &conn,
            "owner/repo",
            42,
            "head",
            Some("receipt-required"),
            100,
        )
        .unwrap();
        drop(conn);
        let conn = status_db(&path).unwrap();
        let row: (String, String) = conn
            .query_row(
                "SELECT head,reason FROM policy_hold_status WHERE pr=42",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("head".into(), "receipt-required".into()));
        observe(&conn, "owner/repo", 42, "new-head", None, 200).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM policy_hold_status", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}
