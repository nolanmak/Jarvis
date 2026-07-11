//! Discord journaling-session write path helpers (#428).
//!
//! `!journal <text>` saves the message verbatim; `!journal done` runs a
//! small no-tools Haiku call that turns the recent Discord conversation
//! into a first-person entry. Both end up as TinyMCE-style HTML because
//! that's what the ShadowNote editor renders.

use augmentagent_channel_core::reasoner::ReasonerOpts;

/// System prompt for the compose call. No tools, no wiki access — the
/// conversation excerpt is the entire input, the entry is the entire
/// output. Journal text is sensitive; this call must stay side-effect
/// free.
pub const COMPOSE_SYSTEM_PROMPT: &str = "You turn a Discord conversation between a user and their \
assistant into the user's journal entry.\n\
\n\
Rules:\n\
- Write in first person, in the user's voice, using ONLY what the user \
actually said. The assistant's prompts and questions are context, never \
content.\n\
- Keep the user's tone and level of detail; do not embellish, moralize, \
or add advice.\n\
- Output format, exactly:\n\
  Line 1: `TITLE: <concise title, max 8 words>`\n\
  Line 2: blank\n\
  Then the entry body as simple HTML: one `<p>…</p>` per paragraph, no \
other tags, no markdown, no preamble, no closing commentary.";

/// User message for the compose call.
pub fn compose_user_message(history: &str) -> String {
    format!(
        "Compose today's journal entry from this conversation.\n\n<conversation>\n{history}\n</conversation>"
    )
}

/// No-tools Haiku opts for the compose call.
pub fn compose_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: COMPOSE_SYSTEM_PROMPT.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: Vec::new(),
        add_dirs: Vec::new(),
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
        settings_json: None,
        restrict_env: false,
        audit_logger: None,
        audit_notifier: None,
        session_id: None,
    }
}

/// Escape + wrap plain text into the paragraph HTML the app's editor
/// renders: blank lines split paragraphs, single newlines become `<br />`.
pub fn text_to_paragraphs(text: &str) -> String {
    fn escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    text.split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| format!("<p>{}</p>", escape(p).replace('\n', "<br />")))
        .collect::<Vec<_>>()
        .join("")
}

/// Parse the compose call's output: optional `TITLE:` first line, then the
/// HTML body. Tolerates a model that forgot the format — the whole text
/// becomes the (paragraph-wrapped) body.
pub fn parse_composed_entry(raw: &str) -> (Option<String>, String) {
    let trimmed = raw.trim();
    let (title, body) = match trimmed.strip_prefix("TITLE:") {
        Some(rest) => match rest.split_once('\n') {
            Some((t, b)) => {
                let t = t.trim();
                ((!t.is_empty()).then(|| t.to_string()), b.trim())
            }
            None => (None, trimmed),
        },
        None => (None, trimmed),
    };
    let html = if body.contains("<p") {
        body.to_string()
    } else {
        text_to_paragraphs(body)
    };
    (title, html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_escape_and_split() {
        assert_eq!(
            text_to_paragraphs("a & b\nc\n\nsecond <one>"),
            "<p>a &amp; b<br />c</p><p>second &lt;one&gt;</p>"
        );
    }

    #[test]
    fn empty_text_yields_no_paragraphs() {
        assert_eq!(text_to_paragraphs("  \n\n  "), "");
    }

    #[test]
    fn composed_entry_with_title_and_html() {
        let raw = "TITLE: A quiet Friday\n\n<p>Slept well.</p><p>Shipped the crate.</p>";
        let (title, html) = parse_composed_entry(raw);
        assert_eq!(title.as_deref(), Some("A quiet Friday"));
        assert_eq!(html, "<p>Slept well.</p><p>Shipped the crate.</p>");
    }

    #[test]
    fn composed_entry_plaintext_body_gets_wrapped() {
        let (title, html) = parse_composed_entry("TITLE: T\n\njust text");
        assert_eq!(title.as_deref(), Some("T"));
        assert_eq!(html, "<p>just text</p>");
    }

    #[test]
    fn composed_entry_without_title_falls_back() {
        let (title, html) = parse_composed_entry("plain reflection, no format");
        assert_eq!(title, None);
        assert_eq!(html, "<p>plain reflection, no format</p>");
    }
}
