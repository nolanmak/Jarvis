//! Text preparation shared with the full-text index, so both retrieval
//! paths see the same words.

use augmentagent_messages::fts;

/// Prepared text for one message: title/container context first, then the
/// visible body (HTML reduced, attachment filenames only, capped).
pub fn message_text(
    platform: &str,
    conversation_title: Option<&str>,
    container: Option<&str>,
    subject: &str,
    body: &str,
) -> String {
    let doc = fts::prepare(platform, conversation_title, container, subject, body);
    let mut out = String::new();
    for part in [doc.title.as_str(), doc.subject.as_str()] {
        if !part.is_empty() {
            out.push_str(part);
            out.push('\n');
        }
    }
    out.push_str(&doc.body);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_preparation_matches_the_fts_path() {
        let html = "<html><style>.x{}</style><body><p>Invoice &amp; receipt</p><script>evil()</script></body></html>";
        let doc = fts::prepare("gmail", None, None, "Receipt", html);
        let text = message_text("gmail", None, None, "Receipt", html);
        assert!(text.starts_with("Receipt\n"));
        assert!(text.ends_with(doc.body.trim_end()));
        assert!(!text.contains("evil") && !text.contains("style"));
    }

    #[test]
    fn chat_text_carries_conversation_context_once() {
        let t = message_text(
            "discord",
            Some("#general"),
            Some("Acme HQ"),
            "",
            "shipping friday",
        );
        assert_eq!(t, "#general Acme HQ\nshipping friday");
    }

    #[test]
    fn oversize_text_is_capped_like_the_index() {
        let big = "word ".repeat(50_000);
        assert!(message_text("imessage", None, None, "", &big).len() <= fts::MAX_BODY_BYTES + 16);
    }
}
