//! Fixture authorization feeds the actual worker metadata phase functions.
use super::*;
pub(super) async fn check_metadata_pass(
    state: &Path,
    worktree: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
    restart: bool,
) {
    let shared = Arc::new(Mutex::new(WorkflowStore::open(state).unwrap()));
    let mut store = WorkflowStore::open(state).unwrap();
    let facts = store
        .owner_publication_recovery_native_facts(&recovery, authorization, millis())
        .unwrap();
    let id = facts.namespace().owner.workflow_id.clone();
    let before = store.snapshot(&id).unwrap().unwrap();
    let control = store.owner_control(&id).unwrap();
    let queued = store.pending_owner_messages(&id).unwrap();
    let db = rusqlite::Connection::open(state).unwrap();
    // Select while expired, then acquire a genuinely new authorized metadata
    // claim. The queue's original request reference must survive that takeover.
    db.execute("UPDATE workflow SET lease_until=?2 WHERE id=?1",rusqlite::params![id,millis()-1]).unwrap();
    let environment=durable::owner_publication_recovery::PreparationEnvironment::observe().unwrap();
    let selected_at=millis();
    let selected=store.next_worker_job("owner/repo",&environment,selected_at).unwrap().unwrap();
    assert!(selected.owner_recovery && selected.workflow==id);
    let plan=store.owner_publication_recovery_plan(&id).unwrap().unwrap();
    let fresh=super::super::targeted::authorize_owner_publication_recovery(worktree,&plan,
        crate::autoship_discord_command::OwnerPolicy {bot:11,owner:22}).await.unwrap();
    let recovery=Arc::new(store.claim_owner_publication_recovery(&fresh,"selected-metadata-fixture",millis(),300_000).unwrap().unwrap());
    let authorization=&fresh;
    if restart {
        recover_known_native_completions(state, Some(worktree), recovery.clone(), authorization)
            .unwrap();
        recover_owner_publication(state, worktree, recovery.clone(), authorization).unwrap();
        assert!(matches!(
            recover_owner_native_handoff(&shared, state, worktree, recovery.clone(), authorization)
                .unwrap(),
            OwnerRecoveryNativeRun::Reconciled {
                result: durable::native_reconcile::ReconciliationRun::Settled(_),
                ..
            }
        ));
        let context: String = db
            .query_row(
                "SELECT record FROM workflow_owner_recovery_native_intent WHERE workflow=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        db.execute(
            "UPDATE workflow SET lease_until=?2 WHERE id=?1",
            rusqlite::params![id, millis() - 1],
        )
        .unwrap();
        let plan = store.owner_publication_recovery_plan(&id).unwrap().unwrap();
        let fresh = super::super::targeted::authorize_owner_publication_recovery(
            worktree,
            &plan,
            crate::autoship_discord_command::OwnerPolicy { bot: 11, owner: 22 },
        )
        .await
        .unwrap();
        let replacement = Arc::new(
            store
                .claim_owner_publication_recovery(
                    &fresh,
                    "worker-after-handoff-crash",
                    millis(),
                    300_000,
                )
                .unwrap()
                .unwrap(),
        );
        assert!(matches!(
            recover_owner_native_handoff(&shared, state, worktree, replacement.clone(), &fresh)
                .unwrap(),
            OwnerRecoveryNativeRun::Current
        ));
        assert_eq!(
            owner_worker::run_metadata_pass(&shared, state, worktree, replacement, &fresh).unwrap(),
            owner_worker::MetadataPass::Ready
        );
        assert_eq!(
            db.query_row(
                "SELECT record FROM workflow_owner_recovery_native_intent WHERE workflow=?1",
                [&id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            context,
            "replacement claim repeated the original handoff"
        );
    } else {
        assert_eq!(
            owner_worker::run_metadata_pass(&shared, state, worktree, recovery, authorization)
                .unwrap(),
            owner_worker::MetadataPass::Ready
        );
    }
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM workflow_owner_successor_ready WHERE workflow=?1",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM workflow_owner_recovery_native_intent WHERE workflow=?1",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let after = store.snapshot(&id).unwrap().unwrap();
    assert_eq!(after.model_calls, before.model_calls);
    assert_eq!(after.checkpoint, before.checkpoint);
    assert_eq!(store.owner_control(&id).unwrap(), control);
    assert_eq!(store.pending_owner_messages(&id).unwrap(), queued);
    assert_eq!(
        db.query_row(
            "SELECT state FROM workflow_owner_turn WHERE workflow=?1 AND state='recovery_ready'",
            [&id],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "recovery_ready"
    );
    assert!(store
        .owner_publication_recovery_plan(&id)
        .unwrap()
        .is_none());
    assert!(store.finish_owner_recovery_attempt(&id,selected_at,durable::submission::AttemptOutcome::Progress,"owner-metadata-ready",millis()).unwrap());
    let update=store.progress_events(&id).unwrap().pop().unwrap();
    assert_eq!(update.reason,"owner-metadata-ready");
    assert_eq!(update.next_action,"await_owner_resume","readiness update lost the existing owner pause");
    assert_eq!(update.merge_state,"not-confirmed");assert_eq!(update.deployment_state,"unknown");
    let pending=store.pending_progress(11,22).unwrap();
    assert!(pending.iter().any(|(o,w,s)|o.channel==authorization.plan().origin().channel && w==&id && *s==update.sequence));
    println!("worker metadata pass fixture passed");
}

pub(super) async fn check_worker_queue(
    state: &Path,
    worktree: &Path,
    recovery: Arc<durable::owner_publication_recovery::RecoveryLease>,
    authorization: &super::super::targeted::AuthorizedOwnerRecovery<'_>,
) {
    use durable::{
        owner_publication_recovery::PreparationEnvironment,
        submission::{AttemptOutcome, Milestone, Request, Target},
    };
    let mut store = WorkflowStore::open(state).unwrap();
    let facts = store
        .owner_publication_recovery_native_facts(&recovery, authorization, millis())
        .unwrap();
    let id = facts.namespace().owner.workflow_id.clone();
    let environment = PreparationEnvironment::observe().unwrap();
    assert!(
        store
            .next_worker_job("owner/repo", &environment, millis())
            .unwrap()
            .is_none(),
        "live owner claim was scheduled for takeover"
    );
    assert!(!store.worker_queue_due("owner/repo", &environment, millis()).unwrap(),
        "wake hint scheduled a live recovery claim");
    let db = rusqlite::Connection::open(state).unwrap();
    db.execute(
        "UPDATE workflow SET lease_until=?2 WHERE id=?1",
        rusqlite::params![id, millis() - 1],
    )
    .unwrap();
    let before = store.snapshot(&id).unwrap().unwrap();
    let progress = store.progress_events(&id).unwrap();
    assert!(!store.command_queue_due("owner/repo", millis()).unwrap(),
        "fixture unexpectedly wakes the ordinary command lane");
    assert!(store.worker_queue_due("owner/repo", &environment, millis()).unwrap(),
        "expired owner recovery cannot wake the real worker");
    assert!(!store.worker_queue_due("different/repo", &environment, millis()).unwrap());
    assert_eq!(store.snapshot(&id).unwrap().unwrap(), before,
        "wake hint changed workflow ownership");

    let peer = store
        .submit_authorized(
            "worker-queue-peer",
            "github:owner",
            &Request {
                repository: "owner/repo".into(),
                target: Target::Issue(78),
                provider: None,
                until: Milestone::Merged,
            },
            millis(),
        )
        .unwrap()
        .workflow;
    let selected_at = millis();
    let first = store
        .next_worker_job("owner/repo", &environment, selected_at)
        .unwrap()
        .unwrap();
    assert!(
        first.owner_recovery && first.workflow == id,
        "eligible original owner recovery was not selected: {first:?}"
    );
    assert_eq!(
        store.snapshot(&id).unwrap().unwrap(),
        before,
        "selection changed the actual workflow claim"
    );
    // The real entry has no fixture-policy overload. Removing configuration in
    // this disposable child must reject before any new recovery claim is made.
    let configured =
        ["DISCORD_ALLOWED_USER_ID", "DISCORD_BOT_TOKEN"].map(|key| (key, std::env::var_os(key)));
    for (key, _) in &configured {
        unsafe {
            std::env::remove_var(key);
        }
    }
    let refused = owner_worker::from_worker(worktree.into(), state.into(), id.clone()).await;
    for (key, value) in configured {
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
    assert!(
        refused.is_err(),
        "unverified process acquired worker authority"
    );
    assert_eq!(store.snapshot(&id).unwrap().unwrap(), before);
    assert!(store.finish_owner_recovery_attempt(&id,selected_at,AttemptOutcome::Progress,"owner-metadata-ready",millis()).is_err(),
        "ready update accepted without a readiness record");
    assert_eq!(store.command_attempt(&id).unwrap().unwrap().finished_at_ms,None);
    db.execute_batch("CREATE TRIGGER deny_recovery_progress BEFORE INSERT ON workflow_progress_event BEGIN SELECT RAISE(ABORT,'injected progress failure'); END;").unwrap();
    assert!(store.finish_owner_recovery_attempt(&id,selected_at,AttemptOutcome::Deferred,"owner-recovery-authorization-blocked",millis()).is_err());
    assert_eq!(store.command_attempt(&id).unwrap().unwrap().finished_at_ms,None,
        "outbox failure committed a terminal attempt without its private update");
    assert_eq!(store.progress_events(&id).unwrap(),progress);
    db.execute_batch("DROP TRIGGER deny_recovery_progress;").unwrap();
    assert!(store
        .finish_owner_recovery_attempt(
            &id,
            selected_at,
            AttemptOutcome::Deferred,
            "owner-recovery-authorization-blocked",
            millis()
        )
        .unwrap());
    assert!(
        !store
            .finish_owner_recovery_attempt(
                &id,
                selected_at,
                AttemptOutcome::Progress,
                "owner-metadata-ready",
                millis()
            )
            .unwrap(),
        "completed attempt was overwritten"
    );
    let events=store.progress_events(&id).unwrap();
    assert_eq!(events.len(),progress.len()+1,"owner recovery result has no durable private update");
    let update=events.last().unwrap();
    assert_eq!(update.reason,"owner-recovery-authorization-blocked");
    assert_eq!(update.merge_state,"not-confirmed");
    assert_eq!(update.deployment_state,"unknown");
    assert_eq!(update.eligible_at_ms,None,"attempt delay misreported as native recovery eligibility");
    check_worker_progress_route(&mut store,&db,&id,authorization.plan().origin(),update.sequence);
    assert!(
        store
            .next_worker_job("owner/repo", &environment, millis())
            .unwrap()
            .is_none(),
        "command lane did not yield its automatic-work turn"
    );
    let next = store
        .next_worker_job("owner/repo", &environment, millis())
        .unwrap()
        .unwrap();
    assert!(
        !next.owner_recovery && next.workflow == peer,
        "owner recovery starved ordinary queued work: {next:?}"
    );
    assert_eq!(
        store.snapshot(&id).unwrap().unwrap().model_calls,
        before.model_calls
    );
    println!("worker recovery queue fixture passed");
}

fn check_worker_progress_route(store:&mut WorkflowStore,db:&rusqlite::Connection,id:&str,origin:&durable::discord_ingress::DiscordOrigin,sequence:i64) {
    store.configure_delivery("owner/repo","github:owner",durable::delivery::Destination {bot:origin.bot,owner:99,channel:88},millis()).unwrap();
    let payload:String=db.query_row("SELECT payload FROM workflow_progress_state WHERE workflow=?1",[id],|r|r.get(0)).unwrap();
    assert!(serde_json::from_str::<durable::progress::Progress>(&payload).is_err());
    let value:serde_json::Value=serde_json::from_str(&payload).unwrap();
    assert_eq!(value["version"],3);
    for fault in ["version","request","owner","channel","reference","unknown"] {
        let mut bad=value.clone();
        match fault {
            "version"=>bad["version"]=2.into(),
            "request"=>bad["owner_scope"]["request"]="discord:forged".into(),
            "owner"=>bad["owner_scope"]["origin"]["owner"]=99.into(),
            "channel"=>bad["owner_scope"]["origin"]["channel"]=88.into(),
            "reference"=>bad["owner_scope"]["reference"]["sha256"]="f".repeat(64).into(),
            _=>bad["owner_scope"]["unknown"]="private".into(),
        }
        db.execute("UPDATE workflow_progress_state SET payload=?2 WHERE workflow=?1",rusqlite::params![id,bad.to_string()]).unwrap();
        assert!(store.claim_progress(origin,id,sequence,millis()).is_err(),"{fault}: private scope corruption accepted");
        db.execute("UPDATE workflow_progress_state SET payload=?2 WHERE workflow=?1",rusqlite::params![id,payload]).unwrap();
    }
    assert!(store.pending_progress(origin.bot,99).unwrap().iter().all(|(_,w,_)|w!=id));
    let pending=store.pending_progress(origin.bot,origin.owner).unwrap();
    let route=pending.iter().find(|(_,w,_)|w==id).unwrap();
    assert_eq!(route.0.channel,origin.channel);
    let mut wrong=origin.clone();wrong.channel=88;wrong.owner=99;
    assert!(store.claim_progress(&wrong,id,sequence,millis()).is_err());
    let (_,content)=store.claim_progress(origin,id,sequence,millis()).unwrap().unwrap();
    assert!(content.contains("current owner and repository authorization"));
    assert!(!content.contains("coding has started"));
    assert!(store.claim_progress(origin,id,sequence,millis()).unwrap().is_none());
    assert_eq!(store.progress_delivery(id).unwrap().state,"delivery-uncertain");
    let dbpath: String=db.query_row("SELECT file FROM pragma_database_list WHERE name='main'",[],|r|r.get(0)).unwrap();
    let reopened=WorkflowStore::open(Path::new(&dbpath)).unwrap();
    assert_eq!(reopened.progress_delivery(id).unwrap().state,"delivery-uncertain");
    assert!(reopened.uncertain_progress(origin.bot,origin.owner).unwrap().iter().any(|(o,w,s,_,_)|o.channel==origin.channel && w==id && *s==sequence));
    store.confirm_progress(origin,id,sequence,777,millis()).unwrap();
    assert_eq!(store.progress_delivery(id).unwrap().state,"delivery-confirmed");
}
