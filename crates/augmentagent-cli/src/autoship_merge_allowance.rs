//! Confirmed autonomous merges are the only shipping debit. Attempt/resource
//! accounting is independent. Pending mutations retain capacity until proved.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAllowance {
    pub confirmed: u32,
    pub pending: u32,
    pub remaining: u32,
    pub available: u32,
    pub cap: u32,
    pub oldest_pending_at_ms: Option<i64>,
    pub last_merged_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeDispatchRecord {
    pub operation: Operation,
    pub repository: String,
    pub pr: u64,
    pub node: String,
    pub state: String,
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MergeReleaseProof {
    NeverDispatched,
    RequestNotExecuted {
        response_sha256: String,
    },
    ExternalMarkedMerge { receipt: serde_json::Value },
    DifferentMergedHead {
        node: String,
        pr: u64,
        head: String,
        merge_commit: String,
        merged_at: String,
    },
}

/// GraphQL request errors precede execution. Execution errors instead include
/// a `data` entry (possibly null) and must never release uncertain capacity.
/// https://spec.graphql.org/September2025/#sec-Errors
pub fn merge_request_not_executed(raw: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    value.is_object()
        && value.get("data").is_none()
        && value["errors"].as_array().is_some_and(|errors| {
            !errors.is_empty()
                && errors.iter().all(|error| {
                    error["message"]
                        .as_str()
                        .is_some_and(|message| !message.is_empty())
                        && error["locations"].as_array().is_some_and(|locations| {
                            !locations.is_empty()
                                && locations.iter().all(|location| {
                                    location["line"].as_u64().is_some_and(|n| n > 0)
                                        && location["column"].as_u64().is_some_and(|n| n > 0)
                                })
                        })
                })
        })
}

/// Legacy stores have no marker table; a read-only preflight must not migrate
/// them. Marked dispatches always require their exact, unique private proof.
fn validate_merge_marker(conn: &Connection, operation: &str, receipt: &serde_json::Value) -> Result<bool> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='merge_private_marker')", [], |r| r.get(0))?;
    if !exists { return Ok(false); }
    let marker: Option<(i64, String)> = conn.query_row(
        "SELECT protocol,nonce FROM merge_private_marker WHERE operation=?1", [operation],
        |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if let Some((protocol, nonce)) = marker {
        ensure!(protocol == 1, "unsupported merge marker protocol");
        let expected = format!("Autoship-Receipt: v1:{}:{nonce}", operation);
        let message = receipt["merge_message"].as_str().context("marked merge lacks commit message proof")?;
        let mut markers = message.lines().filter(|line| line.trim_start().starts_with("Autoship-Receipt:"));
        ensure!(markers.next() == Some(expected.as_str()) && markers.next().is_none(),
            "merge commit does not uniquely prove this private dispatch");
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn validate_merge_receipt_identity(conn: &Connection, operation: &str, receipt: &serde_json::Value) -> Result<()> {
    let marked = validate_merge_marker(conn, operation, receipt)?;
    if receipt.get("commit_marker_operation").is_some() {
        ensure!(marked && receipt["commit_marker_operation"].as_str() == Some(operation)
            && receipt.get("client_mutation_id").is_none(),
            "immutable commit recovery requires private proof and no fabricated mutation echo");
    } else {
        ensure!(receipt["client_mutation_id"].as_str() == Some(operation),
            "merge response mutation identity differs from intent");
    }
    Ok(())
}

pub(super) fn validate_merge_receipt_equivalence(conn:&Connection, operation:&str, first:&serde_json::Value, later:&serde_json::Value) -> Result<()> {
    validate_merge_receipt_identity(conn,operation,first)?;
    validate_merge_receipt_identity(conn,operation,later)?;
    let normalized = |proof:&serde_json::Value| -> Result<serde_json::Value> {
        let mut proof = proof.clone();
        let fields = proof.as_object_mut().context("merge proof is not an object")?;
        fields.remove("client_mutation_id");
        fields.remove("commit_marker_operation");
        if let Some(at) = fields.get("merged_at").and_then(|v|v.as_str()) {
            let at = chrono::DateTime::parse_from_rfc3339(at)?.timestamp_millis();
            fields.insert("merged_at".into(),at.into());
        }
        Ok(proof)
    };
    ensure!(normalized(first)? == normalized(later)?,"immutable merge receipt conflict");
    Ok(())
}

fn validate_existing_autonomous_resolution(conn:&Connection, operation:&str, raw:&str) -> Result<()> {
    let decision:serde_json::Value = serde_json::from_str(raw)?;
    ensure!(decision["classification"] == "autonomous"
        && decision["actor"].as_str().is_some_and(|actor|!actor.trim().is_empty()),"immutable merge resolution conflict");
    let (source,digest) = if let Some(digest) = decision["response_sha256"].as_str() {
        (conn.query_row("SELECT response FROM merge_transport_response WHERE operation=?1",[operation],|r|r.get::<_,String>(0))?,digest)
    } else {
        ensure!(decision["proof_kind"] == "immutable_commit_marker","unknown confirmation audit proof");
        (conn.query_row("SELECT receipt FROM merge_mutation_receipt WHERE operation=?1",[operation],|r|r.get::<_,String>(0))?,
            decision["receipt_sha256"].as_str().context("confirmation audit digest missing")?)
    };
    ensure!(digest == format!("{:x}",Sha256::digest(source.as_bytes())),"confirmation audit evidence changed");
    Ok(())
}

fn validate_undispatched_resolution(conn:&Connection, op:&Operation) -> Result<()> {
    let (workflow,kind,binding,completed,state,resolution,dispatched,journal):(String,String,String,Option<String>,String,Option<String>,Option<i64>,bool) = conn.query_row(
        "SELECT o.workflow,o.kind,o.binding,o.receipt,d.state,d.resolution,d.dispatched_at_ms,
         EXISTS(SELECT 1 FROM merge_transport_response r WHERE r.operation=o.id)
         FROM operation o JOIN merge_allowance_dispatch d ON d.operation=o.id WHERE o.id=?1",[&op.id],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?;
    ensure!(workflow == op.workflow && op.kind == OperationKind::MergePr
        && serde_json::from_str::<OperationKind>(&kind)? == op.kind
        && serde_json::from_str::<Binding>(&binding)? == op.binding,
        "undispatched intent differs");
    ensure!(completed.is_none() && dispatched.is_none() && !journal
        && (state == "reserved" || (state == "released" && resolution.as_deref() == Some("expired_before_dispatch"))),
        "intent does not prove non-dispatch");
    let marker_table:bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='merge_private_marker')",[],|r|r.get(0))?;
    if marker_table {
        let marked:bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM merge_private_marker WHERE operation=?1)",[&op.id],|r|r.get(0))?;
        ensure!(!marked,"marked dispatch cannot be classified as never dispatched");
    }
    Ok(())
}

fn validate_external_merge(conn: &Connection, op: &Operation, receipt: &serde_json::Value) -> Result<()> {
    let (workflow,kind,binding,state,node,pr,dispatched):(String,String,String,String,String,u64,Option<i64>) = conn.query_row(
        "SELECT o.workflow,o.kind,o.binding,d.state,d.pr_node_id,d.pr,d.dispatched_at_ms
         FROM operation o JOIN merge_allowance_dispatch d ON d.operation=o.id WHERE o.id=?1",[&op.id],
        |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
    ensure!(workflow == op.workflow && op.kind == OperationKind::MergePr
        && serde_json::from_str::<OperationKind>(&kind)? == op.kind
        && serde_json::from_str::<Binding>(&binding)? == op.binding,
        "external merge intent differs");
    ensure!(matches!(state.as_str(),"dispatched"|"released") && dispatched.is_some(),
        "external merge requires an unconfirmed dispatched intent");
    ensure!(receipt["expected_head"].as_str() == Some(op.binding.head.as_str())
        && receipt["pr_number"].as_u64() == Some(pr) && receipt["pr_node_id"].as_str() == Some(node.as_str())
        && receipt["commit_marker_operation"].as_str() == Some(op.id.as_str())
        && receipt.get("client_mutation_id").is_none(),"external immutable proof identity differs");
    let commit = receipt["merge_commit"].as_str().context("external commit missing")?;
    ensure!(matches!(commit.len(),40|64) && commit.bytes().all(|b|b.is_ascii_hexdigit()),"invalid external commit");
    ensure!(chrono::DateTime::parse_from_rfc3339(receipt["merged_at"].as_str().context("external merge time missing")?)?.timestamp_millis() >= 0
        && receipt["merged_by"].as_str().is_some_and(|s|!s.trim().is_empty()),"invalid external completion");
    let (protocol,nonce):(i64,String) = conn.query_row(
        "SELECT protocol,nonce FROM merge_private_marker WHERE operation=?1",[&op.id],|r|Ok((r.get(0)?,r.get(1)?)))
        .context("legacy dispatch cannot prove external attribution")?;
    ensure!(protocol == 1 && nonce.len() == 64 && nonce.bytes().all(|b|b.is_ascii_hexdigit()),"invalid private merge protocol");
    let expected = format!("Autoship-Receipt: v1:{}:{nonce}",op.id);
    let message = receipt["merge_message"].as_str().context("complete immutable message missing")?;
    ensure!(serde_json::to_vec(receipt)?.len() <= 16*1024 && !message.contains(&expected),
        "immutable message contains this dispatch or exceeds proof bound");
    let raw:Option<String> = conn.query_row("SELECT response FROM merge_transport_response WHERE operation=?1",[&op.id],|r|r.get(0)).optional()?;
    if let Some(raw) = raw {
        ensure!(!raw.contains(&expected),"journal contains this private dispatch proof");
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            let remote = &value["data"]["mergePullRequest"]["pullRequest"];
            ensure!(remote["mergeCommit"]["message"].as_str().is_none_or(|message|!message.contains(&expected)),
                "decoded journal contains private dispatch proof");
            if remote["state"] == "MERGED" {
                ensure!(remote["mergeCommit"]["oid"].as_str().is_none_or(|sha|sha == commit),
                    "journal contradicts external immutable commit");
            }
        }
    }
    Ok(())
}

fn allowance(conn: &Connection, now: i64, cap: u32) -> Result<MergeAllowance> {
    ensure!(now >= 0 && cap > 0, "invalid merge allowance day or cap");
    let start = now.div_euclid(86_400_000) * 86_400_000;
    let confirmed: u32 = conn.query_row(
        "SELECT count(*) FROM merge_allowance_dispatch WHERE state='confirmed' AND merged_at_ms>=?1 AND merged_at_ms<?2",
        params![start, start + 86_400_000], |r| r.get(0))?;
    let (pending, oldest_pending_at_ms): (u32, Option<i64>) = conn.query_row(
        "SELECT count(*), min(d.reserved_at_ms) FROM merge_allowance_dispatch d
         JOIN operation o ON o.id=d.operation JOIN workflow w ON w.id=o.workflow
         WHERE d.state='dispatched' OR (d.state='reserved' AND w.generation=d.generation
             AND w.lease_until>?1 AND w.owner IS NOT NULL)",
        [now],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let last_merged_at_ms = conn.query_row(
        "SELECT max(merged_at_ms) FROM merge_allowance_dispatch WHERE state='confirmed'",
        [],
        |r| r.get(0),
    )?;
    Ok(MergeAllowance {
        confirmed,
        pending,
        remaining: cap.saturating_sub(confirmed),
        available: cap.saturating_sub(confirmed.saturating_add(pending)),
        cap,
        oldest_pending_at_ms,
        last_merged_at_ms,
    })
}

impl WorkflowStore {
    pub fn validate_external_merge(&self, op: &Operation, receipt: &serde_json::Value) -> Result<()> {
        validate_external_merge(&self.conn, op, receipt)
    }
    pub fn validate_undispatched_resolution(&self, op:&Operation) -> Result<()> {
        validate_undispatched_resolution(&self.conn,op)
    }
    pub fn validate_confirmed_merge_proof(&self, operation:&str, receipt:&serde_json::Value) -> Result<()> {
        let first = self.confirmed_merge_receipt(operation)?.context("confirmed merge proof missing")?;
        validate_merge_receipt_equivalence(&self.conn,operation,&first,receipt)
    }
    pub fn reconcile_confirmed_workflow(&mut self, operation: &str, now: i64) -> Result<bool> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String,String,String,Option<String>,u64,String,i64,String,String,Option<u64>,i64,i64)> = tx.query_row(
            "SELECT o.workflow,o.kind,o.binding,o.receipt,d.pr,d.pr_node_id,d.merged_at_ms,m.receipt,w.checkpoint,w.pr,w.lease_until,w.generation
             FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation JOIN workflow w ON w.id=o.workflow
             JOIN merge_mutation_receipt m ON m.operation=o.id WHERE d.operation=?1 AND d.state='confirmed'",
            [operation], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?))).optional()?;
        let Some((workflow,kind,binding,completed,pr,node,merged_at,proof,checkpoint,current_pr,until,generation)) = row else { return Ok(false); };
        let binding: Binding = serde_json::from_str(&binding)?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&checkpoint)?;
        if until > now || checkpoint.stage == Stage::Retired || checkpoint.binding != binding || current_pr != Some(pr) {
            return Ok(false);
        }
        ensure!(serde_json::from_str::<OperationKind>(&kind)? == OperationKind::MergePr, "confirmed receipt is not a merge operation");
        let proof: serde_json::Value = serde_json::from_str(&proof)?;
        validate_merge_receipt_identity(&tx,operation,&proof)?;
        ensure!(proof["expected_head"].as_str() == Some(binding.head.as_str())
            && proof["pr_number"].as_u64() == Some(pr) && proof["pr_node_id"].as_str() == Some(node.as_str())
            && chrono::DateTime::parse_from_rfc3339(proof["merged_at"].as_str().context("confirmation time missing")?)?.timestamp_millis() == merged_at,
            "confirmed workflow proof differs from dispatch");
        let observed = format!("github:pr:{pr}:merged:{}",binding.head);
        ensure!(completed.as_deref().is_none_or(|value| value == observed), "completed operation contradicts confirmed merge");
        if checkpoint.stage == Stage::Merged && completed.is_some() { return Ok(false); }
        let generation = generation.checked_add(1).context("workflow generation overflow")?;
        if completed.is_none() {
            tx.execute("UPDATE operation SET receipt=?2 WHERE id=?1",params![operation,observed])?;
            // Throughput belongs to the remote merge window, even when its
            // proof arrives after restart or midnight.
            event(&tx,&workflow,generation,"operation_completed",operation,merged_at)?;
        }
        checkpoint.stage = Stage::Merged;
        checkpoint.next = NextAction { reason:"confirmed_merge_reconciled".into(), action:"complete".into(), eligible_at_ms:0 };
        let raw = serde_json::to_string(&checkpoint)?;
        tx.execute("UPDATE workflow SET checkpoint=?2,owner=NULL,lease_until=0,generation=?3,progress_at=?4 WHERE id=?1",
            params![workflow,raw,generation,now])?;
        event(&tx,&workflow,generation,"checkpoint",&raw,now)?;
        // Mark cleanup intent only. Runtime retirement must still verify that
        // children are gone; historical resource charges are never erased.
        tx.execute("UPDATE workflow_native_owner SET retirement_reason=COALESCE(retirement_reason,'terminal_workflow') WHERE workflow=?1 AND retired_at IS NULL",[&workflow])?;
        tx.commit()?;
        Ok(true)
    }

    pub fn reconcile_confirmed_workflows(&mut self, repository: &str, now: i64) -> Result<usize> {
        self.reconcile_confirmed_workflows_for(repository, None, now)
    }

    pub fn reconcile_confirmed_workflows_for(&mut self, repository: &str, workflow: Option<&str>, now: i64) -> Result<usize> {
        let ids = self.conn.prepare("SELECT d.operation FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation JOIN workflow w ON w.id=o.workflow
            WHERE d.repository=?1 AND d.state='confirmed' AND w.lease_until<=?2
            AND (?3 IS NULL OR o.workflow=?3)
            AND json_extract(w.checkpoint,'$.stage')<>'retired'
            AND (json_extract(w.checkpoint,'$.stage')<>'merged' OR o.receipt IS NULL) ORDER BY d.operation")?
            .query_map(params![repository.to_ascii_lowercase(),now,workflow],|r|r.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut completed = 0;
        for operation in ids {
            match self.reconcile_confirmed_workflow(&operation,now) {
                Ok(changed) => completed += usize::from(changed),
                Err(error) => tracing::warn!(operation=%operation,"confirmed workflow reconciliation remains unresolved: {error:#}"),
            }
        }
        Ok(completed)
    }
    /// Operation completion alone does not settle autonomous attribution. A
    /// lost response can leave a completed operation with an uncertain ledger.
    pub fn merge_reconciliation_needed(&self, workflow: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM merge_allowance_dispatch d
            JOIN operation o ON o.id=d.operation JOIN workflow w ON w.id=o.workflow
            WHERE o.workflow=?1 AND (d.state='dispatched' OR
                (d.state='confirmed' AND w.pr=d.pr
                 AND json_extract(w.checkpoint,'$.binding.head')=json_extract(o.binding,'$.head')
                 AND json_extract(w.checkpoint,'$.binding.contract')=json_extract(o.binding,'$.contract')
                 AND json_extract(w.checkpoint,'$.binding.evidence_hash')=json_extract(o.binding,'$.evidence_hash')
                 AND (o.receipt IS NULL OR json_extract(w.checkpoint,'$.stage') NOT IN ('merged','retired')))))",
            [workflow], |row| row.get(0))?)
    }

    pub fn pending_merge_dispatches(&self, repository: &str) -> Result<Vec<MergeDispatchRecord>> {
        let ids = self.conn.prepare("SELECT operation FROM merge_allowance_dispatch WHERE repository=?1 AND state='dispatched' ORDER BY dispatched_at_ms,operation")?
            .query_map([repository], |row| row.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter().map(|id| self.merge_dispatch(&id)?.context("pending dispatch disappeared")).collect()
    }
    pub fn validate_merge_receipt_marker(&self, operation: &str, receipt: &serde_json::Value) -> Result<()> {
        validate_merge_receipt_identity(&self.conn, operation, receipt)
    }

    pub fn merge_dispatch(&self, id: &str) -> Result<Option<MergeDispatchRecord>> {
        let row: Option<(String,String,String,Option<String>,String,u64,String,String,Option<String>)> = self.conn.query_row(
            "SELECT o.workflow,o.kind,o.binding,o.receipt,d.repository,d.pr,d.pr_node_id,d.state,d.resolution FROM operation o
             JOIN merge_allowance_dispatch d ON d.operation=o.id WHERE o.id=?1", [id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?))).optional()?;
        row.map(
            |(workflow, kind, binding, receipt, repository, pr, node, state, resolution)| {
                Ok(MergeDispatchRecord {
                    operation: Operation {
                        id: id.into(),
                        workflow,
                        kind: serde_json::from_str(&kind)?,
                        binding: serde_json::from_str(&binding)?,
                        receipt,
                    },
                    repository,
                    pr,
                    node,
                    state,
                    resolution,
                })
            },
        )
        .transpose()
    }

    pub fn resolve_merge_without_debit(
        &mut self,
        op: &Operation,
        classification: &str,
        actor: &str,
        proof: &MergeReleaseProof,
        now: i64,
    ) -> Result<()> {
        self.resolve_merge_without_debit_inner(op, classification, actor, proof, now, None)
    }

    /// The owning worker may release proven pre-execution rejection without
    /// revoking its own lease. Unknown/partial execution is never eligible.
    pub fn release_rejected_merge(
        &mut self,
        lease: &Lease,
        op: &Operation,
        now: i64,
    ) -> Result<()> {
        let raw: String = self.conn.query_row(
            "SELECT response FROM merge_transport_response WHERE operation=?1",
            [&op.id],
            |r| r.get(0),
        )?;
        let proof = MergeReleaseProof::RequestNotExecuted {
            response_sha256: format!("{:x}", Sha256::digest(raw.as_bytes())),
        };
        self.resolve_merge_without_debit_inner(
            op,
            "not-merged",
            "autoship",
            &proof,
            now,
            Some(lease),
        )
    }

    fn resolve_merge_without_debit_inner(
        &mut self,
        op: &Operation,
        classification: &str,
        actor: &str,
        proof: &MergeReleaseProof,
        now: i64,
        active_lease: Option<&Lease>,
    ) -> Result<()> {
        ensure!(
            matches!(classification, "manual" | "not-merged")
                && !actor.trim().is_empty()
                && actor.len() <= 256,
            "invalid merge resolution classification or actor"
        );
        let decision = serde_json::to_string(
            &serde_json::json!({"classification":classification,"actor":actor,"proof":proof}),
        )?;
        ensure!(
            decision.len() <= 16 * 1024,
            "resolution evidence exceeds bound"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (workflow,binding,state,node,pr,prior):(String,String,String,String,u64,Option<String>)=tx.query_row(
            "SELECT o.workflow,o.binding,d.state,d.pr_node_id,d.pr,d.resolution FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.id=?1",
            [&op.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        ensure!(
            op.kind == OperationKind::MergePr
                && workflow == op.workflow
                && serde_json::from_str::<Binding>(&binding)? == op.binding,
            "resolution intent differs"
        );
        let expired = state == "released" && prior.as_deref() == Some("expired_before_dispatch");
        if state == "released" && !expired {
            ensure!(
                prior.as_deref() == Some(decision.as_str()),
                "immutable merge resolution conflict"
            );
            tx.commit()?;
            return Ok(());
        }
        ensure!(
            matches!(state.as_str(), "reserved" | "dispatched") || expired,
            "confirmed merge cannot be released"
        );
        let (owner, until, generation): (Option<String>, i64, i64) = tx.query_row(
            "SELECT owner,lease_until,generation FROM workflow WHERE id=?1",
            [&workflow],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if let Some(lease) = active_lease {
            ensure!(
                lease.workflow == op.workflow
                    && matches!(proof, MergeReleaseProof::RequestNotExecuted { .. }),
                "active resolution requires this lease's non-execution proof"
            );
            require_binding(&tx, lease, &op.binding, now)?;
        } else {
            ensure!(
                owner.is_none() || until <= now,
                "workflow-active: stop or await the worker before resolving its merge"
            );
        }
        match proof {
            MergeReleaseProof::ExternalMarkedMerge { receipt } => {
                ensure!(classification == "manual", "external merge requires manual classification");
                validate_external_merge(&tx,op,receipt)?;
            }
            MergeReleaseProof::NeverDispatched => validate_undispatched_resolution(&tx,op)?,
            MergeReleaseProof::RequestNotExecuted { response_sha256 } => {
                let raw: String = tx.query_row(
                    "SELECT response FROM merge_transport_response WHERE operation=?1",
                    [&op.id],
                    |r| r.get(0),
                )?;
                ensure!(
                    *response_sha256 == format!("{:x}", Sha256::digest(raw.as_bytes()))
                        && merge_request_not_executed(&raw),
                    "journal does not prove a pre-execution request rejection"
                );
            }
            MergeReleaseProof::DifferentMergedHead {
                node: remote_node,
                pr: remote_pr,
                head,
                merge_commit,
                merged_at,
            } => {
                ensure!(
                    classification == "manual"
                        && remote_node == &node
                        && *remote_pr == pr
                        && head != &op.binding.head,
                    "same-head merge observation cannot distinguish manual from autonomous"
                );
                for sha in [head, merge_commit] {
                    ensure!(
                        matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                        "invalid remote merge identity"
                    );
                }
                ensure!(
                    chrono::DateTime::parse_from_rfc3339(merged_at)?.timestamp_millis() >= 0,
                    "invalid merge timestamp"
                );
                let raw: Option<String> = tx
                    .query_row(
                        "SELECT response FROM merge_transport_response WHERE operation=?1",
                        [&op.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(raw) = raw {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                        ensure!(
                            value
                                .pointer("/data/mergePullRequest/pullRequest/state")
                                .and_then(|v| v.as_str())
                                != Some("MERGED"),
                            "remote observation contradicts original accepted response"
                        );
                    }
                }
            }
        }
        tx.execute(
            "UPDATE merge_allowance_dispatch SET state='released',resolution=?2 WHERE operation=?1",
            params![op.id, decision],
        )?;
        tx.execute(
            "UPDATE operation SET receipt=?2 WHERE id=?1",
            params![op.id, decision],
        )?;
        // Operator recovery fences expired workers. Proven rejection handled
        // by the current owner preserves its lease and checkpoint.
        let event_generation = if active_lease.is_none() {
            tx.execute(
                "UPDATE workflow SET owner=NULL,lease_until=0,generation=generation+1 WHERE id=?1",
                [&workflow],
            )?;
            generation + 1
        } else {
            generation
        };
        if matches!(proof,MergeReleaseProof::ExternalMarkedMerge { .. }) {
            let (raw,current_pr):(String,Option<u64>) = tx.query_row("SELECT checkpoint,pr FROM workflow WHERE id=?1",[&workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
            let mut checkpoint:Checkpoint = serde_json::from_str(&raw)?;
            if checkpoint.binding == op.binding && current_pr == Some(pr) && checkpoint.stage != Stage::Retired {
                checkpoint.stage = Stage::Merged;
                checkpoint.next = NextAction {reason:"external_merge_reconciled".into(),action:"complete".into(),eligible_at_ms:0};
                let raw = serde_json::to_string(&checkpoint)?;
                tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",params![workflow,raw,now])?;
                event(&tx,&workflow,event_generation,"checkpoint",&raw,now)?;
                tx.execute("UPDATE workflow_native_owner SET retirement_reason=COALESCE(retirement_reason,'terminal_workflow') WHERE workflow=?1 AND retired_at IS NULL",[&workflow])?;
            }
        }
        event(&tx,&workflow,event_generation,"merge_capacity_resolved",&serde_json::json!({"operation":op.id,"decision":serde_json::from_str::<serde_json::Value>(&decision)?}).to_string(),now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn merge_allowance(&self, now: i64, cap: u32) -> Result<MergeAllowance> {
        let tx = self.conn.unchecked_transaction()?;
        let status = allowance(&tx, now, cap)?;
        tx.commit()?;
        Ok(status)
    }

    /// Called only after trusted GitHub identity and mandatory checks. A
    /// reserved row grants no network authority until dispatch_merge commits.
    pub fn reserve_merge(
        &mut self,
        lease: &Lease,
        op: &Operation,
        repo: &str,
        pr: u64,
        node: &str,
        now: i64,
        cap: u32,
    ) -> Result<()> {
        ensure!(
            op.kind == OperationKind::MergePr && op.workflow == lease.workflow,
            "merge allowance requires this workflow's merge intent"
        );
        let repo = repo.to_ascii_lowercase();
        ensure!(
            pr > 0
                && !node.is_empty()
                && node.len() <= 256
                && (op.workflow.starts_with(&format!("{repo}#"))
                    || op.workflow.starts_with(&format!("{repo}/pr/"))),
            "merge repository or PR identity differs from workflow"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, &op.binding, now)?;
        super::submission::require_merge_authority(&tx, &op.workflow, &repo, pr)?;
        let (workflow, kind, binding): (String, String, String) = tx.query_row(
            "SELECT workflow,kind,binding FROM operation WHERE id=?1",
            [&op.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        ensure!(
            workflow == op.workflow
                && serde_json::from_str::<OperationKind>(&kind)? == OperationKind::MergePr
                && serde_json::from_str::<Binding>(&binding)? == op.binding,
            "stored merge intent differs"
        );
        let settled: Option<String> =
            tx.query_row("SELECT receipt FROM operation WHERE id=?1", [&op.id], |r| {
                r.get(0)
            })?;
        ensure!(
            settled.is_none(),
            "settled merge intent must not be re-dispatched"
        );
        let attached: Option<u64> =
            tx.query_row("SELECT pr FROM workflow WHERE id=?1", [&op.workflow], |r| {
                r.get(0)
            })?;
        ensure!(
            attached == Some(pr),
            "merge PR differs from attached workflow PR"
        );
        // Only reserved (never dispatched) capacity may be reclaimed on expiry.
        // A replaced worker cannot pass dispatch_merge's generation check.
        tx.execute("UPDATE merge_allowance_dispatch SET state='released',resolution='expired_before_dispatch'
            WHERE state='reserved' AND EXISTS(SELECT 1 FROM operation o JOIN workflow w ON w.id=o.workflow
                WHERE o.id=merge_allowance_dispatch.operation AND (w.generation<>merge_allowance_dispatch.generation OR w.lease_until<=?1 OR w.owner IS NULL))", [now])?;
        let prior: Option<(String, String, u64, String, String)> = tx.query_row(
            "SELECT state,repository,pr,pr_node_id,head FROM merge_allowance_dispatch WHERE operation=?1", [&op.id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        if let Some((state, old_repo, old_pr, old_node, old_head)) = prior {
            ensure!(
                old_repo == repo && old_pr == pr && old_node == node && old_head == op.binding.head,
                "merge reservation identity changed"
            );
            ensure!(state == "released", "merge-reconciliation-required: this operation is already {state}; never replay an uncertain mutation");
        }
        let status = allowance(&tx, now, cap)?;
        ensure!(
            status.available > 0,
            "merge-allowance-exhausted: {} confirmed, {} pending, cap {}",
            status.confirmed,
            status.pending,
            cap
        );
        tx.execute("INSERT INTO merge_allowance_dispatch(operation,repository,pr,pr_node_id,head,generation,reserved_at_ms,state)
            VALUES(?1,?2,?3,?4,?5,?6,?7,'reserved') ON CONFLICT(operation) DO UPDATE SET generation=excluded.generation,
            reserved_at_ms=excluded.reserved_at_ms,dispatched_at_ms=NULL,state='reserved',resolution=NULL",
            params![op.id,repo,pr,node,op.binding.head,lease.generation,now])?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "merge_capacity_reserved",
            &op.id,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Commit immediately before invoking the remote mutation. Neither a crash
    /// nor a lost response can make this slot available for another mutation.
    pub fn dispatch_marked_merge(
        &mut self,
        lease: &Lease,
        op: &Operation,
        now: i64,
        cap: u32,
    ) -> Result<String> {
        // Separate from the public operation ID; never include this secret in
        // pre-merge status, event payloads, or model context.
        let nonce = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        self.dispatch_merge_inner(lease, op, now, cap, Some(&nonce))?;
        Ok(format!("Autoship-Receipt: v1:{}:{nonce}", op.id))
    }

    pub fn dispatch_merge(
        &mut self,
        lease: &Lease,
        op: &Operation,
        now: i64,
        cap: u32,
    ) -> Result<()> {
        self.dispatch_merge_inner(lease, op, now, cap, None)
    }

    fn dispatch_merge_inner(
        &mut self,
        lease: &Lease,
        op: &Operation,
        now: i64,
        cap: u32,
        nonce: Option<&str>,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, &op.binding, now)?;
        ensure!(
            op.workflow == lease.workflow,
            "merge dispatch workflow differs"
        );
        let (workflow, kind, binding): (String, String, String) = tx.query_row(
            "SELECT workflow,kind,binding FROM operation WHERE id=?1", [&op.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        ensure!(workflow == op.workflow && op.kind == OperationKind::MergePr
            && serde_json::from_str::<OperationKind>(&kind)? == op.kind
            && serde_json::from_str::<Binding>(&binding)? == op.binding,
            "merge dispatch differs from stored intent");
        let (repository, pr): (String, u64) = tx.query_row(
            "SELECT repository,pr FROM merge_allowance_dispatch WHERE operation=?1", [&op.id],
            |row| Ok((row.get(0)?, row.get(1)?)))?;
        super::submission::require_merge_authority(&tx, &op.workflow, &repository, pr)?;
        let status = allowance(&tx, now, cap)?;
        ensure!(
            status.confirmed.saturating_add(status.pending) <= cap,
            "merge allowance changed before dispatch"
        );
        let updated = tx.execute(
            "UPDATE merge_allowance_dispatch SET state='dispatched',dispatched_at_ms=?1
            WHERE operation=?2 AND generation=?3 AND state='reserved'",
            params![now, op.id, lease.generation],
        )?;
        ensure!(
            updated == 1,
            "merge dispatch requires an undispatched reservation owned by this lease"
        );
        if let Some(nonce) = nonce {
            tx.execute("INSERT INTO merge_private_marker(operation,protocol,nonce) VALUES(?1,1,?2)",
                params![op.id, nonce])?;
        }
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "merge_mutation_dispatched",
            &op.id,
            now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// An accepted remote response remains evidence after the request's lease
    /// expires. Account it atomically with the existing attribution receipt;
    /// do not advance a newer worker's checkpoint or grant publication rights.
    pub fn confirm_merge_dispatch(
        &mut self,
        op: &Operation,
        receipt: &serde_json::Value,
        now: i64,
    ) -> Result<()> {
        let raw = serde_json::to_string(receipt)?;
        ensure!(
            raw.len() <= 16 * 1024 && op.kind == OperationKind::MergePr,
            "invalid merge receipt"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, pr, node, head, generation, workflow): (String,u64,String,String,i64,String) = tx.query_row(
            "SELECT d.state,d.pr,d.pr_node_id,d.head,d.generation,o.workflow FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE d.operation=?1", [&op.id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        ensure!(
            matches!(state.as_str(), "dispatched" | "confirmed")
                && workflow == op.workflow
                && head == op.binding.head,
            "receipt lacks matching dispatched intent"
        );
        let stored_binding: String =
            tx.query_row("SELECT binding FROM operation WHERE id=?1", [&op.id], |r| {
                r.get(0)
            })?;
        ensure!(
            serde_json::from_str::<Binding>(&stored_binding)? == op.binding,
            "merge receipt contract or evidence differs from dispatched intent"
        );
        validate_merge_receipt_identity(&tx, &op.id, receipt)?;
        ensure!(
            receipt["expected_head"].as_str() == Some(head.as_str())
                && receipt["pr_number"].as_u64() == Some(pr)
                && receipt["pr_node_id"].as_str() == Some(node.as_str()),
            "merge receipt identity differs from reservation"
        );
        let commit = receipt["merge_commit"]
            .as_str()
            .context("merge commit missing")?;
        ensure!(
            matches!(commit.len(), 40 | 64) && commit.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid merge commit"
        );
        ensure!(
            receipt["merged_by"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty()),
            "merge actor missing"
        );
        let merged_at = chrono::DateTime::parse_from_rfc3339(
            receipt["merged_at"]
                .as_str()
                .context("remote merge time missing")?,
        )?
        .timestamp_millis();
        ensure!(merged_at >= 0, "invalid remote merge time");
        let prior: Option<String> = tx
            .query_row(
                "SELECT receipt FROM merge_mutation_receipt WHERE operation=?1",
                [&op.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            validate_merge_receipt_equivalence(&tx,&op.id,&serde_json::from_str(&prior)?,receipt)?;
        } else {
            ensure!(
                state == "dispatched",
                "confirmed allowance lacks its authoritative receipt"
            );
            tx.execute(
                "INSERT INTO merge_mutation_receipt VALUES(?1,?2)",
                params![op.id, raw],
            )?;
        }
        if state != "confirmed" {
            tx.execute("UPDATE merge_allowance_dispatch SET state='confirmed',merged_at_ms=?1 WHERE operation=?2",params![merged_at,op.id])?;
            event(
                &tx,
                &workflow,
                generation,
                "autonomous_merge_confirmed",
                &serde_json::json!({"operation":op.id,"receipt":receipt}).to_string(),
                now,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Preserve the original transport response before parsing or accounting.
    /// A late response is evidence even after its worker's lease expires.
    pub fn record_merge_response(&mut self, op: &Operation, raw: &str, now: i64) -> Result<()> {
        ensure!(
            raw.len() <= 64 * 1024,
            "merge transport response exceeds bound"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, workflow, binding): (String, String, String) = tx.query_row(
            "SELECT d.state,o.workflow,o.binding FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE d.operation=?1",
            [&op.id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        ensure!(
            op.kind == OperationKind::MergePr
                && op.workflow == workflow
                && serde_json::from_str::<Binding>(&binding)? == op.binding
                && matches!(state.as_str(), "dispatched" | "confirmed"),
            "response lacks matching dispatched intent"
        );
        let prior: Option<String> = tx
            .query_row(
                "SELECT response FROM merge_transport_response WHERE operation=?1",
                [&op.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            ensure!(prior == raw, "immutable merge transport response conflict");
        } else {
            tx.execute(
                "INSERT INTO merge_transport_response VALUES(?1,?2,?3)",
                params![op.id, raw, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Return only a response bound to the requested immutable intent/PR.
    pub fn merge_response(
        &self,
        op: &Operation,
        repository: &str,
        pr: u64,
    ) -> Result<Option<(String, String)>> {
        let row: Option<(String,String,String,String,u64,String)> = self.conn.query_row(
            "SELECT r.response,d.pr_node_id,o.binding,d.repository,d.pr,o.workflow FROM merge_transport_response r
             JOIN merge_allowance_dispatch d ON d.operation=r.operation JOIN operation o ON o.id=r.operation WHERE r.operation=?1",
            [&op.id], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        row.map(|(raw, node, binding, repo, number, workflow)| {
            ensure!(
                op.kind == OperationKind::MergePr
                    && op.workflow == workflow
                    && repo == repository.to_ascii_lowercase()
                    && pr == number
                    && serde_json::from_str::<Binding>(&binding)? == op.binding,
                "journal response identity differs"
            );
            Ok((raw, node))
        })
        .transpose()
    }

    /// If a process exits after confirmation but before this audit record, a
    /// repeated operator command completes the audit without another debit.
    pub fn record_confirmed_merge_resolution(
        &mut self,
        op: &Operation,
        actor: &str,
        response_sha256: &str,
        now: i64,
    ) -> Result<()> {
        ensure!(
            !actor.trim().is_empty() && actor.len() <= 256,
            "invalid resolution actor"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state,raw,prior,generation,workflow,kind,binding):(String,String,Option<String>,i64,String,String,String)=tx.query_row(
            "SELECT d.state,r.response,d.resolution,d.generation,o.workflow,o.kind,o.binding FROM merge_allowance_dispatch d JOIN merge_transport_response r ON r.operation=d.operation JOIN operation o ON o.id=d.operation WHERE d.operation=?1",[&op.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))?;
        ensure!(
            op.workflow == workflow
                && op.kind == OperationKind::MergePr
                && serde_json::from_str::<OperationKind>(&kind)? == OperationKind::MergePr
                && serde_json::from_str::<Binding>(&binding)? == op.binding,
            "confirmation audit intent differs from dispatched operation"
        );
        ensure!(
            state == "confirmed"
                && response_sha256 == format!("{:x}", Sha256::digest(raw.as_bytes())),
            "confirmation lacks matching journal evidence"
        );
        let decision=serde_json::json!({"classification":"autonomous","actor":actor,"response_sha256":response_sha256}).to_string();
        if let Some(prior) = prior {
            validate_existing_autonomous_resolution(&tx,&op.id,&prior)?;
        } else {
            tx.execute(
                "UPDATE merge_allowance_dispatch SET resolution=?2 WHERE operation=?1",
                params![op.id, decision],
            )?;
            event(&tx,&workflow,generation,"merge_resolution_confirmed",&serde_json::json!({"operation":op.id,"decision":serde_json::from_str::<serde_json::Value>(&decision)?}).to_string(),now)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record_confirmed_marker_resolution(&mut self, op:&Operation, actor:&str, receipt:&serde_json::Value, now:i64) -> Result<()> {
        ensure!(!actor.trim().is_empty() && actor.len() <= 256,"invalid resolution actor");
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (workflow,kind,binding,raw,prior,generation):(String,String,String,String,Option<String>,i64) = tx.query_row(
            "SELECT o.workflow,o.kind,o.binding,m.receipt,d.resolution,d.generation FROM merge_allowance_dispatch d
             JOIN operation o ON o.id=d.operation JOIN merge_mutation_receipt m ON m.operation=o.id
             WHERE d.operation=?1 AND d.state='confirmed'",[&op.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        ensure!(workflow == op.workflow && op.kind == OperationKind::MergePr
            && serde_json::from_str::<OperationKind>(&kind)? == op.kind
            && serde_json::from_str::<Binding>(&binding)? == op.binding
            && receipt["commit_marker_operation"].as_str() == Some(op.id.as_str()),
            "confirmation audit lacks matching immutable commit proof");
        validate_merge_receipt_equivalence(&tx,&op.id,&serde_json::from_str(&raw)?,receipt)?;
        let decision = serde_json::json!({"classification":"autonomous","actor":actor,
            "proof_kind":"immutable_commit_marker","receipt_sha256":format!("{:x}",Sha256::digest(raw.as_bytes()))}).to_string();
        if let Some(prior) = prior { validate_existing_autonomous_resolution(&tx,&op.id,&prior)?; }
        else {
            tx.execute("UPDATE merge_allowance_dispatch SET resolution=?2 WHERE operation=?1",params![op.id,decision])?;
            event(&tx,&workflow,generation,"merge_resolution_confirmed",&serde_json::json!({"operation":op.id,"decision":serde_json::from_str::<serde_json::Value>(&decision)?}).to_string(),now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Recovery never fabricates a successful response from `state=MERGED`.
    pub fn confirmed_merge_receipt(&self, operation: &str) -> Result<Option<serde_json::Value>> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT r.receipt FROM merge_mutation_receipt r JOIN merge_allowance_dispatch d ON d.operation=r.operation
             WHERE d.operation=?1 AND d.state='confirmed'", [operation], |r|r.get(0)).optional()?;
        raw.map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
}

#[cfg(test)]
#[path = "autoship_merge_allowance_tests.rs"]
mod tests;
