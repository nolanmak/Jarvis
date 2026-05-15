//! Prompt construction for per-email Claude reasoning calls.

use std::fs;
use std::path::{Path, PathBuf};

use augmentagent_store::Email;

/// System prompt for the triage-only call. Decides whether the user should
/// hear about this email today, and if so, whether a reply is expected.
pub const TRIAGE_SYSTEM: &str = r#"You are an email triage classifier for a busy person's inbox. For each email, pick exactly one:

- "reply"  — the sender expects a response from the user (direct question, meeting ask, request, follow-up on something the user started)
- "flag"   — the user should know about this today even though a reply isn't strictly needed (personal message from a known contact, meeting confirmation, update on an active project, anything important/time-sensitive that isn't automated noise)
- "skip"   — pure noise the user can ignore (marketing, newsletter, receipt, OTP, calendar invite auto-ack, shipping notification, LinkedIn/social digest, no-reply automated sender)

Tie-break rules:
- When in doubt between "skip" and "flag", choose "flag".
- When in doubt between "flag" and "reply", choose "reply".
- A known person writing personally (professor, friend, colleague, founder, investor) defaults to at least "flag" even if their ask is "just FYI".
- Automated emails from "no-reply", "notifications", "alerts", "deals", "updates", "newsletter", marketing platforms default to "skip" unless the content is clearly a direct action item.

Using wiki context (when provided):
- You may be given a short hint pointing at a wiki people/ or threads/ page.
- If the sender has a wiki page, OPEN IT with the Read tool before deciding. Its Relationship/Tone/Commitments sections are ground truth on how important this person is to the user — weight the decision accordingly.
- A message from someone with a documented active-collaborator or close-contact relationship should escalate: skip → flag, or flag → reply.
- You may Grep/Glob the wiki for project names, organization names, or keywords you see in the subject/body. Useful for catching cases where the sender isn't in the wiki yet but the topic is (e.g. a new contact emailing about a project the user is known to be active on).
- Prefer wiki-documented context over surface-level pattern matching. "Volunteer sign-up form" from a documented colleague is not the same as the same subject from a stranger.

Return ONLY a single JSON object with this exact shape:
  {"decision": "reply" | "skip" | "flag", "reason": "<one short sentence>"}

No prose, no markdown fences, no extra fields.
"#;

pub struct SkillPrompt {
    pub system: String,
    pub skill_dir: PathBuf,
}

impl SkillPrompt {
    /// Load skills/email-triage/SKILL.md. Returns an empty system prompt if missing
    /// (matches Node's behavior in src/agent.ts:loadSkillFile).
    pub fn load(skill_dir: impl AsRef<Path>) -> Self {
        let skill_dir = skill_dir.as_ref().to_path_buf();
        let system = fs::read_to_string(skill_dir.join("SKILL.md")).unwrap_or_default();
        Self { system, skill_dir }
    }

    /// Gather all `learned/*.json` patterns into a single annotated block.
    pub fn load_learned(&self) -> String {
        let dir = self.skill_dir.join("learned");
        let Ok(entries) = fs::read_dir(&dir) else {
            return String::new();
        };
        let mut sections = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
                continue;
            };
            let Some(arr) = value.as_array() else {
                continue;
            };
            if arr.is_empty() {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("patterns");
            sections.push(format!(
                "### {name}:\n{}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            ));
        }
        if sections.is_empty() {
            String::new()
        } else {
            format!(
                "\n## Learned Patterns (from previous cycles)\n{}",
                sections.join("\n\n")
            )
        }
    }
}

/// Build the triage user message. Minimal — just the email + any learned
/// skip/flag patterns + optional wiki hint. Draft work is deferred to the
/// second call.
pub fn triage_user_message(email: &Email, learned: &str, wiki_hint: &str) -> String {
    let hint_block = if wiki_hint.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n<wiki_hint>\n{wiki_hint}\n</wiki_hint>")
    };
    format!(
        "Classify this email.{learned}{hint_block}\n\n<email>\nFrom: {from}\nSubject: {subject}\nDate: {date}\nMessageId: {message_id}\n\n{body}\n</email>\n",
        from = email.from,
        subject = email.subject,
        date = email.date,
        message_id = email.message_id,
        body = email.body,
    )
}

/// Build the draft user message. `wiki_hint` is an optional pre-built string
/// naming wiki pages Claude may open; empty string disables the hint.
///
/// `tone_block` is the per-recipient/domain/global tone descriptor injected
/// into the draft prompt as a stable prefix (cache-friendly). Empty string =
/// no tone injection (today's behavior). Real injection lands in #73.
pub fn draft_user_message(email: &Email, wiki_hint: &str, tone_block: &str) -> String {
    let hint_block = if wiki_hint.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{wiki_hint}\n")
    };
    // IMPORTANT: tone block sits BEFORE the email body so it occupies a stable
    // prefix position. Claude prompt-caching keys on prefix; keeping the tone
    // block fixed-position across drafts lets the cache hit on subsequent
    // drafts within the same session.
    let tone = if tone_block.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n<tone_profile>\n{tone_block}\n</tone_profile>\n\nMatch the voice in <tone_profile>. When verbatim opener/closer examples appear, weight them heavily — sign off the way the user actually signs off to this recipient.\n"
        )
    };
    format!(
        r#"Draft a reply to this email. Follow the writing-style rules in your system prompt strictly.{tone}{hint_block}

<email>
From: {from}
Subject: {subject}
Date: {date}
ThreadId: {thread_id}
MessageId: {message_id}

{body}
</email>

Return ONLY the reply text — no JSON, no quotes, no commentary, no subject line.
"#,
        from = email.from,
        subject = email.subject,
        date = email.date,
        thread_id = email.thread_id.as_deref().unwrap_or("(none)"),
        message_id = email.message_id,
        body = email.body,
    )
}

/// Build the redraft prompt when the user clicks "Revise" in Discord.
pub fn redraft_message(email: &Email, previous_draft: &str, feedback: &str) -> String {
    format!(
        r#"You are a professional email draft editor. Revise the draft based on the user's feedback and return ONLY the revised email text — no JSON, no quotes, no commentary.

<original_email>
From: {from}
Subject: {subject}

{body}
</original_email>

<previous_draft>
{previous_draft}
</previous_draft>

<user_feedback>
{feedback}
</user_feedback>

Write the revised draft now.
"#,
        from = email.from,
        subject = email.subject,
        body = email.body,
    )
}
