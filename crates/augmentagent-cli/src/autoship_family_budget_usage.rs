//! Aggregate the original charge ledger, not a second budget/debit counter.
use super::*;
use std::collections::BTreeMap;

pub(super) fn reported(conn: &Connection, family: &str) -> Result<Vec<RetainedUsage>> {
    let mut query=conn.prepare("SELECT w.id,w.model_calls,w.wall_ms FROM workflow_family_member m JOIN workflow w ON w.id=m.workflow WHERE m.family=?1 ORDER BY m.position")?;
    let rows = query.query_map([family], |r| {
        Ok(RetainedUsage {
            workflow: r.get(0)?,
            model_admissions: r.get(1)?,
            wall_ms: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
pub(super) fn aggregate(conn: &Connection, family: &str) -> Result<Observation> {
    let ids: Vec<String> = conn
        .prepare("SELECT workflow FROM workflow_family_member WHERE family=?1 ORDER BY position")?
        .query_map([family], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut result = Observation {
        schema_version: 1,
        reason_code: None,
        model_admissions: Some(0),
        wall_ms: Some(0),
        tokens: None,
        retained: reported(conn, family)?,
        limits: None,
    };
    let mut native_ids = BTreeSet::new();
    for id in ids {
        let totals: Option<(u64, u64)> = conn
            .query_row(
                "SELECT model_calls,wall_ms FROM workflow WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((reported_calls, reported_wall)) = totals else {
            result.reason_code = Some("family_spend_unknown".into());
            continue;
        };
        let proofs:Vec<(i64,String)>=conn.prepare("SELECT generation,payload FROM workflow_event WHERE workflow=?1 AND kind='native_model_admitted'")?
            .query_map([&id],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut receipts = BTreeMap::<String, u64>::new();
        for (generation, raw) in proofs {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                result.reason_code = Some("family_spend_unknown".into());
                continue;
            };
            let Some(receipt) = value["receipt"].as_str().filter(|id| !id.is_empty()) else {
                result.reason_code = Some("family_spend_unknown".into());
                continue;
            };
            *receipts.entry(receipt.into()).or_default() += 1;
            if !native_ids.insert(receipt.to_owned()) {
                result.reason_code = Some("family_spend_unknown".into());
            }
            if let Some(reference) = value.get("budget_scope_ref") {
                let scope:Option<String>=conn.query_row("SELECT payload FROM workflow_family_scope WHERE workflow=?1 AND generation=?2",params![id,generation],|r|r.get(0)).optional()?;
                if scope.is_none_or(|scope| {
                    reference.as_str()
                        != Some(&format!("sha256:{:x}", Sha256::digest(scope.as_bytes())))
                }) {
                    result.reason_code = Some("family_spend_unknown".into());
                }
            }
        }
        let charges: Vec<(String, u64, u64)> = conn
            .prepare(
                "SELECT id,model_calls,wall_ms FROM resource_charge WHERE workflow=?1 ORDER BY id",
            )?
            .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let (mut calls, mut wall) = (Some(0u64), Some(0u64));
        for (receipt, n, w) in charges {
            calls = calls.and_then(|v| v.checked_add(n));
            wall = wall.and_then(|v| v.checked_add(w));
            if n > 0 && (n != 1 || receipts.remove(&receipt) != Some(1)) {
                result.reason_code = Some("family_spend_unknown".into());
            }
        }
        if !receipts.is_empty() || calls != Some(reported_calls) || wall != Some(reported_wall) {
            result.reason_code = Some("family_spend_unknown".into());
        }
        result.model_admissions = result
            .model_admissions
            .and_then(|v| calls.and_then(|n| v.checked_add(n)));
        result.wall_ms = result
            .wall_ms
            .and_then(|v| wall.and_then(|n| v.checked_add(n)));
    }
    if result.model_admissions.is_none() || result.wall_ms.is_none() {
        result.reason_code = Some("family_spend_unknown".into());
    }
    if !result.ready() {
        result.model_admissions = None;
        result.wall_ms = None;
    }
    Ok(result)
}
