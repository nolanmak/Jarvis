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
    for (link, expected) in [
        (None, "Calendar event created on owner+work@example.test (primary calendar). Event ID: evt-1436."),
        (Some("https://calendar.google.com/event?eid=test&authuser=owner%2Bwork%40example.test"), "Calendar event created on owner+work@example.test (primary calendar): https://calendar.google.com/event?eid=test&authuser=owner%2Bwork%40example.test (event ID: evt-1436)."),
    ] {
        deliver(
            ApprovalActionOutcome::CalendarCreated {
                organizer_account: "owner+work@example.test".into(),
                event_id: "evt-1436".into(),
                html_link: link.map(str::to_string),
                already_existed: false,
            },
            expected, 200, 1,
        ).await;
    }
}

#[tokio::test]
async fn undelivered_success_receipt_keeps_the_card() {
    deliver(
        ApprovalActionOutcome::CalendarCreated {
            organizer_account: "owner+work@example.test".into(),
            event_id: "evt-1436".into(),
            html_link: None,
            already_existed: false,
        },
        "Calendar event created on owner+work@example.test (primary calendar). Event ID: evt-1436.",
        403,
        0,
    )
    .await;
}

#[tokio::test]
async fn recovered_calendar_receipt_is_delivered_before_retiring_card() {
    deliver(
        ApprovalActionOutcome::CalendarCreated {
            organizer_account: "owner+work@example.test".into(),
            event_id: "evt-1436".into(),
            html_link: Some("https://calendar.google.com/event?eid=test&authuser=owner%2Bwork%40example.test".into()),
            already_existed: true,
        },
        "Calendar event already created on owner+work@example.test (primary calendar): https://calendar.google.com/event?eid=test&authuser=owner%2Bwork%40example.test (event ID: evt-1436).",
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

#[tokio::test]
async fn signing_receipt_delivered_before_card_retirement() {
    for status in [200,403] {
        deliver(ApprovalActionOutcome::DocumentSigned { receipt:"ATTACH: signed.pdf\nSHA-256: fixture".into(),already_existed:false, output: None },
            "Document signed. ATTACH: signed.pdf\nSHA-256: fixture",status,if status==200 {1}else{0}).await;
    }
}

#[tokio::test]
async fn signed_pdf_is_attached_and_a_changed_file_keeps_the_card() {
    let dir=tempfile::tempdir().unwrap(); let file=dir.path().join("signed.pdf");
    let pdf=b"%PDF-1.4\nsynthetic signed document\n%%EOF\n"; std::fs::write(&file,pdf).unwrap();
    let outcome=ApprovalActionOutcome::DocumentSigned { receipt:"Ready".into(),already_existed:false,output:Some((file.clone(),augmentagent_docs::signing::digest(pdf))) };
    let mut server=Server::new_async().await;
    let followup=server.mock("POST",Matcher::Regex("/webhooks/333/fixture-token".into()))
        .match_body(Matcher::Regex("(?s)filename=\"signed.pdf\".*synthetic signed document".into()))
        .with_status(200).with_header("content-type","application/json")
        .with_body(serde_json::to_string(&Message::default()).unwrap()).expect(1).create_async().await;
    let delete=server.mock("DELETE",Matcher::Regex("/channels/222/messages/444".into())).with_status(204).expect(1).create_async().await;
    let http=serenity::http::HttpBuilder::new("test-token").application_id(serenity::all::ApplicationId::new(333)).proxy(server.url()).ratelimiter_disabled(true).build();
    deliver_approval_outcome(&http,&component(),&outcome,"signature-test").await.unwrap();
    std::fs::write(file,b"changed").unwrap();
    assert!(deliver_approval_outcome(&http,&component(),&outcome,"signature-test").await.is_err());
    followup.assert_async().await; delete.assert_async().await;
}

/// #1507 — a component carrying a real card embed, as Discord delivers it.
fn titled_component(title: &str) -> serenity::all::ComponentInteraction {
    let mut click = component();
    click.data.custom_id = "aa:sig-1507:approve".into();
    let mut embed: serenity::all::Embed = serde_json::from_value(serde_json::json!({"title": title})).unwrap();
    embed.title = Some(title.into());
    click.message.embeds = vec![embed];
    click
}

#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().extend_from_slice(b); Ok(b.len()) }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer { self.clone() }
}

/// #1507 AC7/AC9/AC10 — the reply names the card it acted on, the signed file
/// keeps its real filename, and the resolution is logged with the action id.
#[tokio::test(flavor = "current_thread")]
async fn approve_reply_names_the_card_and_logs_the_resolution() {
    // Process-wide: a thread-scoped subscriber races the callsite interest
    // cache against tests running on other threads. Lines are filtered by
    // this test's unique action id below.
    static LOGS: std::sync::OnceLock<Captured> = std::sync::OnceLock::new();
    let logs = LOGS.get_or_init(|| {
        let logs = Captured::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt().with_writer(logs.clone()).with_ansi(false).finish(),
        )
        .expect("no other global subscriber in this test binary");
        logs
    });
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("zz-letter-signed.pdf");
    let pdf = b"%PDF-1.4\nsynthetic signed letter\n%%EOF\n";
    std::fs::write(&file, pdf).unwrap();
    let outcome = ApprovalActionOutcome::DocumentSigned {
        receipt: "Ready".into(),
        already_existed: false,
        output: Some((file, augmentagent_docs::signing::digest(pdf))),
    };
    let mut server = Server::new_async().await;
    let followup = server
        .mock("POST", Matcher::Regex("/webhooks/333/fixture-token".into()))
        .match_body(Matcher::AllOf(vec![
            Matcher::Regex(r"\*\*Sign PDF: deliverables/zz-letter\.pdf\*\* (—|\\u2014) Document signed".into()),
            Matcher::Regex(r#"filename="zz-letter-signed\.pdf""#.into()),
            Matcher::Regex("synthetic signed letter".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::to_string(&Message::default()).unwrap())
        .expect(1)
        .create_async()
        .await;
    let _delete = server
        .mock("DELETE", Matcher::Regex("/channels/222/messages/444".into()))
        .with_status(204)
        .create_async()
        .await;
    let http = serenity::http::HttpBuilder::new("test-token")
        .application_id(serenity::all::ApplicationId::new(333))
        .proxy(server.url())
        .ratelimiter_disabled(true)
        .build();
    let click = titled_component("Sign PDF: deliverables/zz-letter.pdf");
    deliver_approval_outcome(&http, &click, &outcome, "sig-1507").await.unwrap();
    followup.assert_async().await;
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<_> = text
        .lines()
        .filter(|l| l.contains("discord card control resolved") && l.contains("action_id=sig-1507"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(
        lines[0].contains("INFO") && lines[0].contains("verb=\"approve\"") && lines[0].contains("outcome=DocumentSigned"),
        "{}",
        lines[0]
    );
    assert!(!text.contains("synthetic signed letter"), "payload must not reach the log");
}
