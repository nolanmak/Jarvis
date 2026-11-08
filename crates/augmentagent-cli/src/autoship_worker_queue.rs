//! One attempt schedule for ordinary commands and original-owner metadata work.
//! A selected row is data, never a workflow lease or permission to execute.
use super::super::owner_publication_recovery::{PreparationEnvironment, PreparationRetryGate};
use super::*;
#[derive(Debug)]
pub(crate) struct SelectedWorkerJob {
    pub(crate) workflow: String,
    pub(crate) owner_recovery: bool,
}
fn age(tx: &Connection, id: &str) -> Result<(i64, i64, String)> {
    let (attempted,created)=tx.query_row("SELECT coalesce(s.attempted_at,0),j.created_at FROM command_job j LEFT JOIN command_schedule s ON s.workflow=j.workflow WHERE j.workflow=?1",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok((attempted, created, id.into()))
}
pub(super) fn select(
    store: &mut WorkflowStore,
    repository: &str,
    environment: Option<&PreparationEnvironment>,
    now: i64,
) -> Result<Option<SelectedWorkerJob>> {
    let repository = normalized_repository(repository)?;
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let yielding: bool = tx
        .query_row(
            "SELECT yield_normal FROM command_lane_turn WHERE repository=?1",
            [&repository],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if yielding {
        tx.execute(
            "UPDATE command_lane_turn SET yield_normal=0 WHERE repository=?1",
            [&repository],
        )?;
        tx.commit()?;
        return Ok(None);
    }
    let normal: Option<String> = tx
        .query_row(&due_command_sql(&tx)?, params![repository, now], |r| {
            r.get(0)
        })
        .optional()?;
    let mut recovery = None;
    if let Some(environment) = environment {
        let candidates = recovery_candidates(&tx, &repository, now)?;
        for id in candidates {
            tx.execute("INSERT INTO owner_recovery_queue_cursor VALUES(?1,?2) ON CONFLICT(repository) DO UPDATE SET last_workflow=excluded.last_workflow",params![repository,id])?;
            match super::super::owner_publication_recovery::worker_recovery_gate(
                &tx,
                &id,
                environment,
                now,
            ) {
                Ok(Some(PreparationRetryGate::Ready)) => {
                    recovery = Some(id);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(workflow=%id,"owner recovery scheduling evidence unavailable; other work continues: {error:#}")
                }
            }
        }
    }
    let selected = match (normal, recovery) {
        (Some(normal), Some(recovery)) => {
            if age(&tx, &normal)? <= age(&tx, &recovery)? {
                Some(SelectedWorkerJob {
                    workflow: normal,
                    owner_recovery: false,
                })
            } else {
                Some(SelectedWorkerJob {
                    workflow: recovery,
                    owner_recovery: true,
                })
            }
        }
        (Some(workflow), None) => Some(SelectedWorkerJob {
            workflow,
            owner_recovery: false,
        }),
        (None, Some(workflow)) => Some(SelectedWorkerJob {
            workflow,
            owner_recovery: true,
        }),
        (None, None) => None,
    };
    if let Some(job) = &selected {
        tx.execute("INSERT INTO command_schedule(workflow,attempted_at,eligible_at) VALUES(?1,?2,?3) ON CONFLICT(workflow) DO UPDATE SET attempted_at=excluded.attempted_at,eligible_at=excluded.eligible_at",params![job.workflow,now,now.saturating_add(300_000)])?;
        tx.execute("INSERT INTO command_attempt(workflow,selected_at,finished_at,outcome,reason,eligible_at) VALUES(?1,?2,NULL,'selected','worker-attempt-unconfirmed',?3) ON CONFLICT(workflow) DO UPDATE SET selected_at=excluded.selected_at,finished_at=NULL,outcome=excluded.outcome,reason=excluded.reason,eligible_at=excluded.eligible_at",params![job.workflow,now,now.saturating_add(300_000)])?;
        tx.execute("INSERT INTO command_lane_turn(repository,yield_normal) VALUES(?1,1) ON CONFLICT(repository) DO UPDATE SET yield_normal=1",[&repository])?;
    }
    tx.commit()?;
    Ok(selected)
}

fn recovery_candidates(conn: &Connection, repository: &str, now: i64) -> Result<Vec<String>> {
    let cursor: String = conn
        .query_row(
            "SELECT last_workflow FROM owner_recovery_queue_cursor WHERE repository=?1",
            [&repository],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let candidates = {
        let mut q=conn.prepare("SELECT DISTINCT j.workflow FROM command_job j JOIN workflow w ON w.id=j.workflow JOIN workflow_owner_turn t ON t.workflow=w.id JOIN workflow_owner_publication_intent p ON p.workflow=t.workflow AND p.request_id=t.request_id LEFT JOIN command_schedule s ON s.workflow=j.workflow WHERE j.repository=?1 AND w.lease_until<=?2 AND coalesce(s.eligible_at,0)<=?2 AND t.state IN ('claimed','uncertain') AND json_extract(w.checkpoint,'$.stage') NOT IN ('merged','retired') ORDER BY CASE WHEN j.workflow>?3 THEN 0 ELSE 1 END,j.workflow LIMIT 64")?;
        let rows = q
            .query_map(params![repository, now, cursor], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    Ok(candidates)
}
/// Read-only wake hint. Selection, authorization and native proofs are separate.
pub(super) fn due(
    store: &WorkflowStore,
    repository: &str,
    environment: &PreparationEnvironment,
    now: i64,
) -> Result<bool> {
    if store.command_queue_due(repository, now)? {
        return Ok(true);
    }
    let exists: bool = store.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='owner_recovery_queue_cursor')", [], |r|r.get(0))?;
    if !exists {
        return Ok(false);
    }
    let repository = normalized_repository(repository)?;
    let tx = store.conn.unchecked_transaction()?;
    let mut ready = false;
    for id in recovery_candidates(&tx, &repository, now)? {
        match super::super::owner_publication_recovery::worker_recovery_gate(
            &tx,
            &id,
            environment,
            now,
        ) {
            Ok(Some(PreparationRetryGate::Ready)) => {
                ready = true;
                break;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(workflow=%id,"owner recovery wake evidence unavailable: {error:#}")
            }
        }
    }
    tx.commit()?;
    Ok(ready)
}
