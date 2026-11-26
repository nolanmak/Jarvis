//! #2538 approver-flow tests for `socialapi_compose_identity`.
//!
//! These drive `ReplyApprover`, which only exists in the main binary, so they
//! live outside `socialapi_compose_identity.rs`: that file is also compiled
//! into the `autoship_case` binary via `#[path]`.

use crate::socialapi_compose_identity::tests::{action, card, fixture, CONVERSATION};
use crate::socialapi_compose_identity::{pending_compose_cards, supersede_prior_compose_cards};
use crate::*;

/// #2538 — one `socialapi dm --post`: the new card goes up, then the
/// earlier pending compose cards are retired, as `run_socialapi_dm` does.
fn post(store: &Store) -> String {
    let prior = pending_compose_cards(store, CONVERSATION).unwrap();
    let id = action(store, &card(store, None));
    supersede_prior_compose_cards(store, &prior);
    id
}

struct Redraft;
#[async_trait::async_trait]
impl augmentagent_channel_core::Reasoner for Redraft {
    async fn call(
        &self,
        opts: &augmentagent_channel_core::reasoner::ReasonerOpts,
        _prompt: &str,
    ) -> anyhow::Result<String> {
        if opts.system_prompt.contains("You select evidence") {
            return Ok(r#"{"action":"ready","facts":[],"gaps":[]}"#.into());
        }
        Ok("Revised synthetic draft".into())
    }
}

async fn approver(
    dir: &tempfile::TempDir,
    store: &Arc<Store>,
    sends: u64,
) -> (ReplyApprover, wiremock::MockServer) {
    use augmentagent_channel_core::{cooldown::CooldownLatch, providers::ProviderKind};
    use wiremock::matchers::{method, path};
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path(format!(
            "/inbox/conversations/{CONVERSATION}/messages"
        )))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"id": "synthetic-sent"})),
        )
        .expect(sends)
        .mount(&server)
        .await;
    let mut approver = crate::test_support::approver_with_store(Arc::clone(store));
    approver.reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(ProviderKind::Claude, Arc::new(Redraft))],
        CooldownLatch::at(dir.path().join("cooldowns.json")),
    ));
    approver.socialapi = Some(Arc::new(
        augmentagent_channel_socialapi::SocialApiClient::with_base_url(
            augmentagent_channel_socialapi::SocialApiAuth::new("synthetic-key"),
            server.uri(),
        ),
    ));
    (approver, server)
}

fn status(store: &Store, id: &str) -> String {
    store
        .get_action_with_email(id)
        .unwrap()
        .unwrap()
        .action
        .status
}

/// #2538 — post, post again, then Revise on the card the owner still
/// sees: the revise must land on a live action (not "a newer version
/// replaced this draft"), leave exactly one live card, and Send on it
/// must execute.
#[tokio::test]
async fn revise_on_a_replaced_card_leaves_one_live_card_that_sends() {
    let (dir, store) = fixture();
    let store = Arc::new(store);
    let first = post(&store);
    let second = post(&store);
    assert_eq!(
        pending_compose_cards(&store, CONVERSATION).unwrap(),
        vec![second.clone()]
    );
    let (approver, _server) = approver(&dir, &store, 1).await;

    let revised = approver.revise(&first, "make it warmer").await;
    assert!(
        matches!(revised, ApprovalActionOutcome::Revised { .. }),
        "{revised:?}"
    );
    assert_eq!(
        pending_compose_cards(&store, CONVERSATION).unwrap(),
        vec![first.clone()],
        "Revise must leave exactly one live card: the one it reposts"
    );
    assert_eq!(status(&store, &second), "superseded");

    let sent = approver.approve(&first).await;
    assert!(matches!(sent, ApprovalActionOutcome::Approved), "{sent:?}");
    assert_eq!(status(&store, &first), "sent");
}

/// #2538 — the stranded state from the issue: every compose card for the
/// conversation is superseded. Send on one must still go somewhere live.
#[tokio::test]
async fn send_on_a_stranded_conversation_reopens_and_sends_the_clicked_card() {
    let (dir, store) = fixture();
    let store = Arc::new(store);
    let first = post(&store);
    let second = post(&store);
    store
        .mark_pending_superseded_by_ids(&[second.clone()], "superseded by follow-up compose")
        .unwrap();
    assert!(pending_compose_cards(&store, CONVERSATION)
        .unwrap()
        .is_empty());
    let (approver, _server) = approver(&dir, &store, 1).await;

    let sent = approver.approve(&first).await;
    assert!(matches!(sent, ApprovalActionOutcome::Approved), "{sent:?}");
    assert_eq!(status(&store, &first), "sent");
    assert_eq!(status(&store, &second), "superseded");
}

/// #2538 — while a live card exists, Send on a stale one never ships the
/// stale text: it points at the live card, which stays actionable. And a
/// card the owner already settled (sent) is never reopened behind it.
#[tokio::test]
async fn stale_send_points_at_the_live_card_and_settled_threads_stay_closed() {
    let (dir, store) = fixture();
    let store = Arc::new(store);
    let first = post(&store);
    let second = post(&store);
    let (approver, _server) = approver(&dir, &store, 0).await;

    let stale = approver.approve(&first).await;
    assert!(
        matches!(&stale, ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "superseded"),
        "{stale:?}"
    );
    assert_eq!(
        pending_compose_cards(&store, CONVERSATION).unwrap(),
        vec![second.clone()]
    );

    store
        .update_action_status(&second, ActionStatus::Sent, None, None)
        .unwrap();
    for outcome in [
        approver.approve(&first).await,
        approver.revise(&first, "again").await,
    ] {
        assert!(
            matches!(outcome, ApprovalActionOutcome::AlreadyResolved { .. }),
            "{outcome:?}"
        );
    }
    assert_eq!(status(&store, &first), "superseded");
}
