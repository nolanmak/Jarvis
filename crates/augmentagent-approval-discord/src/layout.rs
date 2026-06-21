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

/// Sentinel that fences the "needs your input" payload appended to the
/// persisted draft body by the email channel (#35 Phase 5). It is an HTML
/// comment so it never reaches a recipient even if a code path skipped the
/// strip, and its presence is the ONLY trigger for the card's needs-input
/// field + button. The off / shadow ask-resolve paths never emit it, so a
/// draft with no marker renders byte-identically to pre-#35.
///
/// Body format (one ask per line, between the open/close fences):
/// ```text
/// <!--aa:needs-input
/// share_doc|the Q4 board deck
/// intro|Sarah Chen
/// -->
/// ```
const NEEDS_INPUT_OPEN: &str = "<!--aa:needs-input\n";
const NEEDS_INPUT_CLOSE: &str = "\n-->";

/// One ask the resolver could not auto-fill, decoded from the draft marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsInput {
    /// Resolver-kind tag (`scheduling`, `share_doc`, …). Opaque to the card;
    /// only used to label the field row and key the modal.
    pub kind: String,
    /// The ask paraphrase, shown verbatim so the user knows what to supply.
    pub text: String,
}

/// Append the needs-input marker to a draft body for persistence. Called by
/// the email channel ONLY when live ask-resolve produced unresolved asks;
/// `asks` empty ⇒ the draft is returned unchanged (byte-identical path).
/// `(kind, text)` pairs are sanitized so the marker stays single-block and
/// round-trips through [`split_needs_input`].
pub fn append_needs_input_marker(draft: &str, asks: &[(String, String)]) -> String {
    if asks.is_empty() {
        return draft.to_string();
    }
    let mut out = String::with_capacity(draft.len() + 64 + asks.len() * 48);
    out.push_str(draft.trim_end());
    out.push_str("\n\n");
    out.push_str(NEEDS_INPUT_OPEN);
    for (kind, text) in asks {
        // `|` is the field delimiter and newlines end a record — strip both
        // from the payload so a pathological ask text can't break the frame.
        let k = sanitize_marker_field(kind);
        let t = sanitize_marker_field(text);
        out.push_str(&k);
        out.push('|');
        out.push_str(&t);
        out.push('\n');
    }
    // Trim the trailing record newline so CLOSE's leading `\n` is the only one.
    out.pop();
    out.push_str(NEEDS_INPUT_CLOSE);
    out
}

fn sanitize_marker_field(s: &str) -> String {
    s.replace(['|', '\n', '\r'], " ").trim().to_string()
}

/// Split a persisted draft into the human-facing draft and any needs-input
/// asks carried in the trailing marker. No marker ⇒ `(draft, vec![])`, so
/// every legacy / off-path draft is unaffected. Tolerant: a malformed marker
/// is treated as absent (the raw text is returned, never shown half-parsed).
pub fn split_needs_input(draft: &str) -> (String, Vec<NeedsInput>) {
    let Some(open_at) = draft.rfind(NEEDS_INPUT_OPEN) else {
        return (draft.to_string(), Vec::new());
    };
    let after_open = open_at + NEEDS_INPUT_OPEN.len();
    let Some(rel_close) = draft[after_open..].find(NEEDS_INPUT_CLOSE) else {
        return (draft.to_string(), Vec::new());
    };
    let body = &draft[after_open..after_open + rel_close];
    let mut asks = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((kind, text)) = line.split_once('|') {
            let kind = kind.trim();
            let text = text.trim();
            if !kind.is_empty() && !text.is_empty() {
                asks.push(NeedsInput {
                    kind: kind.to_string(),
                    text: text.to_string(),
                });
            }
        }
    }
    if asks.is_empty() {
        return (draft.to_string(), Vec::new());
    }
    let human = draft[..open_at].trim_end().to_string();
    (human, asks)
}

/// Render the user's supplied values as a structured Revise-feedback string.
/// Reuses the existing redraft path: the drafter re-writes the reply with the
/// concrete values substituted, exactly as if a resolver had filled them. The
/// `<resolved_asks>`-style framing keeps the anti-placeholder contract.
pub fn fill_feedback(filled: &[(NeedsInput, String)]) -> String {
    let mut s = String::from(
        "The user has supplied the previously-missing concrete values below. \
         Rewrite the draft so each is used EXACTLY as given. Do NOT write a \
         placeholder and do NOT say \"I'll send it shortly\". Keep everything \
         else about the draft the same.\n",
    );
    for (ask, value) in filled {
        s.push_str(&format!(
            "- {} (for \"{}\"): {}\n",
            label_for(&ask.kind),
            ask.text,
            value.trim()
        ));
    }
    s
}

/// Human label for a resolver-kind tag. Mirrors
/// `augmentagent_channel_core::ResolverKind::label` but kept local so the
/// approval crate doesn't take a dependency on channel-core (the dep edge
/// runs the other way).
fn label_for(kind: &str) -> &'static str {
    match kind {
        "scheduling" => "Proposed meeting time",
        "calendly" => "Booking link",
        "meeting_link" => "Video-call link",
        "share_doc" => "Document link",
        "intro" => "Introduction",
        _ => "Detail",
    }
}

/// Build the Approve / Reject button row that rides under a weekly invoice
/// draft card. Keyed by `draft_id` (an `invoice_drafts.id`), not an
/// `actions.id` — the event-handler arms route on the verb to keep them
/// off the content-draft path.
pub fn invoice_draft_components(draft_id: &str) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(CustomId::new(draft_id, Verb::InvoiceApprove).to_string())
            .label("Approve & Send")
            .style(ButtonStyle::Success),
        CreateButton::new(CustomId::new(draft_id, Verb::InvoiceRevise).to_string())
            .label("Revise")
            .style(ButtonStyle::Primary),
        CreateButton::new(CustomId::new(draft_id, Verb::InvoiceReject).to_string())
            .label("Reject")
            .style(ButtonStyle::Danger),
    ])]
}

/// Modal to edit an invoice email's subject + body, pre-filled with the
/// current values. Mirrors the reply card's `revise_modal`, but collects the
/// final text directly (no LLM redraft — an invoice email is a template). The
/// two inputs are read back in order by [`extract_invoice_email`].
pub fn invoice_revise_modal(draft_id: &str, subject: &str, body: &str) -> CreateModal {
    let subject_input = CreateInputText::new(InputTextStyle::Short, "Subject", "invoice_subject")
        .required(true)
        .max_length(200)
        .value(subject);
    let body_input = CreateInputText::new(InputTextStyle::Paragraph, "Email body", "invoice_body")
        .required(true)
        .max_length(2000)
        .value(body);
    CreateModal::new(
        CustomId::new(draft_id, Verb::InvoiceReviseModal).to_string(),
        "Revise invoice email",
    )
    .components(vec![
        CreateActionRow::InputText(subject_input),
        CreateActionRow::InputText(body_input),
    ])
}

/// Read (subject, body) back from an invoice-revise modal submission, by input
/// order (subject first, body second — matching [`invoice_revise_modal`]).
pub fn extract_invoice_email(rows: &[serenity::all::ActionRow]) -> (String, String) {
    let mut vals: Vec<String> = Vec::new();
    for row in rows {
        for c in &row.components {
            if let ActionRowComponent::InputText(input) = c {
                vals.push(input.value.clone().unwrap_or_default());
            }
        }
    }
    (
        vals.first().cloned().unwrap_or_default(),
        vals.get(1).cloned().unwrap_or_default(),
    )
}

/// Human-readable body for an invoice draft card. Kept separate from the
/// button row so the post helper can mix it with `CreateAttachment` of the
/// PDF in either a fresh `CreateMessage` or as an edit-source preamble.
#[allow(clippy::too_many_arguments)]
pub fn invoice_draft_content(
    number: u32,
    week_start: &str,
    week_end: &str,
    recipient: &str,
    subject: &str,
    body: &str,
    pdf_filename: &str,
) -> String {
    // Show the exact outgoing email (subject + body) the approver is about to
    // send, mirroring how the reply-approval card surfaces the draft (#346).
    // Body is rendered as a blockquote so newlines read cleanly in Discord.
    let body_quoted = if body.trim().is_empty() {
        "_(no body)_".to_string()
    } else {
        body.lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let subject_line = if subject.trim().is_empty() {
        String::new()
    } else {
        format!("**Subject:** {subject}\n")
    };
    format!(
        "**Invoice #{number} draft**\n\
         Week: {week_start} → {week_end}\n\
         To: `{recipient}`\n\
         {subject_line}\n\
         **Email body:**\n{body_quoted}\n\n\
         Attachment: `{pdf_filename}`\n\
         _Approve & Send to email it, Reject to discard._"
    )
}

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
    // Strip the needs-input marker BEFORE anything else so the human never
    // sees the raw `<!--aa:needs-input …-->` fence and the draft preview is
    // the real reply text. No marker ⇒ `human == draft`, byte-identical card.
    let (human_draft, needs) = split_needs_input(draft);
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
    let mut embed = CreateEmbed::new()
        .title(truncate(&email.subject, 256))
        .description(format_body(&email.body, &human_draft))
        .field("From", truncate(&email.from, 256), true)
        .field("MessageId", truncate(&email.message_id, 256), true);
    if !needs.is_empty() {
        embed = embed.field(
            "⚠️ Needs your input",
            truncate(&format_needs_input(&needs), 1024),
            false,
        );
    }
    let embed = embed.footer(CreateEmbedFooter::new(footer));

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
    // Row 2: the "Provide missing info" button — ONLY when the draft carries
    // unresolved asks. Sits on its own row so the existing quick-refine
    // StringSelect (also row-2-or-later) and the unchanged row-1 buttons are
    // untouched.
    if !needs.is_empty() {
        rows.push(CreateActionRow::Buttons(vec![CreateButton::new(
            CustomId::new(action_id, Verb::FillAsk).to_string(),
        )
        .label("Provide missing info")
        .style(ButtonStyle::Danger)]));
    }
    if !at_cap {
        rows.push(quick_refine_row(action_id));
    }
    CreateMessage::new().embed(embed).components(rows)
}

/// One bullet per unresolved ask for the card field.
fn format_needs_input(needs: &[NeedsInput]) -> String {
    let mut s = String::new();
    for n in needs {
        s.push_str("• **");
        s.push_str(label_for(&n.kind));
        s.push_str("** — ");
        s.push_str(n.text.trim());
        s.push('\n');
    }
    s.push_str("_Click **Provide missing info** to supply these; the draft re-renders with your values._");
    s
}

/// Modal that collects a value for each unresolved ask. One paragraph input
/// per ask (Discord caps modals at 5 inputs — resolver kinds top out at 5, so
/// this is always within budget; extra asks beyond 5 are dropped from the
/// modal but stay listed on the card field).
pub fn fill_ask_modal(action_id: &str, needs: &[NeedsInput]) -> CreateModal {
    let inputs: Vec<CreateActionRow> = needs
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, n)| {
            // custom_id of each input encodes the kind so the submit handler
            // can pair value→ask without relying on positional order.
            let input = CreateInputText::new(
                InputTextStyle::Paragraph,
                truncate(&format!("{} — {}", label_for(&n.kind), n.text), 45),
                format!("ask{i}:{}", n.kind),
            )
            .required(false)
            .placeholder("Paste the value, or leave blank to skip")
            .max_length(1000);
            CreateActionRow::InputText(input)
        })
        .collect();
    CreateModal::new(
        CustomId::new(action_id, Verb::FillAskModal).to_string(),
        "Provide missing info",
    )
    .components(inputs)
}

/// Pair submitted modal inputs back to their asks. Each input's `custom_id`
/// is `ask<i>:<kind>`; the value is the user's text. Blank values are
/// dropped (the user chose to skip that one). Returns `(NeedsInput, value)`
/// pairs ready for [`fill_feedback`].
pub fn extract_fill_values(
    rows: &[serenity::all::ActionRow],
    needs: &[NeedsInput],
) -> Vec<(NeedsInput, String)> {
    let mut out = Vec::new();
    for row in rows {
        for c in &row.components {
            if let ActionRowComponent::InputText(input) = c {
                let Some(val) = input.value.as_ref() else {
                    continue;
                };
                if val.trim().is_empty() {
                    continue;
                }
                // `ask<i>:<kind>` — recover the kind, then match by position
                // or kind back to the original ask for its text.
                let kind = input
                    .custom_id
                    .split_once(':')
                    .map(|(_, k)| k.to_string())
                    .unwrap_or_default();
                let idx = input
                    .custom_id
                    .strip_prefix("ask")
                    .and_then(|r| r.split_once(':'))
                    .and_then(|(i, _)| i.parse::<usize>().ok());
                let ask = idx
                    .and_then(|i| needs.get(i))
                    .filter(|a| a.kind == kind)
                    .or_else(|| needs.iter().find(|a| a.kind == kind))
                    .cloned();
                if let Some(ask) = ask {
                    out.push((ask, val.clone()));
                }
            }
        }
    }
    out
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

    // ---------------------------------------------------------------------
    // #35 Phase 5: needs-input marker round-trip + card UX.
    // ---------------------------------------------------------------------

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

    fn json(m: &CreateMessage) -> String {
        serde_json::to_string(m).expect("CreateMessage serializes")
    }

    #[test]
    fn no_marker_means_no_needs_input_and_unchanged_draft() {
        let (human, needs) = split_needs_input("Hi there,\n\nThanks!\n");
        assert_eq!(human, "Hi there,\n\nThanks!\n");
        assert!(needs.is_empty());
    }

    #[test]
    fn append_then_split_round_trips() {
        let draft = "Hi,\n\nHappy to help.\n\nBest,\nN";
        let asks = vec![
            ("share_doc".to_string(), "the Q4 board deck".to_string()),
            ("intro".to_string(), "Sarah Chen".to_string()),
        ];
        let marked = append_needs_input_marker(draft, &asks);
        assert!(marked.contains(NEEDS_INPUT_OPEN));
        let (human, needs) = split_needs_input(&marked);
        assert_eq!(human, draft.trim_end());
        assert_eq!(needs.len(), 2);
        assert_eq!(needs[0].kind, "share_doc");
        assert_eq!(needs[0].text, "the Q4 board deck");
        assert_eq!(needs[1].kind, "intro");
        assert_eq!(needs[1].text, "Sarah Chen");
    }

    #[test]
    fn append_with_no_asks_is_byte_identical() {
        let draft = "Just a normal reply.";
        assert_eq!(append_needs_input_marker(draft, &[]), draft);
    }

    #[test]
    fn marker_payload_with_pipe_and_newline_is_sanitized() {
        let asks = vec![(
            "share_doc".to_string(),
            "the | deck\nwith newlines".to_string(),
        )];
        let marked = append_needs_input_marker("body", &asks);
        let (_human, needs) = split_needs_input(&marked);
        assert_eq!(needs.len(), 1);
        // No raw pipe/newline survived into the decoded text.
        assert!(!needs[0].text.contains('|'));
        assert!(!needs[0].text.contains('\n'));
        assert!(needs[0].text.contains("deck"));
    }

    #[test]
    fn malformed_marker_is_treated_as_absent() {
        // Open fence but no close fence ⇒ return raw, no asks.
        let broken = format!("body\n{NEEDS_INPUT_OPEN}share_doc|x");
        let (human, needs) = split_needs_input(&broken);
        assert_eq!(human, broken);
        assert!(needs.is_empty());
    }

    #[test]
    fn approval_card_without_marker_is_byte_identical_to_legacy() {
        // The whole off-path guarantee: a marker-free draft must produce the
        // exact same card bytes as before #35 (no extra field, no extra row).
        let plain = "Hello,\n\nSounds good — see you then.\n\nBest,\nN";
        let with = json(&approval_message("act-1", &email(), plain, 0));
        // No needs-input field, no fill button.
        assert!(!with.contains("Needs your input"));
        assert!(!with.contains("Provide missing info"));
        assert!(!with.contains("fill_ask"));
        // Row-1 verbs intact.
        assert!(with.contains("aa:act-1:approve"));
        assert!(with.contains("aa:act-1:revise"));
        assert!(with.contains("aa:act-1:skip"));
        // The draft text is shown verbatim (no marker stripping artifacts).
        assert!(with.contains("Sounds good"));
    }

    #[test]
    fn approval_card_with_marker_adds_field_and_button_only() {
        let marked = append_needs_input_marker(
            "Hi,\n\nI'll get you that.\n\nN",
            &[("share_doc".into(), "the pitch deck".into())],
        );
        let v = json(&approval_message("act-2", &email(), &marked, 0));
        // New field + new button present.
        assert!(v.contains("Needs your input"));
        assert!(v.contains("Provide missing info"));
        assert!(v.contains("aa:act-2:fill_ask"));
        assert!(v.contains("Document link"));
        assert!(v.contains("the pitch deck"));
        // Row-1 still exactly Approve/Revise/Skip.
        assert!(v.contains("aa:act-2:approve"));
        assert!(v.contains("aa:act-2:revise"));
        assert!(v.contains("aa:act-2:skip"));
        // The raw marker fence must NOT leak into the rendered card.
        assert!(!v.contains("aa:needs-input"));
    }

    #[test]
    fn fill_ask_modal_has_one_input_per_ask_keyed_by_kind() {
        let needs = vec![
            NeedsInput {
                kind: "scheduling".into(),
                text: "a 30-min call next week".into(),
            },
            NeedsInput {
                kind: "share_doc".into(),
                text: "the data room".into(),
            },
        ];
        let modal = fill_ask_modal("act-3", &needs);
        let v = serde_json::to_string(&modal).expect("modal serializes");
        assert!(v.contains("aa:act-3:fill_ask_modal"));
        assert!(v.contains("ask0:scheduling"));
        assert!(v.contains("ask1:share_doc"));
    }

    #[test]
    fn fill_feedback_lists_each_value_and_anti_placeholder_rule() {
        let filled = vec![
            (
                NeedsInput {
                    kind: "share_doc".into(),
                    text: "the Q4 deck".into(),
                },
                "https://drive.google.com/file/Q4".to_string(),
            ),
            (
                NeedsInput {
                    kind: "intro".into(),
                    text: "Sarah Chen".into(),
                },
                "Sure, happy to connect you.".to_string(),
            ),
        ];
        let fb = fill_feedback(&filled);
        assert!(fb.contains("Do NOT write a"));
        assert!(fb.contains("EXACTLY as given"));
        assert!(fb.contains("Document link"));
        assert!(fb.contains("the Q4 deck"));
        assert!(fb.contains("drive.google.com/file/Q4"));
        assert!(fb.contains("Introduction"));
        assert!(fb.contains("Sarah Chen"));
    }

    #[test]
    fn label_for_covers_all_kinds() {
        assert_eq!(label_for("scheduling"), "Proposed meeting time");
        assert_eq!(label_for("calendly"), "Booking link");
        assert_eq!(label_for("meeting_link"), "Video-call link");
        assert_eq!(label_for("share_doc"), "Document link");
        assert_eq!(label_for("intro"), "Introduction");
        assert_eq!(label_for("???"), "Detail");
    }
}
