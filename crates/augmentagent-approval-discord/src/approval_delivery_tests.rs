//! #1436: run the production Approve HTTP tail through Serenity, recording
//! actual followup/delete requests. No Discord token or gateway is needed.
use super::*;
use mockito::{Matcher, Server};

fn component() -> serenity::all::ComponentInteraction {
    let mut message = Message::default();
    message.id = MessageId::new(444);
    message.channel_id = ChannelId::new(222);
    serde_json::from_value(serde_json::json!({
        "id":"111", "application_id":"333", "channel_id":"222",
        "data":{"custom_id":"aa:approve:calendar-test", "component_type":2},
        "token":"fixture-token", "version":1, "message":message,
        "locale":"en-US", "entitlements":[], "attachment_size_limit":10485760
    }))
    .unwrap()
}

async fn deliver(outcome: ApprovalActionOutcome, content: &str, status: usize, deletes: usize) {
    let mut server = Server::new_async().await;
    let followup = server
        .mock("POST", Matcher::Regex("/webhooks/333/fixture-token".into()))
        .match_body(Matcher::PartialJson(
            serde_json::json!({"content":content,"flags":64}),
        ))
        .with_status(status)
        .with_header("content-type", "application/json")
        .with_body(if status == 200 {
            serde_json::to_string(&Message::default()).unwrap()
        } else {
            r#"{"message":"Missing Access","code":50001}"#.into()
        })
        .expect(1)
        .create_async()
        .await;
    let delete = server
        .mock(
            "DELETE",
            Matcher::Regex("/channels/222/messages/444".into()),
        )
        .with_status(204)
        .expect(deletes)
        .create_async()
        .await;
    let http = serenity::http::HttpBuilder::new("test-token")
        .application_id(serenity::all::ApplicationId::new(333))
        .proxy(server.url())
        .ratelimiter_disabled(true)
        .build();
    let result = deliver_approval_outcome(&http, &component(), &outcome, "calendar-test").await;
    assert_eq!(result.is_ok(), status == 200, "{result:?}");
    followup.assert_async().await;
    delete.assert_async().await;
}

#[tokio::test]
async fn failure_posts_reason_without_deleting_card() {
    deliver(
        ApprovalActionOutcome::Failed {
            message: "send_updates must be boolean".into(),
        },
        "Failed: send_updates must be boolean",
        200,
        0,
    )
    .await;
}

#[tokio::test]
async fn calendar_success_posts_receipt_then_deletes_card() {
    deliver(
        ApprovalActionOutcome::CalendarCreated {
            event_id: "evt-1436".into(),
            html_link: None,
        },
        "Calendar event created. Event ID: evt-1436.",
        200,
        1,
    )
    .await;
}

#[tokio::test]
async fn undelivered_success_receipt_keeps_the_card() {
    deliver(
        ApprovalActionOutcome::CalendarCreated {
            event_id: "evt-1436".into(),
            html_link: None,
        },
        "Calendar event created. Event ID: evt-1436.",
        403,
        0,
    )
    .await;
}
