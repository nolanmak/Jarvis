//! Jarvis Dashboard `action.resolve` (spec web-dashboard-v2 D5, §5.2.2):
//! decided through the daemon's real `ReplyApprover` wrapped for origin
//! `web`, exactly once against a simulated Discord button, and redrawing the
//! Discord card. The Slack send API is a local mock; data is synthetic.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_approval_discord::{
    ApprovalActionHandler, ApprovalActionOutcome, ApprovalCardSurface, CardSurfaces,
    SyncingActionHandler,
};
use serde_json::json;

use crate::slack_approval_tests::{approver, pending_slack_reply, send_mock, status, store};
use crate::web_surface::{resolve_action, web_outcome_json};

/// Stands in for the Discord bot's card surface: records redraw requests.
struct DiscordCards(Mutex<Vec<(String, String)>>);

#[async_trait]
impl ApprovalCardSurface for DiscordCards {
    fn surface_name(&self) -> &'static str {
        "discord"
    }
    async fn redraw_cards(&self, action_id: &str, origin: &str) {
        self.0
            .lock()
            .unwrap()
            .push((action_id.into(), origin.into()));
    }
}

#[tokio::test]
async fn web_and_discord_racing_on_one_action_resolve_it_once_and_redraw_discord() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let mut server = mockito::Server::new_async().await;
    let sends = send_mock(&mut server, 1).await;
    let approver: Arc<dyn ApprovalActionHandler> =
        Arc::new(approver(Arc::clone(&store), &server.url()));
    let cards = Arc::new(DiscordCards(Mutex::default()));
    let surfaces = CardSurfaces::new();
    let card: Arc<dyn ApprovalCardSurface> = cards.clone();
    surfaces.register(&card);
    let web = SyncingActionHandler::new("web", Arc::clone(&approver), surfaces.clone());
    let discord = SyncingActionHandler::new("discord", Arc::clone(&approver), surfaces.clone());

    let (from_web, from_discord) = tokio::join!(
        resolve_action(&web, &id, "approve", None),
        discord.approve(&id)
    );
    sends.assert_async().await;
    let web_won = from_web.is_ok();
    let discord_won = matches!(from_discord, ApprovalActionOutcome::Approved);
    assert!(web_won ^ discord_won, "{from_web:?} / {from_discord:?}");
    if web_won {
        assert_eq!(
            from_web.unwrap(),
            json!({"outcome": {"kind": "approved", "summary": "Approved and sent."}})
        );
        assert!(matches!(
            from_discord,
            ApprovalActionOutcome::AlreadyResolved { .. }
        ));
        assert_eq!(
            store.action_status_source(&id).unwrap().as_deref(),
            Some("web")
        );
    } else {
        assert_eq!(from_web.unwrap_err().code, "already_resolved");
    }
    assert_eq!(status(&store, &id), "sent");
    // The web-origin resolve redrew the Discord card (it always redraws,
    // win or lose); Discord's own click redraws nothing on itself.
    assert_eq!(
        *cards.0.lock().unwrap(),
        vec![(id.clone(), "web".to_string())]
    );

    // A later web resolve gets already_resolved and sends nothing.
    assert_eq!(
        resolve_action(&web, &id, "skip", None)
            .await
            .unwrap_err()
            .code,
        "already_resolved"
    );
    sends.assert_async().await;
}

#[tokio::test]
async fn web_resolve_validates_verbs_and_notes() {
    let (_d, store) = store();
    let id = pending_slack_reply(&store);
    let approver = approver(Arc::clone(&store), "http://127.0.0.1:9");
    let code =
        |r: Result<serde_json::Value, crate::web_surface_transport::OpError>| r.unwrap_err().code;
    assert_eq!(
        code(resolve_action(&approver, &id, "revise", None).await),
        "bad_request"
    );
    assert_eq!(
        code(resolve_action(&approver, &id, "revise", Some("  ")).await),
        "bad_request"
    );
    assert_eq!(
        code(resolve_action(&approver, &id, "send_now", None).await),
        "bad_request"
    );
    assert_eq!(
        code(resolve_action(&approver, "", "approve", None).await),
        "bad_request"
    );
    assert_eq!(
        code(resolve_action(&approver, "missing", "approve", None).await),
        "action_not_found"
    );
    assert_eq!(status(&store, &id), "pending", "nothing was decided");
}

#[test]
fn web_outcome_json_covers_every_outcome_variant() {
    use ApprovalActionOutcome as O;
    let email = augmentagent_store::Email {
        message_id: "m".into(),
        thread_id: None,
        from: "a@example.com".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "s".into(),
        body: "b".into(),
        date: String::new(),
        account_entity_id: None,
        platform: "gmail".into(),
        kind: "email".into(),
    };
    let cases = vec![
        (
            O::Approved,
            json!({"kind": "approved", "summary": "Approved and sent."}),
        ),
        (
            O::CalendarCreated {
                organizer_account: "a".into(),
                event_id: "e".into(),
                html_link: None,
                join_url: None,
                already_existed: false,
            },
            json!({"kind": "calendar_created", "summary": "Calendar event created."}),
        ),
        (
            O::CalendarResponded {
                account: "a".into(),
                response: "accepted".into(),
                summary: "Lunch".into(),
            },
            json!({"kind": "calendar_responded", "summary": "Responded accepted to Lunch."}),
        ),
        (
            O::CalendarRescheduled {
                account: "a".into(),
                summary: "Lunch".into(),
                when: "Tue 15:00".into(),
                notified: true,
                already_there: false,
            },
            json!({"kind": "calendar_rescheduled", "summary": "Lunch moved to Tue 15:00."}),
        ),
        (
            O::DocumentSigned {
                receipt: "r".into(),
                already_existed: false,
                output: None,
            },
            json!({"kind": "document_signed", "summary": "Document signed."}),
        ),
        (
            O::ManualSendReady {
                text: "hi".into(),
                link: Some("https://wa.me/1".into()),
            },
            json!({"kind": "manual_send_ready", "summary": "Not sent: send this yourself.",
                "text": "hi", "link": "https://wa.me/1"}),
        ),
        (
            O::Skipped,
            json!({"kind": "skipped", "summary": "Skipped."}),
        ),
        (
            O::Revised {
                email,
                draft: "new".into(),
            },
            json!({"kind": "revised", "summary": "Revised: a new draft is waiting for approval."}),
        ),
        (
            O::Scheduled {
                at_ms: 1,
                local: "Tue 09:00".into(),
            },
            json!({"kind": "scheduled", "summary": "Scheduled for Tue 09:00.", "local": "Tue 09:00"}),
        ),
        (
            O::Unscheduled,
            json!({"kind": "unscheduled", "summary": "Back in the queue."}),
        ),
        (
            O::CancelledSchedule,
            json!({"kind": "cancelled_schedule", "summary": "Scheduled send cancelled."}),
        ),
        (
            O::Recomposed,
            json!({"kind": "recomposed", "summary": "Draft restored for approval."}),
        ),
        (
            O::Failed {
                message: "boom".into(),
            },
            json!({"kind": "failed", "summary": "Failed: the action is still pending.", "message": "boom"}),
        ),
    ];
    for (outcome, expected) in cases {
        assert_eq!(web_outcome_json(&outcome).unwrap(), expected, "{outcome:?}");
    }
    assert_eq!(
        web_outcome_json(&O::NotFound).unwrap_err().code,
        "action_not_found"
    );
    let resolved = O::AlreadyResolved {
        status: "sent".into(),
        detail: None,
    };
    assert_eq!(
        web_outcome_json(&resolved).unwrap_err().code,
        "already_resolved"
    );
}
