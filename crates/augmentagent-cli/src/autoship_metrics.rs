//! Windowed observations from controller records. Missing history and unknown
//! token usage are explicit; approvals and PR creation are never shipping.
use crate::autoship_workflow::{Binding, OperationKind};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::{collections::BTreeSet, path::Path};

#[derive(Debug, Serialize)]
pub struct Window {
    pub since_ms: i64,
    pub until_ms: i64,
    pub first_recorded_event_ms: Option<i64>,
    pub confirmed_merge_operations: usize,
    pub autonomous_merges: usize,
    pub model_calls: u64,
    pub observed_wall_ms: u64,
    pub reported_tokens: u64,
    pub charges_without_timestamp: u64,
    pub active_workflows: u64,
}

pub fn read(path: &Path, since: i64, until: i64) -> Result<Window> {
    ensure!(since < until, "invalid autoship report window");
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // One read transaction gives all aggregates the same database snapshot.
    conn.execute_batch("BEGIN")?;
    let first_recorded_event_ms =
        conn.query_row("SELECT min(at_ms) FROM workflow_event", [], |row| {
            row.get(0)
        })?;
    let charges_without_timestamp = conn.query_row(
        "SELECT count(*) FROM resource_charge r WHERE NOT EXISTS (
         SELECT 1 FROM workflow_event e WHERE e.workflow=r.workflow
         AND e.kind='resource_charged' AND e.payload=r.id)",
        [],
        |row| row.get(0),
    )?;
    let (model_calls, observed_wall_ms, reported_tokens) = conn.query_row(
        "SELECT coalesce(sum(r.model_calls),0),coalesce(sum(r.wall_ms),0),coalesce(sum(r.tokens),0)
         FROM resource_charge r JOIN (
           SELECT workflow,payload,min(at_ms) AS first_observed_ms FROM workflow_event
           WHERE kind='resource_charged' GROUP BY workflow,payload
         ) e ON e.workflow=r.workflow AND e.payload=r.id
         WHERE e.first_observed_ms>=?1 AND e.first_observed_ms<?2",
        params![since, until],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let active_workflows = conn.query_row(
        "SELECT count(DISTINCT workflow) FROM workflow_event WHERE at_ms>=?1 AND at_ms<?2",
        params![since, until],
        |row| row.get(0),
    )?;
    let mut statement = conn.prepare(
        "SELECT o.workflow,o.kind,o.binding,o.receipt,w.pr,o.id,m.receipt FROM operation o
         JOIN workflow w ON w.id=o.workflow LEFT JOIN merge_mutation_receipt m ON m.operation=o.id
         JOIN (SELECT workflow,payload,min(at_ms) AS first_observed_ms FROM workflow_event
           WHERE kind='operation_completed' GROUP BY workflow,payload
         ) e ON e.workflow=o.workflow AND e.payload=o.id
         WHERE o.receipt IS NOT NULL AND e.first_observed_ms>=?1 AND e.first_observed_ms<?2",
    )?;
    let rows = statement.query_map(params![since, until], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<u64>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut merges = BTreeSet::new();
    let mut autonomous = BTreeSet::new();
    for row in rows {
        let (workflow, kind, binding, receipt, pr, operation, mutation) = row?;
        if serde_json::from_str::<OperationKind>(&kind)? != OperationKind::MergePr {
            continue;
        }
        let binding: Binding = serde_json::from_str(&binding)?;
        if let Some(pr) = pr {
            if receipt == format!("github:pr:{pr}:merged:{}", binding.head) {
                if let Some(mutation) = mutation {
                    let proof: serde_json::Value = serde_json::from_str(&mutation)?;
                    if proof["client_mutation_id"].as_str() == Some(operation.as_str())
                        && proof["expected_head"].as_str() == Some(binding.head.as_str())
                        && proof["pr_number"].as_u64() == Some(pr)
                    {
                        autonomous.insert((workflow.clone(), pr));
                    }
                }
                merges.insert((workflow, pr));
            }
        }
    }
    Ok(Window {
        since_ms: since,
        until_ms: until,
        first_recorded_event_ms,
        confirmed_merge_operations: merges.len(),
        autonomous_merges: autonomous.len(),
        model_calls,
        observed_wall_ms,
        reported_tokens,
        charges_without_timestamp,
        active_workflows,
    })
}

/// Calendar boundaries are derived in the reporting timezone, never by adding
/// 24 hours. UTC event timestamps and resource reservations remain unchanged.
fn local_day_bounds<T: chrono::TimeZone>(now: &chrono::DateTime<T>) -> Result<(i64, i64)> {
    let date = now.date_naive();
    let next = date.succ_opt().context("local reporting date overflow")?;
    let midnight = |day: chrono::NaiveDate| -> Result<i64> {
        now.timezone()
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
            .earliest()
            .map(|time| time.timestamp_millis())
            .context("local reporting midnight does not exist in configured timezone")
    };
    Ok((midnight(date)?, midnight(next)?))
}

pub fn summary(since: i64, until: i64) -> Result<String> {
    let window = read(&crate::autoship_workflow::default_path(), since, until)?;
    let now = chrono::DateTime::from_timestamp_millis(until)
        .context("invalid report timestamp")?
        .with_timezone(&chrono::Local);
    let (day_start, day_end) = local_day_bounds(&now)?;
    let local = if until > day_start {
        Some(read(
            &crate::autoship_workflow::default_path(),
            day_start,
            until,
        )?)
    } else {
        None
    };
    let day_summary = format!(" Local calendar day {} (host timezone offset {}; UTC boundaries {}..{}; observed through {}): {} autonomous merges, {} model calls, {} ms observed execution. Calendar-day reporting does not reset resource reservations.",
        now.date_naive(), now.offset(), day_start, day_end, until,
        local.as_ref().map_or(0, |row| row.autonomous_merges),
        local.as_ref().map_or(0, |row| row.model_calls),
        local.as_ref().map_or(0, |row| row.observed_wall_ms));
    Ok(format!("Autonomous merges with accepted mutation proof: {}; confirmed merge operations including unattributed observations: {}; workflows with recorded activity: {}; model calls: {}; observed execution time: {} ms; reported tokens: {}. First recorded event (UTC epoch ms): {:?}. Historical charge receipts without timestamps: {} (excluded from window totals). These records do not prove continuous observation or complete token metering. Manual merges and lost responses without accepted mutation proof are excluded from autonomous throughput.",
        window.autonomous_merges, window.confirmed_merge_operations, window.active_workflows, window.model_calls,
        window.observed_wall_ms, window.reported_tokens, window.first_recorded_event_ms,
        window.charges_without_timestamp) + &day_summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::{ResourceCharge, WorkflowStore};

    #[test]
    fn local_reporting_days_follow_dst_and_not_utc_midnight() {
        use chrono::TimeZone;
        let zone = chrono_tz::America::New_York;
        for (month, day, hours) in [(3, 8, 23), (11, 1, 25), (10, 26, 24)] {
            let now = zone
                .with_ymd_and_hms(2026, month, day, 12, 0, 0)
                .single()
                .unwrap();
            let (start, end) = local_day_bounds(&now).unwrap();
            assert_eq!(end - start, hours * 60 * 60 * 1000);
            let midnight = zone.timestamp_millis_opt(start).single().unwrap();
            assert_eq!(midnight.date_naive(), now.date_naive());
            assert_eq!(
                midnight.time(),
                chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap()
            );
        }
        let now = zone
            .with_ymd_and_hms(2026, 10, 26, 23, 30, 0)
            .single()
            .unwrap();
        let (start, end) = local_day_bounds(&now).unwrap();
        assert!(start < now.timestamp_millis() && now.timestamp_millis() < end);
        assert_ne!(
            now.date_naive(),
            now.with_timezone(&chrono::Utc).date_naive()
        );
    }

    #[test]
    fn only_response_bound_accepted_merges_count_toward_autonomous_throughput() {
        for accepted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("workflow.db");
            let mut store = WorkflowStore::open(&path).unwrap();
            let binding = Binding {
                head: "a".repeat(40),
                contract: "contract".into(),
                evidence_hash: "evidence".into(),
            };
            store
                .register("owner/repo#1", "topic", binding.clone(), 10)
                .unwrap();
            let lease = store
                .claim("owner/repo#1", "worker", 10, 1000)
                .unwrap()
                .unwrap();
            store.attach_pr(&lease, 42, 20).unwrap();
            let op = store
                .begin_operation(&lease, OperationKind::MergePr, &binding, 25)
                .unwrap();
            if accepted {
                store.record_merge_mutation(&lease, &op.id, &serde_json::json!({
                    "client_mutation_id":op.id,"expected_head":binding.head,"pr_number":42,
                    "pr_node_id":"PR_fixture","merge_commit":"b".repeat(40),"merged_by":"fixture"
                }), 30).unwrap();
            }
            store
                .complete_operation(
                    &lease,
                    &op.id,
                    &format!("github:pr:42:merged:{}", binding.head),
                    40,
                )
                .unwrap();
            let report = read(&path, 0, 100).unwrap();
            assert_eq!(report.confirmed_merge_operations, 1);
            assert_eq!(report.autonomous_merges, usize::from(accepted));
            assert_eq!(read(&path, 50, 100).unwrap().autonomous_merges, 0);
        }
    }

    #[test]
    fn window_counts_confirmation_not_intent_and_never_double_charges_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register("owner/repo#1", "topic", binding.clone(), 10)
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "worker", 10, 1000)
            .unwrap()
            .unwrap();
        store.attach_pr(&lease, 42, 20).unwrap();
        let op = store
            .begin_operation(&lease, OperationKind::MergePr, &binding, 25)
            .unwrap();
        let charge = ResourceCharge {
            wall_ms: 70,
            model_calls: 2,
            tokens: 0,
        };
        store.charge(&lease, "attempt", charge, 30).unwrap();
        store.charge(&lease, "attempt", charge, 40).unwrap();
        // A duplicated legacy/recovery observation must not join-multiply the
        // measured charge. Aggregation is by the immutable charge receipt.
        let conn = Connection::open(&path).unwrap();
        for observed_at in [41, 42, 80] {
            conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,?2,'resource_charged','attempt',?3)",params![lease.workflow,lease.generation,observed_at]).unwrap();
        }
        let before = read(&path, 20, 50).unwrap();
        assert_eq!(before.confirmed_merge_operations, 0);
        assert_eq!(before.model_calls, 2);
        assert_eq!(before.observed_wall_ms, 70);
        store
            .complete_operation(&lease, &op.id, "github:pr:42:merged:head", 70)
            .unwrap();
        store
            .complete_operation(&lease, &op.id, "github:pr:42:merged:head", 80)
            .unwrap();
        conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,?2,'operation_completed',?3,80)",params![lease.workflow,lease.generation,op.id]).unwrap();
        let after = read(&path, 50, 100).unwrap();
        assert_eq!(after.confirmed_merge_operations, 1);
        assert_eq!(
            after.autonomous_merges, 0,
            "an observed merge is not proof that autoship performed it"
        );
        assert_eq!(after.model_calls, 0);
        assert_eq!(after.charges_without_timestamp, 0);
        assert_eq!(read(&path, 80, 100).unwrap().confirmed_merge_operations, 0);
    }

    #[test]
    fn creation_and_unconfirmed_or_wrong_head_receipts_do_not_count_as_shipping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register("owner/repo#1", "topic", binding.clone(), 10)
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "worker", 10, 1000)
            .unwrap()
            .unwrap();
        store.attach_pr(&lease, 42, 20).unwrap();
        for (kind, receipt) in [
            (OperationKind::CreatePr, "github:pr:42:merged:head"),
            (OperationKind::MergePr, "github:pr:42:merged:unreviewed"),
        ] {
            let op = store.begin_operation(&lease, kind, &binding, 30).unwrap();
            store
                .complete_operation(&lease, &op.id, receipt, 40)
                .unwrap();
        }
        assert_eq!(read(&path, 0, 100).unwrap().confirmed_merge_operations, 0);
    }
}
