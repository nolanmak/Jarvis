use augmentagent_approval_discord::{approval_edit_message, approval_message};
use augmentagent_store::Email;
use serde::Serialize;
use serde_json::Value;

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

fn main() {
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

    println!(
        "{}",
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
    );
}
