//! Durable unsupported-input observation. Never nonrelease or execution authority.
use super::*;
use augmentagent_channel_core::native_vm::{
    ownership::{Authority, LegacyNativeInputWriter, MetadataNativeOwnerProof},
    PreparedLaunchRecord,
};
const KIND: &str = "owner_recovery_legacy_input_blocked";
/// Saved routing expectation only. Every delivery re-reads its original blocker
/// and authenticated private message; this JSON cannot grant a destination.
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerProgress {
    reference:RecordRef,origin:DiscordOrigin,
    #[serde(default,skip_serializing_if="Option::is_none")]
    request:Option<String>,
}
impl OwnerProgress {
    pub(crate) fn envelope_version(&self)->u32 {if self.request.is_some() {3}else{2}}
    pub(crate) fn origin(&self,conn:&Connection,workflow:&str)->Result<DiscordOrigin> {
        if let Some(request)=&self.request {
            let original=original(conn,workflow,request)?;
            let (sequence,sha256):(i64,String)=conn.query_row("SELECT sequence,record_sha256 FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
            ensure!(self.reference==RecordRef {sequence,sha256} && self.origin==original.origin,
                "private progress original publication or destination changed");
            return Ok(self.origin.clone());
        }
        let rows=load(conn,workflow)?;
        let (record,_)=rows.iter().find(|(_,r)|r==&self.reference).context("private progress original blocker missing")?;
        let claims=journal(conn,workflow)?;
        let (claim,_)=claims.iter().find(|(_,r)|r==&record.claim).context("private progress original claim missing")?;
        let message=super::super::owner_inbox::message_evidence(conn,workflow,&record.request_id)?;
        ensure!(self.origin==claim.original.origin && message.origin==self.origin
            && matches!(message.state.as_str(),"pending"|"applied"),"private progress original destination changed");
        Ok(self.origin.clone())
    }
}
pub(super) fn selected_scope(conn:&Connection,workflow:&str)->Result<OwnerProgress> {
    let plan=plan(conn,workflow)?.context("selected owner recovery original plan missing")?;
    let request=plan.original.request_id;
    let (sequence,sha256):(i64,String)=conn.query_row("SELECT sequence,record_sha256 FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let scope=OwnerProgress {reference:RecordRef {sequence,sha256},origin:plan.original.origin,request:Some(request)};
    scope.origin(conn,workflow)?;Ok(scope)
}
pub(super) fn attempt_progress(conn:&Connection,workflow:&str,now:i64)->Result<(OwnerProgress,Option<RecoveryBlockedState>)> {
    let (raw,len):(String,i64)=conn.query_row("SELECT substr(s.scope,1,8193),length(CAST(s.scope AS BLOB)) FROM owner_recovery_attempt_scope s JOIN command_attempt a ON a.workflow=s.workflow AND a.selected_at=s.selected_at WHERE s.workflow=?1 AND a.finished_at IS NOT NULL",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!((1..=8192).contains(&len) && raw.len() as i64==len,"selected recovery scope exceeds bound");
    let scope:OwnerProgress=serde_json::from_str(&raw)?;
    ensure!(scope.envelope_version()==3 && serde_json::to_string(&scope)?==raw,"selected recovery scope encoding changed");
    scope.origin(conn,workflow)?;
    let reason:String=conn.query_row("SELECT reason FROM command_attempt WHERE workflow=?1",[workflow],|r|r.get(0))?;
    if reason=="owner-metadata-ready" {
        let (claim,reference)=history(conn,workflow)?.context("ready progress has no recovery claim")?;
        ensure!(scope.request.as_deref()==Some(claim.original.request_id.as_str())
            && successor_ready::for_claim(conn,&claim,&reference)?.is_some(),
            "ready progress has no validated original successor readiness");
    }
    let blocked=match plan(conn,workflow)? {Some(plan)=>blocked::blocked_state(conn,&plan,now)?,None=>None};
    Ok((scope,blocked))
}
pub(super) fn owner_progress(conn:&Connection,workflow:&str,now:i64)->Result<Option<(OwnerProgress,Option<RecoveryBlockedState>)>> {
    let rows=load(conn,workflow)?;
    let publication_table:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_publication_intent')",[],|r|r.get(0))?;
    let latest:Option<(String,i64,String)>=if publication_table {conn.query_row("SELECT request_id,sequence,record_sha256 FROM workflow_owner_publication_intent WHERE workflow=?1 ORDER BY sequence DESC LIMIT 1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?}else{None};
    if let Some((request,sequence,sha256))=latest {
        if rows.last().is_none_or(|(r,_)|r.request_id!=request) {
            let original=original(conn,workflow,&request)?;
            let scope=OwnerProgress {reference:RecordRef {sequence,sha256},origin:original.origin,request:Some(request)};
            scope.origin(conn,workflow)?;
            let blocked=match plan(conn,workflow)? {Some(plan)=>blocked::blocked_state(conn,&plan,now)?,None=>None};
            return Ok(Some((scope,blocked)));
        }
    }
    let Some((record,reference))=rows.last() else {return Ok(None)};
    let claims=journal(conn,workflow)?;
    let (claim,_)=claims.iter().find(|(_,r)|r==&record.claim).context("private progress original claim missing")?;
    let scope=OwnerProgress {reference:reference.clone(),origin:claim.original.origin.clone(),request:None};
    scope.origin(conn,workflow)?;
    let blocked=match plan(conn,workflow)? {Some(plan)=>blocked::blocked_state(conn,&plan,now)?,None=>None};
    Ok(Some((scope,blocked)))
}
#[cfg(test)]
pub(super) fn fixture_encoding(raw: &str) -> Result<String> {
    let record: Record = serde_json::from_str(raw)?;
    Ok(serde_json::to_string(&record)?)
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    request_id: String,
    intent: super::super::owner_publication::Reference,
    claim: RecordRef,
    generation: i64,
    previous: Option<RecordRef>,
    native_namespace: Authority,
    launch_intent_id: String,
    launch: RecordRef,
    native_launch_protocol: u32,
    observations_sha256: String,
    outcomes_sha256: String,
    control: owner_control::OwnerControl,
    observed_at_ms: i64,
}
#[derive(Clone, PartialEq, Eq, Serialize)]
struct Outcome {
    provider: submission::Provider,
    certainty: super::super::native_observation::Certainty,
    native_session_id: Option<String>,
    launch_sha256: Option<String>,
    creation_receipt_sha256: String,
    completion_sha256: Option<String>,
    alias_sha256: Option<String>,
    export_manifest_sha256: Option<String>,
}
impl Outcome {
    fn from_row(row: &super::super::native_observation::NativeObservation) -> Self {
        Self {
            provider: row.provider,
            certainty: row.certainty.clone(),
            native_session_id: row.native_session_id.clone(),
            launch_sha256: row.launch_sha256.clone(),
            creation_receipt_sha256: row.creation_receipt_sha256.clone(),
            completion_sha256: row.completion_sha256.clone(),
            alias_sha256: row.alias_sha256.clone(),
            export_manifest_sha256: row.export_manifest_sha256.clone(),
        }
    }
}
#[derive(Default)]
struct Outcomes {
    latest: std::collections::BTreeMap<String, (u64, Outcome)>,
    cached: Option<String>,
}
impl Outcomes {
    fn note(&mut self, launch: &str, receipt: u64, outcome: Outcome) -> Result<()> {
        if let Some((previous, current)) = self.latest.get(launch) {
            if receipt < *previous {
                return Ok(());
            }
            if receipt == *previous {
                ensure!(*current == outcome, "native outcome source receipt changed");
                return Ok(());
            }
            if *current != outcome {
                self.cached = None;
            }
        } else {
            self.cached = None;
        }
        self.latest.insert(launch.into(), (receipt, outcome));
        Ok(())
    }
    fn fingerprint(&mut self) -> Result<String> {
        use sha2::Digest;
        if self.cached.is_none() {
            let outcomes = self
                .latest
                .iter()
                .map(|(id, (_, outcome))| (id, outcome))
                .collect::<std::collections::BTreeMap<_, _>>();
            self.cached = Some(format!(
                "{:x}",
                Sha256::digest(
                    augmentagent_channel_core::native_vm::ownership::canonical_bytes(
                        &serde_json::json!({"version":1,"outcomes":outcomes})
                    )?
                )
            ));
        }
        Ok(self.cached.as_ref().unwrap().clone())
    }
}
struct ObservationSnapshot {
    exact: String,
    outcomes: String,
}
struct ObservationRead {
    rows: Vec<super::super::native_observation::NativeObservation>,
    cursor: usize,
    last_before: i64,
    sha256: Sha256,
    outcomes: Outcomes,
}
impl ObservationRead {
    fn read(conn: &Connection, workflow: &str) -> Result<Self> {
        use sha2::Digest;
        let rows = super::super::native_observation::history(conn, workflow)?;
        let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='native_observation_recorded'",[workflow],|r|r.get(0))?;
        ensure!(
            events == rows.len() as i64,
            "legacy input native observation history incomplete"
        );
        Ok(Self {
            rows,
            cursor: 0,
            last_before: 0,
            sha256: Sha256::new(),
            outcomes: Outcomes::default(),
        })
    }
    // Block records are ordered by sequence. Decode and hash each native row
    // once, retaining only a hash prefix for each historical boundary.
    fn at(&mut self, before: i64) -> Result<ObservationSnapshot> {
        use sha2::Digest;
        ensure!(
            before >= self.last_before,
            "legacy input observation boundaries are not ordered"
        );
        while self.cursor < self.rows.len() && self.rows[self.cursor].observation_sequence < before
        {
            let row = &self.rows[self.cursor];
            if row.protected_observation_reference.is_some() && row.proof.is_some() {
                ensure!(
                    row.version == 1,
                    "unsupported legacy blocker native outcome version"
                );
                self.sha256.update(
                    augmentagent_channel_core::native_vm::ownership::canonical_bytes(
                        &serde_json::to_value(row)?,
                    )?,
                );
                self.sha256.update(b"\n");
                self.outcomes.note(
                    &row.launch_intent_id,
                    row.protected_observation_reference
                        .as_ref()
                        .unwrap()
                        .sequence,
                    Outcome::from_row(row),
                )?;
            }
            self.cursor += 1;
        }
        self.last_before = before;
        Ok(ObservationSnapshot {
            exact: format!("{:x}", self.sha256.clone().finalize()),
            outcomes: self.outcomes.fingerprint()?,
        })
    }
}
fn observations(conn: &Connection, workflow: &str, before: i64) -> Result<ObservationSnapshot> {
    ObservationRead::read(conn, workflow)?.at(before)
}
#[cfg(test)]
#[test]
fn native_outcomes_ignore_duplicate_receipts_and_causally_older_arrivals() {
    use super::super::native_observation::Certainty;
    // Semantic consumer only: these values cannot construct a native proof or
    // establish production late-outcome admission/qualification.
    let first = Outcome {
        provider: submission::Provider::Codex,
        certainty: Certainty::Certain,
        native_session_id: Some("original-session".into()),
        launch_sha256: Some("a".repeat(64)),
        creation_receipt_sha256: "b".repeat(64),
        completion_sha256: Some("c".repeat(64)),
        alias_sha256: Some("d".repeat(64)),
        export_manifest_sha256: Some("e".repeat(64)),
    };
    let mut changed = first.clone();
    changed.export_manifest_sha256 = Some("f".repeat(64));
    let mut outcomes = Outcomes::default();
    outcomes.note("original-launch", 20, first.clone()).unwrap();
    let original = outcomes.fingerprint().unwrap();
    outcomes.note("original-launch", 21, first.clone()).unwrap();
    assert_eq!(outcomes.fingerprint().unwrap(), original);
    outcomes
        .note("original-launch", 19, changed.clone())
        .unwrap();
    assert_eq!(outcomes.fingerprint().unwrap(), original);
    outcomes
        .note("original-launch", 22, changed.clone())
        .unwrap();
    let current = outcomes.fingerprint().unwrap();
    assert_ne!(current, original);
    outcomes.note("original-launch", 20, first.clone()).unwrap();
    assert_eq!(outcomes.fingerprint().unwrap(), current);
    assert!(outcomes.note("original-launch", 22, first).is_err());
    assert_eq!(outcomes.fingerprint().unwrap(), current);
    outcomes.note("another-launch", 1, changed).unwrap();
    assert_ne!(outcomes.fingerprint().unwrap(), current);
}
fn launch_reference(
    conn: &Connection,
    id: &str,
) -> Result<(super::super::native_launch::NativeLaunchRecord, RecordRef)> {
    let launch = super::super::native_launch::recorded_launch(conn, id)?;
    let (raw,len):(String,i64)=conn.query_row("SELECT substr(record,1,16385),length(CAST(record AS BLOB)) FROM workflow_native_launch WHERE intent_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        (1..=16384).contains(&len) && raw.len() as i64 == len,
        "legacy input launch exceeds bound"
    );
    let reference = RecordRef {
        sequence: launch.sequence,
        sha256: digest(&raw),
    };
    Ok((launch, reference))
}
fn load(conn: &Connection, workflow: &str) -> Result<Vec<(Record, RecordRef)>> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_legacy_input_blocked')",[],|r|r.get(0))?;
    let events: i64 = conn.query_row(
        "SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind=?2",
        params![workflow, KIND],
        |r| r.get(0),
    )?;
    if !exists {
        ensure!(events == 0, "legacy input blocker index missing");
        return Ok(Vec::new());
    }
    let (count,bytes):(i64,i64)=conn.query_row("SELECT count(*),coalesce(sum(length(CAST(record AS BLOB))),0) FROM workflow_owner_legacy_input_blocked WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        events == count && count <= 1024 && bytes <= 128 * 1024 * 1024,
        "legacy input blocker history missing or oversized"
    );
    if count == 0 {
        return Ok(Vec::new());
    }
    let claims = journal(conn, workflow)?;
    let mut observation_history = ObservationRead::read(conn, workflow)?;
    let mut query=conn.prepare("SELECT generation,sequence,substr(record_sha256,1,65),substr(record,1,32769),length(CAST(record AS BLOB)) FROM workflow_owner_legacy_input_blocked WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let rows = query.query_map([workflow], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut result: Vec<(Record, RecordRef)> = Vec::new();
    for row in rows {
        let (generation, sequence, hash, raw, len) = row?;
        ensure!(
            (1..=32768).contains(&len) && raw.len() as i64 == len && digest(&raw) == hash,
            "legacy input blocker record changed or oversized"
        );
        let record: Record = serde_json::from_str(&raw)?;
        let index = claims
            .iter()
            .position(|(_, r)| *r == record.claim)
            .context("legacy input blocker claim missing")?;
        let claim = &claims[index].0;
        ensure!(
            record.version == 1
                && record.native_launch_protocol == 2
                && serde_json::to_string(&record)? == raw
                && record.request_id == claim.original.request_id
                && record.intent == claim.original.intent
                && generation == claim.generation
                && record.generation == generation
                && record.claim.sequence < sequence
                && record.observed_at_ms >= claim.claimed_at
                && record.observed_at_ms < claim.lease_until
                && record.control.revision >= claim.control.revision
                && record.control.updated_at_ms <= record.observed_at_ms
                && claims
                    .get(index + 1)
                    .is_none_or(|(_, r)| sequence < r.sequence)
                && record.previous == result.last().map(|(_, r)| r.clone()),
            "legacy input blocker original lineage changed"
        );
        ensure!(
            record.native_namespace
                == native_intent::frontier_at(conn, &claim.original, u64::try_from(sequence)?)?,
            "legacy input blocker native frontier changed"
        );
        let snapshot = observation_history.at(sequence)?;
        ensure!(
            record.observations_sha256 == snapshot.exact,
            "legacy input blocker native observation snapshot changed"
        );
        ensure!(
            record.outcomes_sha256 == snapshot.outcomes,
            "legacy input blocker native outcome snapshot changed"
        );
        let (launch, reference) = launch_reference(conn, &record.launch_intent_id)?;
        let (_, binding) = super::super::native_launch::original_context(
            conn,
            workflow,
            record.native_namespace.owner.lease_generation,
            false,
        )?;
        ensure!(
            reference == record.launch
                && reference.sequence < record.claim.sequence
                && launch.workflow_id == workflow
                && launch.source_owner.lease_generation
                    <= record.native_namespace.owner.lease_generation
                && launch.original_native_binding_sha256 == binding,
            "legacy input blocker launch reference changed"
        );
        let (kind,g,payload,at,len):(String,i64,String,i64,i64)=conn.query_row("SELECT kind,generation,substr(payload,1,4097),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        ensure!(
            (1..=4096).contains(&len)
                && payload.len() as i64 == len
                && kind == KIND
                && g == generation
                && at == record.observed_at_ms
                && serde_json::from_str::<serde_json::Value>(&payload)?
                    == serde_json::json!({"version":1,"request_id":record.request_id,"record_sha256":hash}),
            "legacy input blocker event changed"
        );
        result.push((
            record,
            RecordRef {
                sequence,
                sha256: hash,
            },
        ));
    }
    ensure!(
        result.len() as i64 == count,
        "legacy input blocker index changed"
    );
    Ok(result)
}
pub(super) fn retained_count(conn: &Connection, workflow: &str) -> Result<usize> {
    Ok(load(conn, workflow)?.len())
}
pub(super) fn state(
    conn: &Connection,
    plan: &RecoveryPlan,
    now: i64,
    other_count: usize,
) -> Result<Option<RecoveryBlockedState>> {
    let rows = load(conn, &plan.original.workflow)?;
    ensure!(
        rows.len()
            .checked_add(other_count)
            .is_some_and(|n| n <= 1024),
        "recovery blocked history exceeds shared bound"
    );
    let Some((record, _)) = rows.last() else {
        return Ok(None);
    };
    ensure!(
        record.request_id == plan.original.request_id
            && record.intent == plan.original.intent
            && record.generation <= plan.predecessor.generation
            && record.observed_at_ms <= now,
        "legacy input blocker differs from current lineage or clock"
    );
    // New authenticated outcome evidence may wake metadata reconciliation, not
    // input release. Timers, ownership changes and policy edits cannot qualify
    // an old writer; the next actual proof still controls execution.
    if rows.len() + other_count == 1024 {
        return Ok(Some(RecoveryBlockedState::HistoryCapacity));
    }
    if record.outcomes_sha256 != observations(conn, &plan.original.workflow, i64::MAX)?.outcomes {
        return Ok(None);
    }
    Ok(Some(RecoveryBlockedState::LegacyInputWriter))
}
impl WorkflowStore {
    /// Caller retains the actual original native proof/locks, using a connection
    /// independent of its current-owner fence. No callback runs inside SQL.
    pub(crate) fn record_owner_recovery_legacy_writer_blocked(
        &mut self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        proof: &MetadataNativeOwnerProof<'_>,
        now: i64,
    ) -> Result<()> {
        proof.validate_current()?;
        let error = proof
            .with_revoked_input_writers(|_| Ok(()))
            .err()
            .context("legacy input blocker requires an actual unsupported writer")?;
        ensure!(
            error.downcast_ref::<LegacyNativeInputWriter>().is_some(),
            "legacy input blocker requires the typed original protocol refusal"
        );
        let writer = proof.evidence()["audits"]
            .as_array()
            .context("legacy input audit census missing")?
            .iter()
            .filter_map(|audit| audit["native_input_writers"].as_array())
            .flatten()
            .find(|writer| writer["launch_intent"]["version"] == 2)
            .context("legacy input original writer missing")?;
        let prepared = PreparedLaunchRecord {
            intent: serde_json::from_value(writer["launch_intent"].clone())?,
            controller_receipt: serde_json::from_value(writer["launch_receipt"].clone())?,
        };
        prepared.intent.validate()?;
        let expected = self.owner_publication_recovery_native_facts(lease, authorization, now)?;
        ensure!(
            expected.namespace() == proof.namespace()
                && expected.original_binding() == proof.binding(),
            "legacy input blocker original native proof changed"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_authorized_claim(&tx, lease, authorization, now)?;
        let facts = recovery_native_facts_tx(&tx, lease, authorization, now)?;
        ensure!(
            facts.fingerprint()? == expected.fingerprint()?,
            "legacy input blocker native facts changed before commit"
        );
        native_pending::require_clear_abort_barriers(&tx, &lease.claim.original.workflow)?;
        let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[&lease.claim.original.workflow],|r|r.get(0))?;
        ensure!(
            !pending && !native_pending::orphaned(&tx, &lease.claim.original.workflow)?,
            "legacy input blocker cannot hide unsettled native work"
        );
        let (launch, reference) = launch_reference(&tx, &prepared.intent.intent_id)?;
        super::super::owner_dispatch::require_same_prepared(&launch, &prepared)?;
        let prior = load(&tx, &lease.claim.original.workflow)?;
        let snapshot = observations(&tx, &lease.claim.original.workflow, i64::MAX)?;
        if let Some((record, _)) = prior.last().filter(|(r, _)| r.claim == lease.reference) {
            ensure!(
                record.launch == reference
                    && record.native_namespace == *proof.namespace()
                    && record.outcomes_sha256 == snapshot.outcomes,
                "legacy input blocker changed within its claim"
            );
            tx.commit()?;
            proof.validate_current()?;
            return Ok(());
        }
        ensure!(
            prior.len() + blocked::retained_count(&tx, &lease.claim.original.workflow)? < 1024,
            "recovery blocked history capacity reached"
        );
        let record = Record {
            version: 1,
            request_id: lease.request_id().into(),
            intent: lease.claim.original.intent.clone(),
            claim: lease.reference.clone(),
            generation: lease.generation(),
            previous: prior.last().map(|(_, r)| r.clone()),
            native_namespace: proof.namespace().clone(),
            launch_intent_id: prepared.intent.intent_id,
            launch: reference,
            native_launch_protocol: 2,
            observations_sha256: snapshot.exact,
            outcomes_sha256: snapshot.outcomes,
            control: owner_control::read(&tx, &lease.claim.original.workflow)?
                .context("legacy input owner control missing")?,
            observed_at_ms: now,
        };
        let raw = serde_json::to_string(&record)?;
        ensure!(raw.len() <= 32768, "legacy input blocker exceeds bound");
        let hash = digest(&raw);
        require_authorized_claim(
            &tx,
            lease,
            authorization,
            chrono::Utc::now().timestamp_millis(),
        )?;
        let sequence=append_event(
            &tx,
            &lease.claim.original.workflow,
            lease.generation(),
            KIND,
            &serde_json::json!({"version":1,"request_id":record.request_id,"record_sha256":hash})
                .to_string(),
            now,
        )?;
        tx.execute(
            "INSERT INTO workflow_owner_legacy_input_blocked VALUES(?1,?2,?3,?4,?5)",
            params![
                lease.claim.original.workflow,
                lease.generation(),
                sequence,
                hash,
                raw
            ],
        )?;
        load(&tx, &lease.claim.original.workflow)?;
        super::super::progress::project(&tx,&lease.claim.original.workflow,KIND,now)?;
        require_authorized_claim(&tx,lease,authorization,chrono::Utc::now().timestamp_millis())?;
        tx.commit()?;
        proof.validate_current()?;
        Ok(())
    }
}
