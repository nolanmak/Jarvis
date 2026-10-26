//! Durable autoship control-plane state. Workers get fenced leases, not authority
//! from a branch name or model-authored PR body. This store records evidence;
//! callers still enforce authorization, review and CI policy before side effects.
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Intake,
    Scope,
    Build,
    Verify,
    Review,
    Reconcile,
    Publish,
    AwaitChecks,
    Merge,
    Held,
    Retired,
    Merged,
}

/// Stable identity survives branch renames and changed heads. Repository must
/// be the canonical owner/name returned by the trusted repository adapter.
pub fn workflow_id(repo: &str, issue: u64) -> Result<String> {
    ensure!(
        !repo.trim().is_empty() && issue > 0,
        "invalid workflow identity"
    );
    Ok(format!("{}#{issue}", repo.to_ascii_lowercase()))
}

/// PRs adopted without a source issue, or multiple PRs for one issue, each
/// have their own identity. This key does not assert trust or approval.
pub fn pr_workflow_id(repo: &str, pr: u64) -> Result<String> {
    ensure!(
        !repo.trim().is_empty() && pr > 0,
        "invalid PR workflow identity"
    );
    Ok(format!("{}/pr/{pr}", repo.to_ascii_lowercase()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub head: String,
    pub contract: String,
    /// Digest includes reviewed base/tree, verifier receipts and tool environment.
    pub evidence_hash: String,
}
impl Binding {
    fn validate(&self) -> Result<()> {
        ensure!(
            !self.head.trim().is_empty() && self.head.len() <= 256,
            "bounded head is required"
        );
        ensure!(
            !self.contract.trim().is_empty() && self.contract.len() <= 256,
            "contract version is required"
        );
        ensure!(
            !self.evidence_hash.trim().is_empty() && self.evidence_hash.len() <= 256,
            "evidence digest is required"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextAction {
    /// Machine-readable reason, e.g. `ci_pending`, not the model transcript.
    pub reason: String,
    pub action: String,
    pub eligible_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub stage: Stage,
    pub binding: Binding,
    /// Content-addressed receipt IDs; never credentials or private transcripts.
    pub evidence: Vec<String>,
    pub next: NextAction,
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub workflow: String,
    pub owner: String,
    pub generation: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub workflow: String,
    pub branch: String,
    pub pr: Option<u64>,
    pub checkpoint: Checkpoint,
    pub attempts: u64,
    pub wall_ms: u64,
    pub model_calls: u64,
    pub tokens: u64,
    pub heartbeat_at_ms: i64,
    pub progress_at_ms: i64,
    pub lease_until_ms: i64,
    pub generation: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    CreatePr,
    Push,
    MergePr,
    RetirePr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub workflow: String,
    pub kind: OperationKind,
    pub binding: Binding,
    /// Absent means uncertain/pending: reconcile the remote before retrying.
    pub receipt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: String,
    pub binding: Binding,
    pub approved: bool,
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceCharge {
    pub wall_ms: u64,
    pub model_calls: u64,
    pub tokens: u64,
}

pub struct WorkflowStore {
    conn: Connection,
}

impl WorkflowStore {
    pub fn open(path: &Path) -> Result<Self> {
        ensure!(
            path != Path::new(":memory:"),
            "workflow state must be durable"
        );
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        // Create privately before SQLite opens it. Reject links rather than
        // allowing a reasoner-controlled path to redirect control-plane writes.
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "state must be a singly linked regular file"
        );
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "state must be owned by daemon uid"
        );
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS workflow (
                id TEXT PRIMARY KEY, branch TEXT NOT NULL, pr INTEGER,
                checkpoint TEXT NOT NULL, owner TEXT, generation INTEGER NOT NULL DEFAULT 0,
                lease_until INTEGER NOT NULL DEFAULT 0, heartbeat_at INTEGER NOT NULL,
                progress_at INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                wall_ms INTEGER NOT NULL DEFAULT 0, model_calls INTEGER NOT NULL DEFAULT 0,
                tokens INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS adopted_pr (
                repository TEXT NOT NULL, pr INTEGER NOT NULL, head TEXT NOT NULL,
                record TEXT NOT NULL, first_seen INTEGER NOT NULL, observed_at INTEGER NOT NULL,
                PRIMARY KEY(repository,pr));
            CREATE TABLE IF NOT EXISTS adoption_event (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, repository TEXT NOT NULL, pr INTEGER NOT NULL,
                head TEXT NOT NULL, at_ms INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS pr_alias (
                id TEXT PRIMARY KEY, workflow TEXT NOT NULL REFERENCES workflow(id));
            CREATE TABLE IF NOT EXISTS operation (
                id TEXT PRIMARY KEY, workflow TEXT NOT NULL REFERENCES workflow(id),
                kind TEXT NOT NULL, binding TEXT NOT NULL, receipt TEXT, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS review (
                workflow TEXT NOT NULL REFERENCES workflow(id), reviewer TEXT NOT NULL,
                binding TEXT NOT NULL, approved INTEGER NOT NULL, evidence TEXT NOT NULL,
                PRIMARY KEY(workflow, reviewer, binding));
            CREATE TABLE IF NOT EXISTS resource_charge (
                workflow TEXT NOT NULL REFERENCES workflow(id), id TEXT NOT NULL,
                wall_ms INTEGER NOT NULL, model_calls INTEGER NOT NULL, tokens INTEGER NOT NULL,
                PRIMARY KEY(workflow, id));
            CREATE TABLE IF NOT EXISTS bounded_attempt (
                workflow TEXT NOT NULL REFERENCES workflow(id), purpose TEXT NOT NULL, binding TEXT NOT NULL,
                attempts INTEGER NOT NULL, PRIMARY KEY(workflow, purpose, binding));
            CREATE TABLE IF NOT EXISTS workflow_event (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, workflow TEXT NOT NULL REFERENCES workflow(id),
                generation INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, at_ms INTEGER NOT NULL);
            COMMIT;")?;
        Ok(Self { conn })
    }

    /// Ownership observation is separate from execution and approvals. A new
    /// remote commit updates its head without resetting a live worker's lease.
    pub fn observe_adoption(
        &mut self,
        repository: &str,
        pr: u64,
        head: &str,
        record: &str,
        now: i64,
    ) -> Result<bool> {
        ensure!(
            !repository.is_empty() && pr > 0 && !head.is_empty(),
            "adoption identity required"
        );
        ensure!(record.len() <= 128 * 1024, "adoption record too large");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT head FROM adopted_pr WHERE repository=?1 AND pr=?2",
                params![repository, pr],
                |r| r.get(0),
            )
            .optional()?;
        let changed = old.as_deref() != Some(head);
        tx.execute("INSERT INTO adopted_pr(repository,pr,head,record,first_seen,observed_at) VALUES(?1,?2,?3,?4,?5,?5)
            ON CONFLICT(repository,pr) DO UPDATE SET head=excluded.head,record=excluded.record,observed_at=excluded.observed_at",
            params![repository,pr,head,record,now])?;
        if changed {
            tx.execute(
                "INSERT INTO adoption_event(repository,pr,head,at_ms) VALUES(?1,?2,?3,?4)",
                params![repository, pr, head, now],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn adopted_pr(&self, repository: &str, pr: u64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT record FROM adopted_pr WHERE repository=?1 AND pr=?2",
                params![repository, pr],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Register authorized ownership only. Existing registration is not reset
    /// by restart/adoption and never manufactures an approval.
    pub fn register(
        &mut self,
        workflow: &str,
        branch: &str,
        binding: Binding,
        now_ms: i64,
    ) -> Result<bool> {
        binding.validate()?;
        ensure!(
            !workflow.is_empty() && !branch.is_empty(),
            "identity and branch required"
        );
        let checkpoint = Checkpoint {
            stage: Stage::Intake,
            binding,
            evidence: vec![],
            next: NextAction {
                reason: "registered".into(),
                action: "scope".into(),
                eligible_at_ms: now_ms,
            },
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO workflow(id, branch, checkpoint, heartbeat_at, progress_at)
            VALUES (?1,?2,?3,?4,?4) ON CONFLICT(id) DO NOTHING",
            params![
                workflow,
                branch,
                serde_json::to_string(&checkpoint)?,
                now_ms
            ],
        )? == 1;
        if inserted {
            event(&tx, workflow, 0, "registered", branch, now_ms)?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Returns None for an occupied/not-yet-eligible/terminal workflow. Claims
    /// increment a fencing generation even when the same owner resumes.
    pub fn claim(
        &mut self,
        workflow: &str,
        owner: &str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<Option<Lease>> {
        ensure!(
            !owner.is_empty() && ttl_ms > 0,
            "owner and positive lease ttl required"
        );
        let until = now_ms.checked_add(ttl_ms).context("lease overflow")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT checkpoint, lease_until, generation FROM workflow WHERE id=?1",
                [workflow],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((raw, leased, generation)) = row else {
            bail!("unknown workflow");
        };
        let cp: Checkpoint = serde_json::from_str(&raw)?;
        if leased > now_ms
            || cp.next.eligible_at_ms > now_ms
            || matches!(cp.stage, Stage::Merged | Stage::Retired)
        {
            return Ok(None);
        }
        let generation = generation.checked_add(1).context("generation overflow")?;
        tx.execute("UPDATE workflow SET owner=?2,generation=?3,lease_until=?4,heartbeat_at=?5,attempts=attempts+1 WHERE id=?1",
            params![workflow, owner, generation, until, now_ms])?;
        event(&tx, workflow, generation, "claimed", owner, now_ms)?;
        tx.commit()?;
        Ok(Some(Lease {
            workflow: workflow.into(),
            owner: owner.into(),
            generation,
        }))
    }

    pub fn heartbeat(&mut self, lease: &Lease, now_ms: i64, ttl_ms: i64) -> Result<()> {
        ensure!(ttl_ms > 0, "positive lease ttl required");
        let until = now_ms.checked_add(ttl_ms).context("lease overflow")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now_ms)?;
        tx.execute(
            "UPDATE workflow SET heartbeat_at=?2,lease_until=?3 WHERE id=?1",
            params![lease.workflow, now_ms, until],
        )?;
        // Heartbeat is intentionally not useful progress.
        tx.commit()?;
        Ok(())
    }

    /// Keepalive/repeated identical checkpoints do not move progress_at.
    /// Evidence and state changes are recorded; caller determines semantic
    /// progress from executable receipts rather than model claims.
    pub fn checkpoint(
        &mut self,
        lease: &Lease,
        checkpoint: &Checkpoint,
        now_ms: i64,
    ) -> Result<()> {
        checkpoint.binding.validate()?;
        ensure!(
            !checkpoint.next.reason.is_empty() && !checkpoint.next.action.is_empty(),
            "next action and reason required"
        );
        let mut checkpoint = checkpoint.clone();
        checkpoint.evidence.sort();
        checkpoint.evidence.dedup();
        ensure!(
            checkpoint.evidence.len() <= 128,
            "checkpoint evidence must be compact references"
        );
        let raw = serde_json::to_string(&checkpoint)?;
        ensure!(
            raw.len() <= 32 * 1024,
            "checkpoint exceeds bounded controller context"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now_ms)?;
        let previous: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        if previous != raw {
            let previous: Checkpoint = serde_json::from_str(&previous)?;
            let meaningful = previous.stage != checkpoint.stage
                || previous.binding != checkpoint.binding
                || previous.evidence != checkpoint.evidence;
            tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=CASE WHEN ?4 THEN ?3 ELSE progress_at END WHERE id=?1", params![lease.workflow,raw,now_ms,meaningful])?;
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "checkpoint",
                &raw,
                now_ms,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn release(&mut self, lease: &Lease, now_ms: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now_ms)?;
        tx.execute(
            "UPDATE workflow SET owner=NULL,lease_until=0 WHERE id=?1",
            [&lease.workflow],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "released",
            "",
            now_ms,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Must be durably committed BEFORE invoking GitHub. A previously returned
    /// pending intent is not permission to blindly repeat its side effect.
    pub fn begin_operation(
        &mut self,
        lease: &Lease,
        kind: OperationKind,
        binding: &Binding,
        now_ms: i64,
    ) -> Result<Operation> {
        let binding_json = serde_json::to_string(binding)?;
        let kind_json = serde_json::to_string(&kind)?;
        let identity = if kind == OperationKind::CreatePr {
            // Publication identity outlives a changed head/contract. Recovery
            // must find the first remote create, not mint a second PR.
            serde_json::to_vec(&(&lease.workflow, &kind))?
        } else {
            serde_json::to_vec(&(&lease.workflow, &kind, binding))?
        };
        let id = format!("{:x}", Sha256::digest(identity));
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now_ms)?;
        tx.execute("INSERT INTO operation(id,workflow,kind,binding,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO NOTHING",
            params![id,lease.workflow,kind_json,binding_json,now_ms])?;
        let (receipt, recorded_binding): (Option<String>, String) = tx.query_row(
            "SELECT receipt,binding FROM operation WHERE id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        tx.commit()?;
        Ok(Operation {
            id,
            workflow: lease.workflow.clone(),
            kind,
            binding: serde_json::from_str(&recorded_binding)?,
            receipt,
        })
    }

    /// Complete after observing the remote result (including reconciliation).
    /// Existing receipt cannot be rewritten, even by a later valid lease.
    pub fn complete_operation(
        &mut self,
        lease: &Lease,
        id: &str,
        receipt: &str,
        now_ms: i64,
    ) -> Result<()> {
        ensure!(!receipt.is_empty(), "remote receipt required");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now_ms)?;
        let prior: Option<String> = tx.query_row(
            "SELECT receipt FROM operation WHERE id=?1 AND workflow=?2",
            params![id, lease.workflow],
            |r| r.get(0),
        )?;
        ensure!(
            prior.as_deref().is_none_or(|s| s == receipt),
            "operation receipt conflict"
        );
        tx.execute(
            "UPDATE operation SET receipt=?2 WHERE id=?1",
            params![id, receipt],
        )?;
        if prior.is_none() {
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "operation_completed",
                id,
                now_ms,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record the observed PR identity after create/reconcile. Never inferred
    /// from model output. Once attached, changing the PR requires a new workflow.
    pub fn attach_pr(&mut self, lease: &Lease, pr: u64, now_ms: i64) -> Result<()> {
        ensure!(pr > 0, "positive PR number required");
        let pr = i64::try_from(pr)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now_ms)?;
        let prior: Option<i64> = tx.query_row(
            "SELECT pr FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        ensure!(
            prior.is_none_or(|n| n == pr),
            "workflow already bound to a different PR"
        );
        tx.execute(
            "UPDATE workflow SET pr=?2 WHERE id=?1",
            params![lease.workflow, pr],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically attach a remote PR and its canonical alias. The issue-build
    /// workflow keeps its identity; a later resume resolves the same row. Other
    /// PRs for that issue can own independent PR-keyed workflows.
    pub fn attach_pr_in_repo(
        &mut self,
        lease: &Lease,
        repo: &str,
        pr: u64,
        now_ms: i64,
    ) -> Result<()> {
        let alias = pr_workflow_id(repo, pr)?;
        let pr = i64::try_from(pr)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now_ms)?;
        let prior: Option<i64> = tx.query_row(
            "SELECT pr FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        ensure!(
            prior.is_none_or(|p| p == pr),
            "workflow already bound to a different PR"
        );
        let owner: Option<String> = tx
            .query_row("SELECT workflow FROM pr_alias WHERE id=?1", [&alias], |r| {
                r.get(0)
            })
            .optional()?;
        ensure!(
            owner.as_deref().is_none_or(|id| id == lease.workflow),
            "PR alias owned by another workflow"
        );
        tx.execute(
            "INSERT INTO pr_alias(id,workflow) VALUES(?1,?2) ON CONFLICT(id) DO NOTHING",
            params![alias, lease.workflow],
        )?;
        tx.execute(
            "UPDATE workflow SET pr=?2 WHERE id=?1",
            params![lease.workflow, pr],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn resolve_pr(&self, repo: &str, pr: u64) -> Result<Option<String>> {
        let alias = pr_workflow_id(repo, pr)?;
        Ok(self
            .conn
            .query_row("SELECT workflow FROM pr_alias WHERE id=?1", [alias], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Immutable per reviewer/head/contract. Corrections require a new contract
    /// version or reviewer decision identity; a contradictory write is an error.
    pub fn record_review(&mut self, lease: &Lease, review: &Review, now_ms: i64) -> Result<()> {
        ensure!(
            !review.reviewer.is_empty() && !review.evidence.is_empty(),
            "review identity/evidence required"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, &review.binding, now_ms)?;
        let binding = serde_json::to_string(&review.binding)?;
        let prior: Option<(bool,String)> = tx.query_row("SELECT approved,evidence FROM review WHERE workflow=?1 AND reviewer=?2 AND binding=?3",
            params![lease.workflow,review.reviewer,binding],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((approved, evidence)) = prior {
            ensure!(
                approved == review.approved && evidence == review.evidence,
                "immutable review conflict"
            );
        } else {
            tx.execute("INSERT INTO review(workflow,reviewer,binding,approved,evidence) VALUES(?1,?2,?3,?4,?5)",
                params![lease.workflow,review.reviewer,binding,review.approved,review.evidence])?;
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "review_recorded",
                &serde_json::to_string(review)?,
                now_ms,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns only records for the current binding; this is evidence, not a
    /// merge verdict. The caller must apply independence and adjudication rules.
    pub fn current_reviews(&self, workflow: &str) -> Result<Vec<Review>> {
        let snapshot = self.snapshot(workflow)?.context("unknown workflow")?;
        let binding = snapshot.checkpoint.binding;
        let mut statement = self.conn.prepare("SELECT reviewer,approved,evidence FROM review WHERE workflow=?1 AND binding=?2 ORDER BY reviewer")?;
        let rows =
            statement.query_map(params![workflow, serde_json::to_string(&binding)?], |r| {
                Ok(Review {
                    reviewer: r.get(0)?,
                    approved: r.get(1)?,
                    evidence: r.get(2)?,
                    binding: binding.clone(),
                })
            })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Idempotent resource accounting: repeat observation of one stage receipt
    /// cannot charge it twice. Timeout work must have a charge even if not a
    /// builder-quality failure. Different payload under the same ID is rejected.
    pub fn charge(
        &mut self,
        lease: &Lease,
        receipt_id: &str,
        charge: ResourceCharge,
        now_ms: i64,
    ) -> Result<()> {
        ensure!(!receipt_id.is_empty(), "charge receipt required");
        let values = (
            i64::try_from(charge.wall_ms)?,
            i64::try_from(charge.model_calls)?,
            i64::try_from(charge.tokens)?,
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now_ms)?;
        let prior: Option<(i64,i64,i64)> = tx.query_row("SELECT wall_ms,model_calls,tokens FROM resource_charge WHERE workflow=?1 AND id=?2",
            params![lease.workflow,receipt_id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some(prior) = prior {
            ensure!(prior == values, "charge receipt conflict");
        } else {
            // Check sums before SQL arithmetic can promote an overflowing
            // integer to floating point and corrupt accounting.
            let totals: (i64, i64, i64) = tx.query_row(
                "SELECT wall_ms,model_calls,tokens FROM workflow WHERE id=?1",
                [&lease.workflow],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let sums = (
                totals
                    .0
                    .checked_add(values.0)
                    .context("wall accounting overflow")?,
                totals
                    .1
                    .checked_add(values.1)
                    .context("call accounting overflow")?,
                totals
                    .2
                    .checked_add(values.2)
                    .context("token accounting overflow")?,
            );
            tx.execute("INSERT INTO resource_charge(workflow,id,wall_ms,model_calls,tokens) VALUES(?1,?2,?3,?4,?5)",params![lease.workflow,receipt_id,values.0,values.1,values.2])?;
            tx.execute(
                "UPDATE workflow SET wall_ms=?2,model_calls=?3,tokens=?4 WHERE id=?1",
                params![lease.workflow, sums.0, sums.1, sums.2],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically admit at most `limit` calls for one purpose and immutable
    /// evidence binding. Intended for bounded adjudication/recovery attempts;
    /// restart or a new lease does not reset the allowance.
    pub fn admit_attempt(
        &mut self,
        lease: &Lease,
        purpose: &str,
        binding: &Binding,
        limit: u32,
        now_ms: i64,
    ) -> Result<bool> {
        ensure!(
            !purpose.is_empty() && limit > 0,
            "purpose and positive limit required"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now_ms)?;
        let binding = serde_json::to_string(binding)?;
        let count: u32 = tx.query_row("SELECT attempts FROM bounded_attempt WHERE workflow=?1 AND purpose=?2 AND binding=?3",
            params![lease.workflow,purpose,binding],|r|r.get(0)).optional()?.unwrap_or(0);
        if count >= limit {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO bounded_attempt(workflow,purpose,binding,attempts) VALUES(?1,?2,?3,1)
            ON CONFLICT(workflow,purpose,binding) DO UPDATE SET attempts=attempts+1",
            params![lease.workflow, purpose, binding],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "attempt_admitted",
            purpose,
            now_ms,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Check fencing immediately before a remote operation, in addition to
    /// using the remote API's own compare-and-swap head precondition. A local
    /// SQLite lease alone cannot fence a push already accepted by GitHub.
    pub fn validate_lease(&mut self, lease: &Lease, binding: &Binding, now_ms: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    pub fn events(&self, workflow: &str) -> Result<Vec<(i64, String, String, i64)>> {
        let mut statement = self.conn.prepare("SELECT generation,kind,payload,at_ms FROM workflow_event WHERE workflow=?1 ORDER BY seq")?;
        let rows = statement.query_map([workflow], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn pending_operations(&self, workflow: &str) -> Result<Vec<Operation>> {
        let mut statement = self.conn.prepare("SELECT id,kind,binding FROM operation WHERE workflow=?1 AND receipt IS NULL ORDER BY created_at,id")?;
        let rows = statement.query_map([workflow], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|r| {
            let (id, kind, binding) = r?;
            Ok(Operation {
                id,
                workflow: workflow.into(),
                kind: serde_json::from_str(&kind)?,
                binding: serde_json::from_str(&binding)?,
                receipt: None,
            })
        })
        .collect()
    }

    pub fn snapshot(&self, workflow: &str) -> Result<Option<Snapshot>> {
        let row = self.conn.query_row("SELECT branch,pr,checkpoint,attempts,wall_ms,model_calls,tokens,heartbeat_at,progress_at,lease_until,generation FROM workflow WHERE id=?1",[workflow],|r| {
            Ok((r.get::<_,String>(0)?,r.get::<_,Option<u64>>(1)?,r.get::<_,String>(2)?,r.get::<_,u64>(3)?,r.get::<_,u64>(4)?,r.get::<_,u64>(5)?,r.get::<_,u64>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?,r.get::<_,i64>(9)?,r.get::<_,i64>(10)?))
        }).optional()?;
        row.map(
            |(
                branch,
                pr,
                cp,
                attempts,
                wall_ms,
                model_calls,
                tokens,
                heartbeat_at_ms,
                progress_at_ms,
                lease_until_ms,
                generation,
            )| {
                Ok(Snapshot {
                    workflow: workflow.into(),
                    branch,
                    pr,
                    checkpoint: serde_json::from_str(&cp)?,
                    attempts,
                    wall_ms,
                    model_calls,
                    tokens,
                    heartbeat_at_ms,
                    progress_at_ms,
                    lease_until_ms,
                    generation,
                })
            },
        )
        .transpose()
    }
}

fn require_lease(tx: &Transaction<'_>, lease: &Lease, now_ms: i64) -> Result<()> {
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow WHERE id=?1 AND owner=?2 AND generation=?3 AND lease_until>?4)",
        params![lease.workflow,lease.owner,lease.generation,now_ms],|r|r.get(0))?;
    ensure!(valid, "expired or superseded workflow lease");
    Ok(())
}
fn require_binding(
    tx: &Transaction<'_>,
    lease: &Lease,
    binding: &Binding,
    now_ms: i64,
) -> Result<()> {
    require_lease(tx, lease, now_ms)?;
    binding.validate()?;
    let raw: String = tx.query_row(
        "SELECT checkpoint FROM workflow WHERE id=?1",
        [&lease.workflow],
        |r| r.get(0),
    )?;
    let checkpoint: Checkpoint = serde_json::from_str(&raw)?;
    ensure!(checkpoint.binding == *binding, "head or contract changed");
    Ok(())
}
fn event(
    tx: &Transaction<'_>,
    workflow: &str,
    generation: i64,
    kind: &str,
    payload: &str,
    now_ms: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,?2,?3,?4,?5)",
        params![workflow, generation, kind, payload, now_ms],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> Binding {
        Binding {
            head: "head-a".into(),
            contract: "contract-1".into(),
            evidence_hash: "tree-base-env-receipts".into(),
        }
    }
    fn setup(path: &Path) -> (WorkflowStore, Lease) {
        let mut store = WorkflowStore::open(path).unwrap();
        store
            .register("owner/repo#1", "fix/example", binding(), 10)
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "worker-a", 20, 1_000)
            .unwrap()
            .unwrap();
        (store, lease)
    }
    #[test]
    fn reopening_recovers_checkpoint_and_uncertain_remote_operation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, lease) = setup(&path);
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Publish;
        cp.evidence = vec!["tests-passed-at-head-a".into()];
        store.checkpoint(&lease, &cp, 30).unwrap();
        let op = store
            .begin_operation(&lease, OperationKind::CreatePr, &binding(), 31)
            .unwrap();
        drop(store); // Remote creation may have succeeded before this crash.
        let mut store = WorkflowStore::open(&path).unwrap();
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,
            cp
        );
        assert!(store
            .claim(&lease.workflow, "worker-b", 100, 1000)
            .unwrap()
            .is_none());
        let resumed = store
            .claim(&lease.workflow, "worker-b", 1021, 1000)
            .unwrap()
            .unwrap();
        assert_eq!(
            store.pending_operations(&lease.workflow).unwrap(),
            vec![op.clone()]
        );
        assert_eq!(
            store
                .begin_operation(&resumed, OperationKind::CreatePr, &binding(), 1022)
                .unwrap()
                .id,
            op.id
        );
        store
            .complete_operation(&resumed, &op.id, "github-pr:42", 1023)
            .unwrap();
        store.attach_pr(&resumed, 42, 1023).unwrap();
        assert!(store
            .pending_operations(&lease.workflow)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().pr,
            Some(42)
        );
    }
    #[test]
    fn superseded_worker_cannot_checkpoint_publish_review_or_charge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut old, lease) = setup(&path);
        let mut next = WorkflowStore::open(&path).unwrap();
        let successor = next
            .claim(&lease.workflow, "worker-b", 1021, 1000)
            .unwrap()
            .unwrap();
        let cp = next.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        assert!(old.checkpoint(&lease, &cp, 1022).is_err());
        assert!(old
            .begin_operation(&lease, OperationKind::Push, &binding(), 1022)
            .is_err());
        assert!(old
            .record_review(
                &lease,
                &Review {
                    reviewer: "codex-pass-1".into(),
                    binding: binding(),
                    approved: true,
                    evidence: "receipt".into()
                },
                1022
            )
            .is_err());
        assert!(old
            .charge(
                &lease,
                "timeout",
                ResourceCharge {
                    wall_ms: 1000,
                    ..Default::default()
                },
                1022
            )
            .is_err());
        assert!(old.heartbeat(&lease, 1022, 1000).is_err());
        assert!(old.release(&lease, 1022).is_err());
        next.validate_lease(&successor, &binding(), 1022).unwrap();
        assert_eq!(successor.generation, lease.generation + 1);
    }
    #[test]
    fn heartbeat_and_retry_scheduling_are_not_useful_progress() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, lease) = setup(&dir.path().join("state.db"));
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        store.checkpoint(&lease, &cp, 30).unwrap();
        store.heartbeat(&lease, 40, 1000).unwrap();
        cp.next.eligible_at_ms = 900;
        store.checkpoint(&lease, &cp, 50).unwrap();
        let snapshot = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(snapshot.progress_at_ms, 10);
        assert_eq!(snapshot.heartbeat_at_ms, 40);
        cp.evidence.push("verified-test-result".into());
        store.checkpoint(&lease, &cp, 60).unwrap();
        assert_eq!(
            store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .progress_at_ms,
            60
        );
    }
    #[test]
    fn new_head_preserves_ownership_but_invalidates_old_reviews_and_create_identity() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, lease) = setup(&dir.path().join("state.db"));
        let review = Review {
            reviewer: "codex:focused".into(),
            binding: binding(),
            approved: true,
            evidence: "review-receipt".into(),
        };
        store.record_review(&lease, &review, 30).unwrap();
        store.attach_pr(&lease, 42, 31).unwrap();
        let create = store
            .begin_operation(&lease, OperationKind::CreatePr, &binding(), 32)
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.binding.head = "head-b".into();
        store.checkpoint(&lease, &cp, 40).unwrap();
        assert!(store.current_reviews(&lease.workflow).unwrap().is_empty());
        assert_eq!(
            store.snapshot(&lease.workflow).unwrap().unwrap().pr,
            Some(42)
        );
        assert!(store.record_review(&lease, &review, 41).is_err());
        let recovered = store
            .begin_operation(&lease, OperationKind::CreatePr, &cp.binding, 42)
            .unwrap();
        assert_eq!(recovered.id, create.id);
        assert_eq!(recovered.binding, binding());
        assert_eq!(store.pending_operations(&lease.workflow).unwrap().len(), 1);
    }
    #[test]
    fn review_evidence_and_remote_receipts_are_immutable() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, lease) = setup(&dir.path().join("state.db"));
        let mut review = Review {
            reviewer: "codex:focused".into(),
            binding: binding(),
            approved: true,
            evidence: "review-receipt".into(),
        };
        store.record_review(&lease, &review, 30).unwrap();
        store.record_review(&lease, &review, 31).unwrap();
        review.approved = false;
        assert!(store.record_review(&lease, &review, 32).is_err());
        let op = store
            .begin_operation(&lease, OperationKind::MergePr, &binding(), 33)
            .unwrap();
        store
            .complete_operation(&lease, &op.id, "merge:abc", 34)
            .unwrap();
        store
            .complete_operation(&lease, &op.id, "merge:abc", 35)
            .unwrap();
        assert!(store
            .complete_operation(&lease, &op.id, "merge:different", 36)
            .is_err());
        assert!(store.attach_pr(&lease, 42, 37).is_ok());
        assert!(store.attach_pr(&lease, 43, 38).is_err());
    }
    #[test]
    fn charge_receipts_are_idempotent_and_attempt_limits_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, lease) = setup(&path);
        let charge = ResourceCharge {
            wall_ms: 3 * 60 * 60 * 1000,
            model_calls: 2,
            tokens: 300,
        };
        store.charge(&lease, "timed-out-stage", charge, 30).unwrap();
        store.charge(&lease, "timed-out-stage", charge, 31).unwrap();
        assert!(store
            .admit_attempt(&lease, "adjudication", &binding(), 2, 32)
            .unwrap());
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        assert!(store
            .admit_attempt(&lease, "adjudication", &binding(), 2, 33)
            .unwrap());
        assert!(!store
            .admit_attempt(&lease, "adjudication", &binding(), 2, 34)
            .unwrap());
        let snapshot = store.snapshot(&lease.workflow).unwrap().unwrap();
        assert_eq!(
            (
                snapshot.attempts,
                snapshot.wall_ms,
                snapshot.model_calls,
                snapshot.tokens
            ),
            (1, charge.wall_ms, 2, 300)
        );
        assert!(store
            .charge(&lease, "timed-out-stage", ResourceCharge::default(), 35)
            .is_err());
    }
    #[test]
    fn independent_connections_claim_once_and_preserve_unrelated_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        store
            .register("owner/repo#1", "branch", binding(), 10)
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut store = WorkflowStore::open(&path).unwrap();
                    barrier.wait();
                    let lease = store
                        .claim("owner/repo#1", &format!("worker-{n}"), 20, 1000)
                        .unwrap();
                    store
                        .register(&format!("owner/repo#{}", n + 2), "other", binding(), 30)
                        .unwrap();
                    lease
                })
            })
            .collect();
        let winners = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .count();
        assert_eq!(winners, 1);
        for n in 2..10 {
            assert!(store
                .snapshot(&format!("owner/repo#{n}"))
                .unwrap()
                .is_some());
        }
    }
    #[test]
    fn rolled_back_stage_does_not_leak_partial_record_or_audit_event() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, lease) = setup(&dir.path().join("state.db"));
        let before = store.events(&lease.workflow).unwrap();
        {
            let tx = store
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            tx.execute("UPDATE workflow SET pr=99 WHERE id=?1", [&lease.workflow])
                .unwrap();
            event(&tx, &lease.workflow, lease.generation, "partial", "", 30).unwrap();
        }
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().pr, None);
        assert_eq!(store.events(&lease.workflow).unwrap(), before);
    }
    #[test]
    fn terminal_and_scheduled_work_cannot_be_claimed_early() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, lease) = setup(&dir.path().join("state.db"));
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Held;
        cp.next.eligible_at_ms = 5000;
        store.checkpoint(&lease, &cp, 30).unwrap();
        store.release(&lease, 31).unwrap();
        assert!(store
            .claim(&lease.workflow, "other", 1000, 100)
            .unwrap()
            .is_none());
        let lease = store
            .claim(&lease.workflow, "other", 5000, 100)
            .unwrap()
            .unwrap();
        cp.stage = Stage::Merged;
        store.checkpoint(&lease, &cp, 5001).unwrap();
        store.release(&lease, 5002).unwrap();
        assert!(store
            .claim(&lease.workflow, "third", 6000, 100)
            .unwrap()
            .is_none());
    }
    #[test]
    fn evidence_change_invalidates_review_without_changing_head() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, lease) = setup(&dir.path().join("state.db"));
        store
            .record_review(
                &lease,
                &Review {
                    reviewer: "codex".into(),
                    binding: binding(),
                    approved: true,
                    evidence: "receipt".into(),
                },
                30,
            )
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.binding.evidence_hash = "new-base-or-environment".into();
        store.checkpoint(&lease, &cp, 31).unwrap();
        assert!(store.current_reviews(&lease.workflow).unwrap().is_empty());
    }
    #[test]
    fn state_file_is_private_and_symlinks_are_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        WorkflowStore::open(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let link = dir.path().join("link.db");
        symlink(&path, &link).unwrap();
        assert!(WorkflowStore::open(&link).is_err());
    }
    // Invoked in a separate test process by crash_releases_sqlite_transaction.
    // Exiting bypasses Rust Drop, exercising SQLite crash recovery rather than
    // merely testing transaction RAII rollback.
    #[test]
    fn crash_worker() {
        let Some(path) = std::env::var_os("AUGMENTAGENT_WORKFLOW_CRASH_TEST_DB") else {
            return;
        };
        let mut store = WorkflowStore::open(Path::new(&path)).unwrap();
        let tx = store
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        tx.execute("UPDATE workflow SET pr=99 WHERE id='owner/repo#1'", [])
            .unwrap();
        event(&tx, "owner/repo#1", 1, "uncommitted", "", 50).unwrap();
        std::process::exit(73);
    }

    #[test]
    fn crash_releases_sqlite_transaction_without_partial_publication() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (store, lease) = setup(&path);
        let before = store.events(&lease.workflow).unwrap();
        let test_name = format!(
            "{}::crash_worker",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test_name, "--nocapture"])
            .env("AUGMENTAGENT_WORKFLOW_CRASH_TEST_DB", &path)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        drop(store);
        let mut recovered = WorkflowStore::open(&path).unwrap();
        assert_eq!(
            recovered.snapshot(&lease.workflow).unwrap().unwrap().pr,
            None
        );
        assert_eq!(recovered.events(&lease.workflow).unwrap(), before);
        recovered.attach_pr(&lease, 42, 51).unwrap();
    }

    #[test]
    fn productive_stage_can_continue_beyond_six_hours() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        store
            .register("owner/repo#1", "branch", binding(), 10)
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "worker", 20, 60_000)
            .unwrap()
            .unwrap();
        for minute in 1..=361 {
            let now = 20 + minute * 30_000;
            store.heartbeat(&lease, now, 60_000).unwrap();
        }
        // Continue renewing through hour seven; no whole-run TTL exists.
        for minute in 362..=841 {
            let now = 20 + minute * 30_000;
            store.heartbeat(&lease, now, 60_000).unwrap();
        }
        let now = 20 + 841 * 30_000;
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Verify;
        cp.evidence = vec!["long-build-success".into()];
        store.checkpoint(&lease, &cp, now).unwrap();
        assert_eq!(
            store
                .snapshot(&lease.workflow)
                .unwrap()
                .unwrap()
                .progress_at_ms,
            now
        );
    }
    #[test]
    fn publication_alias_survives_restart_and_other_prs_for_same_issue_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, lease) = setup(&path);
        store
            .attach_pr_in_repo(&lease, "Owner/Repo", 42, 30)
            .unwrap();
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        assert_eq!(
            store.resolve_pr("owner/repo", 42).unwrap().as_deref(),
            Some("owner/repo#1")
        );
        let other = pr_workflow_id("owner/repo", 43).unwrap();
        store.register(&other, "second-fix", binding(), 31).unwrap();
        let other_lease = store
            .claim(&other, "second-worker", 32, 1000)
            .unwrap()
            .unwrap();
        store
            .attach_pr_in_repo(&other_lease, "owner/repo", 43, 33)
            .unwrap();
        assert!(store
            .attach_pr_in_repo(&other_lease, "owner/repo", 42, 34)
            .is_err());
        assert_eq!(store.resolve_pr("owner/repo", 43).unwrap(), Some(other));
    }
}
