use augmentagent_store::{
    phone::{CallState, PhoneError, ReserveCall},
    Store,
};
use std::sync::{Arc, Barrier};

const OWNER: &str = "owner-a";
const NUMBER: &str = "+12025550123";
fn request<'a>(id: &'a str, origin: &'a str, now: i64) -> ReserveCall<'a> {
    ReserveCall {
        owner: OWNER,
        request_id: id,
        conversation: origin,
        now_ms: now,
    }
}
fn fixture() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("phone.db")).unwrap());
    store.phone_import_destination(OWNER, NUMBER, 0).unwrap();
    (dir, store)
}

#[test]
fn ac03_ten_simultaneous_requests_have_one_dial_permit() {
    let (_dir, store) = fixture();
    let barrier = Arc::new(Barrier::new(10));
    let jobs: Vec<_> = (0..10)
        .map(|_| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .phone_reserve_call(request("request-1", "discord-thread", 100))
                    .unwrap()
            })
        })
        .collect();
    let receipts: Vec<_> = jobs.into_iter().map(|j| j.join().unwrap()).collect();
    assert_eq!(receipts.iter().filter(|r| r.should_dial).count(), 1);
    assert!(receipts.iter().all(|r| r.call.id == receipts[0].call.id));
    assert!(matches!(
        store.phone_reserve_call(request("request-1", "other-thread", 100)),
        Err(PhoneError::Conflict)
    ));
    assert!(matches!(
        store.phone_reserve_call(request("request-2", "discord-thread", 100)),
        Err(PhoneError::AlreadyActive)
    ));
}

#[test]
fn ac03_uncertain_create_survives_restart_without_a_second_permit() {
    let (dir, store) = fixture();
    let first = store
        .phone_reserve_call(request("request-1", "slack-thread", 100))
        .unwrap();
    store.phone_mark_uncertain(&first.call.id).unwrap();
    drop(store);
    let reopened = Store::open(dir.path().join("phone.db")).unwrap();
    let retry = reopened
        .phone_reserve_call(request("request-1", "slack-thread", 200))
        .unwrap();
    assert!(!retry.should_dial);
    assert_eq!(retry.call.state, CallState::Uncertain);
    assert!(matches!(
        reopened.phone_reserve_call(request("new", "slack-thread", 200)),
        Err(PhoneError::AlreadyActive)
    ));
}

#[test]
fn ac04_callback_before_create_response_and_out_of_order_events_are_safe() {
    let (_dir, store) = fixture();
    let call = store
        .phone_reserve_call(request("request-1", "whatsapp-chat", 100))
        .unwrap()
        .call;
    store
        .phone_provider_event(&call.id, "CA-test", CallState::Ringing, 150)
        .unwrap();
    store.phone_bind_provider(&call.id, "CA-test").unwrap();
    store
        .phone_provider_event(&call.id, "CA-test", CallState::AwaitingAcceptance, 200)
        .unwrap();
    store
        .phone_provider_event(&call.id, "CA-test", CallState::Queued, 250)
        .unwrap();
    assert_eq!(
        store.phone_call(OWNER, &call.id).unwrap().state,
        CallState::AwaitingAcceptance
    );
    store
        .phone_provider_event(&call.id, "CA-test", CallState::Completed, 400)
        .unwrap();
    store
        .phone_provider_event(&call.id, "CA-test", CallState::Ringing, 450)
        .unwrap();
    assert_eq!(
        store.phone_call(OWNER, &call.id).unwrap().state,
        CallState::Completed
    );
    assert!(matches!(
        store.phone_provider_event(&call.id, "CA-forged", CallState::Failed, 500),
        Err(PhoneError::Conflict)
    ));
    assert!(matches!(
        store.phone_call("other-owner", &call.id),
        Err(PhoneError::NotFound)
    ));
}

#[test]
fn ac09_rate_limits_survive_restart_and_release_after_window() {
    let (dir, store) = fixture();
    for i in 0..3 {
        let r = store
            .phone_reserve_call(request(&format!("r{i}"), "thread", i * 100))
            .unwrap();
        store
            .phone_provider_event(
                &r.call.id,
                &format!("CA-{i}"),
                CallState::Busy,
                i * 100 + 50,
            )
            .unwrap();
    }
    drop(store);
    let store = Store::open(dir.path().join("phone.db")).unwrap();
    assert!(matches!(
        store.phone_reserve_call(request("fourth", "thread", 1_000)),
        Err(PhoneError::RateLimited)
    ));
    assert!(
        store
            .phone_reserve_call(request("later", "thread", 600_001))
            .unwrap()
            .should_dial
    );
}

#[test]
fn ac15_destination_snapshot_and_removal_survive_restart() {
    let (dir, store) = fixture();
    let call = store
        .phone_reserve_call(request("request", "thread", 100))
        .unwrap()
        .call;
    assert_eq!(
        store.phone_dial_destination(&call.id).unwrap().expose(),
        NUMBER
    );
    let removed = store.phone_remove_destination(OWNER, 200).unwrap();
    assert_eq!(removed, vec![call.id.clone()]);
    assert!(store.phone_call(OWNER, &call.id).unwrap().stop_requested);
    assert!(store
        .phone_destination_status(OWNER)
        .unwrap()
        .masked
        .is_none());
    assert!(!store.phone_import_destination(OWNER, NUMBER, 300).unwrap());
    drop(store);
    let reopened = Store::open(dir.path().join("phone.db")).unwrap();
    assert!(!reopened
        .phone_import_destination(OWNER, NUMBER, 400)
        .unwrap());
    assert!(matches!(
        reopened.phone_reserve_call(request("new", "thread", 500)),
        Err(PhoneError::NotConfigured)
    ));
    assert!(!format!("{call:?}").contains(NUMBER));
    assert!(!serde_json::to_string(&call).unwrap().contains(NUMBER));
}

#[test]
fn ac05_answered_call_requires_explicit_acceptance_and_honors_deadline() {
    let (_dir, store) = fixture();
    let call = store
        .phone_reserve_call(request("accept", "thread", 100))
        .unwrap()
        .call;
    assert!(matches!(
        store.phone_accept(&call.id, "CA-a", 150),
        Err(PhoneError::Conflict)
    ));
    store
        .phone_provider_event(&call.id, "CA-a", CallState::AwaitingAcceptance, 200)
        .unwrap();
    assert!(store.phone_accept(&call.id, "CA-a", 201).unwrap());
    assert!(!store.phone_accept(&call.id, "CA-a", 202).unwrap());
    assert_eq!(
        store.phone_call(OWNER, &call.id).unwrap().state,
        CallState::Connected
    );
    store.phone_request_stop(OWNER, &call.id).unwrap();
    assert!(matches!(
        store.phone_accept(&call.id, "CA-a", 203),
        Err(PhoneError::Conflict)
    ));
    store
        .phone_provider_event(&call.id, "CA-a", CallState::Completed, 300)
        .unwrap();
    let late = store
        .phone_reserve_call(request("late", "thread", 400))
        .unwrap()
        .call;
    store
        .phone_provider_event(&late.id, "CA-b", CallState::AwaitingAcceptance, 500)
        .unwrap();
    assert!(matches!(
        store.phone_accept(&late.id, "CA-b", 15_500),
        Err(PhoneError::AcceptanceExpired)
    ));
}

#[test]
fn ac14_pending_replacement_expiry_lockout_and_success() {
    use augmentagent_store::phone::BeginVerification;
    let (_dir, store) = fixture();
    let begin = |id, now| {
        store.phone_begin_verification(BeginVerification {
            request: request(id, "private-chat", now),
            number: "+12025550456",
            code_hash: &[1; 32],
        })
    };
    let first = begin("verify-1", 100).unwrap();
    assert!(first.should_dial);
    assert!(!begin("verify-1", 101).unwrap().should_dial);
    assert_eq!(
        store
            .phone_destination_status(OWNER)
            .unwrap()
            .masked
            .as_deref(),
        Some("••••0123")
    );
    for _ in 0..5 {
        assert!(!store
            .phone_verify_destination(OWNER, &[2; 32], 200)
            .unwrap());
    }
    assert!(!store
        .phone_verify_destination(OWNER, &[1; 32], 201)
        .unwrap());
    store
        .phone_provider_event(&first.call.id, "CA-v1", CallState::Completed, 250)
        .unwrap();
    let expired = begin("verify-2", 300).unwrap();
    assert!(!store
        .phone_verify_destination(OWNER, &[1; 32], 300_300)
        .unwrap());
    store
        .phone_provider_event(&expired.call.id, "CA-v2", CallState::Completed, 300_400)
        .unwrap();
    let third = begin("verify-3", 600_500).unwrap();
    assert!(store
        .phone_verify_destination(OWNER, &[1; 32], 600_501)
        .unwrap());
    assert!(!store
        .phone_verify_destination(OWNER, &[1; 32], 600_502)
        .unwrap());
    assert_eq!(
        store
            .phone_destination_status(OWNER)
            .unwrap()
            .masked
            .as_deref(),
        Some("••••0456")
    );
    store
        .phone_provider_event(&third.call.id, "CA-v3", CallState::Completed, 600_600)
        .unwrap();
    assert!(matches!(
        begin("verify-4", 601_000),
        Err(PhoneError::RateLimited)
    ));
}

#[test]
fn ac14_destination_rate_limit_applies_across_owners_and_remove_invalidates_code() {
    use augmentagent_store::phone::BeginVerification;
    let (_dir, store) = fixture();
    for i in 0..3 {
        let owner = format!("owner-{i}");
        let reservation = store
            .phone_begin_verification(BeginVerification {
                request: ReserveCall {
                    owner: &owner,
                    request_id: "verify",
                    conversation: "private",
                    now_ms: 100,
                },
                number: NUMBER,
                code_hash: &[3; 32],
            })
            .unwrap();
        store.phone_remove_destination(&owner, 101).unwrap();
        assert!(!store
            .phone_verify_destination(&owner, &[3; 32], 102)
            .unwrap());
        store
            .phone_provider_event(
                &reservation.call.id,
                &format!("CA-v{i}"),
                CallState::Completed,
                103,
            )
            .unwrap();
    }
    assert!(matches!(
        store.phone_begin_verification(BeginVerification {
            request: request("fourth", "private", 104),
            number: NUMBER,
            code_hash: &[3; 32],
        }),
        Err(PhoneError::RateLimited)
    ));
}

#[test]
fn ac07_relay_can_be_claimed_once_only_after_acceptance_even_across_restart() {
    let (dir, store) = fixture();
    let call = store
        .phone_reserve_call(request("r1", "thread", 100))
        .unwrap()
        .call;
    assert!(store.phone_claim_relay(&call.id, "CA-a").is_err());
    store
        .phone_provider_event(&call.id, "CA-a", CallState::AwaitingAcceptance, 200)
        .unwrap();
    store.phone_accept(&call.id, "CA-a", 201).unwrap();
    assert!(store.phone_claim_relay(&call.id, "CA-wrong").is_err());
    store.phone_claim_relay(&call.id, "CA-a").unwrap();
    drop(store);
    let reopened = Store::open(dir.path().join("phone.db")).unwrap();
    assert!(reopened.phone_claim_relay(&call.id, "CA-a").is_err());
}

#[test]
fn ac09_connected_minutes_cap_and_utc_day_reset() {
    let (_dir, store) = fixture();
    for i in 0..4 {
        let start = 1_000_000 * i;
        let call = store
            .phone_reserve_call(request(&format!("call-{i}"), "thread", start))
            .unwrap()
            .call;
        assert_eq!(call.max_duration_seconds, 900);
        store
            .phone_provider_event(
                &call.id,
                &format!("CA-{i}"),
                CallState::AwaitingAcceptance,
                start,
            )
            .unwrap();
        store
            .phone_provider_event(
                &call.id,
                &format!("CA-{i}"),
                CallState::Completed,
                start + 900_000,
            )
            .unwrap();
    }
    assert!(matches!(
        store.phone_reserve_call(request("over-budget", "thread", 4_000_000)),
        Err(PhoneError::BudgetExhausted)
    ));
    assert_eq!(
        store
            .phone_reserve_call(request("tomorrow", "thread", 86_400_000))
            .unwrap()
            .call
            .max_duration_seconds,
        900
    );
}

#[test]
fn origin_is_bound_to_the_call_and_survives_restart() {
    use augmentagent_store::{
        phone::PhoneOrigin, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef,
        SurfacePlatform,
    };
    let (dir, store) = fixture();
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "guild").unwrap();
    let chat = SurfaceConversationRef::new(account.clone(), "thread", None).unwrap();
    let origin = PhoneOrigin {
        owner: SurfaceOwnerRef::new(account, "sender").unwrap(),
        conversation: chat.clone(),
    };
    let call = store
        .phone_reserve_call(request("r1", &chat.storage_key(), 100))
        .unwrap()
        .call;
    store.phone_register_origin(&call.id, &origin).unwrap();
    store.phone_register_origin(&call.id, &origin).unwrap();
    let mut other = origin.clone();
    other.conversation =
        SurfaceConversationRef::new(other.owner.account().clone(), "elsewhere", None).unwrap();
    assert!(store.phone_register_origin(&call.id, &other).is_err());
    drop(store);
    let reopened = Store::open(dir.path().join("phone.db")).unwrap();
    assert_eq!(reopened.phone_origin(&call.id).unwrap(), origin);
}
