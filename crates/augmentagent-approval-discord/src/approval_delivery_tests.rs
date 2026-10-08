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
            already_existed: false,
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
            already_existed: false,
        },
        "Calendar event created. Event ID: evt-1436.",
        403,
        0,
    )
    .await;
}

#[tokio::test]
async fn recovered_calendar_receipt_is_delivered_before_retiring_card() {
    deliver(
        ApprovalActionOutcome::CalendarCreated {
            event_id: "evt-1436".into(),
            html_link: Some("https://calendar.google.com/event?eid=test".into()),
            already_existed: true,
        },
        "Calendar event already created: https://calendar.google.com/event?eid=test (event ID: evt-1436).",
        200, 1,
    ).await;
}

struct RecordedApproval(std::sync::Arc<augmentagent_store::Store>);
#[async_trait::async_trait]
impl crate::ApprovalActionHandler for RecordedApproval {
    async fn approve(&self,id:&str)->ApprovalActionOutcome {
        let ctx=crate::interaction::current().expect("transport metadata must reach handler");
        assert_eq!(ctx.actor,"777");assert_eq!(ctx.interaction_id,"111");
        assert_eq!(ctx.conversation,"222");assert!(ctx.revision.is_some());
        let seq=self.0.begin_approval(id,&ctx,"approve",ctx.revision.as_deref().unwrap(),"gcal:create_event","Meeting").unwrap().unwrap();
        self.0.finish_approval(seq,"failed","provider rejected request").unwrap();
        ApprovalActionOutcome::Failed{message:"provider rejected request".into()}
    }
    async fn skip(&self,_:&str)->ApprovalActionOutcome {panic!("wrong verb")}
    async fn revise(&self,_:&str,_:&str)->ApprovalActionOutcome {panic!("wrong verb")}
    async fn is_resolved(&self,_:&str)->bool {false}
}

#[tokio::test]
async fn serenity_click_records_actor_and_decision_even_when_receipt_delivery_fails() {
    let d=tempfile::tempdir().unwrap();let store=std::sync::Arc::new(augmentagent_store::Store::open(d.path().join("db")).unwrap());
    let handler=std::sync::Arc::new(RecordedApproval(store.clone())) as std::sync::Arc<dyn crate::ApprovalActionHandler>;
    let mut click=component();click.user.id=UserId::new(777);
    click.data.custom_id=CustomId::new("action-1446",Verb::Approve).with_revision("draft").to_string();
    // Unauthorized serialized interaction cannot reach the effect or audit store.
    let denied=dispatch_component_approval(&click,Some(UserId::new(888)),Some(handler.clone())).await;
    assert!(matches!(denied,ApprovalActionOutcome::Failed{..}));
    assert!(store.approval_history(None,20,None).unwrap().is_empty());
    let outcome=tokio::spawn(async move {dispatch_component_approval(&click,Some(UserId::new(777)),Some(handler)).await}).await.unwrap();
    // Exercise actual Serenity followup serialization and failure handling.
    deliver(outcome,"Failed: provider rejected request",403,0).await;
    let rows=store.approval_history(None,20,None).unwrap();assert_eq!(rows.len(),1);
    assert_eq!(rows[0].verb,"approve");assert_eq!(rows[0].outcome,"failed");
}
