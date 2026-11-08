//! Actual private publication -> explicit yield -> shared metadata recovery.
//! Repository authorization is stubbed; no provider/VM/body is executed.
use super::*;
use std::os::fd::AsRawFd;

pub(super) async fn check(pending:OwnerPublicationGuard<'_>,state:&Path,candidate:&Path,
    canonical:&Path,id:&str,paused:bool,
) {
    let mut store=WorkflowStore::open(state).unwrap();let db=rusqlite::Connection::open(state).unwrap();
    let old_lease=pending._guard.lease.clone();let audit=pending._guard.audit.clone();
    let facts=pending.native_facts().unwrap();
    let initial_plan=store.owner_publication_recovery_plan(id).unwrap().unwrap();
    let origin=initial_plan.origin().clone();
    store.configure_delivery("owner/repo","github:owner",durable::delivery::Destination {bot:origin.bot,owner:99,channel:88},millis()).unwrap();
    if paused {
        let mut later=origin.clone();later.message+=100;
        store.enqueue_owner_message(&later,id,"Retain this later separate instruction",None,millis()).unwrap();
        let revision=store.owner_control(id).unwrap().unwrap().revision;
        store.set_owner_pause(id,"fixture-owner",revision,true,millis()).unwrap();
    }
    let before=store.snapshot(id).unwrap().unwrap();let control=store.owner_control(id).unwrap();
    let queued=store.pending_owner_messages(id).unwrap();let events=store.events(id).unwrap();
    let progress=store.progress_events(id).unwrap();let canonical_bytes=std::fs::read(canonical).unwrap();
    let native_audit=std::fs::read(audit.path()).unwrap();
    let intent:String=db.query_row("SELECT record FROM workflow_owner_publication_intent WHERE workflow=?1",[id],|r|r.get(0)).unwrap();
    let request:String=db.query_row("SELECT request_id FROM workflow_owner_publication_intent WHERE workflow=?1",[id],|r|r.get(0)).unwrap();
    let preparation:String=db.query_row("SELECT record FROM workflow_owner_preparation WHERE request_id=?1",[&request],|r|r.get(0)).unwrap();

    // Ending a metadata lease declares no native closure: it needs no native
    // lock and cannot release the lock another current observer actually holds.
    let binding=serde_json::to_value(facts.original_binding()).unwrap();
    let native=Path::new(binding["canonical_state_root"].as_str().unwrap());
    let lock=std::fs::OpenOptions::new().read(true).open(native.join("state.lock")).unwrap();
    assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
    for (target,definition) in [
        ("event","BEFORE INSERT ON workflow_event WHEN NEW.kind='owner_publication_yielded'"),
        ("release","BEFORE UPDATE OF owner ON workflow WHEN NEW.owner IS NULL"),
        ("progress","BEFORE INSERT ON workflow_progress_event"),
    ] {
        if paused && target=="progress" {continue;}
        db.execute_batch(&format!("CREATE TRIGGER refuse_publication_yield {definition} BEGIN SELECT RAISE(ABORT,'injected publication yield rollback'); END;")).unwrap();
        assert!(store.yield_completed_owner_publication(&pending._metadata,&pending._publication,millis()).is_err(),"{target}: fault did not reach actual writer");
        assert_eq!(store.snapshot(id).unwrap().unwrap(),before,"{target}: partial lease release");
        assert_eq!(store.events(id).unwrap(),events,"{target}: partial event append");
        assert_eq!(store.progress_events(id).unwrap(),progress,"{target}: partial private update");
        db.execute_batch("DROP TRIGGER refuse_publication_yield;").unwrap();
    }
    std::fs::write(canonical,b"{}").unwrap();
    assert!(store.yield_completed_owner_publication(&pending._metadata,&pending._publication,millis()).is_err());
    std::fs::write(canonical,&canonical_bytes).unwrap();
    assert_eq!(store.snapshot(id).unwrap().unwrap(),before);assert_eq!(store.events(id).unwrap(),events);
    pending.yield_to_metadata_recovery().unwrap();
    let yielded=store.snapshot(id).unwrap().unwrap();
    assert_eq!(yielded.lease_until_ms,0);assert!(db.query_row("SELECT owner IS NULL FROM workflow WHERE id=?1",[id],|r|r.get::<_,bool>(0)).unwrap());assert_eq!(yielded.generation,before.generation);
    assert_eq!(yielded.checkpoint,before.checkpoint);
    assert_eq!((yielded.attempts,yielded.wall_ms,yielded.model_calls,yielded.tokens),(before.attempts,before.wall_ms,before.model_calls,before.tokens));
    assert_eq!(store.owner_control(id).unwrap(),control);assert_eq!(store.pending_owner_messages(id).unwrap(),queued);
    assert_eq!(std::fs::read(canonical).unwrap(),canonical_bytes);assert_eq!(std::fs::read(audit.path()).unwrap(),native_audit);
    assert!(audit.dispatch_allowed().is_err());assert!(audit.model_call_admitted("codex").is_err());
    assert!(store.claim(id,"ordinary-cannot-take-pending-owner-input",millis(),60_000).unwrap().is_none());
    let update=store.progress_events(id).unwrap().pop().unwrap();
    assert_eq!(update.reason,if paused {"owner-pause-requested"}else{"owner-publication-yielded"});
    let updates=store.pending_progress(origin.bot,origin.owner).unwrap();
    assert!(updates.iter().any(|(route,workflow,_)|workflow==id && route.channel==origin.channel));
    assert!(store.pending_progress(origin.bot,99).unwrap().iter().all(|(_,workflow,_)|workflow!=id));

    let (sequence,raw):(i64,String)=db.query_row("SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_yielded'",[id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    for fault in ["version","owner","intent","completion","binding","unknown"] {
        let mut value:serde_json::Value=serde_json::from_str(&raw).unwrap();
        match fault {
            "version"=>value["version"]=2.into(),"owner"=>value["owner"]="substituted-owner".into(),
            "intent"=>value["intent"]["record_sha256"]="f".repeat(64).into(),
            "completion"=>value["completion_sha256"]="f".repeat(64).into(),
            "binding"=>value["binding"]["head"]="f".repeat(40).into(),_=>value["unknown"]=true.into(),
        }
        // Restore canonical field ordering to exercise the join, not just text.
        let changed=durable::test_yield_encoding(&value.to_string());
        let changed=if fault=="unknown" {assert!(changed.is_err());value.to_string()}else{changed.unwrap()};
        db.execute("UPDATE workflow_event SET payload=?2 WHERE seq=?1",rusqlite::params![sequence,changed]).unwrap();
        assert!(WorkflowStore::open(state).unwrap().owner_publication_recovery_plan(id).is_err(),"{fault}: changed yield history exposed a recovery plan");
        db.execute("UPDATE workflow_event SET payload=?2 WHERE seq=?1",rusqlite::params![sequence,raw]).unwrap();
    }
    db.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) SELECT workflow,generation,kind,payload,at_ms FROM workflow_event WHERE seq=?1",[sequence]).unwrap();
    let duplicate=db.last_insert_rowid();assert!(store.owner_publication_recovery_plan(id).is_err());
    db.execute("DELETE FROM workflow_event WHERE seq=?1",[duplicate]).unwrap();

    assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);
    let environment=durable::owner_publication_recovery::PreparationEnvironment::observe().unwrap();
    let selected_at=millis();
    let selected=store.next_owner_recovery_job("owner/repo",&environment,selected_at).unwrap().expect("explicit publication yield was not immediately selectable");
    assert!(selected.owner_recovery && selected.workflow==id);
    assert!(store.next_owner_recovery_job("owner/repo",&environment,selected_at+1).unwrap().is_none());
    let plan=store.owner_publication_recovery_plan(id).unwrap().unwrap();
    let policy=crate::autoship_discord_command::OwnerPolicy {bot:11,owner:22};
    let fresh=super::super::targeted::authorize_owner_publication_recovery(candidate,&plan,policy).await.unwrap();
    let recovery=Arc::new(store.claim_owner_publication_recovery(&fresh,"after-completed-publication",millis(),300_000).unwrap()
        .expect("explicit handoff still waits for artificial lease expiry"));
    assert_eq!(recovery.generation(),old_lease.generation+1);
    assert!(store.validate_lease(&old_lease,&before.checkpoint.binding,millis()).is_err());
    assert_eq!(store.owner_publication_recovery_native_facts(&recovery,&fresh,millis()).unwrap().fingerprint().unwrap(),facts.fingerprint().unwrap());
    let shared=Arc::new(Mutex::new(WorkflowStore::open(state).unwrap()));
    assert_eq!(owner_worker::run_metadata_pass(&shared,state,candidate,recovery,&fresh).unwrap(),owner_worker::MetadataPass::Ready);
    assert!(store.finish_owner_recovery_attempt(id,selected_at,durable::submission::AttemptOutcome::Progress,"owner-metadata-ready",millis()).unwrap());
    let ready_update=store.progress_events(id).unwrap().pop().unwrap();
    assert_eq!(ready_update.reason,"owner-metadata-ready");
    let setup_plan=store.owner_successor_setup_plan(id).unwrap().unwrap();
    let setup_auth=super::super::targeted::authorize_owner_successor(candidate,&setup_plan,policy).await.unwrap();
    let setup=store.claim_recovered_owner_turn_for_setup(&setup_auth,"uninterrupted-successor-setup",millis(),300_000).unwrap();
    if paused {assert!(setup.is_none(),"metadata handoff cleared owner's pause");}
    else {
        let setup=Arc::new(setup.expect("uninterrupted publication never reached shared setup"));
        let transferred=run_successor_native_setup(&shared,state,candidate,setup.clone(),&setup_auth).unwrap();
        assert!(matches!(transferred,SuccessorNativeSetupRun::Outcome(durable::native_prepare::NativePreparationRun::Pending {
            outcome:augmentagent_channel_core::native_vm::ownership::TransferOutcome::Completed(_),settlement:Some(_),settlement_failed:false,..})),"{transferred:?}");
        let after_transfer=store.events(id).unwrap();
        assert!(matches!(run_successor_native_setup(&shared,state,candidate,setup.clone(),&setup_auth).unwrap(),
            SuccessorNativeSetupRun::Outcome(durable::native_prepare::NativePreparationRun::Registered {..})));
        assert_eq!(store.events(id).unwrap(),after_transfer,"completed setup created another transfer");
        assert_eq!(store.owner_successor_native_facts(&setup,&setup_auth,millis()).unwrap().namespace().owner.lease_generation,setup.generation());
        let ordinary=Lease {workflow:id.into(),owner:"uninterrupted-successor-setup".into(),generation:setup.generation()};
        assert!(store.validate_dispatch(&ordinary,&before.checkpoint.binding,millis()).is_err(),"native setup became coding authority");
    }
    assert_eq!(db.query_row("SELECT record FROM workflow_owner_publication_intent WHERE workflow=?1",[id],|r|r.get::<_,String>(0)).unwrap(),intent);
    assert_eq!(db.query_row("SELECT record FROM workflow_owner_preparation WHERE request_id=?1",[&request],|r|r.get::<_,String>(0)).unwrap(),preparation);
    assert_eq!(store.owner_control(id).unwrap(),control);assert_eq!(store.pending_owner_messages(id).unwrap(),queued);
    assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
}
