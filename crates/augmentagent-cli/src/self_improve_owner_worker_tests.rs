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
    assert_eq!(
        store.progress_events(&id).unwrap(),
        progress,
        "recovery result used default recipient projection"
    );
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
