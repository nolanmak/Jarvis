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
    pub merge_state:String,
    pub deployment_state:String,
    pub evidence_digest:String,
    pub observed_at_ms:i64,
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
    pub fn latest_progress(&self,workflow:&str)->Result<Option<Progress>> {
        if !exists(&self.conn)? {return Ok(None);}
        let raw:Option<String>=self.conn.query_row("SELECT payload FROM workflow_progress_state WHERE workflow=?1",
            [workflow],|row|row.get(0)).optional()?;
        raw.map(|raw|Ok(serde_json::from_str(&raw)?)).transpose()
    }

    pub fn progress_events(&self,workflow:&str)->Result<Vec<Progress>> {
        if !exists(&self.conn)? {return Ok(Vec::new());}
        let mut stmt=self.conn.prepare("SELECT payload FROM workflow_progress_event WHERE workflow=?1 ORDER BY sequence")?;
        let rows=stmt.query_map([workflow],|r|r.get::<_,String>(0))?;
        rows.map(|row|Ok(serde_json::from_str(&row?)?)).collect()
    }
    pub fn uncertain_progress(&self,bot:u64,owner:u64)->Result<Vec<(super::discord_ingress::DiscordOrigin,String,i64,String,String)>> {
        ensure!(bot>0 && owner>0,"progress recovery identity missing");
        let mut stmt=self.conn.prepare("SELECT d.channel,d.workflow,d.sequence,d.nonce,d.content FROM workflow_progress_delivery d JOIN discord_workflow_binding b ON b.workflow=d.workflow AND b.bot=d.bot AND b.owner=d.owner AND b.channel=d.channel WHERE d.bot=?1 AND d.owner=?2 AND d.state='uncertain' ORDER BY d.dispatched_at,d.workflow LIMIT 128")?;
        let rows=stmt.query_map(params![bot.to_string(),owner.to_string()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?;
        rows.map(|row| {let (channel,workflow,sequence,nonce,content)=row?;Ok((super::discord_ingress::DiscordOrigin {bot,owner,channel:channel.parse()?,message:1},workflow,sequence,nonce,content))}).collect()
    }
    pub fn pending_progress(&self,bot:u64,owner:u64)->Result<Vec<(super::discord_ingress::DiscordOrigin,String,i64)>> {
        ensure!(bot>0 && owner>0,"progress recovery identity missing");
        if !exists(&self.conn)? {return Ok(Vec::new());}
        let mut stmt=self.conn.prepare("SELECT b.channel,b.workflow,s.sequence FROM discord_workflow_binding b JOIN workflow_progress_state s ON s.workflow=b.workflow WHERE b.bot=?1 AND b.owner=?2 AND NOT EXISTS(SELECT 1 FROM workflow_progress_delivery d WHERE d.workflow=b.workflow AND d.bot=b.bot AND d.owner=b.owner AND d.channel=b.channel AND (d.state='uncertain' OR d.sequence>=s.sequence)) ORDER BY s.workflow,b.channel LIMIT 128")?;
        let rows=stmt.query_map(params![bot.to_string(),owner.to_string()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))?;
        rows.map(|row| {let (channel,workflow,sequence)=row?;Ok((super::discord_ingress::DiscordOrigin {bot,owner,channel:channel.parse()?,message:1},workflow,sequence))}).collect()
    }
    pub fn claim_progress(&mut self,origin:&super::discord_ingress::DiscordOrigin,workflow:&str,sequence:i64,now:i64)->Result<Option<(String,String)>> {
        origin.validate()?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::discord_ingress::bound_workflow(&tx,origin,Some(workflow))?;
        let raw:Option<(i64,String)>=tx.query_row("SELECT sequence,payload FROM workflow_progress_state WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((current,raw))=raw else {return Ok(None);};
        if current!=sequence {return Ok(None);}
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_progress_delivery WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND (state='uncertain' OR sequence>=?5))",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence],|r|r.get(0))?;
        if blocked {return Ok(None);}
        let progress:Progress=serde_json::from_str(&raw)?;
        let nonce=format!("{:x}",Sha256::digest(serde_json::to_vec(&(workflow,origin.bot,origin.owner,origin.channel,sequence))?))[..24].to_string();
        let content=format!("Autoship {workflow} · update {sequence}: {}. Stage: {:?}. Reason: {}. Next: {} (eligible at: {:?}). PR: {:?}. Requested provider: {:?}; actual provider: {}. Merge: {}. Deployment: {}. Evidence digest: {}. [autoship:{nonce}]",
            progress.kind,progress.stage,progress.reason,progress.next_action,progress.eligible_at_ms,progress.pr,progress.requested_provider,progress.actual_provider.as_deref().unwrap_or("unknown"),progress.merge_state,progress.deployment_state,progress.evidence_digest);
        ensure!(content.chars().count()<=1900,"progress message exceeds Discord bound");
        tx.execute("INSERT INTO workflow_progress_delivery(workflow,bot,owner,channel,sequence,nonce,content,state,dispatched_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'uncertain',?8)",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence,nonce,content,now])?;
        tx.commit()?;Ok(Some((nonce,content)))
    }
    pub fn confirm_progress(&mut self,origin:&super::discord_ingress::DiscordOrigin,workflow:&str,sequence:i64,message:u64,now:i64)->Result<()> {
        origin.validate()?;ensure!(message>0,"progress remote message missing");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::discord_ingress::bound_workflow(&tx,origin,Some(workflow))?;
        let (state,prior):(String,Option<String>)=tx.query_row("SELECT state,message FROM workflow_progress_delivery WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if state=="confirmed" {ensure!(prior.as_deref()==Some(message.to_string().as_str()),"progress receipt conflict");return Ok(());}
        ensure!(state=="uncertain","progress was never dispatched");
        tx.execute("UPDATE workflow_progress_delivery SET state='confirmed',message=?6,confirmed_at=?7 WHERE workflow=?1 AND bot=?2 AND owner=?3 AND channel=?4 AND sequence=?5",params![workflow,origin.bot.to_string(),origin.owner.to_string(),origin.channel.to_string(),sequence,message.to_string(),now])?;
        tx.commit()?;Ok(())
    }
    pub fn progress_delivery(&self,workflow:&str)->Result<DeliveryStatus> {
        if !exists(&self.conn)? {return Ok(DeliveryStatus {state:"not-observed".into(),pending_events:0,oldest_pending_at_ms:None,latest_sequence:None});}
        let delivery_table:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_progress_delivery')",[],|r|r.get(0))?;
        let query=if delivery_table {"SELECT count(*),min(e.created_at),max(e.sequence) FROM workflow_progress_event e WHERE e.workflow=?1 AND (NOT EXISTS(SELECT 1 FROM discord_workflow_binding b WHERE b.workflow=e.workflow) OR EXISTS(SELECT 1 FROM discord_workflow_binding b WHERE b.workflow=e.workflow AND NOT EXISTS(SELECT 1 FROM workflow_progress_delivery d WHERE d.workflow=b.workflow AND d.bot=b.bot AND d.owner=b.owner AND d.channel=b.channel AND d.state='confirmed' AND d.sequence>=e.sequence)))"}
            else {"SELECT count(*),min(created_at),max(sequence) FROM workflow_progress_event WHERE workflow=?1"};
        let (pending,oldest,latest):(i64,Option<i64>,Option<i64>)=self.conn.query_row(query,[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let bound:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM discord_workflow_binding WHERE workflow=?1)",[workflow],|r|r.get(0))?;
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
    if matches!(value,
        "registered"|"scope"|"ci_pending"|"await_checks"|"await_capacity"|"complete"|
        "reverify"|"reverify_candidate"|"inputs_changed"|"protected_acceptance_stale"|
        "remote_merge_observed"|"resource_capacity"|"checks_pending"|"review_pending"|
        "build"|"verify"|"review"|"reconcile"|"publish"|"merge"|"retry"|
        "execution-readiness-blocked"|"target-authorization-failed"|"execution-failed"|
        "workflow-progressed"|"owner-pause-requested"|"owner-resume-accepted"|
        "await_owner_resume"|"claim_workflow"|"awaiting-worker-claim"|
        "merge-allowance-exhausted"|"merge-reconciliation-required"|
        "review_changes_requested"|"ci_failed"|"merge_conflict"|"head_changed"|
        "retry_worker_attempt"|"inspect_workflow"|"await_worker"|
        "verification_infrastructure_retry"|"retry_verification")
        || crate::autoship_freshness::is_progress_code(value) {value.into()} else {fallback.into()}
}

pub(super) fn project(tx:&Transaction<'_>,workflow:&str,source:&str,now:i64)->Result<()> {
    if !matches!(source,"registered"|"checkpoint"|"verification_infrastructure_retry"|"autonomous_merge_confirmed"|"command_queued"|"command_attempt_finished"|"owner_control_changed") {return Ok(());}
    let previous:Option<(i64,String,bool)>=tx.query_row("SELECT sequence,fingerprint,terminal FROM workflow_progress_state WHERE workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if previous.as_ref().is_some_and(|p|p.2) {return Ok(());}
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
    let ci_retry=checkpoint.as_ref().is_some_and(|c|crate::autoship_freshness::is_ci_retry(&c.next));
    if ci_retry {kind="retry-scheduled".into();}
    let attempt:Option<(String,String,i64)>=tx.query_row("SELECT outcome,reason,eligible_at FROM command_attempt WHERE workflow=?1 AND finished_at IS NOT NULL",[workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if source=="command_attempt_finished" && !ci_retry {
        if let Some((outcome,why,at))=&attempt {
            kind=if outcome=="progress" {"worker-progress"} else {"retry-scheduled"}.into();
            reason=code(why,"worker-attempt-finished");action="inspect_workflow".into();eligible=Some(*at);
        }
    }
    let control:Option<bool>=tx.query_row("SELECT paused FROM workflow_owner_control WHERE workflow=?1",[workflow],|r|r.get(0)).optional()?;
    if control==Some(true) {kind="blocked".into();reason="owner-pause-requested".into();action="await_owner_resume".into();}
    else if source=="owner_control_changed" {kind="resume-requested".into();reason="owner-resume-accepted".into();action="await_worker".into();}
    let terminal=confirmed || stage==Some(Stage::Retired);
    if confirmed {stage=Some(Stage::Merged);kind="merged".into();reason="autonomous-merge-confirmed".into();action="inspect_deployment".into();eligible=None;}
    let evidence=checkpoint.as_ref().map(|c|(&c.binding,&c.evidence,&c.next.reason,&c.next.action));
    let mut evidence_digest=format!("{:x}",Sha256::digest(serde_json::to_vec(&evidence)?));
    if let Some(checkpoint)=checkpoint.as_ref().filter(|c|crate::autoship_freshness::reason_from_retry(&c.next.reason).is_some()) {
        let raw:Option<String>=tx.query_row("SELECT e.payload FROM workflow_event e JOIN workflow w ON w.id=e.workflow WHERE e.workflow=?1 AND e.kind='verification_infrastructure_retry' AND e.generation=w.generation ORDER BY e.seq DESC LIMIT 1",[workflow],|row|row.get(0)).optional()?;
        if let Some(retry)=raw.and_then(|raw|serde_json::from_str::<serde_json::Value>(&raw).ok()) {
            if retry["failure_code"]==checkpoint.next.reason && retry["eligible_at_ms"].as_i64()==Some(checkpoint.next.eligible_at_ms)
                && retry["binding"]==serde_json::to_value(&checkpoint.binding)? {
                if let Some(digest)=retry["dependency_sha256"].as_str().filter(|value|value.len()==64 && value.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f'))) {
                    evidence_digest=digest.into();
                }
            }
        }
    }
    // A changed CI retry deadline is useful owner information. Ordinary
    // checkpoint timestamps still deduplicate unchanged blocker updates.
    let retry_deadline=if ci_retry {eligible} else {None};
    let fingerprint=format!("{:x}",Sha256::digest(serde_json::to_vec(&(&kind,stage,&reason,&action,retry_deadline,&evidence_digest))?));
    let changed=previous.as_ref().is_none_or(|p|p.1!=fingerprint);
    let sequence=previous.as_ref().map_or(1,|p|p.0+i64::from(changed));
    let progress=Progress {workflow:workflow.into(),sequence,kind,stage,reason,next_action:action,eligible_at_ms:eligible,
        pr:snapshot.and_then(|s|s.1),requested_provider,actual_provider:None,
        merge_state:if confirmed {"observed-autonomous-merge"} else {"not-confirmed"}.into(),deployment_state:"unknown".into(),evidence_digest,observed_at_ms:now};
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

}
