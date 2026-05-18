//! Block-kit-ish layout for approval messages.
//!
//! Serenity's model builders are verbose; we build simple component collections
//! here so the broker stays readable.

use augmentagent_store::Email;
use serenity::all::{
    ActionRowComponent, ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter,
    CreateInputText, CreateModal, CreateMessage, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, InputTextStyle,
};

use crate::custom_id::{CustomId, Verb};
use crate::presets::{MAX_REDRAFT_ITERATIONS, PRESETS};

const MAX_EMBED_DESCRIPTION: usize = 3800;
const SEPARATOR: &str = "\n\n— DRAFT —\n\n";

/// Plain-text "heads up" card for triage-flagged emails. No buttons, no draft.
pub fn flag_notice_message(email: &Email, reason: &str) -> CreateMessage {
    let subject = truncate(&email.subject, 256);
    let from = truncate(&email.from, 200);
    let reason = truncate(reason, 500);
    let content = format!(
        "🚩 **Important** — from `{from}`\n**{subject}**\n_reason: {reason}_"
    );
    CreateMessage::new().content(content)
}

/// Build an approval card. `redraft_count` is how many times this draft has
/// already been refined (0 on first post). When the count is under
/// [`MAX_REDRAFT_ITERATIONS`] a second action row — the "Quick refine…"
/// `StringSelect` (#34) — is attached so presets can stack. At/above the cap
/// the menu is dropped (the footer says so) and only Approve/Revise/Skip
/// remain, forcing a terminal action or free-form Revise.
pub fn approval_message(
    action_id: &str,
    email: &Email,
    draft: &str,
    redraft_count: i64,
) -> CreateMessage {
    let at_cap = redraft_count >= MAX_REDRAFT_ITERATIONS;
    let footer = if redraft_count == 0 {
        "AugmentAgent approval".to_string()
    } else if at_cap {
        format!(
            "AugmentAgent approval · draft v{} · refine cap reached — Approve/Skip or use Revise",
            redraft_count + 1
        )
    } else {
        format!("AugmentAgent approval · draft v{}", redraft_count + 1)
    };
    let embed = CreateEmbed::new()
        .title(truncate(&email.subject, 256))
        .description(format_body(&email.body, draft))
        .field("From", truncate(&email.from, 256), true)
        .field("MessageId", truncate(&email.message_id, 256), true)
        .footer(CreateEmbedFooter::new(footer));

    let button_row = CreateActionRow::Buttons(vec![
        CreateButton::new(CustomId::new(action_id, Verb::Approve).to_string())
            .label("Approve & Send")
            .style(ButtonStyle::Success),
        CreateButton::new(CustomId::new(action_id, Verb::Revise).to_string())
            .label("Revise")
            .style(ButtonStyle::Primary),
        CreateButton::new(CustomId::new(action_id, Verb::Skip).to_string())
            .label("Skip")
            .style(ButtonStyle::Secondary),
    ]);

    let mut rows = vec![button_row];
    if !at_cap {
        rows.push(quick_refine_row(action_id));
    }
    CreateMessage::new().embed(embed).components(rows)
}

/// The "Quick refine…" `StringSelect` action row (#34). Each option's value is
/// a preset id; the handler maps it to a canned `redraft_message` feedback
/// string. Re-attached on every re-render so presets stack until the cap.
fn quick_refine_row(action_id: &str) -> CreateActionRow {
    let options: Vec<CreateSelectMenuOption> = PRESETS
        .iter()
        .map(|p| CreateSelectMenuOption::new(p.label, p.id))
        .collect();
    let menu = CreateSelectMenu::new(
        CustomId::new(action_id, Verb::QuickRefine).to_string(),
        CreateSelectMenuKind::String { options },
    )
    .placeholder("Quick refine… (no typing)")
    .min_values(1)
    .max_values(1);
    CreateActionRow::SelectMenu(menu)
}

pub fn revise_modal(action_id: &str, previous_feedback: Option<&str>) -> CreateModal {
    let input = CreateInputText::new(InputTextStyle::Paragraph, "Revision feedback", "feedback")
        .required(true)
        .placeholder("What should change about the draft?")
        .max_length(1500)
        .value(previous_feedback.unwrap_or(""));

    CreateModal::new(
        CustomId::new(action_id, Verb::ReviseModal).to_string(),
        "Revise draft",
    )
    .components(vec![CreateActionRow::InputText(input)])
}

fn format_body(email_body: &str, draft: &str) -> String {
    let budget = MAX_EMBED_DESCRIPTION.saturating_sub(SEPARATOR.len());
    let half = budget / 2;
    let email_part = truncate(email_body, half);
    let draft_part = truncate(draft, budget - email_part.len());
    format!("{email_part}{SEPARATOR}{draft_part}")
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max.saturating_sub(3);
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Extract the user-entered feedback from a submitted modal. Returns `None`
/// if the modal didn't contain a text input (should not happen with our layout).
pub fn extract_feedback(rows: &[serenity::all::ActionRow]) -> Option<String> {
    for row in rows {
        for c in &row.components {
            if let ActionRowComponent::InputText(input) = c {
                if let Some(v) = &input.value {
                    return Some(v.clone());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "aéb"; // é = 2 bytes, total 4 bytes
        let t = truncate(s, 3);
        assert!(t.is_char_boundary(t.len()));
    }

    #[test]
    fn format_body_fits_budget() {
        let big = "a".repeat(10_000);
        let out = format_body(&big, &big);
        assert!(out.len() <= MAX_EMBED_DESCRIPTION);
        assert!(out.contains(SEPARATOR));
    }
}
