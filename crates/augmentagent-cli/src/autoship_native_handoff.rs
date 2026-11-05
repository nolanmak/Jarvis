//! Immutable workflow projection for native handoff. This never dispatches,
//! transfers ownership or certifies physical inventory/kernel quiescence.
use super::*;
use augmentagent_channel_core::native_vm::{
    ownership::{
        canonical_bytes, Authority, RuntimeInventoryRow, TransferBinding, TransferObservation,
        TransferReason,
    },
    RuntimeOwner,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeHandoffSnapshot {
    pub(super) source_owners: Vec<Authority>,
    pub(super) inventory: Vec<RuntimeInventoryRow>,
    pub(super) original_binding: TransferBinding,
    pub(super) observation: TransferObservation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Projection {
    version: u32,
    workflow_id: String,
    revision: u64,
    workflow_binding: Binding,
    source_registration_sha256: String,
    observations: Vec<native_observation::NativeObservation>,
}
fn digest(value: &serde_json::Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(canonical_bytes(value)?)))
}
/// Verify both directions of the immutable event/projection join. Losing the
/// newest uncertain row must not expose an older success as the current state.
fn history_closed(tx: &Transaction<'_>, workflow: &str) -> Result<()> {
    for (kind, table, column, generation) in [
        (
            "native_launch_intent_recorded",
            "workflow_native_launch",
            "sequence",
            "lease_generation",
        ),
        (
            "native_observation_recorded",
            "workflow_native_observation",
            "sequence",
            "lease_generation",
        ),
    ] {
        let missing: bool=tx.query_row(&format!(
            "SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind=?2 AND NOT EXISTS(SELECT 1 FROM {table} p WHERE p.{column}=e.seq AND p.workflow=e.workflow AND p.{generation}=e.generation)) OR EXISTS(SELECT 1 FROM {table} p WHERE p.workflow=?1 AND NOT EXISTS(SELECT 1 FROM workflow_event e WHERE e.seq=p.{column} AND e.workflow=p.workflow AND e.kind=?2 AND e.generation=p.{generation}))"),
            params![workflow,kind],|r|r.get(0))?;
        ensure!(!missing, TransferReason::InventoryMissing);
    }
    let missing_owner:bool=tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind='native_runtime_owner_registered' AND NOT EXISTS(SELECT 1 FROM workflow_native_owner o WHERE o.workflow=e.workflow AND o.lease_generation=e.generation))",
        [workflow],|r|r.get(0))?;
    ensure!(!missing_owner, TransferReason::InventoryMissing);
    let snapshot_gap:bool=tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=?1 AND e.kind='native_handoff_snapshot_recorded' AND NOT EXISTS(SELECT 1 FROM workflow_native_handoff_snapshot s WHERE s.workflow=e.workflow AND s.sequence=e.seq)) OR EXISTS(SELECT 1 FROM workflow_native_handoff_snapshot s WHERE s.workflow=?1 AND NOT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=s.workflow AND e.seq=s.sequence AND e.kind='native_handoff_snapshot_recorded'))",
        [workflow],|r|r.get(0))?;
    ensure!(!snapshot_gap, TransferReason::InventoryMissing);

    Ok(())
}
impl WorkflowStore {
    /// Only workflow metadata is captured here. The original core transfer
    /// remains responsible for selected-store authority, actual runtime closure,
    /// healthy audit seals and kernel quiescence before ownership can change.
    pub fn native_handoff_snapshot(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        now: i64,
    ) -> Result<NativeHandoffSnapshot> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        let checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        ensure!(
            !matches!(
                checkpoint.stage,
                Stage::Held | Stage::Merged | Stage::Retired
            ),
            "native handoff preparation blocked by checkpoint"
        );
        history_closed(&tx, &lease.workflow)?;
        let owners = {
            let mut query=tx.prepare("SELECT lease_generation,audit_path FROM workflow_native_owner WHERE workflow=?1 AND lease_generation<?2 AND retirement_reason IS NULL AND retired_at IS NULL ORDER BY lease_generation,audit_path LIMIT 65")?;
            let rows = query
                .query_map(params![lease.workflow, lease.generation], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        ensure!(!owners.is_empty(), TransferReason::InventoryMissing);
        ensure!(owners.len() <= 64, TransferReason::HistoryCapacity);
        let mut source_owners = Vec::new();
        let mut source_projection = Vec::new();
        let mut original_binding = None;
        for (generation, audit) in owners {
            ensure!(
                generation > 0
                    && audit.len() <= 4096
                    && Path::new(&audit).is_absolute()
                    && !audit.contains('\0')
                    && !Path::new(&audit).components().any(|p| matches!(
                        p,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )),
                TransferReason::InventoryMissing
            );
            let raw:Option<String>=tx.query_row("SELECT binding FROM workflow_native_owner_binding WHERE workflow=?1 AND lease_generation=?2",params![lease.workflow,generation],|r|r.get(0)).optional()?;
            let native: TransferBinding =
                serde_json::from_str(&raw.context(TransferReason::InventoryMissing)?)?;
            native.validate()?;
            ensure!(
                original_binding.as_ref().is_none_or(|old| old == &native),
                TransferReason::AuthorityConflict
            );
            let registered = {
                let mut q=tx.prepare("SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_runtime_owner_registered' ORDER BY seq LIMIT 2")?;
                let rows = q
                    .query_map(params![lease.workflow, generation], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            ensure!(registered.len() == 1, TransferReason::InventoryMissing);
            let payload: serde_json::Value = serde_json::from_str(&registered[0].1)?;
            ensure!(
                payload["original_binding_sha256"] == digest(&serde_json::to_value(&native)?)?,
                TransferReason::InventoryMissing
            );
            let source = Authority {
                owner: RuntimeOwner {
                    workflow_id: lease.workflow.clone(),
                    lease_generation: generation,
                },
                audit_path: audit,
            };
            source_projection.push(serde_json::json!({"source":source,"original_binding":native,"registration_sequence":registered[0].0}));
            source_owners.push(source);
            original_binding = Some(native);
        }
        let launches = native_launch::history(&tx, &lease.workflow)?;
        let observations = native_observation::history(&tx, &lease.workflow)?;
        let eligible = |owner: &RuntimeOwner, audit: &str| {
            source_owners
                .iter()
                .any(|s| s.owner == *owner && s.audit_path == audit)
        };
        // Select the newest launch AND newest observation for each physical
        // runtime, rather than the newest successful provider/session result.
        let mut latest_launch = BTreeMap::new();
        for launch in &launches {
            latest_launch.insert(
                (
                    match launch.intent.provider {
                        submission::Provider::Codex => "codex",
                        submission::Provider::Claude => "claude",
                    },
                    launch.intent.runtime_relative_path.as_str(),
                ),
                launch,
            );
        }
        // Resolve latest history before selecting retained owners; filtering
        // first could revive a superseded success from an older namespace.
        latest_launch.retain(|_, launch| eligible(&launch.source_owner, &launch.source_audit_path));
        let mut latest = BTreeMap::new();
        for row in &observations {
            latest.insert(
                (
                    match row.provider {
                        submission::Provider::Codex => "codex",
                        submission::Provider::Claude => "claude",
                    },
                    row.runtime_relative_path.as_str(),
                ),
                row,
            );
        }
        latest.retain(|_, row| eligible(&row.source_owner, &row.source_audit_path));
        for (key, launch) in &latest_launch {
            ensure!(
                latest
                    .get(key)
                    .is_some_and(|row| row.launch_intent_id == launch.intent.intent_id),
                TransferReason::ConversationUncertain
            );
        }
        ensure!(
            latest.len() == latest_launch.len(),
            TransferReason::InventoryMissing
        );
        ensure!(latest.len() <= 64, TransferReason::HistoryCapacity);
        // A legacy conversation projection without complete immutable history
        // cannot disappear merely because no matching launch remains indexed.
        {
            let mut query=tx.prepare("SELECT provider,native_session FROM workflow_builder_conversation WHERE workflow=?1")?;
            for row in query.query_map([&lease.workflow], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })? {
                let (provider, id) = row?;
                ensure!(
                    observations.iter().any(|o| match o.provider {
                        submission::Provider::Codex => provider == "codex",
                        submission::Provider::Claude => provider == "claude",
                    } && o.native_session_id.as_deref() == Some(&id)),
                    TransferReason::InventoryMissing
                );
            }
        }
        let rows: Vec<_> = latest.values().map(|row| (*row).clone()).collect();
        let registrations_sha = digest(&serde_json::json!(source_projection))?;
        let inputs = serde_json::json!({"version":1,"workflow_id":lease.workflow,"workflow_binding":binding,"source_registration_sha256":registrations_sha,"observations":rows});
        let inputs_sha = digest(&inputs)?;
        let existing:Option<(i64,String)>=tx.query_row("SELECT sequence,record FROM workflow_native_handoff_snapshot WHERE workflow=?1 AND inputs_sha256=?2",params![lease.workflow,inputs_sha],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let projection = if let Some((sequence, raw)) = existing {
            ensure!(
                raw.len() <= 4 * 1024 * 1024,
                TransferReason::HistoryCapacity
            );
            let stored: Projection = serde_json::from_str(&raw)?;
            ensure!(
                sequence > 0 && stored.revision == u64::try_from(sequence)?,
                TransferReason::InventoryMissing
            );
            let mut expected = inputs.clone();
            expected["revision"] = serde_json::json!(stored.revision);
            ensure!(
                serde_json::to_value(&stored)? == expected,
                TransferReason::InventoryMissing
            );
            let event:Option<(String,i64,String)>=tx.query_row("SELECT kind,generation,payload FROM workflow_event WHERE workflow=?1 AND seq=?2",params![lease.workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
            let event = event.context(TransferReason::InventoryMissing)?;
            ensure!(
                event.0 == "native_handoff_snapshot_recorded"
                    && event.1 > 0
                    && event.1 <= lease.generation
                    && serde_json::from_str::<serde_json::Value>(&event.2)?
                        == serde_json::json!({"inputs_sha256":inputs_sha}),
                TransferReason::InventoryMissing
            );
            stored
        } else {
            let (count, bytes):(i64,i64)=tx.query_row(
                "SELECT count(*),coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_native_handoff_snapshot WHERE workflow=?1",
                [&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
            ensure!(
                count < 1024 && bytes >= 0 && bytes <= 124 * 1024 * 1024,
                TransferReason::HistoryCapacity
            );
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "native_handoff_snapshot_recorded",
                &serde_json::json!({"inputs_sha256":inputs_sha}).to_string(),
                now,
            )?;
            let revision: i64 = tx.query_row(
                "SELECT max(seq) FROM workflow_event WHERE workflow=?1",
                [&lease.workflow],
                |r| r.get(0),
            )?;
            let projection = Projection {
                version: 1,
                workflow_id: lease.workflow.clone(),
                revision: u64::try_from(revision)?,
                workflow_binding: binding.clone(),
                source_registration_sha256: registrations_sha,
                observations: rows,
            };
            let raw = serde_json::to_string(&projection)?;
            ensure!(
                raw.len() <= 4 * 1024 * 1024,
                TransferReason::HistoryCapacity
            );
            tx.execute(
                "INSERT INTO workflow_native_handoff_snapshot VALUES(?1,?2,?3,?4)",
                params![lease.workflow, revision, inputs_sha, raw],
            )?;
            projection
        };
        ensure!(
            projection
                .observations
                .iter()
                .all(|row| u64::try_from(row.observation_sequence)
                    .is_ok_and(|seq| seq < projection.revision)),
            TransferReason::InventoryMissing
        );
        let certain = projection
            .observations
            .iter()
            .all(|row| row.certainty == native_observation::Certainty::Certain);
        let mut inventory = Vec::new();
        for row in &projection.observations {
            if row.certainty == native_observation::Certainty::Certain {
                inventory.push(serde_json::from_value::<RuntimeInventoryRow>(
                    row.proof
                        .as_ref()
                        .context(TransferReason::InventoryMissing)?["inventory"]
                        .clone(),
                )?);
            }
        }
        inventory.sort_by(|a, b| {
            (a.provider, a.runtime_relative_path.as_bytes())
                .cmp(&(b.provider, b.runtime_relative_path.as_bytes()))
        });
        let observation = TransferObservation {
            revision: projection.revision,
            sha256: digest(&serde_json::to_value(&projection)?)?,
            certain,
        };
        tx.commit()?;
        Ok(NativeHandoffSnapshot {
            source_owners,
            inventory,
            original_binding: original_binding.context(TransferReason::InventoryMissing)?,
            observation,
        })
    }
}
