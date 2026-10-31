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
    pub fn dispatch_merge(
        &mut self,
        lease: &Lease,
        op: &Operation,
        now: i64,
        cap: u32,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, &op.binding, now)?;
        ensure!(
            op.workflow == lease.workflow,
            "merge dispatch workflow differs"
        );
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
        ensure!(
            receipt["client_mutation_id"].as_str() == Some(op.id.as_str())
                && receipt["expected_head"].as_str() == Some(head.as_str())
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
            ensure!(prior == raw, "immutable merge receipt conflict");
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
