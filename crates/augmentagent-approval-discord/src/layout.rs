//! Block-kit-ish layout for approval messages.
//!
//! Serenity's model builders are verbose; we build simple component collections
//! here so the broker stays readable.

use augmentagent_store::Email;
use serenity::all::{
    ActionRowComponent, ButtonStyle, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedFooter,
    CreateInputText, CreateMessage, CreateModal, InputTextStyle,
};

use crate::custom_id::{CustomId, Verb};

const MAX_EMBED_DESCRIPTION: usize = 3800;
const SEPARATOR: &str = "\n\n— DRAFT —\n\n";

pub fn approval_message(action_id: &str, email: &Email, draft: &str) -> CreateMessage {
    let embed = CreateEmbed::new()
        .title(truncate(&email.subject, 256))
        .description(format_body(&email.body, draft))
        .field("From", truncate(&email.from, 256), true)
        .field("MessageId", truncate(&email.message_id, 256), true)
        .footer(CreateEmbedFooter::new("AugmentAgent approval"));

    let row = CreateActionRow::Buttons(vec![
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

    CreateMessage::new().embed(embed).components(vec![row])
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
