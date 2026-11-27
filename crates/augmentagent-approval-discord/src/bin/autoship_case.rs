use async_trait::async_trait;
use augmentagent_approval_discord::{
    approval_edit_message, approval_message, ApprovalBroker, ApprovalError,
};
use augmentagent_store::{
    alert_schedule::AlertDetails,
    owner_alerts::{AlertState, NewOwnerAlert, Urgency},
    Email, Store,
};
use serde::Serialize;
use serde_json::Value;
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

fn email(body: &str) -> Email {
    Email {
        message_id: "renderer-fixture".into(),
        thread_id: Some("renderer-thread".into()),
        from: "sender@example.test".into(),
        to: "owner@example.test".into(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "Renderer fixture".into(),
        body: body.into(),
        date: "2026-01-01".into(),
        account_entity_id: None,
        platform: "gmail".into(),
        kind: "dm".into(),
    }
}

fn descriptions<T: Serialize>(message: &T) -> String {
    fn collect(value: &Value, output: &mut Vec<String>) {
        match value {
            Value::Array(values) => values.iter().for_each(|value| collect(value, output)),
            Value::Object(values) => {
                for (key, value) in values {
                    if key == "description" {
                        if let Value::String(text) = value {
                            output.push(text.clone());
                        }
                    }
                    collect(value, output);
                }
            }
            _ => {}
        }
    }

    let mut output = Vec::new();
    collect(&serde_json::to_value(message).expect("Discord builder serializes"), &mut output);
    output.join("\n")
}

/// #2426 — the Discord renderers' projection of a stored inbound body.
fn renderer_observation() -> Value {
    let html = r#"<!doctype html><html><head><title>HEAD_SENTINEL</title><style>STYLE_SENTINEL</style><script>DROP_SCRIPT()</script></head><body><p>Hello &amp; welcome <a href="https://example.test/destination">portal</a>.</p></body></html>"#;
    let multipart = "Content-Type: multipart/alternative; boundary=renderer-boundary\r\n\r\n--renderer-boundary\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nPLAIN_ALTERNATIVE_SENTINEL\r\n--renderer-boundary\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>HTML_FALLBACK_SENTINEL</p>\r\n--renderer-boundary--\r\n";
    let draft = "Draft response";
    let html_email = email(html);
    let multipart_email = email(multipart);
    let stored_before = serde_json::to_string(&html_email).expect("email serializes");

    let approval_html = descriptions(&approval_message("renderer-action", &html_email, draft, 0));
    let edit_html = descriptions(&approval_edit_message("renderer-action", &html_email, draft, 0));
    let approval_multipart = descriptions(&approval_message("renderer-action", &multipart_email, draft, 0));
    let edit_multipart = descriptions(&approval_edit_message("renderer-action", &multipart_email, draft, 0));
    let html_previews = format!("{approval_html}\n{edit_html}");

    let plain = descriptions(&approval_message(
        "renderer-action",
        &email("PLAIN_ORIGINAL"),
        draft,
        0,
    ));
    let long_body = "X".repeat(8_000);
    let long_draft = "Y".repeat(8_000);
    let long = descriptions(&approval_message(
        "renderer-action",
        &email(&long_body),
        &long_draft,
        0,
    ));

    let readable_html = |text: &str| {
        text.contains("Hello & welcome")
            && !text.contains("HEAD_SENTINEL")
            && !text.contains("DROP_SCRIPT")
            && !text.contains("STYLE_SENTINEL")
            && !text.contains('<')
            && !text.contains('>')
    };
    let plain_alternative = |text: &str| {
        text.contains("PLAIN_ALTERNATIVE_SENTINEL") && !text.contains("HTML_FALLBACK_SENTINEL")
    };

    serde_json::json!({
        "approval_html": readable_html(&approval_html),
        "edit_html": readable_html(&edit_html),
        "approval_multipart": plain_alternative(&approval_multipart),
        "edit_multipart": plain_alternative(&edit_multipart),
        "html_entities_decoded": html_previews.contains("Hello & welcome"),
        "html_suppressed_content_absent": !html_previews.contains("HEAD_SENTINEL")
            && !html_previews.contains("DROP_SCRIPT")
            && !html_previews.contains("STYLE_SENTINEL"),
        "html_tags_absent": !html_previews.contains('<') && !html_previews.contains('>'),
        "differing_link_destination_visible": html_previews.contains("portal")
            && html_previews.contains("https://example.test/destination"),
        "plain_format_preserved": plain.contains("PLAIN_ORIGINAL\n\n— DRAFT —\n\nDraft response"),
        "truncation_preserved": long.len() <= 3_800,
        "persisted_email_json_unchanged": serde_json::to_string(&html_email)
            .expect("email serializes") == stored_before
    })
}

#[tokio::main]
async fn main() {
    println!("{}", renderer_observation());

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
