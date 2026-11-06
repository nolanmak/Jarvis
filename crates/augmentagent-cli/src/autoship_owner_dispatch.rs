//! One durable owner-body dispatch reservation. It never proves completion.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    workflow: String,
    request_id: String,
    owner: String,
    generation: i64,
    pause_revision: i64,
    binding: Binding,
    provider: submission::Provider,
    body_sha256: String,
    launch_floor: i64,
    started_at_ms: i64,
    #[serde(default,skip_serializing_if="Option::is_none")]
    first_builder:Option<serde_json::Value>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    preparation:Option<OwnerPreparationBinding>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    version: u32,
    workflow: String,
    request_id: String,
    dispatch_sequence: i64,
    binding: Binding,
    launch_intent_id: String,
    reference: augmentagent_channel_core::audit_chain::AppendReceipt,
    evidence: serde_json::Value,
    response_sha256: String,
    completed_at_ms: i64,
}
fn completion_record(conn: &Connection, record: &Record, dispatch_sequence: i64) -> Result<bool> {
    let row:Option<(i64,String,String,String)>=conn.query_row("SELECT sequence,record,response,state FROM workflow_owner_body_result WHERE workflow=?1 AND request_id=?2",params![record.workflow,record.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((sequence, raw, response, state)) = row else {
        return Ok(false);
    };
    ensure!(
        raw.len() <= 256 * 1024
            && response.len() <= 1024 * 1024
            && matches!(state.as_str(), "queued" | "uncertain" | "confirmed"),
        "owner completion reply exceeds bound or has unknown disposition"
    );
    let value: Completion = serde_json::from_str(&raw)?;
    ensure!(
        serde_json::to_value(&value)? == serde_json::from_str::<serde_json::Value>(&raw)?
            && value.version == 1
            && value.workflow == record.workflow
            && value.request_id == record.request_id
            && value.dispatch_sequence == dispatch_sequence
            && sequence > dispatch_sequence
            && value.binding.contract == record.binding.contract
            && value.completed_at_ms >= record.started_at_ms
            && value.response_sha256 == format!("{:x}", Sha256::digest(response.as_bytes())),
        "owner completion record changed"
    );
    value.binding.validate()?;
    let event_row: (String, String, i64, String, i64) = conn.query_row(
        "SELECT workflow,kind,generation,payload,at_ms FROM workflow_event WHERE seq=?1",
        [sequence],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    ensure!(
        event_row
            == (
                record.workflow.clone(),
                "owner_body_completed".into(),
                record.generation,
                raw,
                value.completed_at_ms
            ),
        "owner completion event changed"
    );
    let launch = super::native_launch::recorded_launch(conn, &value.launch_intent_id)?;
    ensure!(
        launch.sequence > dispatch_sequence
            && launch.sequence > record.launch_floor
            && launch.source_owner.lease_generation == record.generation
            && launch.workflow_binding == record.binding,
        "owner completion launch predates or differs from dispatch"
    );
    let observed=super::native_observation::referenced(conn,&record.workflow,&launch.source_audit_path,&value.reference)?;
    ensure!(observed.launch_intent_id==value.launch_intent_id,"owner completion launch reference changed");
    ensure!(
        observed.certainty == native_observation::Certainty::Certain
            && observed.provider == record.provider
            && observed.source_owner.lease_generation == record.generation
            && observed.workflow_binding == record.binding
            && value.evidence["kind"] == "current_native_completion"
            && value.evidence["launch_intent_id"] == value.launch_intent_id
            && value.evidence["completion_reference"] == serde_json::to_value(&value.reference)?
            && value.evidence["inventory"]
                == observed
                    .proof
                    .as_ref()
                    .context("owner completion inventory missing")?["inventory"],
        "owner completion native observation changed"
    );
    let origin=super::owner_inbox::message_evidence(conn,&record.workflow,&record.request_id)?.origin;
    super::owner_reply::validate_persisted_parts(conn,&record.workflow,&record.request_id,&origin,&response)?;
    Ok(true)
}
/// A successful reservation permits one caller to start the accepted body.
/// It is deliberately neither cloneable nor deserializable. A crash after
/// reservation leaves the request pending and its outcome unknown, not replayable.
pub(crate) struct OwnerBodyDispatch {
    record: Record,
    sequence: i64,
}
impl OwnerBodyDispatch {
    pub(crate) fn request_id(&self) -> &str {
        &self.record.request_id
    }
    pub(crate) fn launch_floor(&self) -> i64 {
        self.record.launch_floor
    }
}
fn decode(
    conn: &Connection,
    request: &str,
    workflow: &str,
    sequence: i64,
    raw: &str,
) -> Result<Record> {
    ensure!(raw.len() <= 16384, "owner dispatch record exceeds bound");
    let record: Record = serde_json::from_str(raw)?;
    ensure!(
        serde_json::to_value(&record)? == serde_json::from_str::<serde_json::Value>(raw)?,
        "owner dispatch has unknown nested fields"
    );
    ensure!(
        record.version == 1
            && record.request_id == request
            && record.workflow == workflow
            && record.generation > 0
            && record.pause_revision > 0
            && record.launch_floor >= 0
            && record.launch_floor < sequence
            && record.started_at_ms >= 0,
        "owner dispatch identity changed"
    );
    record.binding.validate()?;
    let actual: (String, String, i64, String, i64) = conn.query_row(
        "SELECT workflow,kind,generation,payload,at_ms FROM workflow_event WHERE seq=?1",
        [sequence],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    ensure!(
        actual
            == (
                workflow.into(),
                "owner_body_dispatch_started".into(),
                record.generation,
                raw.into(),
                record.started_at_ms
            ),
        "owner dispatch event changed"
    );
    if let Some(first)=&record.first_builder {
        let (_,original)=super::native_launch::original_context(conn,workflow,record.generation,false)?;
        let namespace=serde_json::json!({"owner":{"workflow_id":workflow,"lease_generation":record.generation},"audit_path":super::native_launch::original_context(conn,workflow,record.generation,false)?.0});
        ensure!(first["baseline"]==record.binding.head && first["native"]["kind"]=="unused_native_workflow"
            && first["native"]["namespace"]==namespace && first["native"]["original_binding_sha256"]==original,
            "first builder original native evidence changed");
    }
    let input = super::owner_inbox::message_evidence(conn, workflow, request)?;
    ensure!(
        record.body_sha256 == format!("{:x}", Sha256::digest(input.body.as_bytes())),
        "owner dispatch input changed"
    );
    if let Some(preparation)=&record.preparation {
        super::owner_revision::require_preparation(conn,&owner_turn::OwnerTurn {
            lease:Lease {workflow:record.workflow.clone(),owner:record.owner.clone(),generation:record.generation},
            request_id:record.request_id.clone(),body:input.body.clone(),pause_revision:record.pause_revision,binding:record.binding.clone(),
        },preparation)?;
    }
    let turn:(String,i64,i64,String)=conn.query_row("SELECT owner,lease_generation,pause_revision,binding FROM workflow_owner_turn WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    ensure!(
        turn == (
            record.owner.clone(),
            record.generation,
            record.pause_revision,
            serde_json::to_string(&record.binding)?
        ),
        "owner dispatch turn changed"
    );
    let state: String = conn.query_row(
        "SELECT state FROM workflow_owner_turn WHERE request_id=?1",
        [request],
        |r| r.get(0),
    )?;
    if completion_record(conn, &record, sequence)? {
        ensure!(
            state == "completed" && input.state == "applied",
            "owner completion lost its applied input or completed turn"
        );
    } else {
        ensure!(
            matches!(state.as_str(), "claimed" | "uncertain") && input.state == "pending",
            "owner dispatch has no completion disposition"
        );
        super::owner_inbox::require_pending(conn, workflow, request)?;
    }
    Ok(record)
}
pub(super) fn validate_history(conn: &Connection, workflow: &str) -> Result<()> {
    // Read-only inspection may see a pre-migration store. No record is inferred
    // when either half of an already persisted reservation is missing.
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_owner_body_dispatch')",[],|r|r.get(0))?;
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_body_dispatch_started'",[workflow],|r|r.get(0))?;
    if !exists {
        ensure!(events == 0, "owner dispatch index missing");
        return Ok(());
    }
    let mut query=conn.prepare("SELECT request_id,sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 ORDER BY sequence LIMIT 4097")?;
    let rows = query.query_map([workflow], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut count = 0;
    for row in rows {
        let (request, sequence, raw) = row?;
        count += 1;
        ensure!(count <= 4096, "owner dispatch history exceeds bound");
        decode(conn, &request, workflow, sequence, &raw)?;
    }
    ensure!(count == events, "owner dispatch history is incomplete");
    let results: i64 = conn.query_row(
        "SELECT count(*) FROM workflow_owner_body_result WHERE workflow=?1",
        [workflow],
        |r| r.get(0),
    )?;
    let completed: i64 = conn.query_row(
        "SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_body_completed'",
        [workflow],
        |r| r.get(0),
    )?;
    ensure!(
        results == completed && results <= count,
        "owner completion history is incomplete"
    );
    Ok(())
}
/// Read one completed private response with its original authenticated origin.
/// No route change, lease or serialized reference grants dispatch authority.
pub(super) fn completed_reply(conn:&Connection,workflow:&str,request:&str)->Result<(discord_ingress::DiscordOrigin,String,i64)> {
    let (sequence,raw):(i64,String)=conn.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let record=decode(conn,request,workflow,sequence,&raw)?;
    ensure!(completion_record(conn,&record,sequence)?,"owner reply has no completed dispatch");
    let (response,completion):(String,i64)=conn.query_row("SELECT response,sequence FROM workflow_owner_body_result WHERE workflow=?1 AND request_id=?2",params![workflow,request],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let counts:(i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM workflow_owner_body_result WHERE workflow=?1),(SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_body_completed')",[workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(counts.0==counts.1,"owner reply completion history is incomplete");
    Ok((super::owner_inbox::message_evidence(conn,workflow,request)?.origin,response,completion))
}
fn first_builder_policy(conn:&Connection,turn:&owner_turn::OwnerTurn,base:&str)->Result<()> {
    let registration: String=conn.query_row("SELECT r.registration FROM workflow_owner_revision r JOIN workflow_owner_initial_acceptance i ON i.request_id=r.request_id WHERE r.request_id=?1 AND r.next_contract=?2 AND r.binding=?3",params![turn.request_id,turn.binding.contract,serde_json::to_string(&turn.binding)?],|r|r.get(0))?;
    let registration:crate::autoship_contract::RegisteredContract=serde_json::from_str(&registration)?;registration.verify_metadata()?;
    ensure!(registration.contract().revision==1 && registration.baseline_head()==base && turn.binding.head==base,"first owner body differs from original registered baseline");
    let previous:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_builder_conversation WHERE workflow=?1) OR EXISTS(SELECT 1 FROM workflow_owner_body_dispatch WHERE request_id=?2)",params![turn.lease.workflow,turn.request_id],|r|r.get(0))?;
    ensure!(!previous,"first owner body has a prior conversation or reservation");
    #[derive(Serialize,Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Admission {receipt:String,provider:Option<submission::Provider>,reviewer:bool}
    let mut query=conn.prepare("SELECT generation,payload FROM workflow_event WHERE workflow=?1 AND kind='native_model_admitted' ORDER BY seq LIMIT 8193")?;
    let rows=query.query_map([&turn.lease.workflow],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?;let mut count=0i64;
    for row in rows {
        let (generation,raw)=row?;ensure!(raw.len()<=4096 && generation>0 && generation<=turn.lease.generation && count<8192,"first builder attribution exceeds bounds");
        let admission:Admission=serde_json::from_str(&raw)?;
        ensure!(serde_json::to_value(&admission)?==serde_json::from_str::<serde_json::Value>(&raw)? && admission.reviewer && admission.provider.is_some(),"previous native mutation or unknown attribution forbids first builder");
        let linked:i64=conn.query_row("SELECT count(*) FROM resource_charge c JOIN workflow_event e ON e.workflow=c.workflow AND e.payload=c.id AND e.kind='resource_charged' AND e.generation=?3 WHERE c.workflow=?1 AND c.id=?2 AND c.model_calls=1",params![turn.lease.workflow,admission.receipt,generation],|r|r.get(0))?;
        ensure!(linked==1,"first builder model attribution lost its charge");count+=1;
    }
    let (charged,counter):(i64,i64)=conn.query_row("SELECT (SELECT COALESCE(SUM(model_calls),0) FROM resource_charge WHERE workflow=?1),model_calls FROM workflow WHERE id=?1",[&turn.lease.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(charged==count && counter==count,"unclassified model history forbids first builder");Ok(())
}
impl WorkflowStore {
    pub(crate) fn has_owner_acceptance_for_turn(
        &self,
        turn: &owner_turn::OwnerTurn,
    ) -> Result<bool> {
        Ok(super::owner_revision::preparation_for_turn(&self.conn,turn)?.is_some())
    }
    pub(crate) fn validate_owner_preparation_for_turn(&mut self,turn:&owner_turn::OwnerTurn,preparation:&OwnerPreparationBinding,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::owner_turn::validate_turn(&tx,turn,now)?;
        super::owner_revision::require_preparation(&tx,turn,preparation)?;tx.commit()?;Ok(())
    }
    /// Metadata reservation only. The guard independently establishes native
    /// readiness, acceptance registration and provider/session scope before use.
    #[cfg(test)]
    pub(crate) fn begin_owner_body_dispatch(
        &mut self,
        turn: &owner_turn::OwnerTurn,
        provider: submission::Provider,
        now: i64,
    ) -> Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,None,None)
    }
    pub(crate) fn begin_prepared_owner_body_dispatch(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,
        preparation:&OwnerPreparationBinding,now:i64)->Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,None,Some(preparation))
    }
    pub(crate) fn first_owner_body_turn(&mut self,lease:&Lease,provider:submission::Provider,base:&str,now:i64)->Result<owner_turn::OwnerTurn> {
        self.require_command_builder(&lease.workflow,Some(provider))?;
        let turn=self.owner_turn_for_lease(lease,now)?;first_builder_policy(&self.conn,&turn,base)?;Ok(turn)
    }
    pub(crate) fn begin_prepared_first_owner_body(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,base:&str,canonical_main:&str,
        proof:&augmentagent_channel_core::native_vm::ownership::UnusedNativeOwnerProof<'_>,preparation:&OwnerPreparationBinding,now:i64)->Result<OwnerBodyDispatch> {
        self.begin_owner_body_dispatch_inner(turn,provider,now,Some((base,canonical_main,proof)),Some(preparation))
    }
    fn begin_owner_body_dispatch_inner(&mut self,turn:&owner_turn::OwnerTurn,provider:submission::Provider,now:i64,
        first:Option<(&str,&str,&augmentagent_channel_core::native_vm::ownership::UnusedNativeOwnerProof<'_>)>,
        preparation:Option<&OwnerPreparationBinding>,
    )->Result<OwnerBodyDispatch> {
        ensure!(now >= 0, "invalid owner dispatch time");
        self.require_command_builder(&turn.lease.workflow, Some(provider))?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::owner_turn::validate_turn(&tx, turn, now)?;
        if let Some(preparation)=preparation {super::owner_revision::require_preparation(&tx,turn,preparation)?;}
        let prior: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_owner_body_dispatch WHERE request_id=?1)",
            [&turn.request_id],
            |r| r.get(0),
        )?;
        ensure!(
            !prior,
            "owner body already reserved; reconcile outcome instead of replaying"
        );
        let first_builder=if let Some((base,main,proof))=first {
            first_builder_policy(&tx,turn,base)?;
            ensure!(proof.namespace().owner.workflow_id==turn.lease.workflow && proof.namespace().owner.lease_generation==turn.lease.generation,"first builder native namespace changed");
            let (audit,original)=super::native_launch::original_context(&tx,&turn.lease.workflow,turn.lease.generation,true)?;
            ensure!(audit==proof.namespace().audit_path && original==super::native_launch::binding_digest(proof.binding())?,"first builder original native binding changed");
            Some(serde_json::json!({"baseline":base,"canonical_main":main,"native":proof.evidence()}))
        }else{None};
        let launch_floor = super::native_launch::history(&tx, &turn.lease.workflow)?
            .last()
            .map_or(0, |r| r.sequence);
        let record = Record {
            version: 1,
            workflow: turn.lease.workflow.clone(),
            request_id: turn.request_id.clone(),
            owner: turn.lease.owner.clone(),
            generation: turn.lease.generation,
            pause_revision: turn.pause_revision,
            binding: turn.binding.clone(),
            provider,
            body_sha256: format!("{:x}", Sha256::digest(turn.body.as_bytes())),
            launch_floor,
            started_at_ms: now,
            first_builder,
            preparation:preparation.cloned(),
        };
        let raw = serde_json::to_string(&record)?;
        event(
            &tx,
            &record.workflow,
            record.generation,
            "owner_body_dispatch_started",
            &raw,
            now,
        )?;
        let sequence: i64 = tx.query_row(
            "SELECT MAX(seq) FROM workflow_event WHERE workflow=?1",
            [&record.workflow],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO workflow_owner_body_dispatch VALUES(?1,?2,?3,?4)",
            params![record.request_id, record.workflow, sequence, raw],
        )?;
        tx.commit()?;
        Ok(OwnerBodyDispatch { record, sequence })
    }
    /// Only a fresh borrowed core capability can enter this transaction. The
    /// private response is stored per request, never coalesced into progress.
    /// No model text, saved observation or lease timeout can replace `proof`.
    pub(crate) fn complete_owner_body(
        &mut self,
        dispatch: &OwnerBodyDispatch,
        proof: &augmentagent_channel_core::native_vm::ownership::NativeCompletionProof<'_>,
        binding: &Binding,
        response: &str,
        now: i64,
    ) -> Result<()> {
        binding.validate()?;
        let record = &dispatch.record;
        ensure!(
            binding.contract == record.binding.contract
                && now >= record.started_at_ms
                && response.len() <= 1024 * 1024,
            "owner completion changed acceptance or exceeds bound"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let input = super::owner_inbox::require_pending(&tx, &record.workflow, &record.request_id)?;
        let turn = owner_turn::OwnerTurn {
            lease: Lease {
                workflow: record.workflow.clone(),
                owner: record.owner.clone(),
                generation: record.generation,
            },
            request_id: record.request_id.clone(),
            body: input.body,
            pause_revision: record.pause_revision,
            binding: record.binding.clone(),
        };
        super::owner_turn::validate_turn(&tx, &turn, now)?;
        let (sequence,raw):(i64,String)=tx.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE request_id=?1 AND workflow=?2",params![record.request_id,record.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(
            sequence == dispatch.sequence
                && decode(&tx, &record.request_id, &record.workflow, sequence, &raw)? == *record,
            "owner completion dispatch changed"
        );
        let prepared = proof.prepared();
        let launch = super::native_launch::recorded_launch(&tx, &prepared.intent.intent_id)?;
        let (audit, original) =
            super::native_launch::original_context(&tx, &record.workflow, record.generation, true)?;
        let provider = match record.provider {
            submission::Provider::Codex => "codex",
            submission::Provider::Claude => "claude",
        };
        ensure!(
            prepared.intent.source_owner.workflow_id == record.workflow
                && prepared.intent.source_owner.lease_generation == record.generation
                && prepared.intent.provider == provider
                && prepared.intent.source_audit_path == Path::new(&audit)
                && original == super::native_launch::binding_digest(proof.binding())?
                && launch.intent.controller_receipt.as_ref() == Some(&prepared.controller_receipt)
                && launch.sequence > sequence
                && launch.sequence > record.launch_floor
                && launch.workflow_binding == record.binding,
            "owner completion proof differs from reserved native dispatch"
        );
        let value = Completion {
            version: 1,
            workflow: record.workflow.clone(),
            request_id: record.request_id.clone(),
            dispatch_sequence: sequence,
            binding: binding.clone(),
            launch_intent_id: prepared.intent.intent_id.clone(),
            reference: proof.receipt().clone(),
            evidence: proof.evidence().clone(),
            response_sha256: format!("{:x}", Sha256::digest(response.as_bytes())),
            completed_at_ms: now,
        };
        let raw = serde_json::to_string(&value)?;
        ensure!(
            raw.len() <= 256 * 1024,
            "owner completion evidence exceeds bound"
        );
        event(
            &tx,
            &record.workflow,
            record.generation,
            "owner_body_completed",
            &raw,
            now,
        )?;
        let sequence: i64 = tx.query_row(
            "SELECT MAX(seq) FROM workflow_event WHERE workflow=?1",
            [&record.workflow],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO workflow_owner_body_result VALUES(?1,?2,?3,?4,?5,'queued')",
            params![record.request_id, record.workflow, sequence, raw, response],
        )?;
        ensure!(tx.execute("UPDATE workflow_owner_message SET state='applied' WHERE request_id=?1 AND workflow=?2 AND state='pending'",params![record.request_id,record.workflow])?==1,"owner input changed before completion");
        ensure!(tx.execute("UPDATE workflow_owner_turn SET state='completed' WHERE request_id=?1 AND workflow=?2 AND state='claimed'",params![record.request_id,record.workflow])?==1,"owner turn changed before completion");
        let checkpoint = Checkpoint {
            stage: Stage::Build,
            binding: binding.clone(),
            evidence: Vec::new(),
            next: NextAction {
                reason: "owner-input-applied".into(),
                action: if super::owner_inbox::pending(&tx,&record.workflow)?>0 {"process_owner_input"}else{"verify_owner_changes"}.into(),
                eligible_at_ms: 0,
            },
        };
        let raw = serde_json::to_string(&checkpoint)?;
        tx.execute(
            "UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",
            params![record.workflow, raw, now],
        )?;
        tx.execute("DELETE FROM review WHERE workflow=?1", [&record.workflow])?;
        tx.execute(
            "DELETE FROM review_fragment WHERE workflow=?1",
            [&record.workflow],
        )?;
        event(
            &tx,
            &record.workflow,
            record.generation,
            "checkpoint",
            &raw,
            now,
        )?;
        super::owner_reply::enqueue(&tx,&record.workflow,&record.request_id,response)?;
        validate_history(&tx, &record.workflow)?;
        tx.commit()?;
        Ok(())
    }
    /// References only: the caller must revalidate the selected original
    /// protected completion and current physical/kernel state under core locks.
    pub(crate) fn owner_body_completion_reference(
        &mut self,
        dispatch: &OwnerBodyDispatch,
        observed: &augmentagent_channel_core::native_session::ObservedConversation,
        now: i64,
    ) -> Result<(
        augmentagent_channel_core::native_vm::PreparedLaunchRecord,
        augmentagent_channel_core::audit_chain::AppendReceipt,
    )> {
        self.validate_owner_body_dispatch(dispatch, now)?;
        let record = &dispatch.record;
        let provider = match observed.provider {
            augmentagent_channel_core::ProviderKind::Codex => submission::Provider::Codex,
            augmentagent_channel_core::ProviderKind::Claude => submission::Provider::Claude,
            _ => bail!("owner completion requires native provider"),
        };
        ensure!(
            provider == record.provider && !observed.uncertain,
            "owner body provider/session outcome changed"
        );
        let history = self.native_observation_history(&record.workflow)?;
        let last = history
            .iter()
            .rev()
            .find(|row| {
                row.source_owner.lease_generation == record.generation
                    && row.provider == record.provider
            })
            .context("owner body has no actual native completion observation")?;
        ensure!(
            last.workflow_binding == record.binding
                && last.certainty == native_observation::Certainty::Certain
                && last.native_session_id.as_deref() == Some(observed.id.as_str()),
            "owner body native outcome is uncertain or belongs to another dispatch"
        );
        let launch = super::native_launch::recorded_launch(&self.conn, &last.launch_intent_id)?;
        ensure!(
            launch.sequence > record.launch_floor
                && launch.sequence > dispatch.sequence
                && launch.source_owner.lease_generation == record.generation
                && launch.workflow_binding == record.binding,
            "owner body completion predates dispatch reservation"
        );
        let proof = last
            .proof
            .as_ref()
            .context("owner body protected observation reference missing")?;
        let prepared = augmentagent_channel_core::native_vm::PreparedLaunchRecord {
            intent: serde_json::from_value(proof["launch_intent"].clone())?,
            controller_receipt: serde_json::from_value(proof["launch_receipt"].clone())?,
        };
        let reference = last
            .protected_observation_reference
            .clone()
            .context("owner body completion reference missing")?;
        Ok((prepared, reference))
    }
    pub(crate) fn validate_owner_body_dispatch(&mut self,dispatch:&OwnerBodyDispatch,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_dispatch_tx(&tx,dispatch,now)?;tx.commit()?;Ok(())
    }
    pub(crate) fn validate_prepared_owner_body_dispatch(&mut self,lease:&Lease,dispatch:&OwnerBodyDispatch,
        preparation:&OwnerPreparationBinding,provider:submission::Provider,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_prepared_dispatch_tx(&tx,lease,dispatch,preparation,provider,now)?;tx.commit()?;Ok(())
    }

}

fn validate_dispatch_tx(tx:&Transaction<'_>,dispatch:&OwnerBodyDispatch,now:i64)->Result<()> {
    let record=&dispatch.record;
    let input = super::owner_inbox::require_pending(tx, &record.workflow, &record.request_id)?;
    let turn = owner_turn::OwnerTurn {
        lease: Lease {
            workflow: record.workflow.clone(),
            owner: record.owner.clone(),
            generation: record.generation,
        },
        request_id: record.request_id.clone(),
        body: input.body,
        pause_revision: record.pause_revision,
        binding: record.binding.clone(),
    };
    super::owner_turn::validate_turn(tx, &turn, now)?;
    let (sequence,raw):(i64,String)=tx.query_row("SELECT sequence,record FROM workflow_owner_body_dispatch WHERE request_id=?1 AND workflow=?2",params![record.request_id,record.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!(
        sequence == dispatch.sequence
            && decode(tx, &record.request_id, &record.workflow, sequence, &raw)? == *record,
        "owner dispatch reservation changed"
    );
    Ok(())
}
pub(super) fn validate_prepared_dispatch_tx(tx:&Transaction<'_>,lease:&Lease,dispatch:&OwnerBodyDispatch,
    preparation:&OwnerPreparationBinding,provider:submission::Provider,now:i64)->Result<()> {
    let record=&dispatch.record;
    ensure!(record.workflow==lease.workflow && record.owner==lease.owner && record.generation==lease.generation
        && record.provider==provider && record.preparation.as_ref()==Some(preparation),
        "owner coding reservation lacks its original preparation or provider");
    validate_dispatch_tx(tx,dispatch,now)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(path: &Path) -> (WorkflowStore, owner_turn::OwnerTurn) {
        let mut store = WorkflowStore::open(path).unwrap();
        let origin = discord_ingress::DiscordOrigin {
            bot: 11,
            owner: 22,
            channel: 33,
            message: 44,
        };
        let request = submission::Request {
            repository: "owner/repo".into(),
            target: submission::Target::Pr(42),
            provider: Some(submission::Provider::Codex),
            until: submission::Milestone::Merged,
        };
        let id = store
            .submit_discord_authorized(&origin, &request, &"a".repeat(64), 1)
            .unwrap()
            .admission
            .workflow;
        store
            .register(
                &id,
                "candidate",
                Binding {
                    head: "head".into(),
                    contract: "scope".into(),
                    evidence_hash: "evidence".into(),
                },
                2,
            )
            .unwrap();
        store
            .enqueue_owner_message(
                &discord_ingress::DiscordOrigin {
                    message: 45,
                    ..origin
                },
                &id,
                "Private: fix the failing test",
                None,
                3,
            )
            .unwrap();
        let turn = store
            .claim_owner_turn(&id, "consumer", 4, 100)
            .unwrap()
            .unwrap();
        (store, turn)
    }
    #[test]
    fn owner_body_dispatch_is_once_across_restart_and_keeps_input_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let (mut store, turn) = setup(&path);
        let before = store.snapshot(&turn.lease.workflow).unwrap().unwrap();
        let dispatch = store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
            .unwrap();
        assert_eq!(dispatch.request_id(), turn.request_id);
        assert_eq!(dispatch.launch_floor(), 0);
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 6)
            .is_err());
        store.validate_owner_body_dispatch(&dispatch, 6).unwrap();
        assert_eq!(
            store.snapshot(&turn.lease.workflow).unwrap().unwrap(),
            before
        );
        assert_eq!(
            store.pending_owner_messages(&turn.lease.workflow).unwrap(),
            1
        );
        assert!(
            !store
                .owner_control(&turn.lease.workflow)
                .unwrap()
                .unwrap()
                .paused
        );
        for (_, kind, payload, _) in store.events(&turn.lease.workflow).unwrap() {
            if kind == "owner_body_dispatch_started" {
                assert!(!payload.contains(&turn.body));
            }
        }
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        store.validate_owner_body_dispatch(&dispatch, 7).unwrap();
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 7)
            .is_err());
        assert!(store
            .claim_owner_turn(&turn.lease.workflow, "restart", 200, 100)
            .unwrap()
            .is_none());
        assert!(store.validate_owner_body_dispatch(&dispatch, 200).is_err());
    }
    #[test]
    fn owner_body_dispatch_failure_rolls_back_and_wrong_input_never_reserves() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, turn) = setup(&dir.path().join("state.db"));
        let before = store.events(&turn.lease.workflow).unwrap();
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Claude, 5)
            .is_err());
        let mut changed = turn.clone();
        changed.body = "Different instruction".into();
        assert!(store
            .begin_owner_body_dispatch(&changed, submission::Provider::Codex, 5)
            .is_err());
        assert_eq!(store.events(&turn.lease.workflow).unwrap(), before);
        store.conn.execute_batch("CREATE TRIGGER deny_dispatch BEFORE INSERT ON workflow_owner_body_dispatch BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
            .is_err());
        assert_eq!(store.events(&turn.lease.workflow).unwrap(), before);
        store
            .conn
            .execute_batch("DROP TRIGGER deny_dispatch")
            .unwrap();
        store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 6)
            .unwrap();
    }
    #[test]
    fn owner_body_dispatch_session_id_alone_cannot_complete_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, turn) = setup(&dir.path().join("state.db"));
        let dispatch = store
            .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
            .unwrap();
        for (provider, uncertain) in [
            (augmentagent_channel_core::ProviderKind::Codex, false),
            (augmentagent_channel_core::ProviderKind::Codex, true),
            (augmentagent_channel_core::ProviderKind::Claude, false),
        ] {
            let observed = augmentagent_channel_core::native_session::ObservedConversation {
                provider,
                id: "reported-session".into(),
                uncertain,
            };
            assert!(store
                .owner_body_completion_reference(&dispatch, &observed, 6)
                .is_err());
        }
        assert_eq!(
            store.pending_owner_messages(&turn.lease.workflow).unwrap(),
            1
        );
        store.validate_owner_body_dispatch(&dispatch, 6).unwrap();
    }
    #[test]
    fn owner_body_dispatch_missing_or_corrupt_history_never_grants_replay() {
        for fault in ["row", "event", "record", "event-payload"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut store, turn) = setup(&dir.path().join("state.db"));
            let dispatch = store
                .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 5)
                .unwrap();
            store
                .conn
                .pragma_update(None, "foreign_keys", false)
                .unwrap();
            match fault {
                "row" => {
                    store
                        .conn
                        .execute("DELETE FROM workflow_owner_body_dispatch", [])
                        .unwrap();
                }
                "event" => {
                    store
                        .conn
                        .execute(
                            "DELETE FROM workflow_event WHERE kind='owner_body_dispatch_started'",
                            [],
                        )
                        .unwrap();
                }
                "record" => {
                    store.conn.execute("UPDATE workflow_owner_body_dispatch SET record=json_set(record,'$.body_sha256',?1)",["b".repeat(64)]).unwrap();
                }
                "event-payload" => {
                    store.conn.execute("UPDATE workflow_event SET payload='{}' WHERE kind='owner_body_dispatch_started'",[]).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                store.validate_owner_body_dispatch(&dispatch, 6).is_err(),
                "{fault}"
            );
            assert!(
                store
                    .begin_owner_body_dispatch(&turn, submission::Provider::Codex, 6)
                    .is_err(),
                "{fault}"
            );
            assert!(
                store
                    .validate_dispatch(&turn.lease, &turn.binding, 6)
                    .is_err(),
                "{fault}: surviving guard must refuse damaged dispatch history"
            );
        }
    }
}
