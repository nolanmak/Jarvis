use async_trait::async_trait;
use augmentagent_phone::{
    recovery::{reconcile, RecoveryOutcome},
    Clock, DialRequest, FinalUsage, ProviderError, Telephony,
};
use augmentagent_store::{
    phone::{CallState, ProviderUsage, ReserveCall},
    Store,
};
use std::sync::atomic::{AtomicUsize, Ordering};
struct Now;
impl Clock for Now {
    fn now_ms(&self) -> i64 {
        10_000
    }
}
struct Provider {
    result: Option<FinalUsage>,
    reads: AtomicUsize,
}
#[async_trait]
impl Telephony for Provider {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("recovery must never dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        panic!("read-only recovery must never hang up")
    }
    async fn final_usage(&self, sid: &str) -> Result<Option<FinalUsage>, ProviderError> {
        assert_eq!(sid, "CA-synthetic");
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.result)
    }
}
fn fixture() -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let call = store
        .phone_reserve_call(ReserveCall {
            owner: "owner",
            request_id: "first",
            conversation: "chat",
            now_ms: 0,
        })
        .unwrap()
        .call;
    (dir, store, call.id)
}
#[tokio::test]
async fn exhausted_cleanup_recovers_from_provider_terminal_evidence_without_reset_or_redial() {
    let (_dir, store, id) = fixture();
    store.phone_bind_provider(&id, "CA-synthetic").unwrap();
    store.phone_request_stop("owner", &id).unwrap();
    for now in [0, 1000, 3000] {
        assert!(store.phone_claim_cleanup(&id, now).unwrap());
    }
    let provider = Provider {
        result: Some(FinalUsage::Completed(ProviderUsage {
            duration_seconds: 5,
            ended_ms: 6000,
        })),
        reads: AtomicUsize::new(0),
    };
    assert_eq!(
        reconcile(&store, &provider, &Now, &id).await.unwrap(),
        RecoveryOutcome::Finalized
    );
    assert!(store.phone_call("owner", &id).unwrap().state.terminal());
    assert_eq!(store.phone_usage(&id).unwrap().unwrap().duration_seconds, 5);
    assert_eq!(
        store
            .phone_maintenance_status("owner", &id)
            .unwrap()
            .cleanup_attempts,
        3
    );
    assert_eq!(
        reconcile(&store, &provider, &Now, &id).await.unwrap(),
        RecoveryOutcome::AlreadyFinalized
    );
    assert_eq!(provider.reads.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn unknown_sid_and_nonfinal_or_contradictory_evidence_keep_active_slot() {
    let (_dir, store, id) = fixture();
    let mut provider = Provider {
        result: None,
        reads: AtomicUsize::new(0),
    };
    store.phone_mark_uncertain(&id).unwrap();
    assert_eq!(
        reconcile(&store, &provider, &Now, &id).await.unwrap(),
        RecoveryOutcome::UnknownProviderId
    );
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
    store.phone_bind_provider(&id, "CA-synthetic").unwrap();
    assert_eq!(
        reconcile(&store, &provider, &Now, &id).await.unwrap(),
        RecoveryOutcome::ProviderNotFinal
    );
    store
        .phone_provider_event(&id, "CA-synthetic", CallState::AwaitingAcceptance, 1000)
        .unwrap();
    provider.result = Some(FinalUsage::NeverAnswered);
    assert_eq!(
        reconcile(&store, &provider, &Now, &id).await.unwrap(),
        RecoveryOutcome::ContradictoryEvidence
    );
    assert!(!store.phone_call("owner", &id).unwrap().state.terminal());
    assert!(store.phone_usage(&id).unwrap().is_none());
    assert!(store
        .phone_reserve_call(ReserveCall {
            owner: "owner",
            request_id: "second",
            conversation: "chat",
            now_ms: 10_000
        })
        .is_err());
}
#[tokio::test]
async fn provider_never_answered_can_finalize_without_local_answer_evidence() {
    let (_dir, store, id) = fixture();
    store.phone_bind_provider(&id, "CA-synthetic").unwrap();
    let provider = Provider {
        result: Some(FinalUsage::NeverAnswered),
        reads: AtomicUsize::new(0),
    };
    assert_eq!(
        reconcile(&store, &provider, &Now, &id).await.unwrap(),
        RecoveryOutcome::Finalized
    );
    assert_eq!(
        store.phone_call("owner", &id).unwrap().state,
        CallState::Canceled
    );
    assert_eq!(store.phone_usage(&id).unwrap().unwrap().duration_seconds, 0);
}

struct CallbackDuringRead<'a> {
    store: &'a Store,
    id: &'a str,
    finalized: bool,
}
#[async_trait]
impl Telephony for CallbackDuringRead<'_> {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("must not dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        panic!("must not hang up")
    }
    async fn final_usage(&self, sid: &str) -> Result<Option<FinalUsage>, ProviderError> {
        self.store
            .phone_provider_event(self.id, sid, CallState::AwaitingAcceptance, 1000)
            .unwrap();
        if self.finalized {
            self.store
                .phone_provider_event_with_usage(
                    self.id,
                    sid,
                    CallState::Completed,
                    4000,
                    Some(ProviderUsage {
                        duration_seconds: 2,
                        ended_ms: 3000,
                    }),
                )
                .unwrap();
        }
        Ok(Some(FinalUsage::NeverAnswered))
    }
}
#[tokio::test]
async fn callbacks_during_provider_read_keep_answer_and_final_usage_authoritative() {
    for finalized in [false, true] {
        let (_dir, store, id) = fixture();
        store.phone_bind_provider(&id, "CA-synthetic").unwrap();
        let provider = CallbackDuringRead {
            store: &store,
            id: &id,
            finalized,
        };
        assert_eq!(
            reconcile(&store, &provider, &Now, &id).await.unwrap(),
            if finalized {
                RecoveryOutcome::AlreadyFinalized
            } else {
                RecoveryOutcome::ContradictoryEvidence
            }
        );
        assert_eq!(
            store
                .phone_usage(&id)
                .unwrap()
                .map(|usage| usage.duration_seconds),
            finalized.then_some(2)
        );
    }
}
struct FailedRead;
#[async_trait]
impl Telephony for FailedRead {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("must not dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        panic!("must not hang up")
    }
    async fn final_usage(&self, _: &str) -> Result<Option<FinalUsage>, ProviderError> {
        Err(ProviderError::Uncertain)
    }
}
#[tokio::test]
async fn provider_failure_does_not_clear_local_state_or_counters() {
    let (_dir, store, id) = fixture();
    store.phone_bind_provider(&id, "CA-synthetic").unwrap();
    store.phone_request_stop("owner", &id).unwrap();
    assert!(store.phone_claim_cleanup(&id, 0).unwrap());
    assert!(reconcile(&store, &FailedRead, &Now, &id).await.is_err());
    assert!(!store.phone_call("owner", &id).unwrap().state.terminal());
    assert!(store.phone_usage(&id).unwrap().is_none());
    assert_eq!(
        store
            .phone_maintenance_status("owner", &id)
            .unwrap()
            .cleanup_attempts,
        1
    );
}

struct CandidateEvidence {
    confirmed: bool,
    terminal: bool,
}
#[async_trait]
impl Telephony for CandidateEvidence {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("must not dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        panic!("must not hang up")
    }
    async fn final_usage(&self, _: &str) -> Result<Option<FinalUsage>, ProviderError> {
        Ok(self.terminal.then_some(FinalUsage::NeverAnswered))
    }
    async fn confirms_origin(&self, _: &str, _: &str, _: &str) -> Result<bool, ProviderError> {
        Ok(self.confirmed)
    }
}
#[tokio::test]
async fn unknown_provider_association_requires_terminal_exact_origin_evidence() {
    use augmentagent_phone::recovery::associate_and_reconcile;
    for (confirmed, terminal) in [(false, true), (true, false), (true, true)] {
        let (_dir, store, id) = fixture();
        store.phone_mark_uncertain(&id).unwrap();
        let provider = CandidateEvidence {
            confirmed,
            terminal,
        };
        let result = associate_and_reconcile(
            &store,
            &provider,
            &Now,
            &id,
            "CA-candidate",
            "https://phone.example.test",
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            if !terminal {
                RecoveryOutcome::ProviderNotFinal
            } else if !confirmed {
                RecoveryOutcome::UnverifiedProviderId
            } else {
                RecoveryOutcome::Finalized
            }
        );
        let call = store.phone_provider_call(&id).unwrap();
        assert_eq!(call.provider_sid.is_some(), confirmed && terminal);
        assert_eq!(call.state.terminal(), confirmed && terminal);
        if confirmed && terminal {
            assert!(associate_and_reconcile(
                &store,
                &provider,
                &Now,
                &id,
                "CA-other",
                "https://phone.example.test"
            )
            .await
            .is_err());
            assert_eq!(
                store
                    .phone_provider_call(&id)
                    .unwrap()
                    .provider_sid
                    .as_deref(),
                Some("CA-candidate")
            );
        }
    }
}

struct AssociationRace<'a> {
    store: &'a Store,
    id: &'a str,
}
#[async_trait]
impl Telephony for AssociationRace<'_> {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("must not dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        panic!("must not hang up")
    }
    async fn final_usage(&self, _: &str) -> Result<Option<FinalUsage>, ProviderError> {
        Ok(Some(FinalUsage::NeverAnswered))
    }
    async fn confirms_origin(&self, _: &str, _: &str, _: &str) -> Result<bool, ProviderError> {
        self.store
            .phone_bind_provider(self.id, "CA-concurrent-callback")
            .unwrap();
        Ok(true)
    }
}
#[tokio::test]
async fn concurrent_callback_binding_cannot_be_overwritten_by_candidate_association() {
    let (_dir, store, id) = fixture();
    store.phone_mark_uncertain(&id).unwrap();
    let provider = AssociationRace {
        store: &store,
        id: &id,
    };
    assert!(augmentagent_phone::recovery::associate_and_reconcile(
        &store,
        &provider,
        &Now,
        &id,
        "CA-candidate",
        "https://phone.example.test"
    )
    .await
    .is_err());
    let call = store.phone_provider_call(&id).unwrap();
    assert_eq!(call.provider_sid.as_deref(), Some("CA-concurrent-callback"));
    assert!(!call.state.terminal());
    assert!(store.phone_usage(&id).unwrap().is_none());
}

#[test]
fn dispatch_intent_claim_result_and_delivery_survive_reopen() {
    let (dir, store, id) = fixture();
    store
        .phone_enqueue_dispatch(&id, "task", "Synthetic task")
        .unwrap();
    store
        .phone_enqueue_dispatch(&id, "task", "Synthetic task")
        .unwrap();
    assert!(store
        .phone_enqueue_dispatch(&id, "task", "Different task")
        .is_err());
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    assert_eq!(store.phone_pending_dispatches().unwrap().len(), 1);
    assert!(store.phone_start_dispatch(&id, "task").unwrap());
    assert!(!store.phone_start_dispatch(&id, "task").unwrap());
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    assert_eq!(
        store.phone_pending_dispatches().unwrap()[0].state,
        "running"
    );
    store
        .phone_finish_dispatch(&id, "task", "Saved result")
        .unwrap();
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    let jobs = store.phone_pending_dispatches().unwrap();
    assert_eq!(jobs[0].state, "ready");
    assert_eq!(jobs[0].result.as_deref(), Some("Saved result"));
    store.phone_dispatch_delivered(&id, "task").unwrap();
    assert!(store.phone_pending_dispatches().unwrap().is_empty());
    store
        .phone_enqueue_dispatch(&id, "task", "Synthetic task")
        .unwrap();
    assert!(!store.phone_start_dispatch(&id, "task").unwrap());
}
