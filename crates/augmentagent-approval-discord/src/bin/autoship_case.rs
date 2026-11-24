use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_store::{
    alert_schedule::AlertDetails,
    owner_alerts::{AlertState, NewOwnerAlert, Urgency},
    Email, Store,
};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Broker {
    posts: AtomicUsize,
}

#[async_trait]
impl ApprovalBroker for Broker {
    async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
        panic!("digest must bypass the approval carousel")
    }

    async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
        self.posts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn post_owner_alert(
        &self,
        _: &augmentagent_store::alert_schedule::AlertNotice,
    ) -> Result<String, ApprovalError> {
        panic!("seeded alerts must not produce ordinary owner-alert posts")
    }
}

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("digest.db");
    let seeded_at = chrono::DateTime::parse_from_rfc3339("2026-11-18T15:00:00Z")
        .unwrap()
        .timestamp_millis();
    let morning = chrono::DateTime::parse_from_rfc3339("2026-11-19T14:00:00Z")
        .unwrap()
        .timestamp_millis();
    let store = Store::open(&path).unwrap();
    store
        .insert_owner_alert(
            &NewOwnerAlert {
                id: "digest-subject",
                source_url: "https://example.test/alert",
                sender: "sender-handle",
                action: "private-body-marker",
                reason: "private-reason-marker",
                urgency: Urgency::High,
                due_at_ms: None,
                timezone: "America/New_York",
                meeting_id: None,
                expires_at_ms: morning + 86_400_000,
                text_after_ms: morning + 86_400_000,
            },
            seeded_at,
        )
        .unwrap();
    store
        .attach_owner_alert_details(
            "digest-subject",
            &AlertDetails {
                message_id: "message".into(),
                thread_id: None,
                account_id: None,
                subject: "Eligible subject".into(),
                evidence: "private-body-marker".into(),
                deadline_kind: "explicit".into(),
                meeting_start_ms: None,
                meeting_url: None,
                reply_resolves: false,
            },
            seeded_at,
        )
        .unwrap();
    let notice = store.claim_owner_alert_notice(seeded_at).unwrap().unwrap();
    store
        .complete_owner_alert_notice(notice.id, Some("https://example.test"), None, seeded_at)
        .unwrap();
    store
        .set_owner_alert_state("digest-subject", AlertState::Acknowledged, seeded_at + 1)
        .unwrap();

    let broker = Broker { posts: AtomicUsize::new(0) };
    std::env::remove_var("AUGMENTAGENT_ALERT_DIGEST");
    augmentagent_approval_discord::owner_alerts::tick(&store, &broker, morning)
        .await
        .unwrap();
    let disabled = broker.posts.load(Ordering::SeqCst);

    std::env::set_var("AUGMENTAGENT_ALERT_DIGEST", "1");
    augmentagent_approval_discord::owner_alerts::tick(&store, &broker, morning)
        .await
        .unwrap();
    let first = broker.posts.load(Ordering::SeqCst);
    drop(store);

    let reopened = Store::open(&path).unwrap();
    augmentagent_approval_discord::owner_alerts::tick(&reopened, &broker, morning + 60_000)
        .await
        .unwrap();
    let after_restart = broker.posts.load(Ordering::SeqCst);
    println!("{}", serde_json::json!([disabled, first, after_restart]));
}
