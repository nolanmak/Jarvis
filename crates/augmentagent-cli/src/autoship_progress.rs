//! Durable, bounded owner progress projection; never a model transcript.
use super::*;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub struct Progress {
    pub workflow:String,
    pub sequence:i64,
    pub kind:String,
    pub stage:Option<Stage>,
    pub reason:String,
    pub next_action:String,
    pub eligible_at_ms:Option<i64>,
    pub pr:Option<u64>,
    pub requested_provider:Option<submission::Provider>,
    pub actual_provider:Option<String>,
    #[serde(default)]
    pub actual_provider_role:Option<String>,
    pub merge_state:String,
    pub deployment_state:String,
    pub evidence_digest:String,
    pub observed_at_ms:i64,
    #[serde(default)]
    pub resource_refusal:Option<super::resource_refusal::AttemptObservation>,
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub tool_capacity_refusal:Option<super::tool_capacity::ToolCapacityRefusal>,
}
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct DeliveryStatus {
    pub state:String,
    pub pending_events:i64,
    pub oldest_pending_at_ms:Option<i64>,
    pub latest_sequence:Option<i64>,
}
fn exists(conn:&Connection)->Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_progress_event')",[],|r|r.get(0))?)
}
impl WorkflowStore {
    pub fn progress_events(&self,workflow:&str)->Result<Vec<Progress>> {
        if !exists(&self.conn)? {return Ok(Vec::new());}
        let mut stmt=self.conn.prepare("SELECT payload FROM workflow_progress_event WHERE workflow=?1 ORDER BY sequence")?;
        let rows=stmt.query_map([workflow],|r|r.get::<_,String>(0))?;
        rows.map(|row|Ok(serde_json::from_str(&row?)?)).collect()
    }
    pub fn uncertain_progress(&self,bot:u64,owner:u64)->Result<Vec<(super::discord_ingress::DiscordOrigin,String,i64,String,String)>> {
        ensure!(bot>0 && owner>0,"progress recovery identity missing");
        let mut stmt=self.conn.prepare("SELECT d.channel,d.workflow,d.sequence,d.nonce,d.content FROM workflow_progress_delivery d JOIN workflow_delivery_binding b ON b.workflow=d.workflow AND b.bot=d.bot AND b.owner=d.owner AND b.channel=d.channel WHERE d.bot=?1 AND d.owner=?2 AND d.state='uncertain' ORDER BY d.dispatched_at,d.workflow LIMIT 128")?;
        let rows=stmt.query_map(params![bot.to_string(),owner.to_string()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?;
        rows.map(|row| {let (channel,workflow,sequence,nonce,content)=row?;Ok((super::discord_ingress::DiscordOrigin {bot,owner,channel:channel.parse()?,message:1},workflow,sequence,nonce,content))}).collect()
    }
    pub fn pending_progress(&self,bot:u64,owner:u64)->Result<Vec<(super::discord_ingress::DiscordOrigin,String,i64)>> {
        ensure!(bot>0 && owner>0,"progress recovery identity missing");
        if !exists(&self.conn)? {return Ok(Vec::new());}
        let mut stmt=self.conn.prepare("SELECT b.channel,b.workflow,s.sequence FROM workflow_delivery_binding b JOIN workflow_progress_state s ON s.workflow=b.workflow WHERE b.bot=?1 AND b.owner=?2 AND NOT EXISTS(SELECT 1 FROM workflow_progress_delivery d WHERE d.workflow=b.workflow AND d.bot=b.bot AND d.owner=b.owner AND d.channel=b.channel AND (d.state='uncertain' OR d.sequence>=s.sequence)) ORDER BY s.workflow,b.channel LIMIT 128")?;
        let rows=stmt.query_map(params![bot.to_string(),owner.to_string()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?;
        rows.map(|row| {let (channel,workflow,sequence)=row?;Ok((super::discord_ingress::DiscordOrigin {bot,owner,channel:channel.parse()?,message:1},workflow,sequence))}).collect()
    }
    pub fn claim_progress(&mut self,origin:&super::discord_ingress::DiscordOrigin,workflow:&str,sequence:i64,now:i64)->Result<Option<(String,String)>> {
        origin.validate()?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::delivery::require_destination(&tx,origin,workflow)?;
        let raw:Option<(i64,String)>=tx.query_row("SELECT sequence,payload FROM workflow_progress_state WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((current,raw))=raw else {return Ok(None);};
        if current!=sequence {return Ok(None);}
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_progress_delivery WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND (state='uncertain' OR sequence>=?5))",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence],|r|r.get(0))?;
        if blocked {return Ok(None);}
        let progress:Progress=serde_json::from_str(&raw)?;
        let nonce=format!("{:x}",Sha256::digest(serde_json::to_vec(&(workflow,origin.bot,origin.owner,origin.channel,sequence))?))[..24].to_string();
        let resource = progress.resource_refusal.as_ref().map(|value| format!(" {}.", value.render())).unwrap_or_default();
        let tool_capacity=progress.tool_capacity_refusal.as_ref().map(|value|format!(" {}.",value.render())).unwrap_or_default();
        let content=format!("Autoship {workflow} · update {sequence}: {}. Stage: {:?}. Reason: {}. Next: {} (eligible at: {:?}). PR: {:?}. Requested provider: {:?}; actual provider: {} ({}). Merge: {}. Deployment: {}. Evidence digest: {}.{resource}{tool_capacity} [autoship:{nonce}]",
            progress.kind,progress.stage,progress.reason,progress.next_action,progress.eligible_at_ms,progress.pr,progress.requested_provider,progress.actual_provider.as_deref().unwrap_or("unknown"),progress.actual_provider_role.as_deref().unwrap_or("unknown"),progress.merge_state,progress.deployment_state,progress.evidence_digest);
        ensure!(content.chars().count()<=1900,"progress message exceeds Discord bound");
        tx.execute("INSERT INTO workflow_progress_delivery(workflow,bot,owner,channel,sequence,nonce,content,state,dispatched_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'uncertain',?8)",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence,nonce,content,now])?;
        tx.commit()?;Ok(Some((nonce,content)))
    }
    pub fn confirm_progress(&mut self,origin:&super::discord_ingress::DiscordOrigin,workflow:&str,sequence:i64,message:u64,now:i64)->Result<()> {
        origin.validate()?;ensure!(message>0,"progress remote message missing");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::delivery::require_destination(&tx,origin,workflow)?;
        let (state,prior):(String,Option<String>)=tx.query_row("SELECT state,message FROM workflow_progress_delivery WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if state=="confirmed" {ensure!(prior.as_deref()==Some(message.to_string().as_str()),"progress receipt conflict");return Ok(());}
        ensure!(state=="uncertain","progress was never dispatched");
        tx.execute("UPDATE workflow_progress_delivery SET state='confirmed',message=?6,confirmed_at=?7 WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence,message.to_string(),now])?;
        tx.commit()?;Ok(())
    }
    pub fn progress_delivery(&self,workflow:&str)->Result<DeliveryStatus> {
        if !exists(&self.conn)? {return Ok(DeliveryStatus {state:"not-observed".into(),pending_events:0,oldest_pending_at_ms:None,latest_sequence:None});}
        let delivery_table:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_progress_delivery')",[],|r|r.get(0))?;
        let new_routes:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_delivery_binding')",[],|r|r.get(0))?;
        let routes=if new_routes {"workflow_delivery_binding"} else {"discord_workflow_binding"};
        let query=if delivery_table {"SELECT count(*),min(e.created_at),max(e.sequence) FROM workflow_progress_event e WHERE e.workflow=?1 AND (NOT EXISTS(SELECT 1 FROM workflow_delivery_binding b WHERE b.workflow=e.workflow) OR EXISTS(SELECT 1 FROM workflow_delivery_binding b WHERE b.workflow=e.workflow AND NOT EXISTS(SELECT 1 FROM workflow_progress_delivery d WHERE d.workflow=b.workflow AND d.bot=b.bot AND d.owner=b.owner AND d.channel=b.channel AND d.state='confirmed' AND d.sequence>=e.sequence)))"}
            else {"SELECT count(*),min(created_at),max(sequence) FROM workflow_progress_event WHERE workflow=?1"};
        let query=query.replace("workflow_delivery_binding",routes);
        let (pending,oldest,latest):(i64,Option<i64>,Option<i64>)=self.conn.query_row(&query,[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let bound:bool=self.conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {routes} WHERE workflow=?1)"),[workflow],|r|r.get(0))?;
        let uncertain=delivery_table && self.conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_progress_delivery WHERE workflow=?1 AND state='uncertain')",[workflow],|r|r.get::<_,bool>(0))?;
        Ok(DeliveryStatus {state:if uncertain {"delivery-uncertain"} else if pending==0 {"delivery-confirmed"} else if bound {"delivery-pending"} else {"delivery-unconfigured"}.into(),
            pending_events:pending,oldest_pending_at_ms:oldest,latest_sequence:latest})
    }
}

// Only fixed machine codes cross this boundary. Unexpected prose is retained
// solely as a digest for change detection, never copied into progress payloads.
fn code(value:&str,fallback:&str)->String {
    // Keep this vocabulary explicit. Syntactically plausible tokens can still
    // contain credentials or private content from an upstream failure.
    if super::is_review_hold_reason(value) || matches!(value,
        "registered"|"scope"|"ci_pending"|"await_checks"|"await_capacity"|"complete"|
        "reverify"|"reverify_candidate"|"inputs_changed"|"protected_acceptance_stale"|
        "remote_merge_observed"|"resource_capacity"|"checks_pending"|"review_pending"|
        "build"|"verify"|"review"|"reconcile"|"publish"|"merge"|"retry"|
        "execution-readiness-blocked"|"target-authorization-failed"|"execution-failed"|
        "workflow-progressed"|"owner-pause-requested"|"owner-resume-accepted"|
        "await_owner_resume"|"claim_workflow"|"awaiting-worker-claim"|
        "merge-allowance-exhausted"|"merge-reconciliation-required"|
        "review_changes_requested"|"ci_failed"|"merge_conflict"|"head_changed"|"target_closed"|
        "acceptance_verifier_repair_exhausted"|"acceptance_verifier_amendment_declined"|"acceptance_verifier_evidence_unknown"|
        "acceptance_contract_invalid"|"acceptance_verifier_infrastructure_unavailable"|
        "readiness_assessment_invalid"|"await_readiness_input_change"|
        "await_acceptance_input_change"|"await_original_evidence_recovery"|"await_verifier_infrastructure_recovery"|
        "review_main_changed"|"reconcile_main"|"resume_deferred"|"resume_preserved_work"|"resume_preserved_pr"|
        "draft_published"|"resume"|"adopted_review_pending"|"adopted_execution_retry"|
        "owner_execution_retry_pending"|"reconcile_runtime_retirement"|"owner_execution_retry_requested"|"resume_started"|
        "retry_worker_attempt"|"inspect_workflow"|"await_worker"|
        "linked_pr_claim_exists"|"linked_pr_inventory_unknown"|"reconcile_existing_pr"|"retry_github_inventory") {value.into()} else {fallback.into()}
}

pub(super) fn project(tx:&Transaction<'_>,workflow:&str,source:&str,now:i64)->Result<()> {
    if !matches!(source,"registered"|"checkpoint"|"autonomous_merge_confirmed"|"command_queued"|"command_attempt_finished"|"owner_control_changed"|"native_model_admitted"|"resource_capacity_retry") {return Ok(());}
    let previous:Option<(i64,String,bool)>=tx.query_row("SELECT sequence,fingerprint,terminal FROM workflow_progress_state WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if previous.as_ref().is_some_and(|p|p.2) {return Ok(());}
    super::delivery::apply_default(tx,workflow,now)?;
    let snapshot:Option<(String,Option<u64>)>=tx.query_row("SELECT checkpoint,pr FROM workflow WHERE id=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let checkpoint=snapshot.as_ref().map(|(raw,_)|serde_json::from_str::<Checkpoint>(raw)).transpose()?;
    let requested:Option<String>=tx.query_row("SELECT provider FROM command_job WHERE workflow=?1",[workflow],|r|r.get(0)).optional()?;
    let requested_provider=requested.map(|raw|serde_json::from_str(&raw)).transpose()?.flatten();
    let confirmed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation JOIN merge_mutation_receipt r ON r.operation=d.operation WHERE o.workflow=?1 AND d.state='confirmed')",[workflow],|r|r.get(0))?;
    let mut stage=checkpoint.as_ref().map(|c|c.stage);
    let mut kind=match stage {None=>"queued",Some(Stage::Intake)=>"started",Some(Stage::Held)=>"blocked",Some(Stage::AwaitChecks)=>"waiting",Some(Stage::Retired)=>"retired",Some(Stage::Merged)=>"merge-reconciliation",Some(_)=>"stage"}.to_string();
    let mut reason=checkpoint.as_ref().map_or_else(||"awaiting-worker-claim".into(),|c|code(&c.next.reason,"workflow-state-changed"));
    let mut action=checkpoint.as_ref().map_or_else(||"claim_workflow".into(),|c|code(&c.next.action,"inspect_workflow"));
    let mut eligible=checkpoint.as_ref().map(|c|c.next.eligible_at_ms);
    let attempt:Option<(i64,String,String,i64)>=tx.query_row("SELECT selected_at,outcome,reason,eligible_at FROM command_attempt WHERE workflow=?1 AND finished_at IS NOT NULL",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let linked_hold=checkpoint.as_ref().is_some_and(|c|matches!(c.next.reason.as_str(),"linked_pr_claim_exists"|"linked_pr_inventory_unknown"));
    if linked_hold {kind="blocked".into();}
    let initial_verifier_hold=checkpoint.as_ref().is_some_and(|c|c.stage==Stage::Scope && matches!(
        (c.next.reason.as_str(),c.next.action.as_str()),
        ("acceptance_verifier_repair_exhausted"|"acceptance_verifier_amendment_declined"|"acceptance_contract_invalid","await_acceptance_input_change")|
        ("readiness_assessment_invalid","await_readiness_input_change")|
        ("acceptance_verifier_evidence_unknown","await_original_evidence_recovery")|
        ("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery")));
    let typed_review_hold=checkpoint.as_ref().is_some_and(|c|
        matches!(c.stage,Stage::Held|Stage::Review|Stage::Reconcile) && super::is_review_hold_reason(&c.next.reason));
    let main_wait=checkpoint.as_ref().is_some_and(|c|c.stage==Stage::Reconcile
        && c.next.reason=="review_main_changed" && c.next.action=="reconcile_main");
    if initial_verifier_hold || typed_review_hold || linked_hold {kind="blocked".into();}
    let generic_attempt=attempt.as_ref().is_some_and(|(_,outcome,why,_)|
        (outcome=="progress" && why=="workflow-progressed") || (outcome=="deferred" && why=="workflow-held"));
    // A successful wrapper return cannot replace the checkpoint's blocker or
    // backoff. A distinct current execution failure still has its own outcome.
    let preserve_checkpoint=linked_hold || initial_verifier_hold || (generic_attempt && (stage==Some(Stage::Held) || typed_review_hold || main_wait));
    let capacity=checkpoint.as_ref().filter(|c|c.next.reason=="resource_capacity" && c.next.action=="await_capacity"
        && !matches!(c.stage,Stage::Held|Stage::Merged|Stage::Retired|Stage::Merge));
    if capacity.is_some() {kind="blocked".into();}
    if source=="command_attempt_finished" && !preserve_checkpoint {
        if let Some((selected,outcome,why,at))=&attempt {
            let wait=if matches!(outcome.as_str(),"deferred"|"failed"|"progress")
                && matches!(why.as_str(),"workflow-deferred"|"execution-failed"|"workflow-progressed") {
                match capacity {Some(cp)=>super::capacity_retry::command_attempt_wait(tx,workflow,cp,*selected,now)?,None=>None}
            }else{None};
            if let Some(due)=wait {
                kind="blocked".into();reason="resource_capacity".into();action="await_capacity".into();eligible=Some(due);
            } else {
                kind=if outcome=="progress" {"worker-progress"} else {"retry-scheduled"}.into();
                reason=code(why,"worker-attempt-finished");action="inspect_workflow".into();eligible=Some(*at);
                if let Some(blocker)=submission::LinkedPrBlocker::from_code(why) {
                    kind="blocked".into();reason=blocker.reason().into();action=blocker.action().into();
                }
            }
        }
    }
    let control:Option<bool>=tx.query_row("SELECT paused FROM workflow_owner_control WHERE workflow=?1",[workflow],|r|r.get(0)).optional()?;
    if control==Some(true) {kind="blocked".into();reason="owner-pause-requested".into();action="await_owner_resume".into();}
    else if source=="owner_control_changed" {kind="resume-requested".into();reason="owner-resume-accepted".into();action="await_worker".into();}
    let external_merge=checkpoint.as_ref().is_some_and(|c|c.stage==Stage::Merged && c.next.reason=="external_merge_reconciled");
    let terminal=confirmed || external_merge || stage==Some(Stage::Retired);
    if stage==Some(Stage::Retired) {
        kind="retired".into();
        reason=checkpoint.as_ref().map_or_else(||"workflow-state-changed".into(),|c|code(&c.next.reason,"workflow-state-changed"));
        action="complete".into();eligible=None;
    }
    if external_merge {kind="merged".into();reason="external-merge-observed".into();action="complete".into();eligible=None;}
    if confirmed {stage=Some(Stage::Merged);kind="merged".into();reason="autonomous-merge-confirmed".into();action="inspect_deployment".into();eligible=None;}
    let evidence=checkpoint.as_ref().map(|c|(&c.binding,&c.evidence,&c.next.reason,&c.next.action));
    let evidence_digest=format!("{:x}",Sha256::digest(serde_json::to_vec(&evidence)?));
    let actual:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='native_model_admitted' ORDER BY seq DESC LIMIT 1",[workflow],|r|r.get(0)).optional()?;
    let (actual_provider,actual_provider_role)=if let Some(raw)=actual {
        let value:serde_json::Value=serde_json::from_str(&raw)?;
        let provider=match &value["provider"] {
            serde_json::Value::Null=>None,
            serde_json::Value::String(name) if matches!(name.as_str(),"codex"|"claude")=>Some(name.clone()),
            _=>bail!("invalid admitted native provider"),
        };
        let reviewer=value["reviewer"].as_bool().context("native provider role missing")?;
        (provider,Some(if reviewer {"reviewer"}else{"workflow"}.into()))
    }else{(None,None)};
    let resource_refusal = if !terminal && control != Some(true) && reason == "execution-readiness-blocked" {
        super::submission::read_command_attempt(tx, workflow)?.and_then(|attempt| attempt.resource_observation())
    } else { None };
    let resource_identity = resource_refusal.as_ref().map(|value| value.observation.identity());
    let tool_capacity_refusal=if !terminal && control!=Some(true) && reason=="resource_capacity" {
        match checkpoint.as_ref() {Some(cp)=>super::capacity_retry::current_tool_refusal(tx,workflow,cp,now)?,None=>None}
    }else{None};
    let fingerprint_input = if tool_capacity_refusal.is_some() {
        serde_json::to_vec(&(&kind,stage,&reason,&action,&evidence_digest,&actual_provider,&actual_provider_role,&resource_identity,&tool_capacity_refusal))?
    }else if resource_identity.is_some() {
        serde_json::to_vec(&(&kind,stage,&reason,&action,&evidence_digest,&actual_provider,&actual_provider_role,&resource_identity))?
    } else {
        // Preserve the deployed fingerprint for all pre-existing projections.
        serde_json::to_vec(&(&kind,stage,&reason,&action,&evidence_digest,&actual_provider,&actual_provider_role))?
    };
    let fingerprint=format!("{:x}",Sha256::digest(fingerprint_input));
    let changed=previous.as_ref().is_none_or(|p|p.1!=fingerprint);
    let sequence=previous.as_ref().map_or(1,|p|p.0+i64::from(changed));
    let progress=Progress {workflow:workflow.into(),sequence,kind,stage,reason,next_action:action,eligible_at_ms:eligible,
        pr:snapshot.and_then(|s|s.1),requested_provider,actual_provider,actual_provider_role,
        merge_state:if confirmed {"observed-autonomous-merge"} else if external_merge {"observed-external-merge"} else {"not-confirmed"}.into(),deployment_state:"unknown".into(),evidence_digest,observed_at_ms:now,resource_refusal,tool_capacity_refusal};
    let payload=serde_json::to_string(&progress)?;
    if changed {tx.execute("INSERT INTO workflow_progress_event(workflow,sequence,payload,created_at) VALUES(?1,?2,?3,?4)",params![workflow,sequence,payload,now])?;}
    tx.execute("INSERT INTO workflow_progress_state VALUES(?1,?2,?3,?4,?5) ON CONFLICT(workflow) DO UPDATE SET sequence=excluded.sequence,fingerprint=excluded.fingerprint,terminal=excluded.terminal,payload=excluded.payload",params![workflow,sequence,fingerprint,terminal,payload])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(path:&Path)->(WorkflowStore,Lease) {
        let mut store=WorkflowStore::open(path).unwrap();
        store.register("owner/repo#1","fix/one",Binding {head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},1).unwrap();
        let lease=store.claim("owner/repo#1","worker",2,10000).unwrap().unwrap();(store,lease)
    }
    fn queued_capacity(path:&Path)->(WorkflowStore,Lease,super::super::discord_ingress::DiscordOrigin) {
        let mut store=WorkflowStore::open(path).unwrap();
        let origin=super::super::discord_ingress::DiscordOrigin{bot:11,owner:22,channel:33,message:44};
        let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Issue(1),
            provider:Some(submission::Provider::Codex),until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),1).unwrap().admission.workflow;
        assert_eq!(store.next_command_job("owner/repo",10).unwrap().as_deref(),Some(id.as_str()));
        store.register(&id,"fix/one",Binding{head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},11).unwrap();
        let lease=store.claim(&id,"worker",12,60_000).unwrap().unwrap();
        (store,lease,origin)
    }

    #[test]
    fn command_completion_preserves_current_capacity_blocker_for_private_updates() {
        for (outcome,reason) in [(submission::AttemptOutcome::Deferred,"workflow-deferred"),
            (submission::AttemptOutcome::Failed,"execution-failed"),
            (submission::AttemptOutcome::Progress,"workflow-progressed")] {
            let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");
            let (mut store,lease,origin)=queued_capacity(&path);
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.merge_allowance(19,5).unwrap();
            let fingerprint="b".repeat(64);
            let observation=serde_json::json!({"phase":"native_tool_build_storage_reserve","exhausted":true,
                "tool_readiness":{"category":"build_storage_reserve","execution_started":false},"available_bytes":25,"required_bytes":37,"private":"MUST_NOT_ESCAPE"});
            let refusal=super::tool_capacity::ToolCapacityRefusal::capture(&observation).unwrap();
            let due=store.defer_run_capacity_observation(&lease,Some(&fingerprint),Some(&refusal),20).unwrap();
            let projected=store.progress_events(&lease.workflow).unwrap();
            assert!(store.finish_command_attempt(&lease.workflow,10,outcome,reason,21).unwrap());
            let events=store.progress_events(&lease.workflow).unwrap();let last=events.last().unwrap();
            assert_eq!(last.reason,"resource_capacity","command completion hid the actual protected resource refusal");
            assert_eq!(last.kind,"blocked");assert_eq!(last.next_action,"await_capacity");assert_eq!(last.eligible_at_ms,Some(due));
            assert_eq!(events.len(),projected.len(),"unchanged blocker generated a duplicate update");
            assert_eq!(last.requested_provider,Some(submission::Provider::Codex));assert_eq!(last.actual_provider,None);
            assert_eq!(last.merge_state,"not-confirmed");assert_eq!(last.deployment_state,"unknown");
            let after=store.snapshot(&lease.workflow).unwrap().unwrap();
            assert_eq!(after.progress_at_ms,before.progress_at_ms);assert_eq!(after.model_calls,before.model_calls);
            assert_eq!(after.generation,before.generation);assert_eq!(store.merge_allowance(22,5).unwrap(),allowance);
            let sequence=last.sequence;drop(store);
            let mut store=WorkflowStore::open(&path).unwrap();
            assert_eq!(store.progress_events(&lease.workflow).unwrap(),events);
            assert!(store.pending_progress(origin.bot,origin.owner).unwrap().iter().any(|(o,w,s)|o.channel==33 && w==&lease.workflow && *s==sequence));
            let (_,content)=store.claim_progress(&origin,&lease.workflow,sequence,22).unwrap().unwrap();
            assert!(content.contains("resource_capacity") && content.contains("await_capacity"));
            assert!(!content.contains(&fingerprint),"private native fingerprint escaped into a message");
            assert!(content.contains("build_storage_reserve") && content.contains("available 25 bytes, required 37 bytes"));
            assert!(!content.contains("MUST_NOT_ESCAPE"));
        }
    }

    #[test]
    fn command_completion_does_not_reuse_an_earlier_attempt_capacity_refusal() {
        let root=tempfile::tempdir().unwrap();let (mut store,lease,_)=queued_capacity(&root.path().join("state.db"));
        let due=store.defer_run_capacity(&lease,None,20).unwrap();
        store.finish_command_attempt(&lease.workflow,10,submission::AttemptOutcome::Deferred,"workflow-deferred",21).unwrap();
        store.release(&lease,22).unwrap();
        assert!(store.next_command_job("owner/repo",due).unwrap().is_none(),"normal work retains its deliberate fairness turn");
        assert_eq!(store.next_command_job("owner/repo",due).unwrap().as_deref(),Some(lease.workflow.as_str()));
        store.finish_command_attempt(&lease.workflow,due,submission::AttemptOutcome::Failed,"execution-failed",due+1).unwrap();
        let last=store.progress_events(&lease.workflow).unwrap().pop().unwrap();
        assert_eq!(last.reason,"execution-failed");assert_eq!(last.kind,"retry-scheduled");
    }

    #[test]
    fn capacity_progress_preserves_pause_authorization_and_changed_context_priority() {
        for case in ["pause","authorization","readiness","binding","generation","missing-retry"] {
            let root=tempfile::tempdir().unwrap();let (mut store,lease,_)=queued_capacity(&root.path().join("state.db"));
            let due=store.defer_run_capacity(&lease,None,20).unwrap();
            let mut completed_at=22;
            let mut outcome=submission::AttemptOutcome::Failed;let mut reason="execution-failed";
            let expected=match case {
                "pause"=>{store.set_owner_pause(&lease.workflow,"owner",0,true,21).unwrap();"owner-pause-requested"},
                "authorization"=>{outcome=submission::AttemptOutcome::Rejected;reason="target-authorization-failed";reason},
                "readiness"=>{outcome=submission::AttemptOutcome::Deferred;reason="execution-readiness-blocked";reason},
                "binding"=>{let mut cp=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;cp.binding.head="changed-head".into();store.checkpoint(&lease,&cp,21).unwrap();reason},
                "generation"=>{store.release(&lease,21).unwrap();assert!(store.claim(&lease.workflow,"successor",due,60_000).unwrap().is_some());completed_at=due+1;reason},
                _=>{store.conn.execute("DELETE FROM workflow_event WHERE kind='resource_capacity_retry'",[]).unwrap();reason},
            };
            store.finish_command_attempt(&lease.workflow,10,outcome,reason,completed_at).unwrap();
            let last=store.progress_events(&lease.workflow).unwrap().pop().unwrap();
            assert_eq!(last.reason,expected,"{case}: stale capacity metadata hid the newer outcome");
            if case=="pause" {assert!(store.owner_control(&lease.workflow).unwrap().unwrap().paused);assert_eq!(last.next_action,"await_owner_resume");}
        }
    }

    #[test]
    fn capacity_update_projection_failure_does_not_commit_command_completion() {
        let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");let (mut store,lease,_)=queued_capacity(&path);
        store.defer_run_capacity(&lease,None,20).unwrap();
        let attempt=serde_json::to_value(store.command_attempt(&lease.workflow).unwrap()).unwrap();let events=store.progress_events(&lease.workflow).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_capacity_projection BEFORE UPDATE ON workflow_progress_state BEGIN SELECT RAISE(ABORT,'fixture unavailable'); END;").unwrap();
        assert!(store.finish_command_attempt(&lease.workflow,10,submission::AttemptOutcome::Deferred,"workflow-deferred",21).is_err());
        assert_eq!(serde_json::to_value(store.command_attempt(&lease.workflow).unwrap()).unwrap(),attempt);assert_eq!(store.progress_events(&lease.workflow).unwrap(),events);
        store.conn.execute_batch("DROP TRIGGER deny_capacity_projection").unwrap();drop(store);
        let store=WorkflowStore::open(&path).unwrap();
        assert!(store.finish_command_attempt(&lease.workflow,10,submission::AttemptOutcome::Deferred,"workflow-deferred",22).unwrap());
        assert_eq!(store.progress_events(&lease.workflow).unwrap().last().unwrap().reason,"resource_capacity");
    }
    #[test]
    fn progress_is_ordered_durable_and_deduplicates_unchanged_blockers() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let (mut store,lease)=setup(&path);
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next=NextAction {reason:"ci_pending".into(),action:"await_checks".into(),eligible_at_ms:500};
        store.checkpoint(&lease,&checkpoint,3).unwrap();
        let first=store.progress_events(&lease.workflow).unwrap();
        assert!(first.iter().any(|e|e.kind=="blocked"),"committed checkpoint has no durable progress event");
        checkpoint.next.eligible_at_ms=600;store.checkpoint(&lease,&checkpoint,4).unwrap();
        assert_eq!(store.progress_events(&lease.workflow).unwrap().len(),first.len());
        checkpoint.binding.evidence_hash="dependency-b".into();store.checkpoint(&lease,&checkpoint,5).unwrap();
        let events=store.progress_events(&lease.workflow).unwrap();assert_eq!(events.len(),first.len()+1);
        assert!(events.windows(2).all(|e|e[1].sequence==e[0].sequence+1));drop(store);
        assert_eq!(WorkflowStore::open(&path).unwrap().progress_events(&lease.workflow).unwrap(),events);
    }
    #[test]
    fn initial_verifier_holds_remain_precise_after_command_attempt_finishes() {
        for (reason,action) in [("readiness_assessment_invalid","await_readiness_input_change"),
            ("acceptance_verifier_amendment_declined","await_acceptance_input_change"),
            ("acceptance_verifier_repair_exhausted","await_acceptance_input_change"),
            ("acceptance_verifier_evidence_unknown","await_original_evidence_recovery"),
            ("acceptance_contract_invalid","await_acceptance_input_change"),
            ("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery")] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let mut store=WorkflowStore::open(&path).unwrap();
            let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(1),
                provider:None,until:submission::Milestone::Merged};
            let id=store.submit_authorized("request","owner",&request,1).unwrap().workflow;
            store.next_command_job("owner/repo",2).unwrap().unwrap();
            store.register(&id,"fix/one",Binding {head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},3).unwrap();
            let lease=store.claim(&id,"worker",4,10000).unwrap().unwrap();
            let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
            checkpoint.stage=Stage::Scope;checkpoint.next=NextAction {reason:reason.into(),action:action.into(),eligible_at_ms:0};
            store.checkpoint(&lease,&checkpoint,5).unwrap();
            let before=store.progress_events(&id).unwrap();
            store.finish_command_attempt(&id,2,submission::AttemptOutcome::Deferred,"workflow-held",6).unwrap();
            let events=store.progress_events(&id).unwrap();
            assert_eq!(events,before,"generic attempt finish must not replace or duplicate the precise hold");
            let latest=events.last().unwrap();
            assert_eq!(latest.kind,"blocked");assert_eq!(latest.reason,reason);assert_eq!(latest.next_action,action);
            assert_eq!(latest.merge_state,"not-confirmed");
            assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);
            assert_eq!(WorkflowStore::open(&path).unwrap().progress_events(&id).unwrap(),events);
        }
    }

    #[test]
    fn review_hold_survives_generic_attempt_completion_without_private_details() {
        for reason in ["review-recovery-required", "review-evidence-budget", "reviewer-latched"] {
          for (outcome, attempt_reason) in [(submission::AttemptOutcome::Progress,"workflow-progressed"),
              (submission::AttemptOutcome::Deferred,"workflow-held")] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let mut store=WorkflowStore::open(&path).unwrap();
            let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),
                provider:None,until:submission::Milestone::Merged};
            let id=store.submit_authorized("request","owner",&request,1).unwrap().workflow;
            store.next_command_job("owner/repo",2).unwrap().unwrap();
            store.register(&id,"fix/one",Binding {head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},3).unwrap();
            let lease=store.claim(&id,"worker",4,10000).unwrap().unwrap();
            let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
            checkpoint.stage=Stage::Held;checkpoint.next=NextAction {reason:reason.into(),action:"resume_preserved_work".into(),eligible_at_ms:500};
            checkpoint.evidence=vec!["private provider response must not be published".into()];
            store.checkpoint(&lease,&checkpoint,5).unwrap();
            let before=store.progress_events(&id).unwrap();
            store.finish_command_attempt(&id,2,outcome,attempt_reason,6).unwrap();
            let after=store.progress_events(&id).unwrap();
            let latest=after.last().unwrap();
            assert_eq!(latest.kind,"blocked","attempt completion hid the hold");
            assert_eq!(latest.reason,reason);
            assert_eq!(latest.next_action,"resume_preserved_work");
            assert_eq!(latest.eligible_at_ms,Some(500));
            assert_eq!(after,before,"generic attempt outcome must not add a misleading update");
            assert_eq!(latest.actual_provider,None);
            assert_eq!(latest.merge_state,"not-confirmed");
            assert!(!serde_json::to_string(&after).unwrap().contains("private provider"));
            assert_eq!(WorkflowStore::open(&path).unwrap().progress_events(&id).unwrap(),after);
          }
        }
    }

    #[test]
    fn distinct_current_execution_failure_is_not_hidden_by_an_older_review_hold() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_authorized("request","owner",&request,1).unwrap().workflow;
        store.next_command_job("owner/repo",2).unwrap().unwrap();
        store.register(&id,"fix/one",Binding {head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},3).unwrap();
        let lease=store.claim(&id,"worker",4,10000).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next=NextAction {reason:"review-recovery-required".into(),action:"resume_preserved_work".into(),eligible_at_ms:500};
        store.checkpoint(&lease,&checkpoint,5).unwrap();
        store.finish_command_attempt(&id,2,submission::AttemptOutcome::Deferred,"execution-readiness-blocked",600).unwrap();
        let latest=store.progress_events(&id).unwrap().pop().unwrap();
        assert_eq!(latest.kind,"retry-scheduled");assert_eq!(latest.reason,"execution-readiness-blocked");
        assert_eq!(latest.eligible_at_ms,Some(store.command_attempt(&id).unwrap().unwrap().eligible_at_ms));
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint,checkpoint);
    }

    #[test]
    fn generic_completion_preserves_main_dependency_backoff() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_authorized("request","owner",&request,1).unwrap().workflow;
        store.next_command_job("owner/repo",2).unwrap().unwrap();
        store.register(&id,"fix/one",Binding {head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},3).unwrap();
        let lease=store.claim(&id,"worker",4,10000).unwrap().unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Reconcile;checkpoint.next=NextAction {reason:"review_main_changed".into(),action:"reconcile_main".into(),eligible_at_ms:1000};
        store.checkpoint(&lease,&checkpoint,5).unwrap();let before=store.progress_events(&id).unwrap();
        store.finish_command_attempt(&id,2,submission::AttemptOutcome::Progress,"workflow-progressed",600).unwrap();
        let after=store.progress_events(&id).unwrap();assert_eq!(after,before);
        let latest=after.last().unwrap();assert_eq!(latest.reason,"review_main_changed");
        assert_eq!(latest.next_action,"reconcile_main");assert_eq!(latest.eligible_at_ms,Some(1000));
        assert_eq!(latest.merge_state,"not-confirmed");
    }

    #[test]
    fn projection_failure_rolls_back_checkpoint_and_audit() {
        let dir=tempfile::tempdir().unwrap();let (mut store,lease)=setup(&dir.path().join("state.db"));
        let before=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        let events=store.events(&lease.workflow).unwrap();let progress=store.progress_events(&lease.workflow).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_progress BEFORE INSERT ON workflow_progress_event BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        let mut checkpoint=before.clone();checkpoint.stage=Stage::Build;
        assert!(store.checkpoint(&lease,&checkpoint,3).is_err());
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,before);
        assert_eq!(store.events(&lease.workflow).unwrap(),events);
        assert_eq!(store.progress_events(&lease.workflow).unwrap(),progress);
    }
    #[test]
    fn queued_work_retains_progress_without_a_destination_and_control_replays_dedupe() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:Some(submission::Provider::Codex),until:submission::Milestone::Merged};
        let admitted=store.submit_authorized("request","owner",&request,1).unwrap();
        store.submit_authorized("request","owner",&request,2).unwrap();
        let events=store.progress_events(&admitted.workflow).unwrap();assert_eq!(events.len(),1);assert_eq!(events[0].kind,"queued");
        assert_eq!(events[0].requested_provider,Some(submission::Provider::Codex));assert_eq!(events[0].actual_provider,None);
        assert_eq!(store.progress_delivery(&admitted.workflow).unwrap().state,"delivery-unconfigured");
        store.set_owner_pause(&admitted.workflow,"owner",0,true,3).unwrap();
        store.set_owner_pause(&admitted.workflow,"owner",1,true,4).unwrap();
        assert_eq!(store.progress_events(&admitted.workflow).unwrap().len(),2);
        store.set_owner_pause(&admitted.workflow,"owner",2,false,5).unwrap();
        assert_eq!(store.progress_events(&admitted.workflow).unwrap().last().unwrap().kind,"resume-requested");
        let origin=super::super::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        store.submit_discord_authorized(&origin,&request,&"a".repeat(64),6).unwrap();
        assert_eq!(store.progress_delivery(&admitted.workflow).unwrap().state,"delivery-pending");
    }
    #[test]
    fn private_prose_is_digest_only_and_terminal_status_ignores_late_work() {
        let dir=tempfile::tempdir().unwrap();let (mut store,lease)=setup(&dir.path().join("state.db"));
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Retired;checkpoint.next.reason="PRIVATE provider prompt and credentials".into();checkpoint.next.action="PRIVATE /home/owner/private-file".into();checkpoint.evidence=vec!["PRIVATE receipt contents".into()];
        store.checkpoint(&lease,&checkpoint,3).unwrap();
        let events=store.progress_events(&lease.workflow).unwrap();assert!(!serde_json::to_string(&events).unwrap().contains("PRIVATE"));
        checkpoint.stage=Stage::Review;store.checkpoint(&lease,&checkpoint,4).unwrap();
        assert_eq!(store.progress_events(&lease.workflow).unwrap(),events);
    }

    #[test]
    fn progress_survives_process_exit_after_checkpoint_commit() {
        const ENV:&str="AUTOSHIP_PROGRESS_CRASH_PATH";
        if let Some(path)=std::env::var_os(ENV) {
            let (mut store,lease)=setup(Path::new(&path));
            let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            checkpoint.stage=Stage::Review;store.checkpoint(&lease,&checkpoint,3).unwrap();
            std::process::exit(73);
        }
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","autoship_workflow::progress::tests::progress_survives_process_exit_after_checkpoint_commit"]).env(ENV,&path).output().unwrap();
        assert_eq!(output.status.code(),Some(73));
        let store=WorkflowStore::open(&path).unwrap();let events=store.progress_events("owner/repo#1").unwrap();
        assert_eq!(events.last().unwrap().stage,Some(Stage::Review));
        assert_eq!(store.progress_delivery("owner/repo#1").unwrap().pending_events,events.len() as i64);
    }

    #[test]
    fn unrecognized_machine_shaped_private_values_never_escape_projection() {
        let dir=tempfile::tempdir().unwrap();let (mut store,lease)=setup(&dir.path().join("state.db"));
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next.reason="private_lowercase_credential".into();checkpoint.next.action="private_lowercase_path".into();
        store.checkpoint(&lease,&checkpoint,3).unwrap();
        let event=store.progress_events(&lease.workflow).unwrap().pop().unwrap();
        assert_eq!(event.reason,"workflow-state-changed");assert_eq!(event.next_action,"inspect_workflow");
        assert!(!serde_json::to_string(&event).unwrap().contains("private_lowercase"));
    }

    #[test]
    fn clearing_pause_reports_intent_and_preserves_another_hold() {
        let dir=tempfile::tempdir().unwrap();let (mut store,lease)=setup(&dir.path().join("state.db"));
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage=Stage::Held;checkpoint.next.reason="ci_failed".into();store.checkpoint(&lease,&checkpoint,3).unwrap();
        store.set_owner_pause(&lease.workflow,"owner",0,true,4).unwrap();
        store.set_owner_pause(&lease.workflow,"owner",1,false,5).unwrap();
        let event=store.progress_events(&lease.workflow).unwrap().pop().unwrap();
        assert_eq!(event.kind,"resume-requested");assert_eq!(event.stage,Some(Stage::Held));
        assert_eq!(event.reason,"owner-resume-accepted");assert_eq!(event.actual_provider,None);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint,checkpoint);
    }

    #[test]
    fn bound_progress_is_claimed_once_and_unknown_send_blocks_later_updates() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
        let origin=super::super::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),1).unwrap().admission.workflow;
        assert!(store.claim_progress(&origin,&id,1,2).unwrap().is_some(),"bound progress never reaches delivery claim");
        drop(store);let mut store=WorkflowStore::open(&path).unwrap();
        assert!(store.claim_progress(&origin,&id,1,3).unwrap().is_none());
        store.set_owner_pause(&id,"owner",0,true,4).unwrap();
        assert!(store.claim_progress(&origin,&id,2,5).unwrap().is_none(),"unknown older send must reconcile before another update");
    }

    #[test]
    fn receipt_is_bound_and_confirmation_compacts_history_without_erasing_events() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let origin=super::super::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),1).unwrap().admission.workflow;
        store.set_owner_pause(&id,"owner",0,true,2).unwrap();
        assert!(store.claim_progress(&origin,&id,1,3).unwrap().is_none(),"stale selection cannot send history");
        let mut foreign=origin.clone();foreign.channel=99;
        assert!(store.claim_progress(&foreign,&id,2,4).is_err());
        assert!(store.claim_progress(&origin,&id,2,4).unwrap().unwrap().1.contains("owner-pause-requested"));
        assert_eq!(store.progress_delivery(&id).unwrap().state,"delivery-uncertain");
        assert!(store.uncertain_progress(11,99).unwrap().is_empty());
        let uncertain=store.uncertain_progress(11,22).unwrap();assert_eq!(uncertain.len(),1);assert_eq!(uncertain[0].0.channel,33);assert_eq!(uncertain[0].1,id);
        assert!(store.confirm_progress(&foreign,&id,2,55,5).is_err());
        store.confirm_progress(&origin,&id,2,55,5).unwrap();store.confirm_progress(&origin,&id,2,55,6).unwrap();
        assert!(store.confirm_progress(&origin,&id,2,56,7).is_err());
        assert!(store.pending_progress(11,22).unwrap().is_empty());
        assert_eq!(store.progress_delivery(&id).unwrap().pending_events,0);
        assert_eq!(store.progress_events(&id).unwrap().len(),2);
    }

    #[test]
    fn linked_pr_blockers_survive_command_finish_without_false_progress() {
        for (reason,action) in [("linked_pr_claim_exists","reconcile_existing_pr"),
            ("linked_pr_inventory_unknown","retry_github_inventory")] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let mut store=WorkflowStore::open(&path).unwrap();
            let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(1),provider:None,until:submission::Milestone::Merged};
            let id=store.submit_authorized("request","owner",&request,1).unwrap().workflow;
            store.next_command_job("owner/repo",2).unwrap().unwrap();
            store.register(&id,"fix/one",Binding {head:"head-a".into(),contract:"contract-a".into(),evidence_hash:"dependency-a".into()},3).unwrap();
            let lease=store.claim(&id,"worker",4,10000).unwrap().unwrap();
            let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
            checkpoint.stage=Stage::Build;checkpoint.next=NextAction {reason:reason.into(),action:action.into(),eligible_at_ms:300005};
            store.checkpoint(&lease,&checkpoint,5).unwrap();
            let before=store.progress_events(&id).unwrap();
            assert!(store.finish_command_attempt(&id,2,submission::AttemptOutcome::Deferred,"workflow-held",6).unwrap());
            let events=store.progress_events(&id).unwrap();assert_eq!(events,before);
            let latest=events.last().unwrap();assert_eq!(latest.kind,"blocked");
            assert_eq!(latest.reason,reason);assert_eq!(latest.next_action,action);
            assert_eq!(latest.merge_state,"not-confirmed");
            assert_eq!(WorkflowStore::open(&path).unwrap().progress_events(&id).unwrap(),events);
            store.set_owner_pause(&id,"owner",0,true,7).unwrap();
            assert_eq!(store.progress_events(&id).unwrap().last().unwrap().reason,"owner-pause-requested");
        }
    }

    #[test]
    fn linked_pr_precheckpoint_attempts_project_precise_cli_and_private_blockers() {
        for (attempt_reason,reason,action) in [
            ("linked-pr-claim-exists","linked_pr_claim_exists","reconcile_existing_pr"),
            ("target-pr-reconciliation-required","linked_pr_claim_exists","reconcile_existing_pr"),
            ("linked-pr-inventory-unknown","linked_pr_inventory_unknown","retry_github_inventory")] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let mut store=WorkflowStore::open(&path).unwrap();
            let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Issue(1),provider:None,until:submission::Milestone::Merged};
            let id=store.submit_authorized("request","owner",&request,1).unwrap().workflow;
            store.next_command_job("owner/repo",2).unwrap().unwrap();
            assert!(store.finish_command_attempt(&id,2,submission::AttemptOutcome::Deferred,attempt_reason,3).unwrap());
            assert!(store.snapshot(&id).unwrap().is_none(),"a status blocker must not manufacture execution state");
            let latest=store.progress_events(&id).unwrap().pop().unwrap();
            assert_eq!(latest.kind,"blocked");assert_eq!(latest.reason,reason);assert_eq!(latest.next_action,action);
            let (exit,body)=crate::autoship_commands::status(&path,&id,4,5);
            assert_eq!(exit,3);assert_eq!(body["stage"],"queued");
            assert_eq!(body["reason"],reason);assert_eq!(body["next_action"],action);
            assert_eq!(body["worker"]["state"],"unknown");assert_eq!(body["merge"]["state"],"not-confirmed");
            store.set_owner_pause(&id,"owner",0,true,5).unwrap();
            let (_,paused)=crate::autoship_commands::status(&path,&id,6,5);
            assert_eq!(paused["reason"],"owner-pause-requested");assert_eq!(paused["next_action"],"await_owner_resume");
            assert_eq!(store.progress_events(&id).unwrap().last().unwrap().reason,"owner-pause-requested");
        }
    }

}
