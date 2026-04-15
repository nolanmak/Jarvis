//! Prompt construction for per-email Claude reasoning calls.

use std::fs;
use std::path::{Path, PathBuf};

use augmentagent_store::Email;

/// System prompt for the triage-only call. Haiku-sized — no writing style
/// rules, no learned patterns, just decide-and-justify.
pub const TRIAGE_SYSTEM: &str = r#"You are an email triage classifier. For each email, decide:

- "reply"  — deserves a personal reply from the user
- "skip"   — automated, newsletter, no-reply, not actionable
- "flag"   — important but should be handled by a human without auto-reply (legal, financial, sensitive)

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
/// skip/flag patterns. Draft work is deferred to the second call.
pub fn triage_user_message(email: &Email, learned: &str) -> String {
    format!(
        "Classify this email.{learned}\n\n<email>\nFrom: {from}\nSubject: {subject}\nDate: {date}\nMessageId: {message_id}\n\n{body}\n</email>\n",
        from = email.from,
        subject = email.subject,
        date = email.date,
        message_id = email.message_id,
        body = email.body,
    )
}

/// Build the draft user message. `wiki_hint` is an optional pre-built string
/// naming wiki pages Claude may open; empty string disables the hint.
pub fn draft_user_message(email: &Email, wiki_hint: &str) -> String {
    let hint_block = if wiki_hint.trim().is_empty() {
        String::new()
    } else {
        format!("\n\n{wiki_hint}\n")
    };
    format!(
        r#"Draft a reply to this email. Follow the writing-style rules in your system prompt strictly.{hint_block}

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
