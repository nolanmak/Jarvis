//! Durable autoship control-plane state. Workers get fenced leases, not authority
//! from a branch name or model-authored PR body. This store records evidence;
//! callers still enforce authorization, review and CI policy before side effects.
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

#[path = "autoship_merge_allowance.rs"]
mod merge_allowance;
#[path = "autoship_progress.rs"]
pub(crate) mod progress;
#[path = "autoship_milestone.rs"]
mod milestone;
#[path = "autoship_discord_ingress.rs"]
pub(crate) mod discord_ingress;
#[path = "autoship_review_epoch.rs"]
mod review_epoch;
#[path = "autoship_conversations.rs"]
mod conversations;
#[path = "autoship_owner_revision.rs"]
mod owner_revision;
#[path = "autoship_owner_turn.rs"]
pub(crate) mod owner_turn;
#[path = "autoship_owner_inbox.rs"]
pub(crate) mod owner_inbox;
#[path = "autoship_discord_controls.rs"]
pub(crate) mod discord_controls;
#[path = "autoship_owner_control.rs"]
pub(crate) mod owner_control;
#[path = "autoship_submission.rs"]
pub(crate) mod submission;
pub use merge_allowance::{MergeAllowance, MergeDispatchRecord, MergeReleaseProof, merge_request_not_executed};
pub(crate) use merge_allowance::validate_merge_receipt_identity;

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
    ReadyPr,
    RetargetPr,
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

pub fn default_path() -> std::path::PathBuf {
    std::env::var_os("AUGMENTAGENT_AUTOPR_WORKFLOW_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            augmentagent_channel_core::state_dir::state_dir_or(".").join("autopr-workflow.sqlite3")
        })
}

fn observation_available(conn: &Connection, now_ms: i64, required: u64, cap: u64) -> Result<bool> {
    observation_capacity(conn,now_ms,required,cap,false)
}
fn observation_capacity(conn: &Connection, now_ms: i64, required: u64, cap: u64, allow_shipping: bool) -> Result<bool> {
    let shipping: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow WHERE owner IS NOT NULL AND lease_until>?1 AND id NOT LIKE 'autoship-observation/%' AND id NOT LIKE 'autoship-canary/%')",[now_ms],|r|r.get(0))?;
    let used: u64 = conn.query_row("SELECT COALESCE(SUM(calls),0) FROM observation_charge WHERE day=?1",[now_ms.div_euclid(86_400_000)],|r|r.get(0))?;
    let reserved: u64 = conn.query_row("SELECT COALESCE(SUM(r.reserved-r.used),0) FROM observation_reservation r JOIN workflow w ON w.id=r.workflow AND w.generation=r.generation WHERE w.owner IS NOT NULL AND w.lease_until>?1",[now_ms],|r|r.get(0))?;
    Ok((!shipping || allow_shipping) && required > 0 && used.checked_add(reserved).and_then(|n|n.checked_add(required)).is_some_and(|total| total <= cap))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimPurpose { General, Productive, MergeReconciliation }

pub struct WorkflowStore {
    conn: Connection,
}

impl WorkflowStore {
    /// Preview existing protected state without schema initialization or writes.
    pub fn open_read_only(path: &Path) -> Result<Option<Self>> {
        use std::os::unix::fs::{MetadataExt,OpenOptionsExt};
        let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
            Ok(file)=>file,Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(None),
            Err(error)=>return Err(error.into()),
        };
        let meta=file.metadata()?;
        ensure!(meta.is_file() && meta.nlink()==1 && meta.uid()==unsafe {libc::geteuid()} && meta.mode()&0o077==0,"workflow preview state is not owner-private");
        let conn=Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY|rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        let after=std::fs::symlink_metadata(path)?;
        ensure!(meta.dev()==after.dev() && meta.ino()==after.ino() && !after.file_type().is_symlink(),"workflow preview state changed during open");
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Some(Self {conn}))
    }

    pub fn scope_stage_response(&mut self,lease:&Lease,binding:&Binding,key:&str,now:i64)->Result<Option<String>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;require_binding(&tx,lease,binding,now)?;
        let value=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,key],|r|r.get(0)).optional()?;tx.commit()?;Ok(value)
    }
    pub fn record_scope_stage_response(&mut self,lease:&Lease,binding:&Binding,key:&str,response:&str,now:i64)->Result<()> {
        ensure!(key.len()==64 && response.len()<=2*1024*1024,"invalid bounded scope response");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;require_binding(&tx,lease,binding,now)?;
        let prior:Option<String>=tx.query_row("SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",params![lease.workflow,key],|r|r.get(0)).optional()?;
        if let Some(prior)=prior {ensure!(prior==response,"scope response is immutable");} else {
            tx.execute("INSERT INTO scope_stage_response VALUES(?1,?2,?3)",params![lease.workflow,key,response])?;
            event(&tx,&lease.workflow,lease.generation,"scope_stage_response_recorded",&serde_json::json!({"key":key}).to_string(),now)?;
        }tx.commit()?;Ok(())
    }

    pub fn set_readiness_dependency(&mut self,lease:&Lease,binding:&Binding,repository:&str,issue:u64,record:Option<(&str,&str,&str,&str)>,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        if let Some((request_hash,base,kind,evidence))=record {
            ensure!(!request_hash.is_empty() && !base.is_empty() && !kind.is_empty() && evidence.len()<=2*1024*1024,"invalid readiness dependency");
            tx.execute("INSERT INTO issue_readiness_dependencies VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(repository,issue) DO UPDATE SET request_hash=excluded.request_hash,base=excluded.base,kind=excluded.kind,evidence=excluded.evidence",params![repository,issue,request_hash,base,kind,evidence])?;
        } else {tx.execute("DELETE FROM issue_readiness_dependencies WHERE repository=?1 AND issue=?2",params![repository,issue])?;}
        event(&tx,&lease.workflow,lease.generation,"readiness_dependency_changed",&serde_json::json!({"repository":repository,"issue":issue,"kind":record.map(|r|r.2)}).to_string(),now)?;
        tx.commit()?;Ok(())
    }

    pub fn set_pr_readiness_dependency(&mut self,lease:&Lease,binding:&Binding,repository:&str,pr:u64,record:Option<(&str,&str,&str)>,now:i64)->Result<()> {
        ensure!(lease.workflow==pr_workflow_id(repository,pr)?,"PR readiness requires its genuine PR workflow");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        if let Some((request_hash,base,evidence))=record {
            ensure!(request_hash.len()==64 && !base.is_empty() && evidence.len()<=2*1024*1024,"invalid PR readiness dependency");
            tx.execute("INSERT INTO pr_readiness_dependencies VALUES(?1,?2,?3,?4) ON CONFLICT(workflow) DO UPDATE SET request_hash=excluded.request_hash,base=excluded.base,evidence=excluded.evidence",params![lease.workflow,request_hash,base,evidence])?;
        } else {tx.execute("DELETE FROM pr_readiness_dependencies WHERE workflow=?1",[&lease.workflow])?;}
        event(&tx,&lease.workflow,lease.generation,"pr_readiness_dependency_changed",&serde_json::json!({"source":{"pull_request":pr},"blocked":record.is_some()}).to_string(),now)?;
        tx.commit()?;Ok(())
    }

    pub(crate) fn query_connection<T>(&self,query:impl FnOnce(&Connection)->Result<T>)->Result<T> { query(&self.conn) }

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
            CREATE TABLE IF NOT EXISTS command_job (
                workflow TEXT PRIMARY KEY, repository TEXT NOT NULL, target TEXT NOT NULL,
                until_milestone TEXT NOT NULL, provider TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS discord_workflow_binding (
                bot TEXT NOT NULL, owner TEXT NOT NULL, channel TEXT NOT NULL,
                workflow TEXT NOT NULL REFERENCES command_job(workflow), created_at INTEGER NOT NULL,
                PRIMARY KEY(bot,owner,channel,workflow));
            CREATE TABLE IF NOT EXISTS discord_command_ingress (
                request_id TEXT PRIMARY KEY REFERENCES command_request(request_id), origin TEXT NOT NULL,
                message_sha256 TEXT NOT NULL, response TEXT NOT NULL, state TEXT NOT NULL,
                created_at INTEGER NOT NULL, dispatched_at INTEGER, remote_message TEXT, confirmed_at INTEGER);
            CREATE TABLE IF NOT EXISTS discord_control_request (
                request_id TEXT PRIMARY KEY, principal TEXT NOT NULL, origin TEXT NOT NULL,
                message_sha256 TEXT NOT NULL, workflow TEXT NOT NULL REFERENCES command_job(workflow),
                response TEXT NOT NULL, state TEXT NOT NULL, created_at INTEGER NOT NULL,
                dispatched_at INTEGER, remote_message TEXT, confirmed_at INTEGER);
            CREATE TABLE IF NOT EXISTS discord_workflow_selection (
                bot TEXT NOT NULL, owner TEXT NOT NULL, channel TEXT NOT NULL,
                workflow TEXT NOT NULL REFERENCES command_job(workflow), message TEXT NOT NULL,
                PRIMARY KEY(bot,owner,channel));
            CREATE TABLE IF NOT EXISTS discord_control_frontier (
                bot TEXT NOT NULL, owner TEXT NOT NULL, workflow TEXT NOT NULL REFERENCES command_job(workflow),
                message TEXT NOT NULL, PRIMARY KEY(bot,owner,workflow));
            CREATE TABLE IF NOT EXISTS command_schedule (
                workflow TEXT PRIMARY KEY REFERENCES command_job(workflow),
                attempted_at INTEGER NOT NULL, eligible_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS workflow_native_session_owner (
                provider TEXT NOT NULL, native_session TEXT NOT NULL, workflow TEXT NOT NULL REFERENCES workflow(id),
                PRIMARY KEY(provider,native_session));
            CREATE TABLE IF NOT EXISTS workflow_builder_conversation (
                workflow TEXT NOT NULL REFERENCES workflow(id), provider TEXT NOT NULL, native_session TEXT NOT NULL,
                uncertain INTEGER NOT NULL, binding TEXT NOT NULL, updated_at INTEGER NOT NULL,
                PRIMARY KEY(workflow,provider), UNIQUE(provider,native_session));
            CREATE TABLE IF NOT EXISTS workflow_review_floor (
                workflow TEXT PRIMARY KEY REFERENCES command_job(workflow), after_seq INTEGER NOT NULL,
                request_id TEXT NOT NULL REFERENCES discord_control_request(request_id));
            CREATE TABLE IF NOT EXISTS workflow_owner_revision (
                request_id TEXT PRIMARY KEY REFERENCES workflow_owner_turn(request_id), previous_contract TEXT NOT NULL,
                next_contract TEXT NOT NULL, binding TEXT NOT NULL, owner_message_sha256 TEXT NOT NULL,
                registration TEXT NOT NULL, activated_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS workflow_owner_turn (
                workflow TEXT NOT NULL REFERENCES workflow(id), request_id TEXT PRIMARY KEY REFERENCES workflow_owner_message(request_id),
                owner TEXT NOT NULL, lease_generation INTEGER NOT NULL, pause_revision INTEGER NOT NULL,
                binding TEXT NOT NULL, state TEXT NOT NULL, started_at INTEGER NOT NULL);
            CREATE UNIQUE INDEX IF NOT EXISTS one_active_owner_turn ON workflow_owner_turn(workflow) WHERE state IN ('claimed','uncertain');
            CREATE TABLE IF NOT EXISTS workflow_owner_message (
                request_id TEXT PRIMARY KEY REFERENCES discord_control_request(request_id),
                workflow TEXT NOT NULL REFERENCES command_job(workflow), body TEXT NOT NULL,
                state TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS workflow_owner_control (
                workflow TEXT PRIMARY KEY, revision INTEGER NOT NULL, paused INTEGER NOT NULL,
                principal TEXT NOT NULL, updated_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS workflow_owner_control_event (
                workflow TEXT NOT NULL, revision INTEGER NOT NULL, paused INTEGER NOT NULL,
                principal TEXT NOT NULL, at_ms INTEGER NOT NULL, PRIMARY KEY(workflow,revision));
            CREATE TABLE IF NOT EXISTS pr_ready_receipt (
                workflow TEXT NOT NULL REFERENCES workflow(id), binding TEXT NOT NULL, pr INTEGER NOT NULL,
                review_sha256 TEXT NOT NULL, published_at INTEGER NOT NULL, PRIMARY KEY(workflow,binding,pr));
            CREATE TABLE IF NOT EXISTS command_attempt (
                workflow TEXT PRIMARY KEY REFERENCES command_job(workflow), selected_at INTEGER NOT NULL,
                finished_at INTEGER, outcome TEXT NOT NULL, reason TEXT NOT NULL, eligible_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS command_lane_turn (
                repository TEXT PRIMARY KEY, yield_normal INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS command_client_intent (
                request_id TEXT PRIMARY KEY, principal TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS command_request (
                request_id TEXT PRIMARY KEY, principal TEXT NOT NULL, payload TEXT NOT NULL,
                admission TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS scope_stage_response(workflow TEXT NOT NULL REFERENCES workflow(id),key TEXT NOT NULL,response TEXT NOT NULL,PRIMARY KEY(workflow,key));
            CREATE TABLE IF NOT EXISTS pr_readiness_dependencies(workflow TEXT PRIMARY KEY REFERENCES workflow(id),request_hash TEXT NOT NULL,base TEXT NOT NULL,evidence TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS issue_readiness_dependencies(repository TEXT NOT NULL,issue INTEGER NOT NULL,request_hash TEXT NOT NULL,base TEXT NOT NULL,kind TEXT NOT NULL,evidence TEXT NOT NULL,PRIMARY KEY(repository,issue));
            CREATE TABLE IF NOT EXISTS workflow_native_owner (
                workflow TEXT NOT NULL REFERENCES workflow(id), lease_generation INTEGER NOT NULL,
                audit_path TEXT NOT NULL, retirement_reason TEXT, retired_at INTEGER,
                attempted_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(workflow,lease_generation));
            CREATE TABLE IF NOT EXISTS workflow_audit_generation (
                workflow TEXT PRIMARY KEY REFERENCES workflow(id), generation INTEGER NOT NULL,
                lease_generation INTEGER NOT NULL, recovery TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS publication_dispatch(operation TEXT PRIMARY KEY REFERENCES operation(id),attempt INTEGER NOT NULL,dispatched_at INTEGER NOT NULL,pr INTEGER);
            CREATE TABLE IF NOT EXISTS publication_scan_cursor(repository TEXT PRIMARY KEY,last_workflow TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS review_fragment (
                workflow TEXT NOT NULL REFERENCES workflow(id), audit_generation INTEGER NOT NULL,
                key TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(workflow,audit_generation,key));
            CREATE TABLE IF NOT EXISTS pending_publication(workflow TEXT PRIMARY KEY REFERENCES workflow(id),repository TEXT NOT NULL,body TEXT NOT NULL,updated_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS observation_reservation(workflow TEXT NOT NULL,generation INTEGER NOT NULL,reserved INTEGER NOT NULL,used INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(workflow,generation));
            CREATE TABLE IF NOT EXISTS observation_charge (
                workflow TEXT NOT NULL, receipt_id TEXT NOT NULL, day INTEGER NOT NULL, calls INTEGER NOT NULL, PRIMARY KEY(workflow,receipt_id));
            CREATE TABLE IF NOT EXISTS episode_action (
                workflow TEXT NOT NULL REFERENCES workflow(id), epoch TEXT NOT NULL, action TEXT NOT NULL,
                at_ms INTEGER NOT NULL, PRIMARY KEY(workflow,epoch,action));
            CREATE TABLE IF NOT EXISTS episode_dispatch (
                workflow TEXT NOT NULL, epoch TEXT NOT NULL, action TEXT NOT NULL,
                phase TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(workflow,epoch,action));
            INSERT OR IGNORE INTO episode_dispatch SELECT workflow,epoch,action,'uncertain_legacy','{}' FROM episode_action;
            CREATE TABLE IF NOT EXISTS fresh_review_origin (
                receipt TEXT PRIMARY KEY, workflow TEXT NOT NULL REFERENCES workflow(id), binding TEXT NOT NULL, created_at INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS imported_origin (
                receipt TEXT PRIMARY KEY, repository TEXT NOT NULL, pr INTEGER NOT NULL,
                source_head TEXT NOT NULL, candidate_head TEXT NOT NULL, created_at INTEGER NOT NULL);
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
            CREATE TABLE IF NOT EXISTS merge_mutation_receipt (
                operation TEXT PRIMARY KEY REFERENCES operation(id), receipt TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS merge_allowance_dispatch (
                operation TEXT PRIMARY KEY REFERENCES operation(id), repository TEXT NOT NULL,
                pr INTEGER NOT NULL, pr_node_id TEXT NOT NULL, head TEXT NOT NULL,
                generation INTEGER NOT NULL, reserved_at_ms INTEGER NOT NULL,
                dispatched_at_ms INTEGER, merged_at_ms INTEGER, resolution TEXT,
                state TEXT NOT NULL CHECK(state IN ('reserved','dispatched','confirmed','released')));
            CREATE TABLE IF NOT EXISTS merge_transport_response (
                operation TEXT PRIMARY KEY REFERENCES merge_allowance_dispatch(operation),
                response TEXT NOT NULL, observed_at_ms INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS merge_private_marker (
                operation TEXT PRIMARY KEY REFERENCES merge_allowance_dispatch(operation),
                protocol INTEGER NOT NULL CHECK(protocol=1), nonce TEXT NOT NULL UNIQUE);
            CREATE UNIQUE INDEX IF NOT EXISTS one_active_merge_per_pr
                ON merge_allowance_dispatch(repository,pr) WHERE state<>'released';
            CREATE UNIQUE INDEX IF NOT EXISTS one_active_merge_per_node
                ON merge_allowance_dispatch(repository,pr_node_id) WHERE state<>'released';
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
            CREATE TABLE IF NOT EXISTS episode_state (
                workflow TEXT PRIMARY KEY REFERENCES workflow(id), state TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS episode_observation (
                workflow TEXT NOT NULL REFERENCES workflow(id), receipt TEXT NOT NULL,
                observation TEXT NOT NULL, decision TEXT NOT NULL,
                PRIMARY KEY(workflow,receipt));
            CREATE TABLE IF NOT EXISTS workflow_event (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, workflow TEXT NOT NULL REFERENCES workflow(id),
                generation INTEGER NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, at_ms INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS workflow_progress_delivery (
                workflow TEXT NOT NULL, bot TEXT NOT NULL, owner TEXT NOT NULL, channel TEXT NOT NULL,
                sequence INTEGER NOT NULL, nonce TEXT NOT NULL, content TEXT NOT NULL,
                state TEXT NOT NULL, dispatched_at INTEGER NOT NULL, message TEXT,
                confirmed_at INTEGER, PRIMARY KEY(workflow,bot,owner,channel,sequence));
            CREATE TABLE IF NOT EXISTS workflow_progress_state (
                workflow TEXT PRIMARY KEY, sequence INTEGER NOT NULL, fingerprint TEXT NOT NULL,
                terminal INTEGER NOT NULL, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS workflow_progress_event (
                workflow TEXT NOT NULL, sequence INTEGER NOT NULL, payload TEXT NOT NULL,
                created_at INTEGER NOT NULL, delivery_state TEXT NOT NULL DEFAULT 'queued',
                PRIMARY KEY(workflow,sequence));
            COMMIT;")?;
        Ok(Self { conn })
    }

    pub fn issue_fresh_review_origin(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        now: i64,
    ) -> Result<String> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        let receipt = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO fresh_review_origin VALUES(?1,?2,?3,?4)",
            params![
                receipt,
                lease.workflow,
                serde_json::to_string(binding)?,
                now
            ],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "fresh_review_authorized",
            &receipt,
            now,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn verify_fresh_review_origin(
        &self,
        workflow: &str,
        binding: &Binding,
        receipt: &str,
    ) -> Result<bool> {
        let Some(current) = self.snapshot(workflow)? else {
            return Ok(false);
        };
        if current.checkpoint.binding != *binding {
            return Ok(false);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM fresh_review_origin WHERE receipt=?1 AND workflow=?2 AND binding=?3",
                params![receipt, workflow, serde_json::to_string(binding)?],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn issue_imported_origin(
        &mut self,
        repository: &str,
        pr: u64,
        source_head: &str,
        candidate_head: &str,
        now: i64,
    ) -> Result<String> {
        ensure!(!candidate_head.is_empty(), "candidate head required");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owned: Option<String> = tx
            .query_row(
                "SELECT head FROM adopted_pr WHERE repository=?1 AND pr=?2",
                params![repository, pr],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            owned.as_deref() == Some(source_head),
            "candidate lacks current trusted adoption ownership"
        );
        let receipt = uuid::Uuid::new_v4().to_string();
        tx.execute("INSERT INTO imported_origin(receipt,repository,pr,source_head,candidate_head,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![receipt,repository,pr,source_head,candidate_head,now])?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn verify_imported_origin(
        &self,
        repository: &str,
        pr: u64,
        source_head: &str,
        candidate_head: &str,
        receipt: &str,
    ) -> Result<bool> {
        Ok(self.conn.query_row("SELECT 1 FROM imported_origin o JOIN adopted_pr a ON a.repository=o.repository AND a.pr=o.pr
            WHERE o.receipt=?1 AND o.repository=?2 AND o.pr=?3 AND o.source_head=?4 AND o.candidate_head=?5
            AND (a.head=o.source_head OR a.head=o.candidate_head)", params![receipt,repository,pr,source_head,candidate_head], |_|Ok(())).optional()?.is_some())
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

    /// Free merge reconciliation preserves command holds and merge ceilings.
    /// Eligibility and ownership are checked atomically with the new lease.
    pub fn claim_reconciliation(&mut self, workflow: &str, owner: &str, now_ms: i64, ttl_ms: i64) -> Result<Option<Lease>> {
        self.claim_inner(workflow, owner, now_ms, ttl_ms, ClaimPurpose::MergeReconciliation)
    }

    /// Productive workers cannot restart command holds or spend models on work
    /// awaiting free reconciliation. A stale preview cannot authorize a claim.
    pub fn claim_productive(&mut self, workflow: &str, owner: &str, now_ms: i64, ttl_ms: i64) -> Result<Option<Lease>> {
        self.claim_inner(workflow, owner, now_ms, ttl_ms, ClaimPurpose::Productive)
    }

    pub fn claim(
        &mut self,
        workflow: &str,
        owner: &str,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<Option<Lease>> {
        self.claim_inner(workflow, owner, now_ms, ttl_ms, ClaimPurpose::General)
    }

    fn claim_inner(&mut self, workflow: &str, owner: &str, now_ms: i64, ttl_ms: i64, purpose: ClaimPurpose) -> Result<Option<Lease>> {
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
        if purpose != ClaimPurpose::General {
            if owner_control::read(&tx,workflow)?.is_some_and(|c|c.paused) {return Ok(None);}
            let policy: Option<String> = tx.query_row("SELECT until_milestone FROM command_job WHERE workflow=?1", [workflow], |r| r.get(0)).optional()?;
            if let Some(policy) = policy {
                let until: submission::Milestone = serde_json::from_str(&policy)?;
                let pending_merge: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operation o LEFT JOIN merge_allowance_dispatch d ON d.operation=o.id WHERE o.workflow=?1 AND o.kind=?2 AND (o.receipt IS NULL OR d.state='dispatched'))",
                    params![workflow, serde_json::to_string(&OperationKind::MergePr)?], |r| r.get(0))?;
                if cp.stage == Stage::Held || pending_merge
                    || (purpose == ClaimPurpose::Productive && matches!(cp.stage, Stage::AwaitChecks | Stage::Merge))
                    || (purpose == ClaimPurpose::MergeReconciliation && until == submission::Milestone::Pr) {
                    return Ok(None);
                }
            }
        }
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
        if matches!(checkpoint.stage,Stage::Merged|Stage::Retired) {
            tx.execute("UPDATE workflow_native_owner SET retirement_reason=COALESCE(retirement_reason,'terminal_workflow') WHERE workflow=?1 AND retired_at IS NULL",[&lease.workflow])?;
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
        owner_control::require_dispatch(&tx,&lease.workflow)?;
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
    /// Accepted mutation response is distinct from observing somebody's merge.
    /// An uncertain/lost response must never be invented during reconciliation.
    pub fn record_merge_mutation(
        &mut self,
        lease: &Lease,
        operation: &str,
        receipt: &serde_json::Value,
        now: i64,
    ) -> Result<()> {
        let raw = serde_json::to_string(receipt)?;
        ensure!(raw.len() <= 16 * 1024, "merge mutation receipt too large");
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let (kind, binding, completed): (String, String, Option<String>) = tx.query_row(
            "SELECT kind,binding,receipt FROM operation WHERE id=?1 AND workflow=?2",
            params![operation, lease.workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let binding: Binding = serde_json::from_str(&binding)?;
        require_binding(&tx, lease, &binding, now)?;
        ensure!(
            serde_json::from_str::<OperationKind>(&kind)? == OperationKind::MergePr,
            "mutation receipt requires merge operation"
        );
        merge_allowance::validate_merge_receipt_identity(&tx, operation, receipt)?;
        ensure!(
            receipt["expected_head"].as_str() == Some(binding.head.as_str()),
            "mutation response identity differs from intent"
        );
        let pr: Option<u64> = tx.query_row(
            "SELECT pr FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        ensure!(
            pr.is_some() && receipt["pr_number"].as_u64() == pr,
            "mutation response PR differs from workflow"
        );
        for field in ["pr_node_id", "merge_commit", "merged_by"] {
            ensure!(
                receipt[field]
                    .as_str()
                    .is_some_and(|v| !v.trim().is_empty()),
                "mutation response missing {field}"
            );
        }
        let prior: Option<String> = tx
            .query_row(
                "SELECT receipt FROM merge_mutation_receipt WHERE operation=?1",
                [operation],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            merge_allowance::validate_merge_receipt_equivalence(&tx,operation,&serde_json::from_str(&prior)?,receipt)?;
        } else {
            ensure!(
                completed.is_none(),
                "cannot attribute an already-reconciled merge retroactively"
            );
            tx.execute(
                "INSERT INTO merge_mutation_receipt VALUES(?1,?2)",
                params![operation, raw],
            )?;
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "merge_mutation_accepted",
                &serde_json::json!({"operation":operation,"receipt":receipt}).to_string(),
                now,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn has_merge_mutation(&self, operation: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM merge_mutation_receipt WHERE operation=?1)",
            [operation],
            |r| r.get(0),
        )?)
    }

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
        let submitted_alias: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM command_job WHERE workflow=?1)", [&alias], |row| row.get(0))?;
        ensure!(alias == lease.workflow || !submitted_alias, "submission-alias-conflict");

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
    /// Completed evidence groups are not approvals. Keep them separate from the
    /// review index; interrupted audit generations cannot lend them authority.
    pub fn review_fragment(&mut self, lease: &Lease, binding: &Binding, key: &str, now: i64) -> Result<Option<String>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        if owner_inbox::pending(&tx,&lease.workflow)?>0 {return Ok(None);}
        let generation:i64=tx.query_row("SELECT generation FROM workflow_audit_generation WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).optional()?.unwrap_or(0);
        let value=tx.query_row("SELECT payload FROM review_fragment WHERE workflow=?1 AND audit_generation=?2 AND key=?3",
            params![lease.workflow,generation,key],|r|r.get(0)).optional()?;
        tx.commit()?;
        Ok(value)
    }
    pub fn record_review_fragment(&mut self, lease: &Lease, binding: &Binding, key: &str, payload: &str, now: i64) -> Result<()> {
        ensure!(key.len()==64 && key.bytes().all(|b|b.is_ascii_hexdigit()),"invalid review fragment key");
        ensure!(!payload.is_empty() && payload.len()<=8*1024*1024,"review fragment exceeds storage bound");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let generation:i64=tx.query_row("SELECT generation FROM workflow_audit_generation WHERE workflow=?1",[&lease.workflow],|r|r.get(0)).optional()?.unwrap_or(0);
        let prior:Option<String>=tx.query_row("SELECT payload FROM review_fragment WHERE workflow=?1 AND audit_generation=?2 AND key=?3",
            params![lease.workflow,generation,key],|r|r.get(0)).optional()?;
        if let Some(prior)=prior { ensure!(prior==payload,"immutable review fragment conflict"); }
        else {
            tx.execute("INSERT INTO review_fragment VALUES(?1,?2,?3,?4)",params![lease.workflow,generation,key,payload])?;
            event(&tx,&lease.workflow,lease.generation,"review_fragment_completed",&serde_json::json!({"key":key,"audit_generation":generation}).to_string(),now)?;
        }
        tx.commit()?;
        Ok(())
    }

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
        if self.pending_owner_messages(workflow)?>0 {return Ok(Vec::new());}
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
        self.charge_inner(lease, receipt_id, charge, now_ms, None, None)
    }

    pub fn charge_observation(&mut self, lease: &Lease, receipt_id: &str,
        charge: ResourceCharge, now_ms: i64, daily_cap: u64) -> Result<()> {
        ensure!(lease.workflow.starts_with("autoship-observation/") || lease.workflow.starts_with("autoship-canary/"), "observation budget requires diagnostic workflow namespace");
        self.charge_inner(lease, receipt_id, charge, now_ms, Some(daily_cap), None)
    }

    /// Bound actual native dispatches in the same transaction as quota charging.
    /// A setup/quota refusal consumes neither; an admitted lost response consumes both.
    pub fn charge_bounded_call(&mut self, lease:&Lease, receipt_id:&str, now_ms:i64,
        observation_cap:Option<u64>, purpose:&str, binding:&Binding, limit:u32)->Result<()> {
        ensure!(observation_cap.is_none() || lease.workflow.starts_with("autoship-observation/") || lease.workflow.starts_with("autoship-canary/"),
            "observation budget requires diagnostic workflow namespace");
        self.charge_inner(lease,receipt_id,ResourceCharge{model_calls:1,..Default::default()},now_ms,observation_cap,
            Some((purpose,binding,limit)))
    }

    pub fn observation_available(&self, now_ms: i64, required: u64, daily_cap: u64) -> Result<bool> {
        observation_available(&self.conn, now_ms, required, daily_cap)
    }

    pub fn reserve_observation(&mut self, lease: &Lease, calls: u64, cap: u64, now: i64, allow_shipping: bool) -> Result<bool> {
        ensure!(lease.workflow.starts_with("autoship-canary/"),"whole probe reservation requires canary namespace");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx,lease,now)?;
        let prior: Option<u64>=tx.query_row("SELECT reserved FROM observation_reservation WHERE workflow=?1 AND generation=?2",params![lease.workflow,lease.generation],|r|r.get(0)).optional()?;
        if let Some(prior)=prior { ensure!(prior==calls,"probe reservation changed"); return Ok(true); }
        if !observation_capacity(&tx,now,calls,cap,allow_shipping)? { return Ok(false); }
        tx.execute("INSERT INTO observation_reservation(workflow,generation,reserved) VALUES(?1,?2,?3)",params![lease.workflow,lease.generation,i64::try_from(calls)?])?;
        tx.commit()?; Ok(true)
    }

    fn charge_inner(&mut self, lease: &Lease, receipt_id: &str,
        charge: ResourceCharge, now_ms: i64, observation_cap: Option<u64>, bounded:Option<(&str,&Binding,u32)>) -> Result<()> {
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
            if charge.model_calls>0 {owner_turn::require_native_dispatch(&tx,lease,now_ms)?;}
            if let Some((purpose,binding,limit))=bounded {
                ensure!(!purpose.is_empty() && limit>0,"bounded model-call purpose and limit required");
                require_binding(&tx,lease,binding,now_ms)?;
                // Calibration uses one observation identity per UTC day. An
                // admitted lost response still counts after audit recovery
                // changes its evidence binding; never count only successes.
                if let Some(day)=purpose.strip_prefix("triage-calibration-day:") {
                    ensure!(day.parse::<i64>()?==now_ms.div_euclid(86_400_000),"calibration day changed before admission");
                    ensure!(lease.workflow.starts_with("autoship-observation/"),"calibration requires observation identity");
                    let admitted:u64=tx.query_row("SELECT COALESCE(SUM(model_calls),0) FROM resource_charge WHERE workflow=?1",params![lease.workflow],|r|r.get(0))?;
                    ensure!(admitted<u64::from(limit),"daily calibration admitted-call budget exhausted");
                }
                let key=serde_json::to_string(binding)?;
                let count:u32=tx.query_row("SELECT attempts FROM bounded_attempt WHERE workflow=?1 AND purpose=?2 AND binding=?3",
                    params![lease.workflow,purpose,key],|r|r.get(0)).optional()?.unwrap_or(0);
                ensure!(count<limit,"bounded final-authority actual native calls exhausted");
                tx.execute("INSERT INTO bounded_attempt(workflow,purpose,binding,attempts) VALUES(?1,?2,?3,1) ON CONFLICT(workflow,purpose,binding) DO UPDATE SET attempts=attempts+1",
                    params![lease.workflow,purpose,key])?;
                event(&tx,&lease.workflow,lease.generation,"native_attempt_started",purpose,now_ms)?;
            }
            if let Some(cap) = observation_cap {
                let reservation: Option<(u64,u64)>=tx.query_row("SELECT reserved,used FROM observation_reservation WHERE workflow=?1 AND generation=?2",params![lease.workflow,lease.generation],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some((reserved,used))=reservation {
                    ensure!(used.checked_add(charge.model_calls).is_some_and(|n|n<=reserved),"canary exceeded its complete probe reservation");
                    tx.execute("UPDATE observation_reservation SET used=used+?3 WHERE workflow=?1 AND generation=?2",params![lease.workflow,lease.generation,values.1])?;
                } else {
                    ensure!(observation_available(&tx, now_ms, charge.model_calls, cap)?, "optional observation deferred: shipping active or daily model-call budget exhausted");
                }
                tx.execute("INSERT INTO observation_charge VALUES(?1,?2,?3,?4)",params![lease.workflow,receipt_id,now_ms.div_euclid(86_400_000),values.1])?;
            }
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
            // Retain the observation time, not only cumulative totals, so a
            // seven-day report never attributes old spend to today's work.
            // This shares the receipt's insert transaction and is idempotent.
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "resource_charged",
                receipt_id,
                now_ms,
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

    /// Strategy budgets belong to a contract/base/runner epoch, not a mutable
    /// candidate head. Committing an unsuccessful fix cannot reset this budget.
    pub fn admit_epoch_attempt(
        &mut self,
        lease: &Lease,
        purpose: &str,
        epoch: &str,
        limit: u32,
        now: i64,
    ) -> Result<bool> {
        ensure!(
            !purpose.is_empty() && !epoch.is_empty() && epoch.len() <= 1024 && limit > 0,
            "bounded epoch purpose and positive limit required"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let key = format!("epoch:{epoch}");
        let count:u32=tx.query_row("SELECT attempts FROM bounded_attempt WHERE workflow=?1 AND purpose=?2 AND binding=?3",params![lease.workflow,purpose,key],|r|r.get(0)).optional()?.unwrap_or(0);
        if count >= limit {
            return Ok(false);
        }
        tx.execute("INSERT INTO bounded_attempt VALUES(?1,?2,?3,1) ON CONFLICT(workflow,purpose,binding) DO UPDATE SET attempts=attempts+1",params![lease.workflow,purpose,key])?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "epoch_attempt_admitted",
            &serde_json::json!({"purpose":purpose,"epoch":epoch,"attempt":count+1}).to_string(),
            now,
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Check fencing immediately before a remote operation, in addition to
    /// using the remote API's own compare-and-swap head precondition. A local
    /// SQLite lease alone cannot fence a push already accepted by GitHub.
    /// The pointer is controller state; failed generations remain immutable on disk.
    pub fn audit_generation(&self, workflow: &str) -> Result<i64> {
        Ok(self.conn.query_row("SELECT generation FROM workflow_audit_generation WHERE workflow=?1",
            [workflow], |row| row.get(0)).optional()?.unwrap_or(0))
    }

    /// Publish a prepared replacement sink and invalidate all current evidence in
    /// the same fenced transaction. The old sink is never repaired or deleted.
    pub fn recover_audit_generation(&mut self, lease: &Lease, previous: i64,
        recovery: &serde_json::Value, now: i64) -> Result<Binding> {
        let raw = serde_json::to_string(recovery)?;
        ensure!(raw.len() <= 1024 * 1024, "audit recovery evidence exceeds bound");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let current: Option<(i64,i64)> = tx.query_row(
            "SELECT generation,lease_generation FROM workflow_audit_generation WHERE workflow=?1",
            [&lease.workflow], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        ensure!(current.map(|v|v.0).unwrap_or(0) == previous, "audit generation moved");
        ensure!(lease.generation > previous, "replacement audit namespace must be fresh");
        ensure!(current.is_none_or(|v|lease.generation > v.1), "audit recovery requires a new fenced lease");
        let checkpoint: String = tx.query_row("SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow], |r|r.get(0))?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&checkpoint)?;
        let marker = format!("audit-recovery:{:x}",Sha256::digest(raw.as_bytes()));
        checkpoint.binding.evidence_hash = format!("{:x}", Sha256::digest(
            format!("{}:{marker}",checkpoint.binding.evidence_hash).as_bytes()));
        checkpoint.stage = Stage::Reconcile;
        checkpoint.evidence = vec![marker];
        checkpoint.next = NextAction { reason:"audit_generation_recovered".into(),
            action:"reverify_candidate".into(),eligible_at_ms:now };
        tx.execute("INSERT INTO workflow_audit_generation VALUES(?1,?2,?3,?4) ON CONFLICT(workflow) DO UPDATE SET generation=excluded.generation,lease_generation=excluded.lease_generation,recovery=excluded.recovery",
            params![lease.workflow,lease.generation,lease.generation,raw])?;
        tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",
            params![lease.workflow,serde_json::to_string(&checkpoint)?,now])?;
        // This is a current-authority index, not the immutable history (which
        // remains in review_recorded events). Hash recomputation can return to
        // the same candidate binding after recovery; it must not revive approval.
        tx.execute("DELETE FROM review WHERE workflow=?1", [&lease.workflow])?;
        if let Some(old_path)=recovery.get("old_audit_path").and_then(|value|value.as_str()) {
            tx.execute("UPDATE workflow_native_owner SET retirement_reason=COALESCE(retirement_reason,'quiescent_audit_recovery') WHERE workflow=?1 AND audit_path=?2 AND lease_generation<?3 AND retired_at IS NULL",
                params![lease.workflow,old_path,lease.generation])?;
        }
        event(&tx,&lease.workflow,lease.generation,"audit_generation_recovered",&raw,now)?;
        tx.commit()?;
        Ok(checkpoint.binding)
    }

    /// Immutable owner registration precedes any native transport dispatch.
    pub fn register_native_owner(&mut self,lease:&Lease,audit_path:&str,now:i64)->Result<()> {
        ensure!(std::path::Path::new(audit_path).is_absolute() && audit_path.len()<=4096,"invalid native owner audit path");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx,lease,now)?;
        let old:Option<String>=tx.query_row("SELECT audit_path FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,lease.generation],|row|row.get(0)).optional()?;
        if let Some(old)=old {ensure!(old==audit_path,"native owner audit path is immutable");}
        else {
            tx.execute("INSERT INTO workflow_native_owner(workflow,lease_generation,audit_path) VALUES(?1,?2,?3)",params![lease.workflow,lease.generation,audit_path])?;
            event(&tx,&lease.workflow,lease.generation,"native_runtime_owner_registered",audit_path,now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Rotate a bounded free reconciliation window before external filesystem IO.
    pub fn pending_native_retirements(&mut self,now:i64,limit:usize)->Result<Vec<(String,i64,String)>> {
        ensure!(limit<=64,"native retirement batch too large");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows={
            let mut statement=tx.prepare("SELECT workflow,lease_generation,audit_path FROM workflow_native_owner WHERE retirement_reason IS NOT NULL AND retired_at IS NULL ORDER BY attempted_at,workflow,lease_generation LIMIT ?1")?;
            let rows=statement.query_map([limit as i64],|row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (workflow,generation,_) in &rows {
            tx.execute("UPDATE workflow_native_owner SET attempted_at=?3 WHERE workflow=?1 AND lease_generation=?2",params![workflow,generation,now])?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// Acknowledgement is idempotent and only settles the exact authorized intent.
    pub fn complete_native_retirement(&mut self,workflow:&str,generation:i64,audit_path:&str,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed=tx.execute("UPDATE workflow_native_owner SET retired_at=?4 WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3 AND retirement_reason IS NOT NULL AND retired_at IS NULL",
            params![workflow,generation,audit_path,now])?;
        if changed>0 {event(&tx,workflow,generation,"native_runtime_owner_retired",audit_path,now)?;}
        tx.commit()?;
        Ok(())
    }

    pub fn validate_dispatch(&mut self,lease:&Lease,binding:&Binding,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        owner_turn::require_native_dispatch(&tx,lease,now)?;
        tx.commit()?;Ok(())
    }

    pub fn validate_lease(&mut self, lease: &Lease, binding: &Binding, now_ms: i64) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// Fenced, idempotent convergence accounting. Full observations remain in
    /// the journal; only the bounded best-candidate controller state is folded.
    pub fn admit_episode_action(
        &mut self,
        lease: &Lease,
        epoch: &str,
        action: &str,
        now: i64,
    ) -> Result<bool> {
        ensure!(
            matches!(action, "fresh_session" | "rescope"),
            "unknown episode action"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let state: String = tx.query_row(
            "SELECT state FROM episode_state WHERE workflow=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        ensure!(
            serde_json::from_str::<crate::autoship_episode::Episode>(&state)?.epoch == epoch,
            "episode action epoch changed"
        );
        let inserted = tx.execute(
            "INSERT INTO episode_action VALUES(?1,?2,?3,?4) ON CONFLICT DO NOTHING",
            params![lease.workflow, epoch, action, now],
        )? == 1;
        if inserted {
            tx.execute(
                "INSERT INTO episode_dispatch VALUES(?1,?2,?3,'prepared','{}')",
                params![lease.workflow, epoch, action],
            )?;
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "episode_action_admitted",
                action,
                now,
            )?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    pub fn episode_dispatch(
        &self,
        workflow: &str,
        epoch: &str,
    ) -> Result<Vec<(String, String, String)>> {
        let mut statement = self.conn.prepare("SELECT action,phase,payload FROM episode_dispatch WHERE workflow=?1 AND epoch=?2 ORDER BY action")?;
        let rows = statement.query_map(params![workflow, epoch], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn checkpoint_episode_dispatch(
        &mut self,
        lease: &Lease,
        epoch: &str,
        action: &str,
        phase: &str,
        payload: &str,
        now: i64,
    ) -> Result<()> {
        ensure!(
            payload.len() <= 32 * 1024,
            "episode checkpoint exceeds context budget"
        );
        ensure!(
            matches!(
                phase,
                "prepared"
                    | "scope_dispatch"
                    | "scope_complete"
                    | "repair_dispatch"
                    | "repair_complete"
                    | "reconciled"
            ),
            "invalid episode phase"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        let state: String = tx.query_row(
            "SELECT state FROM episode_state WHERE workflow=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        ensure!(
            serde_json::from_str::<crate::autoship_episode::Episode>(&state)?.epoch == epoch,
            "episode epoch changed"
        );
        let old: String = tx.query_row(
            "SELECT phase FROM episode_dispatch WHERE workflow=?1 AND epoch=?2 AND action=?3",
            params![lease.workflow, epoch, action],
            |r| r.get(0),
        )?;
        let legal = (old == phase && !matches!(phase, "scope_dispatch" | "repair_dispatch"))
            || matches!(
                (old.as_str(), phase),
                (
                    "prepared",
                    "scope_dispatch" | "repair_dispatch" | "reconciled"
                ) | ("scope_dispatch", "scope_complete" | "reconciled")
                    | ("scope_complete", "repair_dispatch" | "reconciled")
                    | ("repair_dispatch", "repair_complete" | "reconciled")
                    | ("uncertain_legacy", "reconciled")
            );
        ensure!(legal, "invalid episode transition {old} -> {phase}");
        tx.execute("UPDATE episode_dispatch SET phase=?4,payload=?5 WHERE workflow=?1 AND epoch=?2 AND action=?3",params![lease.workflow,epoch,action,phase,payload])?;
        event(&tx,&lease.workflow,lease.generation,"episode_dispatch_checkpoint",&serde_json::json!({"epoch":epoch,"action":action,"phase":phase,"payload":serde_json::from_str::<serde_json::Value>(payload)?}).to_string(),now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn observe_episode(
        &mut self,
        lease: &Lease,
        observation: &crate::autoship_episode::Observation,
        now_ms: i64,
    ) -> Result<crate::autoship_episode::Decision> {
        use crate::autoship_episode::{Decision, Episode};
        let raw = serde_json::to_string(observation)?;
        ensure!(
            raw.len() <= 32 * 1024,
            "episode observation exceeds bounded context"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, &observation.candidate.binding, now_ms)?;
        let prior:Option<(String,String)>=tx.query_row("SELECT observation,decision FROM episode_observation WHERE workflow=?1 AND receipt=?2",params![lease.workflow,observation.receipt],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((old, decision)) = prior {
            ensure!(old == raw, "episode receipt cannot be rewritten");
            return Ok(serde_json::from_str::<Decision>(&decision)?);
        }
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM episode_state WHERE workflow=?1",
                [&lease.workflow],
                |r| r.get(0),
            )
            .optional()?;
        let mut state: Episode = state
            .map(|raw| serde_json::from_str(&raw))
            .transpose()?
            .unwrap_or_default();
        let decision = state.observe(observation)?;
        tx.execute("INSERT INTO episode_state VALUES(?1,?2) ON CONFLICT(workflow) DO UPDATE SET state=excluded.state",params![lease.workflow,serde_json::to_string(&state)?])?;
        tx.execute(
            "INSERT INTO episode_observation VALUES(?1,?2,?3,?4)",
            params![
                lease.workflow,
                observation.receipt,
                raw,
                serde_json::to_string(&decision)?
            ],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "episode_observed",
            &raw,
            now_ms,
        )?;
        tx.commit()?;
        Ok(decision)
    }

    pub fn events(&self, workflow: &str) -> Result<Vec<(i64, String, String, i64)>> {
        let mut statement = self.conn.prepare("SELECT generation,kind,payload,at_ms FROM workflow_event WHERE workflow=?1 ORDER BY seq")?;
        let rows = statement.query_map([workflow], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn save_publication(&mut self,lease:&Lease,binding:&Binding,repository:&str,body:&str,now:i64)->Result<()> {
        ensure!(body.len()<=256*1024,"publication request exceeds bound");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        tx.execute("INSERT INTO pending_publication VALUES(?1,?2,?3,?4) ON CONFLICT(workflow) DO UPDATE SET body=excluded.body,updated_at=excluded.updated_at",
            params![lease.workflow,repository,body,now])?;
        event(&tx,&lease.workflow,lease.generation,"publication_request",body,now)?;tx.commit()?;Ok(())
    }
    pub fn forget_publication(&mut self,lease:&Lease,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;require_lease(&tx,lease,now)?;
        tx.execute("DELETE FROM pending_publication WHERE workflow=?1",[&lease.workflow])?;tx.commit()?;Ok(())
    }
    /// Advance the scan cursor before external I/O, including unavailable rows.
    pub fn pending_publications(&mut self,repository:&str)->Result<Vec<(String,String)>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cursor:String=tx.query_row("SELECT last_workflow FROM publication_scan_cursor WHERE repository=?1",[repository],|r|r.get(0)).optional()?.unwrap_or_default();
        let values:Vec<(String,String)>={
            let mut stmt=tx.prepare("SELECT workflow,body FROM pending_publication WHERE repository=?1 ORDER BY CASE WHEN workflow>?2 THEN 0 ELSE 1 END,workflow LIMIT 16")?;
            let rows=stmt.query_map(params![repository,cursor],|r|Ok((r.get(0)?,r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if let Some((last,_))=values.last() {
            tx.execute("INSERT INTO publication_scan_cursor VALUES(?1,?2) ON CONFLICT(repository) DO UPDATE SET last_workflow=excluded.last_workflow",params![repository,last])?;
        }
        tx.commit()?;Ok(values)
    }
    pub fn publication_dispatch(&self,operation:&str)->Result<Option<(i64,i64,Option<u64>)>> {
        Ok(self.conn.query_row("SELECT attempt,dispatched_at,pr FROM publication_dispatch WHERE operation=?1",[operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)
    }
    pub fn reserve_publication_dispatch(&mut self,lease:&Lease,operation:&Operation,now:i64)->Result<i64> {
        ensure!(operation.kind==OperationKind::CreatePr,"not a publication operation");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,&operation.binding,now)?;
        owner_control::require_dispatch(&tx,&lease.workflow)?;
        let owner:String=tx.query_row("SELECT workflow FROM operation WHERE id=?1 AND receipt IS NULL",[&operation.id],|r|r.get(0))?;
        ensure!(owner==lease.workflow,"publication operation ownership mismatch");
        let prior:Option<(i64,i64,Option<u64>)>=tx.query_row("SELECT attempt,dispatched_at,pr FROM publication_dispatch WHERE operation=?1",[&operation.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let attempt=if let Some((count,at,pr))=prior {
            ensure!(pr.is_none(),"known publication identity forbids new creation");
            let delay=60_000_i64.saturating_mul(1_i64 << count.saturating_sub(1).clamp(0,6)).min(3_600_000);
            ensure!(now>=at.saturating_add(delay),"publication visibility backoff remains active");
            count+1
        } else {1};
        tx.execute("INSERT INTO publication_dispatch VALUES(?1,?2,?3,NULL) ON CONFLICT(operation) DO UPDATE SET attempt=excluded.attempt,dispatched_at=excluded.dispatched_at",params![operation.id,attempt,now])?;
        event(&tx,&lease.workflow,lease.generation,"publication_dispatch_reserved",&serde_json::json!({"operation":operation.id,"attempt":attempt}).to_string(),now)?;
        tx.commit()?;Ok(attempt)
    }
    pub fn record_publication_identity(&mut self,lease:&Lease,operation:&Operation,pr:u64,now:i64)->Result<()> {
        ensure!(pr>0,"invalid publication identity");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,&operation.binding,now)?;
        let owner:String=tx.query_row("SELECT workflow FROM operation WHERE id=?1",[&operation.id],|r|r.get(0))?;
        ensure!(owner==lease.workflow,"publication identity ownership mismatch");
        let prior:Option<u64>=tx.query_row("SELECT pr FROM publication_dispatch WHERE operation=?1",[&operation.id],|r|r.get(0))?;
        ensure!(prior.is_none_or(|value|value==pr),"publication identity changed");
        tx.execute("UPDATE publication_dispatch SET pr=?2 WHERE operation=?1",params![operation.id,pr])?;
        event(&tx,&lease.workflow,lease.generation,"publication_direct_identity",&serde_json::json!({"operation":operation.id,"pr":pr}).to_string(),now)?;
        tx.commit()?;Ok(())
    }
    pub fn has_any_pending_publications(&self)->Result<bool> {
        let exists:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='pending_publication')",[],|r|r.get(0))?;
        if !exists { return Ok(false); }
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM pending_publication)",[],|r|r.get(0))?)
    }
    pub fn has_pending_publication(&self,workflow:&str)->Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM pending_publication WHERE workflow=?1)",[workflow],|r|r.get(0))?)
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
    progress::project(tx,workflow,kind,now_ms)?;
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
    fn command_claim_rechecks_late_holds_and_pending_merges_atomically() {
        for stage in [Stage::Held, Stage::AwaitChecks, Stage::Merge, Stage::Build] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let (mut controller,lease)=setup(&path);
            controller.submit_authorized("command","owner",&submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(1),provider:None,until:submission::Milestone::Merged},21).unwrap();
            let mut command=WorkflowStore::open(&path).unwrap();
            assert_eq!(command.snapshot(&lease.workflow).unwrap().unwrap().checkpoint.stage,Stage::Intake);
            let mut checkpoint=controller.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            checkpoint.stage=stage;checkpoint.next.reason="late-owner-hold".into();
            controller.checkpoint(&lease,&checkpoint,30).unwrap();
            if stage==Stage::Build {controller.begin_operation(&lease,OperationKind::MergePr,&binding(),31).unwrap();}
            controller.release(&lease,32).unwrap();
            let before=command.snapshot(&lease.workflow).unwrap().unwrap();
            assert!(command.claim_productive(&lease.workflow,"command",40,1000).unwrap().is_none(), "command bypassed {stage:?}");
            let after=command.snapshot(&lease.workflow).unwrap().unwrap();
            assert_eq!(after.checkpoint,before.checkpoint);assert_eq!(after.generation,before.generation);
        }
    }

    #[test]
    fn productive_claim_cannot_ignore_completed_operation_with_uncertain_dispatch() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let (mut store,lease)=setup(&path);
        store.submit_authorized("command","owner",&submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(1),provider:None,until:submission::Milestone::Merged},21).unwrap();
        store.attach_pr(&lease,1,22).unwrap();
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Build;
        store.checkpoint(&lease,&checkpoint,23).unwrap();
        let op=store.begin_operation(&lease,OperationKind::MergePr,&binding(),24).unwrap();
        store.reserve_merge(&lease,&op,"owner/repo",1,"PR_1",25,5).unwrap();
        store.dispatch_marked_merge(&lease,&op,26,5).unwrap();
        store.complete_operation(&lease,&op.id,"observed-merge",27).unwrap();
        store.release(&lease,28).unwrap();
        assert!(store.merge_reconciliation_needed(&lease.workflow).unwrap());
        assert!(store.pending_operations(&lease.workflow).unwrap().is_empty());
        assert!(store.claim_productive(&lease.workflow,"command",30,1000).unwrap().is_none(),"completed operation hid uncertain merge dispatch");
        assert!(store.claim_reconciliation(&lease.workflow,"sweep",30,1000).unwrap().is_none(),"free claim bypassed uncertain dispatch");
    }

    #[test]
    fn reconciliation_claim_respects_command_ceiling_and_late_hold() {
        use submission::{Request,Target,Milestone};
        for (stage,until,allowed) in [(Stage::Held,Milestone::Merged,false),(Stage::AwaitChecks,Milestone::Pr,false),(Stage::AwaitChecks,Milestone::Merged,true)] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let mut controller=WorkflowStore::open(&path).unwrap();
            controller.submit_authorized("command","owner",&Request {repository:"owner/repo".into(),target:Target::Issue(1),provider:None,until},1).unwrap();
            controller.register("owner/repo#1","feature/fix",binding(),10).unwrap();
            let lease=controller.claim("owner/repo#1","controller",20,1000).unwrap().unwrap();
            let mut reconciler=WorkflowStore::open(&path).unwrap();
            let mut checkpoint=reconciler.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            checkpoint.stage=stage;controller.checkpoint(&lease,&checkpoint,30).unwrap();controller.release(&lease,31).unwrap();
            assert_eq!(reconciler.claim_reconciliation(&lease.workflow,"sweep",40,1000).unwrap().is_some(),allowed,"{stage:?} {until:?}");
        }
    }

    #[test]
    fn terminal_runtime_retirement_intent_survives_crash_and_settles_once() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let (mut store,lease)=setup(&path);
        store.register_native_owner(&lease,"/audit/shared",30).unwrap();
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Merged;
        store.checkpoint(&lease,&checkpoint,31).unwrap();
        drop(store); // crash before the filesystem side effect
        let mut store=WorkflowStore::open(&path).unwrap();
        let expected=vec![(lease.workflow.clone(),lease.generation,"/audit/shared".into())];
        assert_eq!(store.pending_native_retirements(32,16).unwrap(),expected);
        store.complete_native_retirement(&lease.workflow,lease.generation,"/audit/wrong",33).unwrap();
        assert_eq!(store.pending_native_retirements(34,16).unwrap(),expected);
        store.complete_native_retirement(&lease.workflow,lease.generation,"/audit/shared",35).unwrap();
        store.complete_native_retirement(&lease.workflow,lease.generation,"/audit/shared",36).unwrap();
        assert!(store.pending_native_retirements(37,16).unwrap().is_empty());
        assert_eq!(store.events(&lease.workflow).unwrap().iter().filter(|(_,kind,_,_)|kind=="native_runtime_owner_retired").count(),1);
    }
    #[test]
    fn audit_recovery_queues_old_owner_atomically_without_retiring_future_shared_path() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let (mut store,old)=setup(&path);
        store.register_native_owner(&old,"/audit/shared",30).unwrap();
        store.release(&old,31).unwrap();
        assert!(store.pending_native_retirements(32,16).unwrap().is_empty(),"normal park preserves resumable sessions");
        let lease=store.claim(&old.workflow,"recovery",40,1000).unwrap().unwrap();
        assert!(store.register_native_owner(&old,"/audit/shared",41).is_err());
        store.recover_audit_generation(&lease,0,&serde_json::json!({"old_audit_path":"/audit/shared","quiescent":true}),42).unwrap();
        store.register_native_owner(&lease,"/audit/shared",43).unwrap();
        drop(store); // recovery pointer is committed, retirement hasn't executed
        let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.pending_native_retirements(44,16).unwrap(),vec![(old.workflow.clone(),old.generation,"/audit/shared".into())]);
    }

    #[test]
    fn unavailable_publication_frontier_cannot_starve_later_workflows() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        for number in 0..33 {
            let id=format!("publication-{number:02}");
            store.register(&id,"branch",binding(),1).unwrap();
            let lease=store.claim(&id,"fixture",2,1000).unwrap().unwrap();
            store.save_publication(&lease,&binding(),"owner/repo","missing retained worktree",3).unwrap();
        }
        let first=store.pending_publications("owner/repo").unwrap();assert_eq!(first.len(),16);
        drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        let second=store.pending_publications("owner/repo").unwrap();
        assert!(second.iter().all(|row|!first.contains(row)),"failed acquisition must still rotate scan");
        let third=store.pending_publications("owner/repo").unwrap();
        assert!(third.iter().any(|(id,_)|id=="publication-32"));
    }
    #[test]
    fn publication_dispatch_reservation_recovers_interruption_without_model_charge() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
        let (mut store,lease)=setup(&path);
        let op=store.begin_operation(&lease,OperationKind::CreatePr,&binding(),30).unwrap();
        assert_eq!(store.reserve_publication_dispatch(&lease,&op,31).unwrap(),1);
        assert!(store.reserve_publication_dispatch(&lease,&op,32).is_err());
        drop(store);let mut store=WorkflowStore::open(&path).unwrap();
        let next=store.claim(&lease.workflow,"recovery",60_032,1_000_000).unwrap().unwrap();
        // Crash before actual dispatch leaves the same uncertain state as lost
        // response. After visibility reconciliation, same-identity retry is
        // admitted with backoff; GitHub's open head/base uniqueness is required.
        assert_eq!(store.reserve_publication_dispatch(&next,&op,60_033).unwrap(),2);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().model_calls,0);
        store.record_publication_identity(&next,&op,42,60_034).unwrap();
        assert!(store.reserve_publication_dispatch(&next,&op,600_000).is_err());
        assert!(store.record_publication_identity(&next,&op,43,60_035).is_err());
        assert_eq!(store.publication_dispatch(&op.id).unwrap().unwrap().2,Some(42));
    }

    #[test]
    fn completed_review_fragments_survive_restart_but_never_become_approvals() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
        let (mut store,old)=setup(&path);let key="a".repeat(64);
        store.record_review_fragment(&old,&binding(),&key,"completed-group",25).unwrap();
        assert!(store.current_reviews(&old.workflow).unwrap().is_empty());
        assert!(store.record_review_fragment(&old,&binding(),&key,"conflicting-group",26).is_err());
        drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        let new=store.claim(&old.workflow,"worker-b",1021,1000).unwrap().unwrap();
        assert_eq!(store.review_fragment(&new,&binding(),&key,1022).unwrap().as_deref(),Some("completed-group"));
        assert!(store.review_fragment(&old,&binding(),&key,1022).is_err());
        let mut changed=binding();changed.head="other-head".into();
        assert!(store.review_fragment(&new,&changed,&key,1022).is_err());
        // Recovery retains the old immutable row but changes its namespace.
        store.conn.execute("INSERT INTO workflow_audit_generation VALUES(?1,?2,?2,'{}')",params![old.workflow,new.generation]).unwrap();
        assert!(store.review_fragment(&new,&binding(),&key,1022).unwrap().is_none());
        assert!(store.current_reviews(&old.workflow).unwrap().is_empty());
        assert_eq!(store.snapshot(&old.workflow).unwrap().unwrap().model_calls,0);
    }

    #[test]
    fn publication_request_survives_reopen_is_fenced_and_does_not_rebill_models() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
        let (mut store,old)=setup(&path);
        store.charge(&old,"paid-build",ResourceCharge {model_calls:3,..Default::default()},25).unwrap();
        let body=serde_json::json!({"issue":1,"head":"head-a","args":["pr","create"]}).to_string();
        store.save_publication(&old,&binding(),"owner/repo",&body,30).unwrap();
        let before=store.snapshot(&old.workflow).unwrap().unwrap();drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert!(store.has_pending_publication(&old.workflow).unwrap());
        assert_eq!(store.pending_publications("owner/repo").unwrap(),vec![(old.workflow.clone(),body.clone())]);
        assert!(store.pending_publications("other/repo").unwrap().is_empty());
        let new=store.claim(&old.workflow,"free-reconciler",1021,1000).unwrap().unwrap();
        assert!(store.forget_publication(&old,1022).is_err());
        store.save_publication(&new,&binding(),"owner/repo",&body,1023).unwrap();
        assert_eq!(store.snapshot(&old.workflow).unwrap().unwrap().model_calls,before.model_calls);
        store.forget_publication(&new,1024).unwrap();
        assert!(!store.has_any_pending_publications().unwrap());
        assert!(store.events(&old.workflow).unwrap().iter().any(|(_,kind,_,_)|kind=="publication_request"));
    }

    #[test]
    fn audit_recovery_is_fenced_durable_and_invalidates_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, old) = setup(&path);
        store.record_review(&old, &Review { reviewer:"independent".into(),binding:binding(),
            approved:true,evidence:"old approval".into() }, 30).unwrap();
        let lease = store.claim(&old.workflow,"successor",1021,1000).unwrap().unwrap();
        let proof = serde_json::json!({"verified_chain":"digest","quiescent":true,"interrupted_intents":["tool-1"]});
        assert!(store.recover_audit_generation(&old,0,&proof,1022).is_err());
        let current = store.recover_audit_generation(&lease,0,&proof,1022).unwrap();
        assert_ne!(current,binding());
        assert!(store.current_reviews(&old.workflow).unwrap().is_empty());
        let mut reset = store.snapshot(&old.workflow).unwrap().unwrap().checkpoint;
        reset.binding = binding();
        store.checkpoint(&lease, &reset, 1023).unwrap();
        assert!(store.current_reviews(&old.workflow).unwrap().is_empty(),
            "recomputing the previous candidate hash cannot revive old approval");
        reset.binding = current.clone();
        store.checkpoint(&lease, &reset, 1023).unwrap();
        assert!(store.recover_audit_generation(&lease,1,&proof,1023).is_err());
        drop(store);
        let reopened = WorkflowStore::open(&path).unwrap();
        assert_eq!(reopened.audit_generation(&old.workflow).unwrap(),lease.generation);
        let checkpoint = reopened.snapshot(&old.workflow).unwrap().unwrap().checkpoint;
        assert_eq!(checkpoint.next.action,"reverify_candidate");
        assert_eq!(checkpoint.binding,current);
        let events = reopened.events(&old.workflow).unwrap();
        assert!(events.iter().any(|(_,kind,payload,_)|kind=="audit_generation_recovered" && payload.contains("tool-1")));
        assert!(events.iter().any(|(_,kind,payload,_)|kind=="review_recorded" && payload.contains("old approval")));
    }

    #[test]
    fn episode_dispatch_is_fenced_and_once_per_epoch_across_restart() {
        use crate::autoship_episode::{Candidate, Observation};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, lease) = setup(&path);
        let mut observation = Observation {
            receipt: "review-1".into(),
            epoch: "epoch-a".into(),
            candidate: Candidate {
                binding: binding(),
                tests_passed: true,
                guards_passed: true,
                verified_criteria: 1,
                open_findings: ["finding".into()].into_iter().collect(),
                lines_changed: 1,
            },
            review_complete: true,
            contradictory_requirements: false,
            infrastructure_failed: false,
            autonomous_merge_observed: false,
        };
        store.observe_episode(&lease, &observation, 30).unwrap();
        assert!(store
            .admit_episode_action(&lease, "epoch-a", "fresh_session", 31)
            .unwrap());
        assert!(store
            .admit_episode_action(&lease, "epoch-a", "rescope", 32)
            .unwrap());
        store
            .checkpoint_episode_dispatch(
                &lease,
                "epoch-a",
                "fresh_session",
                "repair_dispatch",
                r#"{"best_head":"head-a"}"#,
                32,
            )
            .unwrap();
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        for action in ["fresh_session", "rescope"] {
            assert!(!store
                .admit_episode_action(&lease, "epoch-a", action, 33)
                .unwrap());
        }
        let recovered = store.episode_dispatch(&lease.workflow, "epoch-a").unwrap();
        assert!(
            recovered
                .iter()
                .any(|(action, phase, _)| action == "rescope" && phase == "prepared"),
            "admitted but unlaunched intent must remain resumable"
        );
        assert!(
            recovered
                .iter()
                .any(|(action, phase, _)| action == "fresh_session" && phase == "repair_dispatch"),
            "uncertain launch must survive restart"
        );
        assert!(
            store
                .checkpoint_episode_dispatch(
                    &lease,
                    "epoch-a",
                    "fresh_session",
                    "prepared",
                    "{}",
                    34
                )
                .is_err(),
            "unknown launch cannot be reset and repeated"
        );
        store
            .checkpoint_episode_dispatch(
                &lease,
                "epoch-a",
                "fresh_session",
                "reconciled",
                r#"{"review_receipt":"review-after-crash"}"#,
                35,
            )
            .unwrap();
        assert!(store
            .checkpoint_episode_dispatch(
                &lease,
                "epoch-a",
                "fresh_session",
                "repair_dispatch",
                "{}",
                36
            )
            .is_err());
        store
            .checkpoint_episode_dispatch(&lease, "epoch-a", "rescope", "scope_dispatch", "{}", 37)
            .unwrap();
        store
            .checkpoint_episode_dispatch(
                &lease,
                "epoch-a",
                "rescope",
                "scope_complete",
                r#"{"plan":"preserved plan"}"#,
                38,
            )
            .unwrap();
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        assert!(store
            .episode_dispatch(&lease.workflow, "epoch-a")
            .unwrap()
            .iter()
            .any(|(_, phase, payload)| phase == "scope_complete"
                && payload.contains("preserved plan")));
        assert!(store
            .admit_episode_action(&lease, "epoch-b", "rescope", 34)
            .is_err());
        let next = store
            .claim(&lease.workflow, "new-worker", 1021, 1000)
            .unwrap()
            .unwrap();
        assert!(store
            .admit_episode_action(&lease, "epoch-a", "rescope", 1022)
            .is_err());
        observation.epoch = "epoch-b".into();
        observation.receipt = "review-2".into();
        store.observe_episode(&next, &observation, 1023).unwrap();
        assert!(store
            .admit_episode_action(&next, "epoch-b", "rescope", 1024)
            .unwrap());
        assert!(!store
            .admit_episode_action(&next, "epoch-b", "rescope", 1025)
            .unwrap());
    }
    #[test]
    fn autonomous_attribution_requires_accepted_exact_mutation_before_confirmation() {
        for accepted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let (mut store, lease) = setup(&path);
            store.attach_pr(&lease, 42, 30).unwrap();
            let operation = store
                .begin_operation(&lease, OperationKind::MergePr, &binding(), 31)
                .unwrap();
            let receipt = serde_json::json!({"client_mutation_id":operation.id,"expected_head":binding().head,
                "pr_number":42,"pr_node_id":"PR_fixture","merge_commit":"merged-sha","merged_by":"owner"});
            let mut wrong = receipt.clone();
            wrong["expected_head"] = serde_json::json!("other-head");
            assert!(store
                .record_merge_mutation(&lease, &operation.id, &wrong, 32)
                .is_err());
            if accepted {
                store
                    .record_merge_mutation(&lease, &operation.id, &receipt, 33)
                    .unwrap();
            }
            store
                .complete_operation(&lease, &operation.id, "observed-merged-exact", 34)
                .unwrap();
            drop(store);
            let mut store = WorkflowStore::open(&path).unwrap();
            assert_eq!(store.has_merge_mutation(&operation.id).unwrap(), accepted);
            if !accepted {
                assert!(
                    store
                        .record_merge_mutation(&lease, &operation.id, &receipt, 35)
                        .is_err(),
                    "manual/lost-response merge must not gain retroactive attribution"
                );
            } else {
                store
                    .record_merge_mutation(&lease, &operation.id, &receipt, 35)
                    .unwrap();
                let mut changed = receipt.clone();
                changed["merged_by"] = serde_json::json!("other-actor");
                assert!(store
                    .record_merge_mutation(&lease, &operation.id, &changed, 36)
                    .is_err());
            }
        }
    }

    #[test]
    fn completed_scope_response_survives_worker_restart_and_is_not_an_approval() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let (mut store,old)=setup(&path);let key="a".repeat(64);
        assert!(store.scope_stage_response(&old,&binding(),&key,30).unwrap().is_none());
        store.record_scope_stage_response(&old,&binding(),&key,"synthetic output",31).unwrap();
        assert!(store.record_scope_stage_response(&old,&binding(),&key,"different output",32).is_err());
        drop(store);let mut store=WorkflowStore::open(&path).unwrap();let new=store.claim(&old.workflow,"next",1021,1000).unwrap().unwrap();
        assert_eq!(store.scope_stage_response(&new,&binding(),&key,1022).unwrap().as_deref(),Some("synthetic output"));
        assert!(store.scope_stage_response(&old,&binding(),&key,1022).is_err());
        assert_eq!(store.conn.query_row("SELECT COUNT(*) FROM review",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }

    #[test]
    fn expired_controller_cannot_install_or_clear_readiness_dependency() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let (mut store,old)=setup(&path);
        store.set_readiness_dependency(&old,&binding(),"repo",970,Some(("request","base","external_decision","evidence")),30).unwrap();
        let current=store.claim(&old.workflow,"next",1021,1000).unwrap().unwrap();
        assert!(store.set_readiness_dependency(&old,&binding(),"repo",970,None,1022).is_err());
        assert!(store.set_readiness_dependency(&old,&binding(),"repo",971,Some(("request","base","external_decision","evidence")),1022).is_err());
        let count:i64=store.conn.query_row("SELECT COUNT(*) FROM issue_readiness_dependencies",[],|r|r.get(0)).unwrap();assert_eq!(count,1);
        store.set_readiness_dependency(&current,&binding(),"repo",970,None,1023).unwrap();
        let count:i64=store.conn.query_row("SELECT COUNT(*) FROM issue_readiness_dependencies",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
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
    fn epoch_budget_survives_candidate_commits_restart_and_fences_stale_worker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, lease) = setup(&path);
        assert!(store
            .admit_epoch_attempt(&lease, "acceptance-repair", "contract/base/runner", 2, 30)
            .unwrap());
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.binding.head = "new-failed-candidate".into();
        store.checkpoint(&lease, &checkpoint, 31).unwrap();
        assert!(store
            .admit_epoch_attempt(&lease, "acceptance-repair", "contract/base/runner", 2, 32)
            .unwrap());
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        let successor = store
            .claim(&lease.workflow, "new-worker", 1021, 1000)
            .unwrap()
            .unwrap();
        assert!(!store
            .admit_epoch_attempt(
                &successor,
                "acceptance-repair",
                "contract/base/runner",
                2,
                1022
            )
            .unwrap());
        assert!(store
            .admit_epoch_attempt(&lease, "acceptance-repair", "new-epoch", 2, 1023)
            .is_err());
        assert!(store
            .admit_epoch_attempt(&successor, "acceptance-repair", "new-epoch", 2, 1023)
            .unwrap());
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
    fn episode_process_crash_worker() {
        let Some(path) = std::env::var_os("AUGMENTAGENT_EPISODE_CRASH_DB") else {
            return;
        };
        let phase = std::env::var("AUGMENTAGENT_EPISODE_CRASH_PHASE").unwrap();
        let (mut store, lease) = setup(Path::new(&path));
        let observation = crate::autoship_episode::Observation {
            receipt: "review-before-crash".into(),
            epoch: "epoch".into(),
            candidate: crate::autoship_episode::Candidate {
                binding: binding(),
                tests_passed: true,
                guards_passed: true,
                verified_criteria: 1,
                open_findings: ["finding".into()].into_iter().collect(),
                lines_changed: 1,
            },
            review_complete: true,
            contradictory_requirements: false,
            infrastructure_failed: false,
            autonomous_merge_observed: false,
        };
        store.observe_episode(&lease, &observation, 30).unwrap();
        store
            .admit_episode_action(&lease, "epoch", "rescope", 31)
            .unwrap();
        if phase != "prepared" {
            store
                .checkpoint_episode_dispatch(&lease, "epoch", "rescope", "scope_dispatch", "{}", 32)
                .unwrap();
            store
                .checkpoint_episode_dispatch(
                    &lease,
                    "epoch",
                    "rescope",
                    "scope_complete",
                    r#"{"plan":"durable-plan"}"#,
                    33,
                )
                .unwrap();
        }
        if phase == "repair_dispatch" {
            store
                .checkpoint_episode_dispatch(
                    &lease,
                    "epoch",
                    "rescope",
                    "repair_dispatch",
                    r#"{"plan":"durable-plan"}"#,
                    34,
                )
                .unwrap();
        }
        // No destructor or graceful release: the parent must recover exactly
        // the last committed phase and fence this dead worker after expiry.
        std::process::exit(73);
    }

    #[test]
    fn process_death_preserves_unlaunched_plans_and_uncertain_dispatch() {
        let test_name = format!(
            "{}::episode_process_crash_worker",
            module_path!().split_once("::").unwrap().1
        );
        for phase in ["prepared", "scope_complete", "repair_dispatch"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env("AUGMENTAGENT_EPISODE_CRASH_DB", &path)
                .env("AUGMENTAGENT_EPISODE_CRASH_PHASE", phase)
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let mut store = WorkflowStore::open(&path).unwrap();
            let recovered = store.episode_dispatch("owner/repo#1", "epoch").unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].1, phase);
            if phase != "prepared" {
                assert!(recovered[0].2.contains("durable-plan"));
            }
            assert!(store
                .claim("owner/repo#1", "successor", 100, 1000)
                .unwrap()
                .is_none());
            let lease = store
                .claim("owner/repo#1", "successor", 1021, 1000)
                .unwrap()
                .unwrap();
            if phase == "repair_dispatch" {
                assert!(store
                    .checkpoint_episode_dispatch(
                        &lease,
                        "epoch",
                        "rescope",
                        "repair_dispatch",
                        "{}",
                        1022
                    )
                    .is_err());
                store
                    .checkpoint_episode_dispatch(
                        &lease,
                        "epoch",
                        "rescope",
                        "reconciled",
                        r#"{"review_receipt":"verified-after-death"}"#,
                        1023,
                    )
                    .unwrap();
            } else {
                store
                    .checkpoint_episode_dispatch(
                        &lease,
                        "epoch",
                        "rescope",
                        "repair_dispatch",
                        &recovered[0].2,
                        1022,
                    )
                    .unwrap();
            }
        }
    }

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

#[cfg(test)]
mod observation_accounting_tests {
    use super::*;
    fn binding() -> Binding { Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()} }
    #[test]
    fn bounded_native_admission_rolls_back_quota_refusal_and_retains_started_calls() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("ledger.db");
        let mut store=WorkflowStore::open(&path).unwrap();let id="autoship-observation/bounded";
        store.register(id,"probe",binding(),1).unwrap();
        let lease=store.claim(id,"first",2,1000).unwrap().unwrap();
        for _ in 0..5 {assert!(store.charge_bounded_call(&lease,"refused",3,Some(0),"component",&binding(),2).is_err());}
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,0);
        store.charge_bounded_call(&lease,"started-lost-response",4,Some(10),"component",&binding(),2).unwrap();
        store.charge_bounded_call(&lease,"started-lost-response",5,Some(10),"component",&binding(),2).unwrap();
        store.release(&lease,6).unwrap();drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();let next=store.claim(id,"next",7,1000).unwrap().unwrap();
        store.charge_bounded_call(&next,"malformed-response",8,Some(10),"component",&binding(),2).unwrap();
        assert!(store.charge_bounded_call(&next,"exhausted",9,Some(10),"component",&binding(),2).is_err());
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,2);
        assert_eq!(store.events(id).unwrap().iter().filter(|(_,kind,_,_)|kind=="native_attempt_started").count(),2);
    }
    #[test]
    fn calibration_day_budget_counts_lost_dispatches_across_restart_and_rebinding() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("ledger.db");
        let id="autoship-observation/calibration-day";
        let mut store=WorkflowStore::open(&path).unwrap();store.register(id,"probe",binding(),1).unwrap();
        let lease=store.claim(id,"first",2,1000).unwrap().unwrap();
        assert!(store.charge_bounded_call(&lease,"not-admitted",3,Some(0),"triage-calibration-day:0",&binding(),2).is_err());
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,0);
        store.charge_bounded_call(&lease,"lost-response",4,Some(12),"triage-calibration-day:0",&binding(),2).unwrap();
        store.release(&lease,5).unwrap();drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();let lease=store.claim(id,"recovered",6,1000).unwrap().unwrap();
        let mut checkpoint=store.snapshot(id).unwrap().unwrap().checkpoint;
        checkpoint.binding.evidence_hash="recovered-audit-generation".into();
        store.checkpoint(&lease,&checkpoint,7).unwrap();
        store.charge_bounded_call(&lease,"second-lost-response",8,Some(12),"triage-calibration-day:0",&checkpoint.binding,2).unwrap();
        assert!(store.charge_bounded_call(&lease,"third-refused",9,Some(12),"triage-calibration-day:0",&checkpoint.binding,2).is_err());
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,2);
    }
    #[test]
    fn observations_are_charged_once_bounded_and_subordinate_to_shipping() {
        let dir=tempfile::tempdir().unwrap();
        let path=dir.path().join("ledger.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let observation="autoship-observation/replay";
        store.register(observation,"probe",binding(),1).unwrap();
        let lease=store.claim(observation,"probe-owner",2,1000).unwrap().unwrap();
        let cost=||ResourceCharge {model_calls:1,..Default::default()};
        store.charge_observation(&lease,"call1",cost(),3,2).unwrap();
        store.charge_observation(&lease,"call1",cost(),4,2).unwrap();
        assert_eq!(store.snapshot(observation).unwrap().unwrap().model_calls,1);
        store.register("owner/repo#1","shipping",binding(),5).unwrap();
        let shipping=store.claim("owner/repo#1","builder",6,1000).unwrap().unwrap();
        assert!(!store.observation_available(7,1,2).unwrap());
        assert!(store.charge_observation(&lease,"blocked",cost(),7,2).is_err());
        assert_eq!(store.snapshot(observation).unwrap().unwrap().model_calls,1);
        // Shipping work retains its own budget; diagnostic exhaustion cannot block it.
        store.charge(&shipping,"real-builder-call",cost(),8).unwrap();
        store.release(&shipping,9).unwrap();
        store.charge_observation(&lease,"call2",cost(),10,2).unwrap();
        assert!(store.charge_observation(&lease,"call3",cost(),11,2).is_err());
        store.release(&lease,12).unwrap();
        drop(store);
        let mut store=WorkflowStore::open(&path).unwrap();
        assert!(!store.observation_available(13,1,2).unwrap());
        assert!(store.observation_available(86_400_001,1,2).unwrap());
        let again=store.claim(observation,"next-day",86_400_001,1000).unwrap().unwrap();
        store.charge_observation(&again,"call3",cost(),86_400_002,2).unwrap();
        assert_eq!(store.snapshot(observation).unwrap().unwrap().model_calls,3);
        assert_eq!(store.events(observation).unwrap().iter().filter(|(_,kind,_,_)|kind=="resource_charged").count(),3);
    }
    #[test]
    fn complete_probe_reservation_survives_midflight_shipping_and_midnight() {
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("ledger.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let id="autoship-canary/probe#840";
        store.register(id,"probe",binding(),1).unwrap();
        let probe=store.claim(id,"probe",2,100_000_000).unwrap().unwrap();
        store.register("owner/repo#1","shipping",binding(),3).unwrap();
        let shipping=store.claim("owner/repo#1","builder",4,100_000_000).unwrap().unwrap();
        assert!(!store.reserve_observation(&probe,11,12,5,false).unwrap());
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,0,"free deferral cannot charge or pass a probe");
        // After a controller-recorded full day of starvation, bounded parallel
        // admission may proceed without delaying or charging shipping capacity.
        assert!(store.reserve_observation(&probe,11,12,6,true).unwrap());
        let cost=||ResourceCharge {model_calls:1,..Default::default()};
        for i in 0..10 { store.charge_observation(&probe,&format!("call-{i}"),cost(),10+i,12).unwrap(); }
        store.charge_observation(&probe,"call-last",cost(),86_400_001,12).unwrap();
        assert!(store.charge_observation(&probe,"overflow",cost(),86_400_002,12).is_err());
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,11);
        store.charge(&shipping,"shipping-call",cost(),86_400_003).unwrap();
        store.release(&shipping,86_400_004).unwrap();
        store.release(&probe,86_400_005).unwrap();
        assert!(store.observation_available(86_400_006,11,12).unwrap());
    }

}
