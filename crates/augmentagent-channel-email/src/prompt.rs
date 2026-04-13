//! Prompt construction for per-email Claude reasoning calls.

use std::fs;
use std::path::{Path, PathBuf};

use augmentagent_store::Email;

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
    /// Matches Node's `loadLearnedPatterns`.
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
            let Ok(raw) = fs::read_to_string(&path) else { continue };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
            let Some(arr) = value.as_array() else { continue };
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
            format!("\n## Learned Patterns (from previous cycles)\n{}", sections.join("\n\n"))
        }
    }
}

/// Build the per-email user message that Claude reasons over.
pub fn user_message(email: &Email, learned: &str) -> String {
    format!(
        r#"Decide triage for the email below and return ONLY a single JSON object matching this schema:
{{
  "decision": "reply" | "skip" | "flag",
  "draft":    "<string, required if decision is reply>",
  "reason":   "<short string>"
}}

Do not include any prose outside the JSON object.{learned}

<email>
From: {from}
Subject: {subject}
Date: {date}
ThreadId: {thread_id}
MessageId: {message_id}

{body}
</email>
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
/// Mirrors src/agent.ts:redraftWithFeedback.
pub fn redraft_message(
    email: &Email,
    previous_draft: &str,
    feedback: &str,
) -> String {
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
