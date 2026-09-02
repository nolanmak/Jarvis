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
    (title, sanitize_paragraph_html(body))
}

/// The composer is told "one `<p>…</p>` per paragraph, no other tags", but a
/// model's formatting instruction is not a security boundary: whatever it
/// emits is encrypted, persisted, and later rendered by the ShadowNote
/// editor. Keep only bare `<p>`, `</p>` and `<br>`/`<br />`; every other tag
/// (attributes included — `<p onclick=…>`) is escaped into visible text, so a
/// stray `<img onerror=…>` can never become markup. Plain text is wrapped.
pub fn sanitize_paragraph_html(body: &str) -> String {
    if !body.contains("<p") && !body.contains("<P") {
        return text_to_paragraphs(body);
    }
    fn escape_text(s: &str) -> String {
        s.replace('<', "&lt;").replace('>', "&gt;")
    }
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find('<') {
        out.push_str(&escape_text(&rest[..i]));
        let from_tag = &rest[i..];
        let Some(j) = from_tag.find('>') else {
            out.push_str(&escape_text(from_tag));
            rest = "";
            break;
        };
        let tag = &from_tag[..=j];
        let norm: String = tag
            .to_ascii_lowercase()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        match norm.as_str() {
            "<p>" => out.push_str("<p>"),
            "</p>" => out.push_str("</p>"),
            "<br>" | "<br/>" => out.push_str("<br />"),
            _ => out.push_str(&escape_text(tag)),
        }
        rest = &from_tag[j + 1..];
    }
    out.push_str(&escape_text(rest));
    out
}

/// Entry titles come from two untrusted sources — the composer model and the
/// user's own `!journal done <title>` — and ShadowNote's title rendering
/// contract is not ours to rely on. Make markup impossible rather than
/// escaping it: drop `<`/`>` and control characters, collapse whitespace, cap
/// the length (char-boundary safe). `None` when nothing is left.
pub const MAX_TITLE_CHARS: usize = 120;

pub fn sanitize_title(title: &str) -> Option<String> {
    let cleaned: String = title
        .chars()
        .filter(|c| !matches!(c, '<' | '>'))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let capped: String = cleaned.chars().take(MAX_TITLE_CHARS).collect();
    let capped = capped.trim().to_string();
    (!capped.is_empty()).then_some(capped)
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
    fn hostile_html_is_neutralized_not_persisted() {
        // Stored-XSS shape from the #438 codex review: a paragraph followed
        // by an event-handler tag.
        let (_, html) = parse_composed_entry("<p>entry</p><img src=x onerror=alert(1)>");
        assert_eq!(html, "<p>entry</p>&lt;img src=x onerror=alert(1)&gt;");
        // Attributes on an allowed tag are not allowed either.
        assert_eq!(
            sanitize_paragraph_html("<p onclick=x>hi</p>"),
            "&lt;p onclick=x&gt;hi</p>"
        );
        // Case and self-closing forms of the allowed tags are normalized.
        assert_eq!(
            sanitize_paragraph_html("<P>Up</P><BR/>a<br>b"),
            "<p>Up</p><br />a<br />b"
        );
        // An unclosed tag cannot leak past the end of the body.
        assert_eq!(sanitize_paragraph_html("<p>a<b"), "<p>a&lt;b");
        // No paragraphs at all: still plain text, still escaped.
        assert_eq!(
            sanitize_paragraph_html("<script>alert(1)</script>"),
            "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>"
        );
    }

    #[test]
    fn titles_cannot_carry_markup_and_are_capped() {
        assert_eq!(
            sanitize_title("Friday <img src=x onerror=alert(1)> review").as_deref(),
            Some("Friday img src=x onerror=alert(1) review")
        );
        assert_eq!(sanitize_title("  <script>  ").as_deref(), Some("script"));
        assert_eq!(sanitize_title("<>"), None);
        assert_eq!(
            sanitize_title("tabs\tand\nnewlines").as_deref(),
            Some("tabs and newlines")
        );
        let long = "é".repeat(500);
        assert_eq!(
            sanitize_title(&long).unwrap().chars().count(),
            MAX_TITLE_CHARS
        );
    }

    #[test]
    fn composed_entry_without_title_falls_back() {
        let (title, html) = parse_composed_entry("plain reflection, no format");
        assert_eq!(title, None);
        assert_eq!(html, "<p>plain reflection, no format</p>");
    }
}
