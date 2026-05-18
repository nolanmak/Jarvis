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

/// Hard character cap on the rendered `<thread_history>` block (#32 Phase 1).
/// ~12k chars ≈ 3k tokens — generous for "last ~5 messages" without blowing
/// the per-draft budget. The fetch layer already trims to the last K messages;
/// this is the belt-and-suspenders byte ceiling the issue calls for.
pub const THREAD_HISTORY_CHAR_CAP: usize = 12_000;

/// Render a verbatim `<thread_history>` block from prior messages in the same
/// Gmail thread (oldest-first), newest-biased truncation.
///
/// `messages` is `(from, date, body)` tuples, chronological. We emit them in
/// order but, when over `THREAD_HISTORY_CHAR_CAP`, drop the OLDEST entries
/// first — recent turns carry the most signal for "what was already said".
/// Returns `String::new()` when there's nothing to show (no prior messages),
/// which `draft_user_message` interprets as "no thread block" — keeping the
/// prompt byte-identical to today's single-message behavior.
pub fn format_thread_history(messages: &[(String, String, String)]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    // Build per-message chunks, then keep as many of the most-recent as fit.
    let chunks: Vec<String> = messages
        .iter()
        .map(|(from, date, body)| {
            format!("--- message ---\nFrom: {from}\nDate: {date}\n\n{}\n", body.trim())
        })
        .collect();
    let mut kept: Vec<&String> = Vec::new();
    let mut total = 0usize;
    for chunk in chunks.iter().rev() {
        if total + chunk.len() > THREAD_HISTORY_CHAR_CAP {
            break;
        }
        total += chunk.len();
        kept.push(chunk);
    }
    if kept.is_empty() {
        return String::new();
    }
    kept.reverse();
    let mut out = String::with_capacity(total + 48);
    out.push_str("<thread_history>\n");
    for chunk in kept {
        out.push_str(chunk);
    }
    out.push_str("</thread_history>\n");
    out
}

/// Build the draft user message. `wiki_hint` is an optional pre-built string
/// naming wiki pages Claude may open; empty string disables the hint.
///
/// `tone_block` is the per-recipient/domain/global tone descriptor injected
/// into the draft prompt as a stable prefix (cache-friendly). Empty string =
/// no tone injection (today's behavior).
///
/// `thread_block` is a pre-rendered `<thread_history>` string (see
/// [`format_thread_history`]) or empty for no thread context (#32). It sits at
/// a STABLE position right after the tone block and before the wiki hint —
/// the prompt-cache prefix (system + tone) is unaffected, and an empty
/// `thread_block` makes the rendered prompt byte-identical to pre-#32 output.
///
/// `archetype_block` is a pre-rendered `<draft_archetype>` fragment (see
/// [`crate::archetype::fragment_block`]) or empty for none (#36). It is
/// composed AFTER tone and thread history — closest to the new inbound
/// message so it anchors the reply structure without disturbing the
/// tone+system cache prefix. Empty = byte-identical to pre-#36 output.
pub fn draft_user_message(
    email: &Email,
    wiki_hint: &str,
    tone_block: &str,
    thread_block: &str,
    archetype_block: &str,
) -> String {
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
    // Thread history sits AFTER the tone block (so the cache prefix stays
    // tone+system) and BEFORE the wiki hint / new inbound message. Empty =
    // nothing emitted, exactly today's output.
    let thread = if thread_block.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n{thread_block}\nThe block above is the prior conversation in this thread, oldest first. Do not repeat questions already answered, honor commitments the user already made, and acknowledge anything already sent.\n"
        )
    };
    // Archetype fragment is composed last among the context blocks — closest
    // to the inbound message so it anchors structure. Empty = byte-identical
    // to pre-#36 output.
    let archetype = if archetype_block.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n{archetype_block}\nUse the archetype above to anchor the STRUCTURE and intent of the reply. Write natural prose in the user's voice — do not output the slot placeholders literally; fill them from the email, or omit cleanly if unknown.\n"
        )
    };
    format!(
        r#"Draft a reply to this email. Follow the writing-style rules in your system prompt strictly.{tone}{thread}{archetype}{hint_block}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn email() -> Email {
        Email {
            message_id: "m1".into(),
            thread_id: Some("t1".into()),
            from: "a@b.com".into(),
            subject: "Re: hi".into(),
            body: "the inbound message".into(),
            date: "2026-05-18T00:00:00Z".into(),
            account_entity_id: Some("acc".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    #[test]
    fn empty_blocks_are_byte_identical_to_legacy_output() {
        // The whole cache-safety argument rests on this: empty tone/thread/
        // archetype must produce exactly the pre-#32/#36 prompt.
        let got = draft_user_message(&email(), "", "", "", "");
        assert!(got.starts_with(
            "Draft a reply to this email. Follow the writing-style rules in your system prompt strictly.\n\n<email>"
        ));
        assert!(!got.contains("<tone_profile>"));
        assert!(!got.contains("<thread_history>"));
        assert!(!got.contains("<draft_archetype"));
        assert!(got.contains("the inbound message"));
    }

    #[test]
    fn tone_prefix_is_stable_regardless_of_thread_or_archetype() {
        // Prompt-cache keys on prefix. The bytes from the start through the
        // end of <tone_profile> must be invariant whether or not thread /
        // archetype blocks are present.
        let tone = "voice: terse";
        let a = draft_user_message(&email(), "", tone, "", "");
        let b = draft_user_message(
            &email(),
            "",
            tone,
            "<thread_history>\n--- message ---\nx\n</thread_history>\n",
            "<draft_archetype id=\"decline\">\nintent\n</draft_archetype>",
        );
        let marker = "</tone_profile>";
        let a_pre = &a[..a.find(marker).unwrap() + marker.len()];
        let b_pre = &b[..b.find(marker).unwrap() + marker.len()];
        assert_eq!(a_pre, b_pre, "tone+system prefix must be cache-stable");
    }

    #[test]
    fn thread_then_archetype_order_after_tone() {
        let out = draft_user_message(
            &email(),
            "",
            "voice",
            "<thread_history>\nT\n</thread_history>\n",
            "<draft_archetype id=\"fyi\">\nF\n</draft_archetype>",
        );
        let i_tone = out.find("<tone_profile>").unwrap();
        let i_thread = out.find("<thread_history>").unwrap();
        let i_arch = out.find("<draft_archetype").unwrap();
        let i_email = out.find("<email>").unwrap();
        assert!(i_tone < i_thread && i_thread < i_arch && i_arch < i_email);
    }

    #[test]
    fn format_thread_history_empty_for_no_messages() {
        assert_eq!(format_thread_history(&[]), "");
    }

    #[test]
    fn format_thread_history_drops_oldest_over_cap() {
        let big = "x".repeat(8_000);
        let msgs = vec![
            ("old@x.com".into(), "d1".into(), big.clone()),
            ("mid@x.com".into(), "d2".into(), big.clone()),
            ("new@x.com".into(), "d3".into(), big.clone()),
        ];
        let out = format_thread_history(&msgs);
        assert!(out.len() <= THREAD_HISTORY_CHAR_CAP + 64);
        // Newest must survive; oldest dropped.
        assert!(out.contains("new@x.com"));
        assert!(!out.contains("old@x.com"));
        assert!(out.starts_with("<thread_history>"));
    }

    #[test]
    fn format_thread_history_preserves_chronology() {
        let msgs = vec![
            ("first@x.com".into(), "d1".into(), "earliest".into()),
            ("second@x.com".into(), "d2".into(), "latest".into()),
        ];
        let out = format_thread_history(&msgs);
        assert!(out.find("first@x.com").unwrap() < out.find("second@x.com").unwrap());
    }
}
