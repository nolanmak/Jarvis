//! Durable admission for the shared command service. Channel adapters must
//! authorize the caller and target before admission. This module never calls a
//! provider or grants publication authority; dispatch must enforce its policy.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Milestone {
    Pr,
    Merged,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Codex,
    Claude,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "number", rename_all = "snake_case")]
pub enum Target {
    Issue(u64),
    Pr(u64),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub repository: String,
    pub target: Target,
    pub provider: Option<Provider>,
    pub until: Milestone,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub schema_version: u32,
    pub workflow: String,
    pub accepted: bool,
    pub complete: bool,
    pub joined: bool,
    pub reason: String,
    pub requested_until: Milestone,
    pub workflow_until: Milestone,
    pub provider: Option<Provider>,
}

impl Request {
    fn normalized(&self) -> Result<Self> {
        ensure!(
            self.repository.len() <= 256,
            "repository identity exceeds bound"
        );
        let parts: Vec<_> = self.repository.split('/').collect();
        ensure!(
            parts.len() == 2
                && parts.iter().all(|part| !part.is_empty()
                    && !matches!(*part, "." | "..")
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
            "invalid repository identity"
        );
        let number = match self.target {
            Target::Issue(n) | Target::Pr(n) => n,
        };
        ensure!(
            number > 0 && number <= i64::MAX as u64,
            "invalid target number"
        );
        let mut request = self.clone();
        request.repository.make_ascii_lowercase();
        Ok(request)
    }
    fn identity(&self) -> Result<String> {
        match self.target {
            Target::Issue(n) => workflow_id(&self.repository, n),
            Target::Pr(n) => pr_workflow_id(&self.repository, n),
        }
    }
}

/// A policy ceiling is not approval: owner trust, leases, exact-head review,
/// CI, resource admission and global opt-in remain independently required.
fn policy(conn: &Connection, workflow: &str) -> Result<Option<(Milestone, Option<Provider>)>> {
    let raw: Option<(String, String)> = conn
        .query_row(
            "SELECT until_milestone,provider FROM command_job WHERE workflow=?1",
            [workflow],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    raw.map(|(until, provider)| {
        Ok((
            serde_json::from_str(&until)?,
            serde_json::from_str(&provider)?,
        ))
    })
    .transpose()
}

impl WorkflowStore {
    /// Read-only and compatible with stores created before command admission.
    pub fn command_builder(&self, workflow: &str) -> Result<Option<Provider>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='command_job')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        Ok(policy(&self.conn, workflow)?.and_then(|(_, provider)| provider))
    }

    pub fn require_command_builder(&self, workflow: &str, actual: Option<Provider>) -> Result<()> {
        if let Some(expected) = self.command_builder(workflow)? {
            ensure!(
                actual == Some(expected),
                "workflow-builder-provider-mismatch"
            );
        }
        Ok(())
    }

    /// The caller has authenticated the principal and authorized this target.
    /// One immediate transaction serializes CLI, Discord and worker joins. No
    /// workflow lease, model, Git operation or merge capacity is acquired here.
    pub fn submit_authorized(
        &mut self,
        request_id: &str,
        principal: &str,
        request: &Request,
        now: i64,
    ) -> Result<Admission> {
        ensure!(
            !request_id.is_empty()
                && request_id.len() <= 128
                && request_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b)),
            "invalid request ID"
        );
        ensure!(
            !principal.is_empty()
                && principal.len() <= 256
                && principal
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:@".contains(&b)),
            "invalid authorized principal"
        );
        let request = request.normalized()?;
        let payload = serde_json::to_string(&request)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<(String, String, String)> = tx
            .query_row(
                "SELECT principal,payload,admission FROM command_request WHERE request_id=?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((actor, original, admission)) = prior {
            ensure!(
                actor == principal && original == payload,
                "request-id-conflict"
            );
            return Ok(serde_json::from_str(&admission)?);
        }
        let identity = request.identity()?;
        let workflow = if matches!(request.target, Target::Pr(_)) {
            tx.query_row(
                "SELECT workflow FROM pr_alias WHERE id=?1",
                [&identity],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or(identity)
        } else {
            identity
        };
        let queued = policy(&tx, &workflow)?;
        let registered: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1)",
            [&workflow],
            |row| row.get(0),
        )?;
        let joined = queued.is_some() || registered;
        // Existing pre-command workflows had no command-specific provider pin
        // or PR-only ceiling. Joining them never changes the existing pipeline's
        // merge eligibility; all its original gates still apply.
        let (until, provider) = queued.unwrap_or(if registered {
            (Milestone::Merged, None)
        } else {
            (request.until, request.provider)
        });
        let reason = if joined && request.provider.is_some() && request.provider != provider {
            "provider-change-required"
        } else if joined && until == Milestone::Pr && request.until == Milestone::Merged {
            "authorization-upgrade-required"
        } else if joined {
            "joined"
        } else {
            "queued"
        };
        let admission = Admission {
            schema_version: 1,
            workflow: workflow.clone(),
            accepted: matches!(reason, "queued" | "joined"),
            complete: false,
            joined,
            reason: reason.into(),
            requested_until: request.until,
            workflow_until: until,
            provider,
        };
        if admission.accepted {
            tx.execute("INSERT INTO command_job(workflow,repository,target,until_milestone,provider,created_at)
                VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(workflow) DO NOTHING",
                params![workflow,request.repository,serde_json::to_string(&request.target)?,serde_json::to_string(&until)?,serde_json::to_string(&provider)?,now])?;
        }
        tx.execute("INSERT INTO command_request(request_id,principal,payload,admission,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![request_id,principal,payload,serde_json::to_string(&admission)?,now])?;
        tx.commit()?;
        Ok(admission)
    }
}

/// Check inside the merge admission transaction, before reserving or sending.
/// A late PR alias collision is held rather than bypassing a submitted ceiling.
pub(super) fn require_merge_authority(
    conn: &Connection,
    workflow: &str,
    repository: &str,
    pr: u64,
) -> Result<()> {
    let alias = pr_workflow_id(repository, pr)?;
    ensure!(
        alias == workflow || policy(conn, &alias)?.is_none(),
        "submission-alias-conflict"
    );
    ensure!(
        policy(conn, workflow)?.is_none_or(|(until, _)| until == Milestone::Merged),
        "workflow-authorized-until-pr"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(until: Milestone) -> Request {
        Request {
            repository: "Owner/Repo".into(),
            target: Target::Pr(42),
            provider: Some(Provider::Codex),
            until,
        }
    }
    #[test]
    fn enqueue_survives_restart_and_retry_replays_original_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let original = WorkflowStore::open(&path)
            .unwrap()
            .submit_authorized("cli-1", "owner", &request(Milestone::Merged), 10)
            .unwrap();
        assert!(original.accepted && !original.complete && !original.joined);
        assert_eq!(original.workflow, "owner/repo/pr/42");
        let replay = WorkflowStore::open(&path)
            .unwrap()
            .submit_authorized("cli-1", "owner", &request(Milestone::Merged), 100)
            .unwrap();
        assert_eq!(original, replay);
    }
    #[test]
    fn join_cannot_upgrade_merge_authority_or_change_builder() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let original = store
            .submit_authorized("pr-only", "owner", &request(Milestone::Pr), 1)
            .unwrap();
        assert!(original.accepted);
        let upgrade = store
            .submit_authorized("join-merged", "owner", &request(Milestone::Merged), 2)
            .unwrap();
        assert!(!upgrade.accepted);
        assert_eq!(upgrade.reason, "authorization-upgrade-required");
        let mut changed = request(Milestone::Pr);
        changed.provider = Some(Provider::Claude);
        let changed = store
            .submit_authorized("join-provider", "owner", &changed, 3)
            .unwrap();
        assert!(!changed.accepted);
        assert_eq!(changed.reason, "provider-change-required");
        assert_eq!(
            store
                .submit_authorized("pr-only", "owner", &request(Milestone::Pr), 4)
                .unwrap(),
            original
        );
    }
    #[test]
    fn pr_observer_does_not_downgrade_existing_merge_authority() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        store
            .submit_authorized("first", "owner", &request(Milestone::Merged), 1)
            .unwrap();
        let joined = store
            .submit_authorized("discord-join", "owner", &request(Milestone::Pr), 2)
            .unwrap();
        assert!(joined.accepted && joined.joined && !joined.complete);
        assert_eq!(joined.workflow_until, Milestone::Merged);
        assert_eq!(joined.requested_until, Milestone::Pr);
    }
    #[test]
    fn request_id_payload_or_principal_conflicts_do_not_change_submission() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let original = store
            .submit_authorized("request", "owner", &request(Milestone::Merged), 1)
            .unwrap();
        assert!(store
            .submit_authorized("request", "owner", &request(Milestone::Pr), 2)
            .is_err());
        assert!(store
            .submit_authorized("request", "different-owner", &request(Milestone::Merged), 3)
            .is_err());
        assert_eq!(
            store
                .submit_authorized("request", "owner", &request(Milestone::Merged), 4)
                .unwrap(),
            original
        );
    }
    fn merge_intent(store: &mut WorkflowStore, id: &str) -> (Lease, Operation) {
        let binding = Binding {
            head: "a".repeat(40),
            contract: "contract-v1".into(),
            evidence_hash: "review-evidence".into(),
        };
        store.register(id, "repair", binding.clone(), 10).unwrap();
        let lease = store.claim(id, "worker", 10, 10000).unwrap().unwrap();
        store
            .attach_pr_in_repo(&lease, "owner/repo", 42, 10)
            .unwrap();
        let op = store
            .begin_operation(&lease, OperationKind::MergePr, &binding, 10)
            .unwrap();
        (lease, op)
    }

    #[test]
    fn pr_only_submission_cannot_reserve_merge_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let admission = store
            .submit_authorized("pr-only", "owner", &request(Milestone::Pr), 1)
            .unwrap();
        let (lease, op) = merge_intent(&mut store, &admission.workflow);
        let result = store.reserve_merge(&lease, &op, "owner/repo", 42, "PR_42", 11, 5);
        assert_eq!(
            result.unwrap_err().to_string(),
            "workflow-authorized-until-pr"
        );
        assert!(store.merge_dispatch(&op.id).unwrap().is_none());
        assert_eq!(store.merge_allowance(11, 5).unwrap().available, 5);
    }

    #[test]
    fn dispatch_rechecks_policy_after_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let admission = store
            .submit_authorized("merge", "owner", &request(Milestone::Merged), 1)
            .unwrap();
        let (lease, op) = merge_intent(&mut store, &admission.workflow);
        store
            .reserve_merge(&lease, &op, "owner/repo", 42, "PR_42", 11, 5)
            .unwrap();
        // Simulate a policy transition at the control boundary between durable
        // reservation and sending. No transport call may follow this ceiling.
        store
            .conn
            .execute(
                "UPDATE command_job SET until_milestone=?1 WHERE workflow=?2",
                params![
                    serde_json::to_string(&Milestone::Pr).unwrap(),
                    admission.workflow
                ],
            )
            .unwrap();
        assert!(
            store.dispatch_marked_merge(&lease, &op, 12, 5).is_err(),
            "dispatch ignored a PR-only ceiling"
        );
        assert_eq!(
            store.merge_dispatch(&op.id).unwrap().unwrap().state,
            "reserved"
        );
    }

    #[test]
    fn simultaneous_surfaces_share_one_durable_job() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(WorkflowStore::open(&path).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["cli", "discord"]
            .into_iter()
            .map(|id| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut store = WorkflowStore::open(&path).unwrap();
                    barrier.wait();
                    store
                        .submit_authorized(id, "owner", &request(Milestone::Merged), 10)
                        .unwrap()
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(results.iter().all(|r| r.accepted && !r.complete));
        assert_eq!(results.iter().filter(|r| !r.joined).count(), 1);
        assert_eq!(results[0].workflow, results[1].workflow);
        let store = WorkflowStore::open(&path).unwrap();
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM command_job", [], |r| r
                    .get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM command_request", [], |r| r
                    .get::<_, u32>(0))
                .unwrap(),
            2
        );
        assert!(
            store.snapshot(&results[0].workflow).unwrap().is_none(),
            "enqueue must not fabricate an executing workflow"
        );
    }

    #[test]
    fn existing_pr_alias_joins_original_workflow_without_downgrade() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let (lease, _) = merge_intent(&mut store, "owner/repo#7");
        store
            .attach_pr_in_repo(&lease, "owner/repo", 42, 11)
            .unwrap();
        let mut observer = request(Milestone::Pr);
        observer.provider = None;
        let joined = store
            .submit_authorized("observer", "owner", &observer, 12)
            .unwrap();
        assert_eq!(joined.workflow, "owner/repo#7");
        assert!(joined.accepted && joined.joined);
        assert_eq!(joined.workflow_until, Milestone::Merged);
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().generation,
            lease.generation
        );
    }

    #[test]
    fn invalid_target_does_not_create_a_job_or_reserve_request_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        for repository in ["owner/../repo", "owner/repo;command", "owner/", "/repo"] {
            let mut invalid = request(Milestone::Merged);
            invalid.repository = repository.into();
            assert!(store
                .submit_authorized("invalid", "owner", &invalid, 1)
                .is_err());
        }
        let mut invalid = request(Milestone::Merged);
        invalid.target = Target::Issue(0);
        assert!(store
            .submit_authorized("invalid", "owner", &invalid, 1)
            .is_err());
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM command_request", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM command_job", [], |row| row
                    .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn rejected_request_replays_original_result_after_policy_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        store
            .submit_authorized("pr-only", "owner", &request(Milestone::Pr), 1)
            .unwrap();
        let rejected = store
            .submit_authorized("join", "owner", &request(Milestone::Merged), 2)
            .unwrap();
        assert!(!rejected.accepted);
        store
            .conn
            .execute(
                "UPDATE command_job SET until_milestone=?1",
                [serde_json::to_string(&Milestone::Merged).unwrap()],
            )
            .unwrap();
        assert_eq!(
            store
                .submit_authorized("join", "owner", &request(Milestone::Merged), 3)
                .unwrap(),
            rejected
        );
        assert!(
            store
                .submit_authorized("new-join", "owner", &request(Milestone::Merged), 4)
                .unwrap()
                .accepted
        );
    }

    #[test]
    fn late_alias_binding_cannot_erase_a_submitted_pr_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        store
            .submit_authorized("pr-only", "owner", &request(Milestone::Pr), 1)
            .unwrap();
        let binding = Binding {
            head: "a".repeat(40),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register("owner/repo#7", "repair", binding, 2)
            .unwrap();
        let lease = store
            .claim("owner/repo#7", "worker", 2, 10000)
            .unwrap()
            .unwrap();
        let error = store
            .attach_pr_in_repo(&lease, "owner/repo", 42, 3)
            .unwrap_err();
        assert_eq!(error.to_string(), "submission-alias-conflict");
        assert!(store.resolve_pr("owner/repo", 42).unwrap().is_none());
        assert_eq!(
            policy(&store.conn, "owner/repo/pr/42").unwrap().unwrap().0,
            Milestone::Pr
        );
    }
    #[test]
    fn committed_enqueue_survives_client_exit_before_response() {
        const CHILD: &str = "AUTOSHIP_SUBMISSION_EXIT_FIXTURE";
        if let Some(path) = std::env::var_os(CHILD) {
            let mut store = WorkflowStore::open(Path::new(&path)).unwrap();
            store
                .submit_authorized("lost-response", "owner", &request(Milestone::Merged), 10)
                .unwrap();
            // No result is delivered and Rust destructors do not run. The
            // accepted transaction must already be durable at this point.
            unsafe { libc::_exit(73) }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "autoship_workflow::submission::tests::committed_enqueue_survives_client_exit_before_response", "--nocapture"])
            .env(CHILD, &path).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut store = WorkflowStore::open(&path).unwrap();
        let replay = store
            .submit_authorized("lost-response", "owner", &request(Milestone::Merged), 100)
            .unwrap();
        assert!(replay.accepted && !replay.joined && !replay.complete);
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM command_job", [], |r| r
                    .get::<_, u32>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM command_request", [], |r| r
                    .get::<_, u32>(0))
                .unwrap(),
            1
        );
    }
    #[test]
    fn queued_builder_pin_is_read_only_and_cannot_bind_another_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let admission = WorkflowStore::open(&path)
            .unwrap()
            .submit_authorized("pinned", "owner", &request(Milestone::Merged), 1)
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let store = WorkflowStore::open_read_only(&path).unwrap().unwrap();
        assert_eq!(
            store.command_builder(&admission.workflow).unwrap(),
            Some(Provider::Codex)
        );
        assert!(store
            .require_command_builder(&admission.workflow, None)
            .is_err());
        assert!(store
            .require_command_builder(&admission.workflow, Some(Provider::Claude))
            .is_err());
        store
            .require_command_builder(&admission.workflow, Some(Provider::Codex))
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn pre_command_state_has_no_implicit_builder_pin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = WorkflowStore::open(&path).unwrap();
        store.conn.execute("DROP TABLE command_job", []).unwrap();
        drop(store);
        let bytes = std::fs::read(&path).unwrap();
        let store = WorkflowStore::open_read_only(&path).unwrap().unwrap();
        assert_eq!(store.command_builder("owner/repo#1").unwrap(), None);
        store.require_command_builder("owner/repo#1", None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
