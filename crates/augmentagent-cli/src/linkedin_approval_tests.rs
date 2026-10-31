//! Actual approver -> Voyager membership check -> local outbound HTTP.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn auth() -> LinkedInAuth {
    LinkedInAuth {
        member_urn: "urn:li:fsd_profile:OWNER".into(),
        cookies: std::collections::BTreeMap::from([
            ("li_at".into(), "SYNTHETIC".into()),
            ("JSESSIONID".into(), "ajax:synthetic".into()),
        ]),
        user_agent: "test".into(),
        harvested_at_ms: 0,
    }
}

async fn server(group: Arc<AtomicBool>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(move |_: &wiremock::Request| {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":{"messengerConversationsById":{
            "entityUrn":"thread:synthetic","title":"Synthetic", "groupChat":group.load(Ordering::SeqCst), "lastActivityAt":1000,
            "conversationParticipants":[{"hostIdentityUrn":"urn:li:fsd_profile:OWNER"},{"hostIdentityUrn":"urn:li:fsd_profile:PEER"}]
        }}}))
    }).mount(&server).await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(50))
                .set_body_json(serde_json::json!({"backendUrn":"sent:synthetic"})),
        )
        .mount(&server)
        .await;
    server
}

fn seed(store: &Store) -> String {
    let email = social_card_row(
        "linkedin",
        "dm",
        "compose:linkedin:dm:synthetic",
        "thread:synthetic",
        None,
        "Synthetic peer",
        "Synthetic",
        "Context",
    );
    store.upsert_email(&email).unwrap();
    store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some("Approved synthetic reply"),
            ActionStatus::Pending,
        )
        .unwrap()
}

fn approver(store: Arc<Store>, server: &MockServer) -> ReplyApprover {
    let mut approver = crate::test_support::approver_with_store(store);
    approver.linkedin = Some(Arc::new(
        VoyagerClient::for_test(auth(), server.uri()).unwrap(),
    ));
    approver
}

async fn posts(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .count()
}

#[tokio::test]
async fn direct_approval_sends_once_and_replay_does_not_send() {
    let server = server(Arc::new(AtomicBool::new(false))).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(Store::open(file.path()).unwrap());
    let id = seed(&store);
    assert_eq!(posts(&server).await, 0);
    let approver = approver(store.clone(), &server);
    assert!(matches!(
        approver.run_approve(&id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(posts(&server).await, 1);
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "sent"
    );
    drop(approver);
    drop(store);
    let reopened = Arc::new(Store::open(file.path()).unwrap());
    let reloaded = self::approver(reopened, &server);
    assert!(matches!(
        reloaded.run_approve(&id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert_eq!(posts(&server).await, 1);
}

#[tokio::test]
async fn reloaded_stale_direct_card_cannot_send_to_a_group() {
    let group = Arc::new(AtomicBool::new(false));
    let server = server(group.clone()).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    // At drafting time the authoritative conversation is direct.
    VoyagerClient::for_test(auth(), server.uri())
        .unwrap()
        .archive_reader()
        .unwrap()
        .unwrap()
        .require_direct("thread:synthetic")
        .await
        .unwrap();
    let id = seed(&store);
    drop(store);
    group.store(true, Ordering::SeqCst);
    let store = Arc::new(Store::open(file.path()).unwrap());
    let approver = approver(store.clone(), &server);
    assert!(matches!(
        approver.run_approve(&id).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    assert_eq!(posts(&server).await, 0);
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "error"
    );
}

#[tokio::test]
async fn concurrent_direct_approvals_cannot_send_twice() {
    let server = server(Arc::new(AtomicBool::new(false))).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(Store::open(file.path()).unwrap());
    let id = seed(&store);
    let a = approver(store.clone(), &server);
    let second_connection = Arc::new(Store::open(file.path()).unwrap());
    let b = approver(second_connection, &server);
    let _ = tokio::join!(a.run_approve(&id), b.run_approve(&id));
    assert_eq!(posts(&server).await, 1);
}

struct ClaimDuringRedraft {
    store: Arc<Store>,
    id: String,
    claim: bool,
}
#[async_trait]
impl augmentagent_channel_core::Reasoner for ClaimDuringRedraft {
    async fn call(
        &self,
        _: &augmentagent_channel_core::ReasonerOpts,
        _: &str,
    ) -> anyhow::Result<String> {
        if self.claim {
            assert!(self.store.claim_action_for_send(
                &self.id,
                ActionStatus::Pending,
                "concurrent-approval"
            )?);
        }
        Ok("replacement draft".into())
    }
}

#[tokio::test]
async fn linkedin_revision_cannot_reopen_a_claimed_send() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(temp.path().join("db")).unwrap());
    let id = seed(&store);
    let mut approver = crate::test_support::approver_with_store(store.clone());
    approver.reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(
            augmentagent_channel_core::providers::ProviderKind::Claude,
            Arc::new(ClaimDuringRedraft {
                store: store.clone(),
                id: id.clone(),
                claim: true,
            }),
        )],
        augmentagent_channel_core::CooldownLatch::at(temp.path().join("cooldown")),
    ));
    let before = store.get_action_with_email(&id).unwrap().unwrap();
    let outcome = approver.revise_linkedin(&id, "shorter", before).await;
    assert!(matches!(
        outcome,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    let after = store.get_action_with_email(&id).unwrap().unwrap();
    assert_eq!(after.action.status, "sending");
    assert_eq!(
        after.action.draft_body.as_deref(),
        Some("Approved synthetic reply")
    );
    assert!(!store
        .claim_action_for_send(&id, ActionStatus::Pending, "replay")
        .unwrap());
}

#[test]
fn linkedin_skip_cannot_overwrite_a_claimed_or_completed_send() {
    for finish in [false, true] {
        let store = Arc::new(Store::open(":memory:").unwrap());
        let id = seed(&store);
        let stale = store.get_action_with_email(&id).unwrap().unwrap();
        assert!(store
            .claim_action_for_send(&id, ActionStatus::Pending, "approval")
            .unwrap());
        if finish {
            assert!(store.finish_send_sent(&id, "approval").unwrap());
        }
        let approver = crate::test_support::approver_with_store(store.clone());
        assert!(matches!(
            approver.skip_linkedin(&id, stale),
            ApprovalActionOutcome::AlreadyResolved { .. }
        ));
        assert_eq!(
            store
                .get_action_with_email(&id)
                .unwrap()
                .unwrap()
                .action
                .status,
            if finish { "sent" } else { "sending" }
        );
    }
}

#[tokio::test]
async fn linkedin_stalled_send_is_unknown_and_cannot_automatically_retry() {
    let server = server(Arc::new(AtomicBool::new(false))).await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(120))
                .set_body_json(serde_json::json!({"backendUrn":"late:synthetic"})),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let store = Arc::new(Store::open(":memory:").unwrap());
    let id = seed(&store);
    let approver = Arc::new(approver(store.clone(), &server));
    let worker = approver.clone();
    let action_id = id.clone();
    let task = tokio::spawn(async move { worker.run_approve(&action_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while posts(&server).await == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("mock POST must have started before advancing fake time");
    tokio::time::pause();
    let result = tokio::time::timeout(std::time::Duration::from_secs(61), task).await;
    assert!(
        result.is_ok(),
        "send must terminate before the outer test deadline"
    );
    assert!(matches!(
        result.unwrap().unwrap(),
        ApprovalActionOutcome::Failed { .. }
    ));
    let action = store.get_action_with_email(&id).unwrap().unwrap().action;
    assert_eq!(action.status, "error");
    assert!(action.error_message.as_deref().unwrap().contains("unknown"));
    assert!(store
        .list_retryable_replies(
            "linkedin",
            chrono::Utc::now().timestamp_millis() + 60_000,
            86_400_000,
            0,
            5,
            10
        )
        .unwrap()
        .is_empty());
    assert!(matches!(
        approver.run_approve(&id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert_eq!(posts(&server).await, 1);
}

#[tokio::test]
async fn linkedin_pending_revision_and_skip_still_work() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(temp.path().join("db")).unwrap());
    let id = seed(&store);
    let mut approver = crate::test_support::approver_with_store(store.clone());
    approver.reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(
            augmentagent_channel_core::providers::ProviderKind::Claude,
            Arc::new(ClaimDuringRedraft {
                store: store.clone(),
                id: id.clone(),
                claim: false,
            }),
        )],
        augmentagent_channel_core::CooldownLatch::at(temp.path().join("cooldown")),
    ));
    assert!(matches!(
        approver.run_revise(&id, "shorter").await,
        ApprovalActionOutcome::Revised { .. }
    ));
    let revised = store.get_action_with_email(&id).unwrap().unwrap();
    assert_eq!(revised.action.status, "pending");
    assert_eq!(
        revised.action.draft_body.as_deref(),
        Some("replacement draft")
    );
    assert!(matches!(
        approver.run_skip(&id).await,
        ApprovalActionOutcome::Skipped
    ));
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "rejected"
    );
}
